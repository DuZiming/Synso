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

## 2. 文件变更事件 → 火焰图自动刷新未生效（用户报告）——已查证：刷新链路无缺陷

- **2026-10-30 取证结论（v1.1 任务 2）**:在带插桩的 release 构建、真实规模
  （28GB 索引、含回收站范围）下逐环验证：watcher → store/hot → FsChanged →
  前端 listener → 600ms 去抖 → size_tree 重取 → setOption 重绘,**全链路正
  常**。四个场景全部通过:强制删除、回收站式删除(宽视角 + 下钻视角)、新建
  目录、事件风暴(20 次操作坍缩为 1 次刷新,无级联)
- **用户所见"地图没动"的真实成因**:资源管理器删除 = 移入 `$Recycle.Bin`,
  而回收站同被索引且物理空间未释放——宽视角(卷根/(all))下索引总量几乎
  不变(字节只是换了位置),160 万文件规模下成分变化为亚像素级;下钻到被删
  目录的父级时地图**确实会立即缩(已验证)**
- **待用户决策的后续项**:`$Recycle.Bin` 是否默认排除出索引(排除后资源管
  理器删除会立刻在宽视角可见;其空间归入"未索引"块,与根层使用率叠加设计
  天然衔接)——产品决定,不在本轮擅自改动
- 相关记录:Task 16 的 minor "listen().then(push) 卸载竞态" 至今没修(卸载
  早于 listen resolve 时 unlisten 泄漏,实践中影响极小,留观)

## 3. 集合功能整体移除（用户决定，后续重新规划）——已完成（v1.1 任务 4/5）

- 已删除：UI（CollectionsView、CollectionPicker、快速窗多选/勾选、容量地图集合管线
  与死 prop）、commands（coll_* 与 file_id_of）、engine（collections.rs、7 个
  coll_* 方法、`coll:` 修饰符——现在按字面回退、三处 reconcile 调用点）
- DB 的 `collections`/`collection_members` 表保留未迁移，代码不再触碰；使用手册
  §6 删除、章节重排；i18n 键删净（128 键对齐）
- 重新规划时参考：引擎侧墓碑/重绑经验见 v1 提交历史（collections.rs 于
  6f033a3 删除）

## 4. 「火焰图」改名「容量地图」（用户决定）——已完成（v1.1 任务 3）

- i18n：zh `tabs.flame` → 「容量地图」、`flame.chartLabel` 同步；en → "Capacity Map"；
  代码符号（FlameView/FlameChartSunburst）未改名（用户可见文案优先）

## 5. 小问题备忘

- 设置页首次进入偶发空白（Promise.all 守卫 loaded 未放行，Task 29 会话
  实测出现过一次）——查 settingsGet/listVolumes 失败路径
- 应用关闭期间的磁盘变更仍需手动「重新扫描」（设计如此；用户曾问过，
  可考虑启动时轻量补扫）
- 空目录在容量地图不可见（目录行只在有子项时创建——0 字节纯外观）
- 调试版与发行版内存差距大（调试版无优化），验收性能一律以发行版为准
- 首扫后 dedup 手动触发时如果还没点过「启动分析」，首次会慢（要算全盘
  哈希；设计如此）
