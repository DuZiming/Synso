# Synso — v1 验收后修复轮设计文档(v1.1)

日期:2026-10-30
状态:待用户批准

## 这是什么 / 为什么

2026-10-30 v1 通过验收并合并(含验收工作:`origin/feat/synso-v1` → main,合并提交 ac48bdc)。用户随后在实机(Windows 11,约 160 万文件,release 构建 `target\release\synso-app.exe`)复测反馈六个问题,与 `docs/known-issues-v1.md` 合并整理为本轮范围。用户已确认任务顺序与两个产品拍板(根层使用率形态、集合 DB 表保留)。

实机关键数字:**release 构建、160 万文件、进程内存 ≈ 1GB、启动"未响应"数十秒后自愈**。

## 任务(按执行顺序;每任务独立 commit)

### 任务 0(已完成):合并验收分支

`origin/feat/synso-v1`(25 个提交)并入 main。

### 任务 1:启动卡死与设置页慢(根因已定位,修锁)

**现象**:启动后主窗口"未响应"数十秒、自愈;设置页打开要等很久。

**根因(已读码核实)**:

1. `Engine::open` 起后台线程 `reload_hot_index`,全程持有 store `Mutex` 直到 `HotIndex::load` 完成(160 万行,数十秒);
2. 主线程 `.setup()` → `quick::init` → `engine.settings()` → `settings::load(&store)` 抢同一把锁 → 主线程消息循环冻结 = Windows 判"未响应";
3. 前端首屏与设置页的 `settings_get` 同样排队;设置保存期间 `set_scan_options` 与整个扫描 walk 串行,雪上加霜;
4. `list_volumes` 是同步命令(主线程执行),`sysinfo` 磁盘枚举本身可能 1–3 秒。

**修法**:

- **设置读快路径**:Engine 内缓存 `Settings`(独立小锁);`open` 时经 `load_persisted`(独立连接,已有)预热;`settings()` 返回缓存副本;`set_settings` 写穿 store 后更新缓存。设置**读取**从此不等索引加载/扫描;
- **范围界定(评审后定)**:只做读路径。设置**写入**仍经 store 锁——加载/扫描期间点保存,保存按钮会等到它们结束(async 命令,不冻结 UI,只是等)。此为已接受行为,记录在案;若实际使用中咬人再立任务(那要拆扫描的整段持锁,是锁架构级改动,不在本轮);
- `list_volumes` 命令改 async(线程池);
- 复查其余同步命令(`engine_stats`、`dedup_*` 等)在锁下的最大持有时长,凡可能秒级的改 async 或在设计文档外记录保留理由。

**验收**:160 万文件库冷启动——主窗口 2 秒内可交互(骨架即可),快捷键/托盘立即可用;设置页打开 <300ms;现有测试全绿。

### 任务 2:容量地图删除文件夹后不刷新

**现象**:运行中删除文件夹,仪表盘文件数变化,容量地图不动。

**已排除**:引擎侧无缓存(`size_tree_dirs` 每次现算);事件在发(`FsChanged` → `fs-changed` emit)。静态读码另显示 sunburst 的 `$effect` 依赖 tree/root prop 并以 `notMerge` 重跑 `setOption`,"数据变了图不重画"这条嫌疑被削弱(评审注),但复现取证前不最终排除。

**嫌疑(按序排查;先复现取证据再动手,系统化调试纪律)**:

1. `listen("fs-changed")` 注册/解注册时序竞态(Task 16 记录过的 `listen().then(un)` 模式);
2. 事件到窗格的路径:对比模式双窗格、`visiblePanes` 取值时机;
3. sunburst 重绘路径(静态读码已削弱,兜底保留)。

**验收**:运行中删除/新建文件夹,600ms 去抖后地图刷新且保持钻取位置;对比模式两窗格都刷新。

### 任务 3:根层磁盘使用率 + 「火焰图」→「容量地图」改名

