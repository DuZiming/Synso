# 扫描核心瘦身实施计划(开址 slot_by_parent_name + StrRef u16)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `slot_by_parent_name` 从 std HashMap(≈60MB @2.1M)换成 8B 条目开址探测表(−37MB 预期),`StrRef` 压到 6B(−17MB),语义零变化,实测回填。

**Architecture:** hotindex.rs 内新增私有 `NameSlotTable`(组合哈希指纹 + 槽位号定位,backward-shift 删除,原地重绑),替换四个调用点的 HashMap 操作;`StrRef` 改 `#[repr(packed)] { start: u32, len: u16 }`,`put` 变 fallible,>64KiB 名字跳过计数;mem_probe 实测验收。

**Tech Stack:** Rust(std,无新依赖)。

**Spec:** `docs/superpowers/specs/2026-11-05-scan-core-slimming-design.md`(含 builder-check 三条裁定,实施时随行阅读)

## Global Constraints

- 不新增依赖、不动 `dir_by_parent_name`/`dirs`/`exts`、不改查询语义、不动 SQLite 侧。
- 指纹 = `h(parent, name)` 组合哈希高 32 位;home 桶由存储指纹取模(条目自包含)。
- 身份契约:读路径活列终验(parents[slot] + arena 名字字节);变更路径以 (指纹, 槽位号) 定位,不读宿主列;swap-remove 重绑 = 原地改写槽位号。
- 回退门槛:搜索 p95 同会话相对劣化 >10% → 回退 12B 全哈希条目(spec 验收口径)。
- 验收只取同会话相对差(机器常被远程编码占 CPU,绝对数不可用);release 口径。
- 提交信息:conventional 前缀 + 中文摘要,末尾加 `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`。
- 注释密度与风格对齐既有代码。

## Review Focus

1. **删除/重绑时序陷阱**(身份列已改写后再删)→ Task 1:改名与中位删除后,旧键查无、新键查得、表内无泄漏条目(len 断言)。
2. **同名聚簇** → Task 1:同名重压夹具(≥5000 同名不同父)构建后逐键可查且最大探测链有界。
3. **扩容重铺** → Task 1:跨扩容阈值的大量插入后全键可查、无重复条目。
4. **StrRef 64KiB 边界** → Task 2:65535 通过、65536 拒绝且跳过计数、布局 6B 断言。
5. **packed 纪律**(不取字段引用;派生宏兼容)→ Task 2:`size_of::<StrRef>() == 6` 静态断言 + 全量编译与既有 Hash/Debug 派生用例回归。

---

### Task 1: NameSlotTable + 调用点映射(HashMap 退役)

**Files:**
- Modify: `crates/synso-core/src/hotindex.rs`(新增表 + 组合哈希;替换 ~550/666/775/841-848/927 五处调用点;`MemBreakdown` 口径;文件内 `mod tests` 的 1629/1725 两处直连测试改写)

**Interfaces:**
- Produces(全部私有于 hotindex.rs):
  - `fn parent_name_hash(parent: FileId, name: &str) -> u64` —— hasher 先混入 parent 再喂 name(与 `name_hash` 同 hasher、同构造方式;`name_hash` 本身保留,`dir_by_parent_name` 继续用);
  - `struct NameSlotTable { entries: Vec<(u32, u32)>, len: usize }` —— 指纹存 `h >> 32`;`slot == u32::MAX` 为空槽哨兵;容量 2 的幂,负载 ≤0.72(×1.4 预留,×2 扩容重铺);`new(expected: usize)`;`mem_bytes() -> u64`(= entries.len() × 8);
  - `insert(&mut self, fp: u32, slot: u32)` —— 线性探测插入(同 fp 已存在不同 slot 属不同键,共存);
  - `get_matching(&self, fp: u32, is_match: impl Fn(u32) -> bool) -> Option<u32>` —— 沿 fp 链走,返回首个 `is_match(slot)` 的槽位(宿主闭包做 parents+arena 终验);链上无匹配返回 None;
  - `remove_by_slot(&mut self, fp: u32, slot: u32) -> bool` —— 删除 (fp, slot) 条目,backward-shift 回移;身份仅凭 (fp, slot),不读宿主列;
  - `rebind(&mut self, fp: u32, old_slot: u32, new_slot: u32)` —— 链上找存储槽位 == old_slot 的条目原地改写;找不到则退化为 insert(fp, new_slot)(镜像 HashMap::insert 的容错)。
- 调用点映射(load ~550:按 COUNT 预估 `new()` + 批量 insert;upsert 改名 666-668:`remove_by_slot(fp_old, slot)` + `insert(fp_new, slot)`;get 775/927:`get_matching` + parents/arena 闭包;swap-remove 841-848:`remove_by_slot(fp_key, slot)` + `rebind(fp_moved, last, slot)`)。
- `MemBreakdown.slot_by_parent_name_bytes` 改 `entries.len() * 8` 口径(est_map 对该字段退役,其余字段照旧)。

