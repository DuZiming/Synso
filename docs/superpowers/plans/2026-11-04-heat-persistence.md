# 热度持久化实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 三层缓存的 Heat 落库 SQLite 小表,启动/重扫后按衰减分回填热区;删除 CMS 死重;`preload_on_start` 兑现为真启动预热。

**Architecture:** store 增加 schema v4 的 `heat` 小表与全量快照替换写;tiers 增加带分回填 API 与墙钟衰减助手,删除只写不读的 count-min sketch;engine 增加 `persist_heat`(定期线程 + 重扫后 + 退出钩子三个触发)与启动回填线程;app 层接 Tauri 退出事件并更新两处文案。

**Tech Stack:** Rust(rusqlite、std mpsc)、Tauri v2、Svelte i18n(zh/en)。

**Spec:** `docs/superpowers/specs/2026-11-04-heat-persistence-design.md`(含已拍板决策与评审折入的三条裁定,实施时随行阅读)

## Global Constraints

- 不新增依赖、不新增设置项;常量:`HEAT_MAX_ROWS: usize = 10_000`、`HEAT_PERSIST_INTERVAL: Duration = 10 分钟`。
- 地板分单位:存储列为 ×256 定点分,两端过滤一律 `raw / 256 >= hot_threshold`(与晋升门同单位)。
- 持久化全程 best-effort:任何失败记日志跳过,不影响正确性路径;退出钩子对 store 锁 **try_lock,拿不到即跳过**(spec §2 裁定 1)。
- 带分回填契约:入热接管 staging 孪生;分数与已有条目取 max(spec §4 裁定 2)。
- 提交信息:conventional 前缀 + 中文摘要,末尾加 `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`。
- 验收一律 release 口径;用户免安装版固定 `C:\Users\admin\Desktop\Synso\target\release\synso-app.exe`(本仓 target,不在任何工作树)。
- 注释密度与风格对齐既有代码(密集文档注释、中文设计词汇混合英文标识符)。

## Review Focus

1. **中扫退出**:store 锁被整场扫描持有时,退出落库必须跳过而非阻塞主线程 → 测试 `exit_persist_skips_when_store_locked`(Task 3)。
2. **时钟回拨/未来 `at`**:离线时长为负时按 0 处理,分数不变、不 panic → 测试 `offline_decay_drops_below_floor` 的未来时间戳分支(Task 3)。
3. **陈旧 id**(文件已删/row id 复用):冷读缺行静默消失,不报错不入热 → 测试 `stale_id_absent_in_preheat`(Task 3)。
4. **回填撞活状态**(重启后秒开、已被 `put_staging` 播 0 分):max 合并 + staging 接管,不产生双驻留/0 分热居民 → Task 2 两条测试。
5. **地板分单位两处一致**:一次命中(raw 256,÷256=1 < 阈值 8)不得落库;回填端同样过滤 → 测试 `floor_units_one_touch_not_persisted`(Task 3)+ Task 3 回填断言。

---

### Task 1: schema v4 + Store::replace_heat + ColdReader::read_heat + 墙钟衰减助手

**Files:**
- Modify: `crates/synso-core/src/store.rs`(SCHEMA_V3_SQL 之后、`SCHEMA_VERSION` 一带,及 impl Store;测试在同文件 `mod tests`)
- Modify: `crates/synso-core/src/dimensions.rs`(impl ColdReader)
- Modify: `crates/synso-core/src/tiers.rs`(Heat::decay 抽出助手)

**Interfaces:**
- Produces:
  - `const SCHEMA_V4_SQL: &str` — `CREATE TABLE heat(file_id INTEGER PRIMARY KEY, score INTEGER NOT NULL, at INTEGER NOT NULL);`
  - `SCHEMA_VERSION: i64 = 4`;`migrate()` 增加 `if version < 4` 台阶(纯 DDL,无回填)
  - `Store::replace_heat(&mut self, rows: &[(FileId, u32, i64)]) -> Result<()>` — 一个事务内 `DELETE FROM heat` + 预编译 INSERT 全量替换;`FileId` 按 model 现定义(v1.2 后 u32)转换入库
  - `ColdReader::read_heat(&self, limit: usize) -> Result<Vec<(FileId, u32, i64)>>` — `SELECT file_id, score, at FROM heat ORDER BY score DESC LIMIT ?1`
  - `tiers::decay_score(score: u32, elapsed: Duration, half_life: Duration) -> u32`(**pub(crate)**,engine 墙钟侧要用)— 从 `Heat::decay` 抽出的半衰公式:**round 不截断**(256×(1−ε) 必须仍是 256);`half_life` 零或 `elapsed` 零返回原值;`Heat::decay` 改为调用它

