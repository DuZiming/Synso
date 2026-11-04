# Synso — 热度持久化设计文档(Heat 落库 + 启动预热)

日期:2026-11-04。前置:v1.2 三层内存策略(2026-11-02 设计文档,已并入 main)。
用户已拍板方向:定期把 Heat 稀疏落库;启动时按分数回填热区直到预算;`preload_on_start`
改为真启动预热;CMS 死重删除。

## 这是什么 / 为什么

v1.2 的三层维度缓存里,热度(`TierCache::heat`)只活在进程内:重启清零,连**重扫**也会把
分数清掉——`flush_dimensions_and_preheat` 先 `clear()` 再按 id 回填值,`put_hot_direct`
重新播种 `Heat::new`(分数 0)。`mem_preload_on_start` 设置(默认开)因此名不副实:它只
能管"重扫后重预热",启动时没有任何持久化数据可载。结果:每天常用的那批文件,重启后第一轮
详情查看全部走冷读。

本轮把热度持久化到索引库的一张小表,启动与重扫后按衰减分回填热区;顺带删掉
count-min sketch(4×65536×u16 = 512KB,只在 `record_hit` 里被写、从未被读——纯死重)。

## 目标

- 重启后热区按"衰减后的分数"回填,直到 hot 预算或高分条目用尽;
- 离线时间参与衰减(上周的热点隔三天重启,分数按三天半衰,不是原样复活);
- 重扫重预热**携带原分数**,不再清零;
- `mem_preload_on_start` 语义兑现为"启动 + 重扫都预热";
- 删除 CMS,省 512KB 常驻与每次命中的无用哈希开销;
- 持久化是尽力而为(best-effort):失败只影响下次预热的准头,绝不影响任何正确性路径。

## 非目标

- 不做多维度热度泛化——只做当前唯一维度(FileDetail);表结构不预设维度轴。
- 不新增设置项(落库间隔、地板分、行数上限都是常量;地板分直接复用 `hot_threshold`
  这个既有旋钮)。
- 不做增量同步/合并——每次落库是全量快照替换,不做 upsert 差量。
- 不启用 CMS(替代方案即删除)。

## 方案

### 1. 存储:schema v4 小表

```sql
CREATE TABLE heat(file_id INTEGER PRIMARY KEY, score INTEGER NOT NULL, at INTEGER NOT NULL);
```

`score` = 衰减后的定点分(×256,与 `Heat::score` 同单位);`at` = 快照时刻的 Unix epoch
秒。迁移走既有 `migrate()` 台阶(`version < 4`),与 v1/v2/v3 同款一次性 DDL。

写入语义:**全量快照替换**——一个事务里 `DELETE FROM heat` + 批量 INSERT 当次快照。
表天然有界(≤ `HEAT_MAX_ROWS`),幂等,死 id/失效条目下次快照自动消失,不需要任何清理
逻辑。

### 2. 落库内容与时机

**内容**:`tiers.heat` 里每条先衰减到当前时刻(复用既有 `decay` 公式,精确反映会话内
 ageing),过滤 `衰减分 / 256 < hot_threshold` 的条目(地板分与晋升门同单位:整数命中数;
 存储列本身保持 ×256 定点,两端过滤都必须先除 256),按分数降序取前 `HEAT_MAX_ROWS`
(10_000)行,共用快照 epoch。地板分复用 `hot_threshold` 的理由:能回热区的就是"够格晋升"
的条目,低于它的 staging 居民重开后反正也回不去。

**时机(三处,同一个 `Engine::persist_heat()`)**:
1. **定期**:后台线程每 `HEAT_PERSIST_INTERVAL`(10 分钟)一次。线程在 `Engine::open`
   启动,拿 tiers/store 的 Arc 与一个 `recv_timeout` 通道;Engine drop → 发送端 drop →
   线程退出(watcher/dedup 同款生命周期模式)。
2. **退出**:`lib.rs` 的 `.run()` 换成带回调形式,`RunEvent::ExitRequested` 时同步调
   `persist_heat()`(主窗关闭被拦截为隐藏,该事件只会来自托盘退出路径)。store 锁用
   **try_lock**:拿不到(中扫退出的确认路径上,整场扫描持锁数分钟)就**跳过**——托盘
   "退出安全快速"的承诺绝不能为落库反悔,上一张 ≤10 分钟的快照足够;这也正好是
   "best-effort" 原则的兑现。定期线程保持阻塞锁(后台线程,挡住无害)。
3. **重扫后**:重扫伴生线程完成回填后顺手落库一次(捕获携带分数的最新状态)。

落库走 store 互斥锁(既有惯例:dedup 持久化同款)。可能被整场扫描的锁挡住——后台线程,
挡住无害;失败记日志跳过。

`preload_on_start = false` 时:跳过启动回填;落库照常(表保持新鲜,用户随时重新打开都
有数据,代价是一张小表的低频写)。

