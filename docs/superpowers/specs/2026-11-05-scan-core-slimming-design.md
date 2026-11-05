# Synso — 扫描核心瘦身设计文档(开址 slot_by_parent_name + StrRef u16)

日期:2026-11-05。前置:v1.2 三层内存策略(2026-11-02 设计文档附录将这两件列为缓办;
v1.2 实测纯加载 working set 401MB @172.4 万文件)。

## 这是什么 / 为什么

v1.2 瘦身做完 u32 列与稠密 slot_by_id 后,扫描核心(hotindex.rs)还有两件已规划的
内存大件:

1. **`slot_by_parent_name: HashMap<(FileId, u64), usize>`** ——(父目录 id, 名字哈希)→
   槽位的二级索引,2.1M 槽位下 std HashMap 口径约 60MB(est_map)。哈希表的开销(负载
   因子、控制字节、16B 键)对"键本身就是两个整数"的场景是纯浪费。
2. **`StrRef { start: u32, len: u32 }` = 8B/引用** —— 每槽位 4 个引用(lower/orig/
   py_full/py_init),2.1M 槽位 × 4 × 8B ≈ 67MB,其中 len 的高 2 字节几乎永远为零。

两件做完,加载态 live 预期 ≈340MB(v1.2 附录口径),working set ≈415-430MB。

## 目标

- `slot_by_parent_name` 换定制开址探测表,内存 −38MB 量级(实测为准);
- `StrRef` 压到 6B,−17MB(2.1M × 4 引用 × 2B);
- 语义零变化:精确大小写匹配、读时验证、增删改查四操作与现有调用点一一对应;
- 全量测试绿,mem_probe 实测数字回填 known-issues。

## 非目标

- `dirs` / `dir_by_parent_name`(目录树表,行数小两个量级)不动;
- `exts`、各列数组不动(v1.2 已做或体量小);
- 不改查询/排序语义,不动 SQLite 侧;
- 不做 arena 本体压缩(字符串内容压缩是另一个量级的工程,未规划)。

## 方案 A:开址 slot_by_parent_name

**新结构**(hotindex.rs 内私有):

```rust
/// 线性探测开址表:(parent, name) 组合哈希 → slot。
/// 条目 8B:{指纹, slot},指纹 = 组合哈希高 32 位,空槽 slot == u32::MAX。
struct NameSlotTable {
    entries: Vec<(u32, u32)>, // (fingerprint, slot)
    len: usize,
}
```

**容量与几何**:初始按预估槽数 ×1.4 分配(负载 ≤0.72),插入超阈值 ×2 扩容重铺。
2.1M 槽位 → 2.94M × 8B ≈ 23.5MB,对比 HashMap ≈60MB,**−37MB 左右**。

**指纹 = 组合哈希 `h(parent, name)` 的高 32 位**(builder-check 裁定 2):若指纹只取
`name_hash`,同名文件(desktop.ini / README.md 动辄数万份)会共享同一 home 桶,探测链
被同名聚簇毁掉。父目录混入哈希后,同名不同父自然散开。home 桶由**存储的指纹自身**取模
得出(指纹低位即哈希 32..32+k 位,与指纹匹配自洽,经典"存哈希即存索引"布局)——条目
完全自包含,backward-shift 回移重放只需存储数据。

**身份契约**(builder-check 裁定 1,三个删除/重绑调用点的时序陷阱):两处 remove 都在
身份列已被改写**之后**执行(update_slot 先写 parents/orig_refs 再 remove(&old_key);
remove_slot_at 先 swap-pop 全部列再 remove(&key)),通过活列验证身份在删除路径不可行。
契约拆分:

- **读路径**(find / remove_slot 的 get,均在改动前运行):探测命中指纹后,以
  `parents[slot] == parent && arena 名字字节匹配` 终验——比现状(remove_slot 今天不做
  名字验证)更严,结果同;
- **变更路径**(remove / 重绑,身份列可能已失真):**以 (指纹, 槽位号) 定位条目**——
  同一槽位至多存在于一个条目,槽位号在操作全程稳定,不需要读任何宿主列。
  upsert 改名(666)= remove(fp(old_key), slot);swap-remove 回填重绑(848)不是
  remove+insert 而是**原地改写槽位号**(fp(moved_key) 链上找到存储槽位 == last 的条目,
  改写为 slot)——不破坏探测链,连 backward-shift 都不需要。

调用点由此只需多传一个早已在手的老槽位号,无需重排先后顺序,守住 review-fix #5 的
"守卫先于变更"纪律。

`MemBreakdown.slot_by_parent_name_bytes` 口径同步改为 `entries.len() * 8`(est_map
退役该字段)。

## 方案 B:StrRef u16

```rust
#[repr(packed)]
struct StrRef { start: u32, len: u16 } // 8B → 6B
```

- 4 个引用数组 67MB → 50MB(−16.8MB @2.1M);
- `put` 变 fallible:`len > 65535` 返回 `None`,load/upsert 调用点对该行**跳过 + 计数
  日志**——与 v1.2 的 u32 id 载入守卫同一类纯防御(NTFS 名字分量 ≤255 UTF-16 字符,
  UTF-8 ≤~1KB,64KiB 现实不可达;守卫只防手工构造的库);
- `#[repr(packed)]` 注意事项:现有代码只按值读取字段(`r.start as usize`、`r.len`),
  从不取字段引用——保持这一纪律(克隆/复制出栈再读),不引入 packed 引用;
- `str_of` 的越界/UTF-8 防御 unwrap_or 不变。

## 测试

- hotindex 单测:表操作四件套(插入/命中/冲突链/删除回移后仍可命中)、扩容重铺、
  指纹碰撞走验证兜底(构造同指纹条目)、swap-remove **原地重绑**不丢邻槽、
  改名/中位删除后旧键查无新键查得;**同名重压夹具**(数千同名不同父条目,防聚簇
  回归——builder-check 裁定 2 的守卫);StrRef 边界(65535 通过 / 65536 拒绝且计数)、
  packed 布局 `size_of == 6` 断言;
- 既有 hotindex/engine/query 全量回归(改名、watcher 增删、compaction 路径都有测试);
- mem_probe(真实库副本)实测,数字回填 known-issues。

## 验收口径(builder-check 裁定 3)

- 基线 = main@7712757,分支 = 本轮,同会话背靠背各跑一次 `mem_probe` 与
  `mem_probe_scan`(用户真实索引的副本;取副本前让用户退出应用,复制 db + -wal/-shm
  三件);机器高负载日绝对数不可用,只取相对差;
- **回退门槛:8B 条目若令搜索 p95 相对劣化 >10%,回退 12B 全哈希条目**(−37MB → −25MB,
  文档化逃生口);
- 内存预期:live ≈340MB 量级、working set 降幅 ≥35MB(以实测为准回填 known-issues)。

## 阶段划分(实施顺序)

1. NameSlotTable + 四操作映射 + 单测(HashMap 退役);
2. StrRef 压缩 + fallible put + 单测;
3. mem_probe 实测 + known-issues/文档回填。
