# 查重管线重构 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 dedup 管线重构为四道闸(尺寸→首4KB探针→采样层→全量认证),峰值内存 ≤800MB、冷跑显著提速、可取消可调速,结果与旧管线逐组一致,并顺手把 `$Recycle.Bin` 排除出索引与查重。

**Architecture:** 新管线以 SoA 候选数组 + 名字竞技场 + 排序代分组取代全量快照与 HashMap-of-Vec;闸②目录序分块、工作池并行、拒共享写打开;闸③尺寸自适应多窗采样、认证伙伴短路;闸④仅对幸存者全量认证。旧管线在新管线组装完成前保持原位可用,最后一次性换轨删除。

**Tech Stack:** Rust(synso-core / synso-app,tauri 2,Svelte 前端),rusqlite,xxhash_rust::xxh64。

**Spec:** `docs/superpowers/specs/2026-11-06-dedup-pipeline-design.md`(含全部拍板与 builder-check 裁定,执行者必读)

## Global Constraints

- 零 schema 迁移:不动 `files` 表结构;新 meta 键(`recycle_purge_done`、`settings.dedup_gear`)除外。
- 哈希全程不持 store 锁;锁只用于快照装载、批量持久化(现有 `PERSIST_BATCH`/守卫语义不变)。
- 最终分组语义不变:`(size, hash_full)` ≥2 成员,排序 size 降序、hash 升序。
- `FileId = i64` 压 u32 一律带检查转换,超 `u32::MAX` 显式报错,禁止静默截断。
- DedupPhase/DedupState 的 serde 变体与前端同发布同步,不做跨版本兼容。
- i18n zh/en 键对齐;前端字段拼写与既有 `types.ts` 同款。
- 性能/内存验收以 release 构建为准;绝对数字受机器负载影响,同会话相对比较;免安装 exe 用 `npm run tauri build -- --no-bundle`(在 crates/synso-app)。
- 采样窗公式(裁定):`n = max(2, min(16, ceil(size / 256MB)))`,窗口 64KB,偏移 `i × (size − 64KB) / n`(i=1..n,i=n 即尾窗)。
- mtime 宽限 `FRESH_SECS = 300`,只作用于将要 open 的候选;凭持久化哈希复用者不受影响。
- 共享冲突判定:`raw_os_error() == Some(32)`(ERROR_SHARING_VIOLATION)→ 锁定跳过;其余打开失败维持 errors 语义。

## Review Focus

1. **正被写的文件**:拒共享写 open 失败→锁定跳过;mtime<300s→新鲜跳过;已持久化哈希的刚写文件不受宽限 → Task 3 测试钉死三分支。
2. **闸④中途取消**(巨型文件):每 64KB 缓冲步检查,秒级生效;已算哈希照常落库;组列表保持上一轮 → Task 6(管线级 cancelled 标志)+ Task 7(状态机 Cancelled)测试。
3. **认证伙伴增量**(文件改动还原/重新拷贝):有持久化 hash_full 伴的新文件必须短路进闸④、不被采样击落 → Task 5 测试。
4. **跑后删文件的组溶解**:`total_alive`/`wasted_bytes` 只计存活组 → Task 8 测试。
5. **回收站任意深度**(含根下第一层、嵌套子树):索引排除、查重硬规则、一次性清行三处一致 → Task 2/9 测试。

---

### Task 1: 基线留存——probe dump 模式 + 自测夹具 + 基线 exe

**Files:**
- Modify: `crates/synso-core/examples/mem_probe_scan.rs`

**Interfaces:**
- Produces: `dump <db>` 运行模式(完整跑一次 dedup 后分页导出组集 TSV,行格式 `hash\tsize\tid\tpath` 每成员一行,组间空行;末行打印组数);`selftest` 模式(临时目录造夹具→dedup→dump→比对预期行)。后续 Task 12 的对拍直接复用本工具与 TSV 格式。

- [ ] **Step 1: 实现 selftest 模式(先当测试写)**