- [ ] **Step 1: 写失败测试**(store tests,仿 `meta_roundtrip` 模式)
  - `heat_snapshot_replaces`:临时库 → `replace_heat(&[(a, 300, 10)])` → 直连 `conn` 断言 1 行 → `replace_heat(&[(b, 500, 20)])` → 断言只剩 b 一行(全量替换,旧行消失)
  - `heat_migration_reopen_noop`:开库两次,断言 `PRAGMA user_version == 4` 且第二次打开不报错
  - (dimensions,若该文件无测试模块则测试放 store tests 里用临时库走 ColdReader)`read_heat_orders_and_limits`:写入 (1, 100, t) (2, 500, t) (3, 300, t),`read_heat(2)` 返回 [(2,500),(3,300)]
  - (tiers tests)`decay_score_rounds_never_truncates`:`decay_score(256, 1ms, 24h) == 256`;`decay_score(256, 0s, 24h) == 256`;`decay_score(1024, 2×half, 1×half) == 256`;`decay_score(100, 1h, 0) == 100`
- [ ] **Step 2: 跑测试确认失败** — `cargo test -p synso-core heat 2>&1 | tail -20`,预期编译错误(方法/常量不存在)
- [ ] **Step 3: 实现** — 按 Interfaces 三处落地;`replace_heat` 用 `self.conn.transaction()` + 预编译语句
- [ ] **Step 4: 跑测试确认通过** — `cargo test -p synso-core`,预期全绿(既有 store/engine 测试随 schema 升级不应受影响——若存在断言 user_version==3 的测试,改为 4)
- [ ] **Step 5: 提交** — `feat(core): schema v4 heat 表 + 快照替换写 + 冷读 + 墙钟衰减助手`

### Task 2: tiers — put_hot_direct_with_score + 删除 CMS

**Files:**
- Modify: `crates/synso-core/src/tiers.rs`

**Interfaces:**
- Produces:
  - `pub fn put_hot_direct_with_score(&mut self, key: K, value: V, bytes: u64, score: u32)` — 预算驱逐同 `put_hot_direct`;入热后**从 staging 移除孪生**(staging_bytes 相应扣减);Heat 合并:无条目 → 播种 `Heat { score, at: now, window_hits: 1, window_started: now }`;有条目 → 先衰减到 now,分数取 `max(existing, score)`,窗口字段保留
  - `pub fn put_hot_direct(&mut self, key: K, value: V, bytes: u64)` — 保留,委托 `put_hot_direct_with_score(key, value, bytes, 0)`(既有测试与兼容面)
  - CMS 删除:`cms` 字段、`cms_bump`、`cms_hash`、`CMS_ROWS`、`CMS_WIDTH`、模块头与 `record_hit` 文档里的草图表述(注释改为只描述 Heat 记账)

- [ ] **Step 1: 写失败测试**(tiers tests)
  - `with_score_supersedes_staging_and_merges_max`:`put_staging("k")` 后 `record_hit` 数次(现场分 < 传入分),`put_hot_direct_with_score("k", v, 10, 8*256)` → `get_hot` 命中、`staging_entries == 0`、`hot_bytes == 10`(无双份计账);再 `record_hit` 返回 ≥ 8(播种分数生效)
  - `with_score_does_not_clobber_higher_live`:现场把分数打到 > 传入分(如 20 次命中),`put_hot_direct_with_score(…, 8*256)` → `record_hit` 返回 ≥ 20(max 保留现场分)
- [ ] **Step 2: 跑测试确认失败** — `cargo test -p synso-core with_score`,预期编译错误
- [ ] **Step 3: 实现** — 按 Interfaces;CMS 五处一并删除
- [ ] **Step 4: 跑测试确认通过** — `cargo test -p synso-core`,预期全绿
- [ ] **Step 5: 提交** — `perf(core): 带分回填 API(max 合并/接管 staging)+ 删除只写不读的 CMS(−512KB)`

