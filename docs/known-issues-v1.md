# v1 已知问题清单（下个会话的规划输入）

> 2026-10-30 用户验收后确认合并 v1，以下问题留待下一轮规划解决。按影响排序。

## 1. 内存占用仍偏高（用户报告）

- 现状：竞技场化重构后 471MB @ 166万文件（原 976MB），但用户仍觉得高
- 剩余大头：两张 HashMap 二级索引（slot_by_id + slot_by_parent_name ≈
  130MB）、目录树 HashMap<FileId, DirEntry>（35.8万条）、并行 Vec 列
  （ids/parents/sizes/mtimes/ext_ids/logical_bytes ≈ 45MB）、bytes 竞技场
  本体 109MB
- 可选方向：合并数值列到 SoA 更紧凑类型（u32 代替 usize/i64 哈希值）、
  dir 树改用扁平数组 + 索引、内存分代（只常驻热区，冷区按需从 SQLite 读）
- 验收口径：重开 perf 测试里的 working_set 探针（mem_probe example 已在
  仓库，直接复用）

## 2. 文件变更事件 → 火焰图自动刷新未生效（用户报告）

- 现状：FlameView 挂载时 `listen("fs-changed")` + 600ms 去抖重取，但用户
  实测删文件夹后容量地图没动（仪表盘的文件数是动的——事件本身在发）
- 待查：事件名/载荷匹配、Svelte 5 `listen().then(un)` 注册时序（Task 16
  就记录过这个延迟注册模式在卸载时有竞态）、对比模式双窗格的刷新路径
- 相关记录：Task 16 的 minor "listen().then(push) 卸载竞态" 至今没修

## 3. 集合功能整体移除（用户决定，后续重新规划）

- 移除范围建议：UI（CollectionsView、快速窗多选加入、火焰图右键加入、
  CollectionPicker）→ commands（coll_*）→ engine（collections.rs、
  reconcile 调用点、coll: 查询修饰符）→ schema 表保留（不迁移，代码不再
  触碰）或一并删表
- 注意删干净 i18n 键（check:i18n 会兜底）和使用手册 §6
- 用户说"后面重新再规划"——留好引擎侧的墓碑/重绑经验，重新设计时可能
  复用

## 4. 「火焰图」改名「容量地图」（用户决定）

- i18n：zh 的 tabs.flame 及 flame.* 命名空间、en 同步；快捷键无涉
- 代码符号（FlameView/FlameChartSunburst）可顺手改名也可不动（用户可见
  文案优先）

## 5. 小问题备忘

- 设置页首次进入偶发空白（Promise.all 守卫 loaded 未放行，Task 29 会话
  实测出现过一次）——查 settingsGet/listVolumes 失败路径
- 应用关闭期间的磁盘变更仍需手动「重新扫描」（设计如此；用户曾问过，
  可考虑启动时轻量补扫）
- 空目录在容量地图不可见（目录行只在有子项时创建——0 字节纯外观）
- 调试版与发行版内存差距大（调试版无优化），验收性能一律以发行版为准
- 首扫后 dedup 手动触发时如果还没点过「启动分析」，首次会慢（要算全盘
  哈希；设计如此）