夹具:tempdir 下 `a/x.txt` 与 `b/x.txt`(同内容 10 字节)、`a/y.bin`(唯一尺寸)。
`EngineOptions { volumes: vec![tempdir], exclusions: vec![] }`,`dedup_start()` 后等
`DedupFinished`,分页 `engine.dedup_groups(offset, 500)` 拉全,断言恰 1 组、成员 2、
size=10。

- [ ] **Step 2: 实现 dump 模式**

同上分页循环,写出 TSV(格式见 Interfaces),文件路径参数给定;dedup 完成前沿用
现有事件等待循环。

- [ ] **Step 3: 验证**

Run: `cargo run --release -p synso-core --example mem_probe_scan -- selftest`
Expected: 打印 1 组/2 成员,进程正常退出。

- [ ] **Step 4: 基线 exe 留存(改动前的行为快照)**

Run: `cargo build --release -p synso-core --example mem_probe_scan && mkdir -p target/dedup-baseline && cp target/release/examples/mem_probe_scan.exe target/dedup-baseline/`
Expected: exe 就位(基线 TSV 在 Task 12 与新构建背靠背生成,避免盘面漂移)。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-core/examples/mem_probe_scan.rs
git commit -m "feat(core): mem_probe_scan 增 dump/selftest 模式(查重对拍基线工具)"
```

---

### Task 2: Store 候选流式查询 + SoA/竞技场装载 + 回收站目录位

**Files:**
- Modify: `crates/synso-core/src/store.rs`(新增查询)
- Modify: `crates/synso-core/src/dedup.rs`(新增装载结构,旧 `load_snapshot` 原位保留)
- Test: `crates/synso-core/src/dedup.rs` tests 模块

**Interfaces:**
- Produces(dedup.rs,Task 4-6 消费):
  - `pub(crate) const NO_HASH: u64 = u64::MAX;`
  - `struct CandidateRow { id: FileId, parent: FileId, size: u64, mtime: i64, hash_first: Option<u64>, hash_full: Option<u64>, name: String }`
  - `Store::iter_dedup_candidates(&self) -> Result<CandidateCursor>`,`CandidateCursor: Iterator<Item = Result<CandidateRow>>`(SQL:`WHERE is_dir = 0 AND size > 0 ORDER BY id`,十二列只取所需七列)
  - `struct Candidates { ids: Vec<u32>, parents: Vec<u32>, sizes: Vec<u64>, mtimes: Vec<i64>, hash_first: Vec<u64>, hash_full: Vec<u64>, name_off: Vec<u32>, names: Vec<u8> }`
  - `struct Dirs { parents: Vec<Option<u32>>, name_off: Vec<u32>, names: Vec<u8>, recycle: Vec<bool>, by_id: HashMap<FileId, u32> }`
  - `fn load_candidates(store: &Mutex<Store>) -> Option<(Candidates, Dirs)>`(store 错误 → None,镜像旧 `load_snapshot` 的"平凡跑"语义)
  - `impl Candidates { fn index_path(&self, i: usize, dirs: &Dirs) -> Option<String> }`(目录链拼接,沿用旧 `Snapshot::index_path` 的 budget 防环)
  - 回收站位:`dirs.recycle` —— 目录名 == `$Recycle.Bin` 或父级 recycle 位为真;目录表按 id 升序传播(parent id < child id,`resolve_dir` 的建表不变量,debug_assert)。

- [ ] **Step 1: 失败测试**

`load_candidates`:夹具 store 里放 3 文件(2 个 size>0 同尺寸、1 个 size=0、1 个目录行)→ 候选数 2;`hash_first/full` 的 `None ↔ NO_HASH` 往返;`index_path` 重建链;`$Recycle.Bin\a\b` 下文件 → 对应目录 recycle 位为真且嵌套子目录位为真;`FileId::MAX` 行 → 装载返回显式错误(带检查转换)。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p synso-core dedup::tests::candidates_ -- --nocapture`
Expected: FAIL(符号未定义)。

- [ ] **Step 3: 实现**

store.rs 加七列流式查询(参照 `all_files` 的 query_map 写法);dedup.rs 装载双 SoA
+ 竞技场 + 回收位传播。u32 转换:`u32::try_from(id).map_err(...)`。