### 3. 启动回填

`Engine::open` 在热索引加载线程之后再起一个 `synso-heat-preheat` 线程:

1. 自开一个 `ColdReader`(既有惯例:引擎自身的连接非 Sync,伴生线程自开只读连接);
   `SELECT file_id, score, at FROM heat ORDER BY score DESC LIMIT HEAT_MAX_ROWS`;
2. `preload_on_start` 为假则直接返回(表不动);
3. 每条按 `now_epoch − at` 用同一半衰公式离线衰减,丢弃 `衰减后 / 256 < hot_threshold`
   的(与落库端同一地板、同一单位);
4. 存活条目分批(500/批)冷读 `file_details`,缺行(文件已删)自然消失;
5. 逐条 `put_hot_direct_with_score`——预算内驱逐最低分是既有行为,顺序降序插入即可。

这条顺便给了批处理 `ColdReader::file_details` 第一个真实调用方(终审 Minor 之一)。

### 4. tiers.rs 改动

- **新增 `put_hot_direct_with_score(key, value, bytes, score)`**:同 `put_hot_direct`
  的预算驱逐语义,但契约更完整(回填面对的是**活缓存**,不是重扫那种清空后的空缓存):
  - **接管 staging 孪生**:同既有晋升路径,入热时从 staging 移除——否则回填窗口内被摸过
    的 key 会热/staging 双驻留,双份计账直到 TTL 过期(今天的设计里这个状态不可达);
  - **分数取 max**:已有 Heat 条目则与落库衰减分取大者(`or_insert_with` 会把"用户重启后
    秒开、已被 put_staging 播了 0 分"的活跃文件压成分数 0 的头号驱逐对象——恰是本功能
    要改善的时刻;无条件覆盖又会盖掉现场挣到的分,取大两头都对),窗口状态保留已有值;
  - `put_hot_direct` 变为它的 `score = 0` 特例(或直接删除、调用点迁移)。
- **重扫重预热携带分数**:`flush_dimensions_and_preheat`(及其在 `start_full_scan` 伴生
  线程里的内联重复)改为:快照 `(id, 衰减分)` → `clear()` → 冷读 → 逐条带分回填。两份
  重复逻辑顺手统一(伴生线程改为调公开方法)。
- **删除 CMS**:`cms` 字段、`cms_bump`、`cms_hash`、`CMS_ROWS/CMS_WIDTH` 常量,及模块头
  与 `record_hit` 文档里的草图表述。`record_hit` 语义不变(只更新 Heat)。

### 5. 离线衰减的单位约定

`Heat::at` 是 `Instant`,进程内无法换算墙钟——但落库前每条已**衰减到当前时刻**,分值里
已经含了全部会话内 ageing,所以快照共用一个墙钟时间戳是精确的,不是近似。回填端用
`SystemTime` 差值继续半衰即可。时钟回拨(差值为负)按 0 处理。

### 6. UI 文案

设置页 `memPreload` 文案更新:现英文
"Re-preheat the hot tier after rescans (persistent heat is a follow-up)" →
"Preheat the hot tier at startup and after rescans";zh 同步(现"重扫后重新预热热区…" →
"启动与重扫后预热热区")。

## 已接受的近似

- **id 复用**:files 表无 AUTOINCREMENT,重扫的清空重插会复用 row id(v1.2 已知)。热度
  键是 id 不是文件——重扫后旧分数可能加在新文件上。v1.2 的重扫重预热已经带着这个近似,
  本轮不放大它(重扫回填的"新鲜值"始终是当前 id 的正确数据,错的只是分数归属,且只影响
  预热准头)。
- **崩溃丢一拍**:最多丢一个落库间隔(10 分钟)内的热度变化。缓存热度,不值得更重。

## 测试

- `tiers`:带分回填播种分数正确;`put_hot_direct` 特例等价。
- `store`:heat 表快照替换语义(第二次快照完全取代第一次)。
- `engine`:落库→重开→回填链路(直接调 `preheat_from_persisted` 的 crate 内缝);
  预写旧行 `at` 模拟离线时长,断言:衰减正确、低于地板分的被丢弃、已删文件缺行消失;
  `rescan_flushes_then_preheats` 扩展断言分数被携带。
- 定期线程与退出钩子不单测(线程生命周期与 Tauri 事件),靠日志与实测。

## 阶段划分(实施顺序)

1. schema v4 + `Store::replace_heat` + `ColdReader::read_heat`(含测试);
2. tiers:`put_hot_direct_with_score` + 删 CMS(含测试);
3. engine:`persist_heat` + 定期线程 + `preheat_from_persisted` + 重扫携带分数/统一重复
   逻辑(含集成测试);
4. app:退出钩子 + 设置页两处文案;
5. 实测验收(release 构建,重启场景对照 hot_hits/memlog),更新 known-issues/手册。