### Task 3: engine — 落库、启动回填、重扫携带分数、统一重复逻辑、两个线程

**Files:**
- Modify: `crates/synso-core/src/engine.rs`(Engine 字段、`open`、`start_full_scan` 伴生线程约 300-365 行、`flush_dimensions_and_preheat` 约 595 行;测试 `mod tests`,夹具 `detail_fixture`/`open_engine`/`wait_scan_finished` 已有)
- Modify: `crates/synso-core/src/tiers.rs`(两个快照 API)

**Interfaces:**
- Consumes: Task 1 的 `replace_heat`/`read_heat`/`decay_score`,Task 2 的 `put_hot_direct_with_score`
- Produces(tiers):
  - `pub fn heat_above_floor(&mut self, floor_hits: u32) -> Vec<(K, u32)>` — 每条先衰减到 now,返回 `raw/256 >= floor_hits` 的 `(key, raw)` 按分数降序(副作用:全体 heat.at 刷新,正确无害)
  - `pub fn hot_ids_with_scores(&mut self) -> Vec<(K, u32)>` — 热区居民的 `(key, 衰减分)`
- Produces(engine):
  - `const HEAT_MAX_ROWS: usize = 10_000;`、`const HEAT_PERSIST_INTERVAL: Duration = Duration::from_secs(600);`
  - `fn persist_heat_into(store: &mut Store, tiers: &Arc<Mutex<TierCache<FileId, FileDetail>>>)` — 取锁 tiers → `heat_above_floor(policy.hot_threshold)` 截前 `HEAT_MAX_ROWS` → epoch 秒 → `store.replace_heat`;错误 `log::warn!` 跳过
  - `pub fn Engine::persist_heat(&self)` — 阻塞锁 store 后调 `persist_heat_into`
  - `pub fn Engine::persist_heat_on_exit(&self)` — `self.store.try_lock()`,`WouldBlock`/`Poisoned` 均记 debug 日志直接返回
  - `fn reinsert_with_scores(tiers: &mut TierCache<FileId, FileDetail>, cold: &ColdReader, scored: &[(FileId, u32)])` — 分批(500)冷读 `file_details`,存活行 `put_hot_direct_with_score(id, detail, cached_bytes(), score)`
  - `fn flush_snapshot(tiers: &mut TierCache<FileId, FileDetail>) -> Option<Vec<(FileId, u32)>>` — `preload_on_start` 为假返回 None;否则 `hot_ids_with_scores` + `clear()`
  - `fn rescan_flush_thread_side(tiers: &Arc<Mutex<…>>, store: &Arc<Mutex<Store>>, db_path: &Path)` — 自开 ColdReader:flush_snapshot → reinsert → `persist_heat_into`(替代 `start_full_scan` 伴生线程的内联重复体)
  - `fn preheat_thread_side(tiers: &Arc<Mutex<…>>, db_path: &Path)` — 自开 ColdReader → `read_heat(HEAT_MAX_ROWS)` → 读 policy(`preload_on_start` 假则直接返回,表不动)→ 每行 `elapsed = now_epoch.saturating_sub(at)`、`decay_score(score, elapsed, decay_half_life)` → 过滤 `÷256 < hot_threshold` → `reinsert_with_scores`
  - `Engine::open`:hot-load 线程之后追加 spawn `synso-heat-preheat`(克隆 tiers Arc + db_path,调 `preheat_thread_side`)与 `synso-heat-persist`(克隆 tiers/store Arc;`mpsc::channel<()>`,`recv_timeout(HEAT_PERSIST_INTERVAL)` → Timeout 即 `persist_heat_into`,Disconnected 即退出);Engine 新增字段 `heat_keepalive: Mutex<Option<Sender<()>>>` 持有发送端保证连接(Engine 须保持 Sync)
  - `Engine::flush_dimensions_and_preheat(&self)` — 改为:`flush_snapshot`(经 self.tiers)→ `reinsert_with_scores`(经 self.cold)→ `self.persist_heat()`