- [ ] **Step 4: 跑测试确认通过,并跑全量回归**

Run: `cargo test -p synso-core`
Expected: PASS(旧管线测试不受影响)。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-core/src/store.rs crates/synso-core/src/dedup.rs
git commit -m "feat(core): dedup 候选流式装载(SoA+名字竞技场+回收站目录位)"
```

---

### Task 3: 打开语义原语——拒共享写 / 锁定分类 / mtime 宽限

**Files:**
- Modify: `crates/synso-core/src/dedup.rs`
- Test: 同文件 tests 模块(Windows 实锁用例参照 engine.rs:1985 的 `share_mode(0)` 先例)

**Interfaces:**
- Produces(Task 4/5/6 消费):
  - `enum OpenSkip { Locked, Error }`;`fn classify_open_err(err: &io::Error) -> OpenSkip`(仅 `raw_os_error()==Some(32)` → Locked;`#[cfg(not(windows))]` 恒 Error)
  - `RealFileReader::open` 改为 `OpenOptions::new().read(true)` + `#[cfg(windows)] .share_mode(FILE_SHARE_READ)`(注释:持有期间写者无法打开,读到即稳定;现状允许共享写、能读到漂移内容——本改动即修复)
  - `const FRESH_SECS: i64 = 300;` 与判定 `fn is_fresh(mtime: i64, now: i64) -> bool { now - mtime < FRESH_SECS }`(now 由管线注入,测试可控)

- [ ] **Step 1: 失败测试**

(a) fake reader 返回 `io::Error::from_raw_os_error(32)` → `classify_open_err` = Locked;os_error 5 → Error;(b) `is_fresh(now-299)=true、is_fresh(now-300)=false`;(c) Windows only:独占打开(`share_mode(0)`)真实文件 → 走 open 路径的辅助函数将其计为 Locked 跳过且不读内容。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p synso-core dedup::tests::open_semantics -- --nocapture`
Expected: FAIL。

- [ ] **Step 3: 实现**

按 Interfaces;`use std::os::windows::fs::OpenOptionsExt` 走 `#[cfg(windows)]`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p synso-core dedup::tests::`
Expected: PASS(旧管线若受 share_mode 影响——现有独占锁测试断言 errors 计数——同步改断言为锁定语义,提交信息注明)。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-core/src/dedup.rs
git commit -m "feat(core): dedup 打开语义原语(拒共享写/锁定分类/mtime 宽限)"
```

---

### Task 4: 闸①尺寸闸 + 闸②首探针(分块/工作池/终局/复用)

**Files:**
- Modify: `crates/synso-core/src/dedup.rs`
- Test: 同文件 tests 模块

**Interfaces:**
- Consumes: Task 2 的 `Candidates/Dirs/load_candidates/index_path`、Task 3 的打开原语。
- Produces(Task 5/6 消费):
  - `struct GateKills { files: u64, bytes: u64 }`
  - `struct OutcomeCounters { size_gate: GateKills, head_gate: GateKills, sample_gate: GateKills, errors: u64, locked: u64, fresh: u64, placeholders: u64, recycle: u64 }`
  - `struct HeadOutcome { head: Vec<u64>, /* NO_HASH=未得 */ counters: OutcomeCounters, hashed_first: u64, pending: Vec<HashUpdate> }`
  - `fn gate_size(c: &Candidates, counters: &mut OutcomeCounters) -> Vec<u32>`(按 size 排序取连续段 ≥2 的候选下标,含 u32 索引复用;排序 `sort_unstable_by_key` 于 `sizes`)
  - `fn gate_head(c: &Candidates, dirs: &Dirs, cand: &[u32], roots, reader, workers: usize, cancel: &AtomicBool, now: i64, progress: &dyn Fn(u64,u64), store: &Mutex<Store>) -> HeadOutcome`
  - 行为契约:目录序(按 `(parents, ids)` 排序后分块,块 65,536);持久化 hash_first 复用(不 open、不受宽限);将 open 者过占位检查→宽限检查→open→读 4KB;`size ≤ 4096` 时 `hash_full = hash_first` 同批落库(HashUpdate 双 Some)并标终局;锁定/新鲜/占位/回收站/错误各计数;cancel 文件间生效;块界批量 `flush_persist` + 进度(PROGRESS_EVERY=500 沿用);工作线程领块内下标(atomic 游标),workers=1 时单线程直跑(测试默认)。

- [ ] **Step 1: 失败测试**

夹具(FakeReader 计数 open):同尺寸 2 文件 + 单例尺寸 1 文件 → 尺寸闸后 2 候选、size_gate 计 1 文件;持久化 hash_first 的文件不被 open 且 `fresh` mtime 也不跳过(复用豁免);无持久化 + mtime=now−100 → fresh 计数、不 open;FakeReader attributes 返回占位位 → placeholders;路径 `$Recycle.Bin\...` 候选 → recycle 计数;≤4KB 文件 → pending 含 hash_first 与 hash_full 同值;>4KB → hash_full 为 NO_HASH;workers=2 与 1 结果一致(同哈希)。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p synso-core dedup::tests::gate_ -- --nocapture`
Expected: FAIL。