- `VolumeInfo` 增加 `available_bytes`(sysinfo `available_space`),随 `list_volumes` 返回;
- **「根层」的精确定义(评审后定)**:
  - 物理容量叠加(「未索引」块 = 物理已用 − 根层索引合计,≤0 隐藏;灰色「剩余空间」块 = `available_bytes`;中心/悬停「C: 已用 800.2 GB / 共 931 GB」文案)**仅当窗格根是单个卷挂载点**(与 `list_volumes` 结果按挂载点匹配);
  - 子目录作为扫描根(如 `D:\data`)时不叠加——"物理已用"在子树上没有无歧义含义,索引合计即该子树的真实;
  - `(all)` 总览视图不加块;各卷弧的悬停/标签补充「已用 / 共」文案;
  - 卷数据随每次树刷新(含 fs-changed 去抖重取)一起重取,保证删除文件后"已用"立即跟上;根匹配不到已列出的固定卷(可移动/网络卷)时隐藏叠加,不报错。
- i18n:zh「容量地图」/ en "Capacity Map";`tabs.flame` 与 `flame.*` 命名空间同步;代码符号(FlameView 等)不改名(用户可见文案优先,known-issues 已有此定夺)。

**验收**:C 盘根层"已用"与资源管理器一致(允许小误差);两语言键完整(`check:i18n` 过)。

### 任务 4:集合功能整体移除

用户决定,后续重新规划。删除范围:

- **UI**:`CollectionsView`、`CollectionPicker`、快速窗多选「加入集合」;容量地图的集合管线(`addToCollection`、`pickerIds` 及 sunburst 组件里**从未接线的** `onAddToCollection` 等 prop 声明一并删除——评审勘误:右键菜单本就未实现,验收不再检查"右键菜单无集合入口");
- **commands**:`coll_*` 全部;
- **engine**:`collections.rs`、engine 内 `reconcile` 调用点、`coll:` 查询修饰符(解析器不再识别,按未知 token 字面回退——与 `size:abc` 的既有行为一致);
- **DB**:`collections`/成员表保留不迁移,代码不再触碰(用户已同意);
- **文档**:使用手册 §6 删除;i18n 键删净(`check:i18n` 兜底)。

**验收**:代码全文检索 coll/collection 无残留(除 store schema 声明与迁移历史);手测快速窗多选、容量地图无任何集合入口;查重/搜索/容量地图不受影响。

### 任务 5:内存占用(先测量拆账,后定方案)

**现象**:release、160 万文件、实机 RSS ≈ 1GB;基准 `mem_probe` @166 万 = 471MB。缺口 ~500MB 未解释,不预设结论。

**第一步:测量拆账(强制)**:

1. 真实 `synso.db`(160 万)上跑 `mem_probe`,复核引擎侧数字;
2. 真实 app 进程加运行时内存日志:引擎各结构尺寸(多数已有 `stats` 口径)+ Windows 进程级 working set / commit 周期采样写日志;
3. 三种状态对比:纯加载 / 加载+全扫一遍 / watcher 空闲 1 小时,区分常驻结构、高水位与碎片。

**候选方向**(拆账后选,known-issues 已列):二级 HashMap → 紧凑 SoA(u32 slot 索引)、目录树扁平数组化、冷区回落 SQLite 按需读、arena 高水位回收。

**验收**:实机 release、160 万文件 RSS 显著下降——**具体目标数字在拆账完成后补记进本文件并经用户确认**;搜索 p95 < 50ms 不回归;churn/扫描 perf 测试不回归。

## 非目标(本轮不做)

- 集合的替代设计(只移除,不做新的);
- 启动时轻量补扫(关机期间磁盘变更,备忘在案);
- 空目录在容量地图可见;
- 设置页偶发空白的独立修复——若任务 1 顺带解决则记录,不单独立项;
- 订阅激活 / 账号系统(商业发布前独立工作流)。

## 约束

- 沿用 v1 技术栈与纪律:TDD(先失败测试)、`cargo clippy -- -D warnings` 干净、每任务独立 commit;
- 性能验收一律 release 构建(调试版内存/速度不作数);
- `synso-core` 不依赖 Tauri 的 crate 边界保持不变;
- 本轮完成后更新 `docs/known-issues-v1.md` 勾销对应条目。