- [ ] **Step 1: 写失败测试**(engine tests)
  - `persist_then_preheat_restores_heat`:`detail_fixture` → policy `hot_threshold: 1, promote_hits: 2` → `file_detail`×2 使 id 入热 → `persist_heat` → 同库**新开 Engine** → `preheat_thread_side(&new.tiers, db)` → 断言新引擎 `get_hot` 命中该 id
  - `offline_decay_drops_below_floor`:直写 `replace_heat(&[(id, 8*256, now_epoch − 3×half_life)])`(half_life 取该 policy 值)→ `preheat_thread_side` → id **不在**热区;同分 `at = now` → 在热区;`at = now + 3600`(未来)→ 在热区且不 panic(Review Focus 2)
  - `stale_id_absent_in_preheat`:`replace_heat(&[(999_999, 100*256, now)])` → `preheat_thread_side` → 不 panic、热区空
  - `floor_units_one_touch_not_persisted`:默认 policy(阈值 8)下 `put_staging`+`record_hit` 一次(现场 raw 256)→ `persist_heat` → ColdReader `read_heat` 为空
  - `exit_persist_skips_when_store_locked`:`let guard = lock_unpoisoned(&engine.store)` 持锁 → 计时调 `persist_heat_on_exit` → 耗时 < 1s 且不 panic,drop guard 后再调一次 → 表非空
  - `rescan_carries_scores`:`rescan_flushes_then_preheats` 扩展——flush 前先把 id 打到分数 ≥ 5,flush 后 `heat_above_floor(5)` 含该 id(分数被携带,不再清零)
- [ ] **Step 2: 跑测试确认失败** — `cargo test -p synso-core persist preheat offline stale floor exit rescarries`,预期编译错误/断言失败
- [ ] **Step 3: 实现** — 按 Interfaces;伴生线程体内联段整体替换为 `rescan_flush_thread_side` 调用
- [ ] **Step 4: 跑测试确认通过** — `cargo test -p synso-core`,预期全绿
- [ ] **Step 5: 提交** — `feat(core): 热度落库(定期/重扫/退出三触发)+ 启动按分离线衰减回填 + 重扫携带分数`

### Task 4: app — 退出钩子 + i18n 文案

**Files:**
- Modify: `crates/synso-app/src/lib.rs:164-165`(`.run(ctx).expect(...)` 一行)
- Modify: `crates/synso-app/ui/src/i18n/en.ts:197`、`crates/synso-app/ui/src/i18n/zh.ts:187`

**Interfaces:**
- Consumes: Task 3 的 `Engine::persist_heat_on_exit`
- Produces: 无(装配层)

- [ ] **Step 1: 改 lib.rs** — `.run(tauri::generate_context!())` 换为 `.build(tauri::generate_context!()).expect("error while running tauri application")` + `app.run(|app, event| { if let tauri::RunEvent::ExitRequested { .. } = event { if let Some(state) = app.try_state::<AppState>() { state.engine.persist_heat_on_exit(); } } })`(expect 语义保持)
- [ ] **Step 2: 改两处文案** — en `"Preheat the hot tier at startup and after rescans"`;zh `"启动与重扫后预热热区(热度跨重启持久化)"`
- [ ] **Step 3: 验证** — `cargo check -p synso-app` 通过;`cd crates/synso-app/ui && npm run check:i18n && npm run build` 通过
- [ ] **Step 4: 提交** — `feat(app): 退出时落库热度 + 设置页预热文案兑现持久化`

### Task 5: 验收 + 文档收尾

**Files:**
- Modify: `docs/known-issues-v1.md`(v1.2 遗留节:热度持久化标记完成;终审 Minor 的 `file_details` 条目标记随本轮消解)
- Modify: `docs/使用手册.md:97`(预热句补"(热度跨重启保留)")

- [ ] **Step 1: 全量测试** — `cargo test --workspace`,预期全绿
- [ ] **Step 2: release 构建** — `cargo build --release`;产物在 `target/release/synso-app.exe`(用户免安装版路径,勿动)
- [ ] **Step 3: 手测脚本(交给用户或实机)** — 启动 → 快速窗查文件开几次「详情」→ 等 10 分钟或直接退出 → 重启 → 仪表盘热区非零、同一文件详情秒开(命中热区)
- [ ] **Step 4: 文档更新** — known-issues 与手册按上;提交 `docs: 热度持久化落地收尾(known-issues/手册)`
- [ ] **Step 5: 汇报** — 向用户报告:实测口径(重启前后 hot_hits 对照)、变更清单、剩余排期(② 瘦身两件)