- [ ] **Step 3: 实现**

块内并行用 `std::thread::scope` + `AtomicUsize` 游标;每 worker 独立 pending 汇入;目录序排序在 gate_head 入口一次完成。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p synso-core dedup::tests::`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-core/src/dedup.rs
git commit -m "feat(core): dedup 闸①尺寸闸+闸②首4KB探针(目录序分块/工作池/≤4KB终局)"
```

---

### Task 5: 闸③采样层——窗口函数/采样键/认证短路/4–64KB 终局

**Files:**
- Modify: `crates/synso-core/src/dedup.rs`
- Test: 同文件 tests 模块

**Interfaces:**
- Consumes: Task 4 的 `HeadOutcome/OutcomeCounters`,Task 2 的 SoA。
- Produces(Task 6 消费):
  - `const SAMPLE_WINDOW: usize = 64 * 1024;`
  - `fn sample_window_offsets(size: u64) -> Vec<u64>`(n = `max(2, min(16, ceil(size/256MB)))`,偏移 `i×(size−64KB)/n`,i=1..n)
  - `fn sample_key(hashes: &[u64]) -> u64`(级联再 xxh64)
  - `struct SampleOutcome { full_todo: Vec<u32>, counters: OutcomeCounters, pending: Vec<HashUpdate>, hashed_full_small: u64 }`
  - `fn gate_sample(c, dirs, cand: &[u32], head: &HeadOutcome, roots, reader, workers, cancel, now, progress, store) -> SampleOutcome`
  - 行为契约:输入 = 闸②后按 `(size, head)` 连续段 ≥2 的**新**成员(hash_full==NO_HASH);段内**含任一认证成员**(hash_full≠NO_HASH)→ 段内新成员全部直入 full_todo(短路,不采样);无认证段 → `size ≤ 64KB` 整读得 hash_full 终局落库,`> 64KB` 逐窗采样,按 `(size, sample_key)` 连续段 ≥2 幸存进 full_todo,单键段击落(sample_gate 计数);打开沿拒共享写/宽限/占位语义。

- [ ] **Step 1: 失败测试**

