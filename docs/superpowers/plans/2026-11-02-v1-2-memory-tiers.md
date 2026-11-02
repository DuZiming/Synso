# Synso v1.2 内存策略(三层索引)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地三层内存策略(扫描核心常驻 + 热度准入的维度缓存)+ 扫描核心瘦身 + 全套可调设置与仪表盘,使新维度数据零基线内存。

**Architecture:** 纯新增 `tiers.rs`(热/中转/准入/统计)挂在 Engine 上;维度读走独立 SQLite 只读 WAL 连接;watcher 按 id 失效、重扫粗清空+重预热;扫描核心四件套结构替换(u32 列、稠密 id 直查、开址名字表、u16 StrRef)。全部参数进设置、在线生效。

**Tech Stack:** Rust(edition 2021)· rusqlite(bundled, WAL)· Tauri v2 · Svelte 5。

**Spec:** `docs/superpowers/specs/2026-11-02-v1-2-memory-tiers-design.md`(含评审裁定:只读连接、失效契约、别名修正、4 角点矩阵、验收排除 dedup 瞬态)

## Global Constraints

- TDD:先失败测试;`cargo clippy --workspace --all-targets -- -D warnings` 干净;fmt 干净;性能口径一律 release。
- 搜索 p95 <50ms 不回归(`tests/perf.rs` 忽略测试,阶段 5 前后各跑一次)。
- synso-core 不依赖 Tauri;SQLite schema 只增不破;真实库只以运行与副本探针两种形态使用。
- 设置新增字段全部带默认值,旧库无迁移打开即用(meta 键模式)。

## Review Focus

1. **预算硬上限**:任何路径(预热、晋升、批量取页)都不得使热/中转区超预算——`tiers_capacity_never_exceeded_under_flood` 测试灌 10× 预算数据断言不超。
2. **失效正确性**:watcher 更新一个已缓存条目后,取回的必须是新值——`stale_entry_invalidated_by_upsert` 端到端。
3. **重扫后重预热**:scan-finished → 缓存清空 → 持久化热度 Top-K 回填——`rescan_flushes_then_preheats`。
4. **只读连接不阻塞**:持主连接写锁时,维度读照常返回(并发测试:写线程持锁 2s,读线程 <100ms 完成)。
5. **u32 收窄安全**:行 id 超 u32 上界的库打开不 panic(守卫+错误);`slot_by_id` 稠密化后洞位查无此 id 返回 None。

---

### Task 1: 三层缓存引擎(纯新增,synso-core::tiers)

**Files:**
- Create: `crates/synso-core/src/tiers.rs`(TierCache + 策略 + 统计;测试在内)
- Modify: `crates/synso-core/src/lib.rs`(mod + re-export `TierStats`/`MemoryPolicy`)

**Interfaces:**
- Produces:
  - `pub struct MemoryPolicy { hot_budget_bytes, staging_budget_bytes, staging_ttl, promote_hits, promote_window, hot_threshold, decay_half_life, preload_on_start }`(Default = 设计值:64MB/32MB/300s/3/10min/8/24h/开)
  - `pub struct TierStats { hot_bytes, hot_entries, hot_hits, hot_misses, staging_bytes, staging_entries, staging_hits, evictions, promotions }`
  - `impl TierCache<V: Clone>`:`new(policy)`、`get(&key) -> Option<V>`(计命中/晋升判定)、`put_staging(key, val, bytes_of)`、`put_hot_direct`(预热用)、`evict_to_policy()`、`stats()`、`clear()`;内部 count-min sketch 命中计数 + 指数衰减(半衰期参数;时间注入 `now: Instant` 便于测试)
  - 通用值不绑定维度——维度注册由 Task 2 挂接

- [ ] **Step 1: 失败测试**:`tiers_capacity_never_exceeded_under_flood`(灌 10× 预算,断言 hot/staging 字节数 ≤ 预算且淘汰发生);`promotion_cold_to_staging_to_hot`(同一 key 窗口内命中 3 次进中转,衰减加权累计过阈值进热区);`ttl_expiry_returns_to_cold`(时间推进过 TTL 后 get 为 None)
- [ ] **Step 2: 确认 RED** → 实现 TierCache → 全绿
- [ ] **Step 3: 衰减测试**(`decay_demotes_yesterdays_hot`:时间推进 2× 半衰期,热度减半,淘汰顺序反转)
- [ ] **Step 4: Commit** `feat(core): tier cache — bounded hot/staging with LFU-decay admission and CMS counters`

### Task 2: 冷层读通道 + 维度注册 + 失效契约

**Files:**
- Modify: `crates/synso-core/src/store.rs`(WAL pragma;`open_readonly` 类方法)
- Create: `crates/synso-core/src/dimensions.rs`(注册表 + `dimension_page`)
- Modify: `crates/synso-core/src/engine.rs`(持只读连接;`file_detail` 试点 API;失效钩子)
- Modify: `crates/synso-core/src/watcher.rs`(批处理后按 id 失效)