- [ ] **Step 1: 写失败测试**(hotindex tests)
  - `name_slot_table_roundtrip_and_shift`:插入含冲突簇 → 全键可查 → 删簇中位条目(backward-shift)→ 余键仍可查、被删键查无、`len` 正确;
  - `name_slot_table_rebind_in_place`:插入 → rebind 改槽位 → 新槽位可查、旧槽位查无、`len` 不变;
  - `name_slot_table_grow_preserves_all`:跨 ≥2 次扩容阈值插入 10 万键(合成指纹)→ 全部可查、无重复;
  - `same_name_heavy_no_clustering`:构建 ≥5000 同名不同父条目(经真实 `push`/`parent_name_hash` 路径,非直塞表)→ 逐键 `find` 命中;记录最大探测步数并断言 < 64(聚簇守卫);
  - `rename_and_mid_removal_no_stale_entries`:真实 upsert 改名 + watcher 中位删除后,旧 (parent,旧名) 查无、新键查得,`table.len() == 预期槽位数`(无泄漏——Review Focus 1);
  - 既有 1629/1725 直连测试改写到新 API。
- [ ] **Step 2: 跑测试确认失败** — `cargo test -p synso-core name_slot_table 2>&1 | tail -20`,预期编译错误(类型/方法不存在)
- [ ] **Step 3: 实现** — 按 Interfaces;哨兵与 `u32::MAX` 槽位冲突不存在(槽数 < u32::MAX/2,load 守卫已有)
- [ ] **Step 4: 跑测试确认通过** — `cargo test -p synso-core`,全绿
- [ ] **Step 5: 提交** — `perf(core): 开址 NameSlotTable 替换 slot_by_parent_name(−37MB 预期,指纹=组合哈希,backward-shift+原地重绑)`

### Task 2: StrRef 压缩 6B + fallible put

**Files:**
- Modify: `crates/synso-core/src/hotindex.rs`(StrRef 定义 ~43、`put` ~312、load/upsert/合成构造器的 `put` 调用点)

**Interfaces:**
- `#[repr(packed)] struct StrRef { start: u32, len: u16 }` —— 派生宏照旧(Copy/Clone/Debug/Default/PartialEq/Eq/Hash;packed 下按值读,现代 rustc 兼容;若某派生不兼容,手写等价实现并注释);
- `fn put(&mut self, s: &str) -> Option<StrRef>` —— `s.len() > u16::MAX as usize` 返回 None(不写 arena);
- 调用点:load 批量路径与 upsert 遇 None **跳过该行 + 计数 + log::warn!(一次性聚合计数,不逐行刷屏)**;目录名 `put` 同守卫(跳过该目录行);合成构造器断言名字短、`.expect` 即可;
- 静态断言:`const _: () = assert!(size_of::<StrRef>() == 6);`
- 字段只按值读取纪律:现有 `r.start as usize` / `r.len` 用法即如此,不引入 `&r.field`。

- [ ] **Step 1: 写失败测试**
  - `strref_packed_size`:静态断言 6B(先写,当前 8B 必红);
  - `strref_rejects_64k_name`:`put(&"x".repeat(65536))` 为 None 且 arena 字节数不变、跳过计数 +1;65535 通过;
  - `load_skips_oversized_names`:构造含超长名字行的库 → load 后该行缺席、计数可见、其余行完好。
- [ ] **Step 2: 确认失败** — `cargo test -p synso-core strref 2>&1 | tail -10`,预期断言失败/编译错误
- [ ] **Step 3: 实现** — 按 Interfaces;`str_of` 逻辑不变
- [ ] **Step 4: 确认通过** — `cargo test -p synso-core`,全绿
- [ ] **Step 5: 提交** — `perf(core): StrRef 压缩 8B→6B(packed u16 长度,fallible put 守卫 64KiB)`

### Task 3: 实测验收 + 文档回填

**Files:**
- Modify: `docs/known-issues-v1.md`(v1.2 遗留节瘦身两条改完成态 + 实测数字)

**Interfaces:**
- Consumes: main@7712757 基线;`crates/synso-core/examples/mem_probe.rs`(用法:`cargo run --release -p synso-core --example mem_probe -- <db 副本>`);perf 测试 `perf_obs_churn_search_latency`(ignored,`--ignored` 显式跑,搜索 p95 口径)

- [ ] **Step 1: 取库副本**(需用户配合:确认应用已退出)—— 复制 `%APPDATA%/…/synso/synso.db` + `-wal` + `-shm` 三件到临时目录;若用户不便,以合成 2M 文件库代用并注明口径
- [ ] **Step 2: 同会话背靠背实测** — main 基线(7712757)与本分支各跑一次 `mem_probe`(live/interned 口径)与 `--ignored` perf 搜索 p95;记录四组数字;**p95 相对劣化 >10% 即触发 spec 的 12B 回退**(此时回到 Task 1 改 12B 条目,重测)
- [ ] **Step 3: workspace 全量 + release 构建** — `cargo test --workspace` 全绿;`cargo build --release` 零新警告
- [ ] **Step 4: 文档回填 + 提交** — known-issues 两条改完成态(带实测数字与口径说明);提交 `docs: 扫描核心瘦身实测回填(known-issues)`
- [ ] **Step 5: 汇报** — 向用户报告数字、是否触发回退、剩余排期