窗口数表:`size=64KB+1→2、256MB→2(边界 ceil=1 取 max)、256MB+1→3、13.5GB→16`;同尺寸两文件偏移全等;`4KB<size≤64KB` 整读且 hash_full 落库终局;认证短路:段内 1 认证 + 1 新 → 新成员进 full_todo 且零采样窗读;无认证段采样单键 → 击落计数 +1;两新成员采样键同 → 双双进 full_todo;fake os_error(32) → locked。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p synso-core dedup::tests::sample_ -- --nocapture`
Expected: FAIL。

- [ ] **Step 3: 实现**

采样读:一次 open,`Seek` 到各偏移读 64KB;键计算按偏移序。分块/工作池复用闸②机制。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p synso-core dedup::tests::`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-core/src/dedup.rs
git commit -m "feat(core): dedup 闸③采样层(自适应窗/采样键/认证伙伴短路)"
```

---

### Task 6: 闸④全量认证 + 最终分组 + run_gates 组装

**Files:**
- Modify: `crates/synso-core/src/dedup.rs`
- Test: 同文件 tests 模块

**Interfaces:**
- Consumes: Task 2-5 全部。
- Produces(Task 7 消费):
  - `DedupOutcome` 扩展:`counters: OutcomeCounters`、`cancelled: bool`(既有 `groups/hashed_first/hashed_full/errors/placeholders` 保留;errors 语义 = 打开/读失败,锁定/新鲜/占位/回收单列)
  - `pub(crate) fn run_gates(store: &Mutex<Store>, roots: &[String], reader: &dyn FileReader, workers: usize, cancel: &AtomicBool, now: i64, progress: &dyn Fn(DedupPhase, u64, u64)) -> DedupOutcome`
  - 行为契约:Grouping → HashingFirst → Sampling → HashingFull 四段进度(总口径:闸②=全部尺寸闸候选(含复用),③=采样参与者,④=full_todo);闸④ open 加 `FILE_FLAG_SEQUENTIAL_SCAN`(cfg windows),64KB 流式,每缓冲步查 cancel;最终分组 = 全体有 hash_full 者(认证+新算)按 `(size, hash_full)` 连续段 ≥2,size 降序/hash 升序;cancel 任一检查点命中 → 立即收尾(flush pending、cancelled=true、组列表为空——上一轮结果由引擎层保留)。

- [ ] **Step 1: 失败测试**

端到端夹具:同内容 3 份不同目录 → 1 组 3 成员;同尺寸异内容(首 4KB 即异)→ 无组;同尺寸同首 4KB 异尾 → 无组(采样击落);认证短路场景:预置 hash_full 的 A + 新文件同内容 F → 组 {A,F};cancel:progress 回调在 HashingFull 首次触发时置 cancel → outcome.cancelled=true、groups 空、已算 hash_first 已 flush(重开 store 可见);分组排序断言(两不同尺寸组,大在前)。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p synso-core dedup::tests::gates_ -- --nocapture`
Expected: FAIL。

- [ ] **Step 3: 实现**

闸④沿用闸②分块机制;`custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)` 走 `#[cfg(windows)]`。

- [ ] **Step 4: 跑测试确认通过 + 全量回归(新旧管线并存)**

Run: `cargo test -p synso-core`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-core/src/dedup.rs
git commit -m "feat(core): dedup 闸④全量认证+run_gates 组装(取消/计数器/终局分组)"
```

---

### Task 7: 引擎换轨——相位增补/取消/档位 + 删旧管线

**Files:**
- Modify: `crates/synso-core/src/engine.rs`(DedupPhase、DedupShared、launch_dedup/dedup_cycle、`pub fn dedup_cancel`)
- Modify: `crates/synso-core/src/settings.rs`(`DedupGear`)
- Modify: `crates/synso-core/src/dedup.rs`(删 `run_pipeline/load_snapshot` 旧路径与过时模块头注释,迁存活测试)
- Test: `crates/synso-core/src/engine.rs`、`crates/synso-core/src/settings.rs` tests

**Interfaces:**
- Consumes: Task 6 `run_gates`。
- Produces(Task 8/10/11 消费):
  - `DedupPhase` 增 `Sampling`、`Cancelled`(serde snake_case 自动);模块头相位注释更新
  - `settings.rs`:`pub enum DedupGear { Gentle, Standard, Turbo }`(serde snake_case,Default=Standard);`Settings` 增 `pub dedup_gear: DedupGear`;meta 键 `settings.dedup_gear` 存字符串("gentle"/"standard"/"turbo"),解析失败回落 Standard;`Engine::settings()` 现有缓存路径自然携带
  - `Engine::pub fn dedup_cancel(&self)`(置 `DedupShared.cancel`,幂等)
  - `DedupShared` 增 `cancel: AtomicBool`;`launch_dedup` 启动前 reset,档位 = `self.settings().dedup_gear → workers(Gentle=1/Standard=2/Turbo=4)`,`now = unix_now()` 注入;`dedup_cycle` 调 `run_gates`,outcome.cancelled → phase=Cancelled(不广播 DedupFinished,或广播带 groups=上轮——**拍板:不广播**,前端以 state 轮询/progress 事件感知)
  - 线程优先级:Gentle 档 worker `SetThreadPriority(BELOW_NORMAL)`(cfg windows;单行 unsafe,注释注明)

- [ ] **Step 1: 失败测试**

settings:dedup_gear 默认 Standard、序列化往返、坏值回落;engine:idle 时 `dedup_cancel()` 无副作用;跑动中 cancel(cancel 由测试用 FakeReader 慢读触发:在 attributes 回调里置 cancel)→ `dedup_state().phase == Cancelled`、book.groups 保持旧 Arc;DedupPhase serde 新变体字符串 "sampling"/"cancelled"。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p synso-core dedup -- --nocapture; cargo test -p synso-core settings`
Expected: FAIL。