**Interfaces:**
- Produces:
  - `Engine::dimension_page(dim: &str, ids: &[FileId]) -> Result<Vec<DimRow>>`(miss 层级递降;命中即入统计)
  - `Engine::file_detail(id: FileId) -> Result<FileDetail>`(ctime/atime/hash_first/hash_full/has_hash ——试点维度)
  - 首个注册维度 `"detail"`;失效:`invalidate_dimensions(ids: &[FileId])`(watcher 调用);`flush_dimensions_and_preheat()`(扫描完成钩子调用,Top-K 回填)

- [ ] **Step 1: 失败测试**:`readonly_connection_unblocked_by_write_lock`(写线程持主锁 2s,读线程 dimension_page <100ms);`stale_entry_invalidated_by_upsert`;`rescan_flushes_then_preheats`
- [ ] **Step 2-4: RED → 实现 → 绿**(WAL 切换注意:journal_mode=WAL 一次性 pragma,主/只读连接都生效)
- [ ] **Step 5: Commit** `feat(core): cold-tier read path (read-only WAL conn), dimension registry, invalidation contract`

### Task 3: 设置项 + 在线生效 + 设置页「内存」节 + 仪表盘

**Files:**
- Modify: `crates/synso-core/src/settings.rs`(8 个新字段带默认)、`engine.rs`(`apply_memory_policy` 热切换)
- Modify: `crates/synso-app/src/commands.rs`(`memory_policy_get/set`、`tier_stats`)、`lib.rs` 注册
- Modify: `ui/src/lib/{types,tauri}.ts`、`ui/src/settings/SettingsView.svelte`(「内存」节:8 个控件 + 三层仪表盘)
- Modify: `ui/src/i18n/{zh,en}.ts`

- [ ] **Step 1: 失败测试**:`policy_live_apply_resizes_tiers`(预算 64MB→8MB 后立即淘汰到位)
- [ ] **Step 2-4: RED → 实现 → 绿**;UI 门禁(svelte-check/i18n/build)全过
- [ ] **Step 5: 手动验收**:设置页改参数即时生效、仪表盘数字随访问变化
- [ ] **Step 6: Commit** `feat(app): memory policy settings (live-apply) + tier dashboard`

### Task 4: 试点消费面——快速窗详情浮层

**Files:**
- Modify: `ui/src/quick/QuickWindow.svelte`(选中行「详情」开关:ctime/atime/哈希状态面板)
- Modify: `commands.rs`/`tauri.ts`/`types.ts`/i18n

- [ ] **Step 1: 实现**(选中行存在时 footer 出现「详情」按钮,面板显示 file_detail 返回值;每次打开走 dimension_page——真实命中率来源)
- [ ] **Step 2: 验收**:打开→秒出;重复打开同一行命中缓存(仪表盘 hit 上升)
- [ ] **Step 3: Commit** `feat(app): quick-window file detail popover (pilot dimension consumer)`

### Task 5: 扫描核心瘦身四件套

**Files:**
- Modify: `crates/synso-core/src/hotindex.rs`(u32 列、稠密 `slot_by_id`、开址 `slot_by_name`、u16 StrRef len;别名仅同串)

**Interfaces:** 对外 API 不变;`ids/parents/ext_ids` 存储收窄(u32),`FileId`(i64)在边界转换+守卫。

- [ ] **Step 1: 基线**:release 跑 `perf_1m_search_under_50ms` 记录数字
- [ ] **Step 2: 失败测试**:`id_beyond_u32_guard`(构造超大 id 打开不 panic);`slot_by_id_dense_with_holes`;`slot_by_name_open_addressing_lookup_fuzz`(随机键 10 万次,结果与旧哈希表一致)
- [ ] **Step 3-4: RED → 逐件实现(每件一 commit,可独立回滚)→ 全绿 + perf 复测不回归**
- [ ] **Step 5: 实测**:真实库副本 mem_probe 对比 live 455→≈310MB,写进设计文档附录
- [ ] **Step 6: Commit(s)** `perf(core): …`(四件各一)

### Task 6: 观测、调参与收尾

**Files:**
- Modify: `memlog.rs`(tiers 行)、`scripts/tune-tiers.ps1`(4 角点:32/128MB × 5/60s TTL,在线换参-收日志-出曲线)
- Modify: 手册(§内存策略)、known-issues(条目 1 更新 v1.2 结论)、设计文档附录(基线数字)

- [ ] **Step 1: memlog tiers 行 + 基线半天采集**
- [ ] **Step 2: 角点脚本跑通(每段 ≥2h 真实负载),曲线择优写附录**
- [ ] **Step 3: 文档三件更新;Commit** `perf(app): tier observability + tuning matrix + v1.2 baseline ledger`

---

## 执行说明

- 顺序 1→6 固定;Task 5 独立可回滚,若它拖慢主线可单独后置(不阻塞 1-4/6 的验收)。
- PR:#2(v1.1)先合并,v1.2 在新分支(或 PR #2 续作,开工前与用户确认)。
- u32 收窄的边界守卫成本:打开库时校验 max(id) < u32::MAX,超界报错拒开(现阶段库远达不到)。