- [ ] **Step 3: 实现 + 删旧**

换轨调用点;删 `run_pipeline`/`load_snapshot`/`Snapshot`/`by_size` 旧体;迁移仍有效的旧测试语义(placeholder/复用/守卫)到 gates 路径,删除已由新测试覆盖的重复用例;修正模块头"重扫抹哈希"过时注释(实际按 mtime+size 保留)。

- [ ] **Step 4: 全量回归**

Run: `cargo test -p synso-core`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-core/src/engine.rs crates/synso-core/src/settings.rs crates/synso-core/src/dedup.rs
git commit -m "feat(core): dedup 引擎换轨四道闸(取消/档位/Sampling+Cancelled 相位,删旧管线)"
```

---

### Task 8: 存活聚合 + dedup_groups 分页返回 total_alive

**Files:**
- Modify: `crates/synso-core/src/engine.rs`(DedupState、DedupBookkeeping、dedup_groups、计数器缓存)
- Modify: `crates/synso-core/examples/mem_probe_scan.rs`(dump 模式尾打印计数行)
- Test: engine.rs tests

**Interfaces:**
- Produces(Task 11/12 消费):
  - `DedupState` 增 `pub alive_groups: u64, pub wasted_bytes: u64`(跑完成时经热索引计算:存活成员 ≥2 的组数;wasted = Σ size×(存活−1);完成后缓存于 `DedupBookkeeping`,状态机直接携带)
  - `DedupBookkeeping` 同时缓存 `counters: OutcomeCounters`(DedupFinished 时),`Engine::pub fn dedup_counters(&self) -> OutcomeCounters`(默认零值)——spec"探针打印击落计数"的通路;dump 模式在 DedupFinished 后打印一行各闸击落/跳过汇总(基线 exe 无此调用,行为不变)
  - `pub struct DedupPage { pub groups: Vec<DupeGroup>, pub total_alive: u32 }`;`Engine::dedup_groups(&self, offset: u32, limit: u32) -> DedupPage`(签名变更;total_alive = 全量扫描存活过滤计数——现有实现本就线性扫到 offset+limit,total 需另走一遍 O(组数) 计数,页粒度调用可接受)

- [ ] **Step 1: 失败测试**

夹具跑出 2 组后从 store 删一组全部成员 → `dedup_groups(0,10).total_alive == 1`、该组成员不物化、wasted_bytes 只计存活组;未跑过 → 0/0;`dedup_counters()` 跑后非零、跑前全零。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p synso-core dedup_page -- --nocapture`
Expected: FAIL(编译错——签名变更牵连 dedup.rs 测试内旧调用,一并更新)。

- [ ] **Step 3: 实现**

- [ ] **Step 4: 全量回归**

Run: `cargo test -p synso-core`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-core/src/engine.rs
git commit -m "feat(core): dedup 存活聚合(alive_groups/wasted_bytes)+分页返回 total_alive"
```

---

### Task 9: 回收站索引排除——matcher 硬规则 + 一次性清行

**Files:**
- Modify: `crates/synso-core/src/scanner.rs`(`build_matcher` 追加硬规则)
- Modify: `crates/synso-core/src/store.rs`(`purge_recycle_bin_rows`)
- Modify: `crates/synso-core/src/engine.rs`(open 时一次性执行)
- Test: scanner.rs / store.rs / engine.rs tests

**Interfaces:**
- Produces:`build_matcher` 恒追加段规则 `$Recycle.Bin`(调用方传入 patterns 之外,scan 与 reconcile 单点覆盖);`Store::purge_recycle_bin_rows(&mut self) -> Result<usize>`(事务内:找 name='$Recycle.Bin' 的 dirs 及其子树 dir id 集 → DELETE files/dir 行,返回删除文件行数);engine `open` 在 settings 预热后、热索引加载 spawn 前执行一次,meta 键 `recycle_purge_done` 防重(失败仅 log,下次再试)。

- [ ] **Step 1: 失败测试**

scanner:`$Recycle.Bin` 任意深度(根下第一层与嵌套)扫描被剪、同名的普通目录(如 `KRECYCLE`)不受影响、`build_matcher(&[])` 现在匹配 $Recycle.Bin(修 622 行旧断言);store:夹具造 `$Recycle.Bin\S-1-5\file` → purge 删净、返回计数、二次调用 0;engine:首次 open 清行且写 meta、热索引不含该行、二次 open 不再执行。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p synso-core recycle -- --nocapture`
Expected: FAIL。

- [ ] **Step 3: 实现**

子树收集:递归 CTE 或内存 parent→children 表(dirs 表量级 37 万,内存法简单)。

- [ ] **Step 4: 全量回归**

Run: `cargo test -p synso-core`
Expected: PASS(注意既有 watcher/reconcile 测试是否依赖 $Recycle.Bin 内夹具——有则迁移)。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-core/src/scanner.rs crates/synso-core/src/store.rs crates/synso-core/src/engine.rs
git commit -m "feat(core): $Recycle.Bin 系统级索引排除(matcher 硬规则+启动一次性清行)"
```

---

### Task 10: app 层——dedup_cancel 命令 / 相位映射 / 新返回形

**Files:**
- Modify: `crates/synso-app/src/commands.rs`(dedup_cancel;dedup_groups 返回 DedupPage)
- Modify: `crates/synso-app/src/events.rs`(PhasePayload 增两臂)
- Modify: `crates/synso-app/src/lib.rs`(命令注册)

**Interfaces:**
- Consumes: Task 7/8 的引擎 API。
- Produces(Task 11 消费):IPC 面——`dedup_cancel()`(sync 命令);`dedup_groups(offset, limit) → DedupPage{groups,total_alive}`;`dedup-progress` 事件 payload 相位字符串集合 +`sampling`/`cancelled`。

- [ ] **Step 1: 实现三处改动(commands 无独立测试基建,以编译+引擎测试兜底)**

- [ ] **Step 2: 验证**

Run: `cargo test -p synso-app 2>$null; cargo build -p synso-app`
Expected: 构建通过、既有测试 PASS。

- [ ] **Step 3: Commit**

```bash
git add crates/synso-app/src/commands.rs crates/synso-app/src/events.rs crates/synso-app/src/lib.rs
git commit -m "feat(app): dedup_cancel 命令+相位映射+dedup_groups 分页返回形"
```

---

### Task 11: 前端——按页取组/取消按钮/档位按钮/相位文案

**Files:**
- Modify: `crates/synso-app/ui/src/lib/types.ts`(DedupState 新字段、DedupPage)
- Modify: `crates/synso-app/ui/src/lib/tauri.ts`(dedupCancel 包装、dedupGroups 返回形)
- Modify: `crates/synso-app/ui/src/dedup/DedupView.svelte`
- Modify: `crates/synso-app/ui/src/i18n/{zh,en}.ts`

**Interfaces:**
- Consumes: Task 10 IPC 面;settings_get/settings_set 现有整结构命令(档位持久化走它)。
- 行为契约:`loadGroups` 全量循环删除 → 页状态 `page/totalAlive`,`dedupGroups(page*20,20)` 按需;头部"组数/浪费"改用 `state.alive_groups/wasted_bytes`(短页容忍:总页数取 `ceil(totalAlive/20)`,组溶解致越界时自动回落末页);running 时显示取消按钮 → `dedupCancel()`;档位按钮三态循环(gentle→standard→turbo),即时 `settings_set` 持久化,当前档高亮,运行中可切(下一批生效);相位标签覆盖 sampling/cancelled;i18n 键:dedup.phase.sampling、dedup.phase.cancelled、dedup.action.cancel、dedup.gear.{label,gentle,standard,turbo}(zh/en 对齐)。

- [ ] **Step 1: types/tauri/i18n 先行(类型对齐)**

- [ ] **Step 2: DedupView 改造**

- [ ] **Step 3: 构建验证**

Run: `cd crates/synso-app && npm run build`
Expected: 构建通过无 TS 错误。

- [ ] **Step 4: 手动烟测清单(dev 运行)**

`npm run tauri dev`:查重视图分页翻页、取消(造一次长跑)、档位切换持久化(重启仍记)、sampling/cancelled 文案、头部数字在跑完后刷新。

- [ ] **Step 5: Commit**

```bash
git add crates/synso-app/ui/src
git commit -m "feat(ui): 查重视图按页取组+取消按钮+档位按钮+新相位文案"
```

---

### Task 12: 验收执行——对拍/内存/时间/文档回填

**Files:**
- Create: `scripts/dedup-diff.py`(对拍脚本,入库)
- Modify: `docs/known-issues-v1.md`、`docs/使用手册.md`

**Interfaces:**
- Consumes: Task 1 基线 exe(`target/dedup-baseline/mem_probe_scan.exe`)、dump 模式、库副本 `%TEMP%\synso-probe\synso.db`。

- [ ] **Step 1: 对拍脚本**

`dedup-diff.py baseline.tsv new.tsv`:基线剔除路径含 `$Recycle.Bin` 段成员(组 <2 消解)后逐组比对(排序后哈希+尺寸+成员 id 集);输出 diff 摘要或 `IDENTICAL`。

- [ ] **Step 2: 背靠背两连跑(库副本,同会话)**

Run(记录时长与峰值输出):
```powershell
cd target/dedup-baseline; ./mem_probe_scan.exe <db副本> dedup | Tee-Object baseline-run.log
./mem_probe_scan.exe <db副本> dump > baseline.tsv   # 增量二次跑,秒级
# 切新构建
cargo run --release -p synso-core --example mem_probe_scan -- <db副本> dedup | Tee-Object new-run.log
cargo run --release -p synso-core --example mem_probe_scan -- <db副本> dump > new.tsv
python scripts/dedup-diff.py baseline.tsv new.tsv
```
Expected: `IDENTICAL`(或仅回收站差异——若基线 dump 前库已有回收站哈希,以脚本剔除为准);新跑峰值 ≤800MB;时长与各闸击落计数(dump 模式尾行)记录。

- [ ] **Step 3: known-issues 回填**(条目 1 实测数字、条目 2 回收站定案、击落率)

- [ ] **Step 4: 使用手册更新**(查重四道闸行为、取消/档位、回收站系统排除、锁定/新鲜/占位跳过说明)

- [ ] **Step 5: 免安装 exe 重建**(先停运行中 synso-app:PowerShell 工具 `Stop-Process -Name synso-app -Confirm:$false`)

Run: `cd crates/synso-app; npm run tauri build -- --no-bundle`
Expected: 构建成功;`grep` 二进制内嵌 `main-*.js` 资源名确认非 dev-server 版;用户实测通过。

- [ ] **Step 6: Commit**

```bash
git add scripts/dedup-diff.py docs/known-issues-v1.md docs/使用手册.md
git commit -m "docs+test: 查重重构验收(对拍 IDENTICAL/峰值回填/手册更新)"
```
