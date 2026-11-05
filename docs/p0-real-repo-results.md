# P0 真实仓库首轮测量（语法层）

> 日期 2026-11-04。本轮只覆盖 [p0.md](p0.md) 末尾"仍需独立验收的质量门槛"中的**规模与语法层索引耗时**，不构成正式验收。

## 方法

- 仓库从 GitHub codeload 按固定 commit 下载 tarball，解压到工作区外的独立目录，不执行其中任何构建。
- 命令：`semweft --workspace <repo> --format json index --syntax-only`，**未授权 trust**，只有 L1/L2，无 L3 语义。
- 耗时为 CLI 客户端 `/usr/bin/time -l` 的墙钟时间，每个仓库只跑 1 次，冷 daemon，未重复，无置信区间。
- 机器：macOS（Darwin 25.6.0）；未锁定 CPU/内存型号，不满足 [quality.md](quality.md) 要求的固定基准机器。

## 结果

|仓库|commit|.rs 文件|行数|索引墙钟(s)|客户端峰值 RSS(MB)|符号|调用点|L1 关系|
|---|---|---|---|---|---|---|---|---|
|ripgrep|91f5d22e|102|55,477|0.14|15|4,578|13,448|275,456|
|clap|ec93e712|337|84,668|1.23|17|5,997|23,824|611,111|
|tokio|695a07dd|814|190,059|0.40|27|—|—|—|
|rust-analyzer|d385d3dc|1,504|593,360|1.32|40|—|—|—|

- tokio 与 rust-analyzer 的符号/关系数本轮未采集（`doctor` 输出结构不同，脚本没取到），只确认 `refresh.state=current`、语法 epoch 为 1。
- RSS 是客户端进程，不含 daemon，不能当作索引内存占用。
- 所有关系均为 L1 candidate（definite 为 0）；ripgrep 与 clap 各有约 5 万、7.9 万未解析 occurrence。

## 观察到的问题

- 未授权时 `index` 直接返回 `trust_required`，但该失败耗时随仓库增大（ripgrep 2.9s，rust-analyzer 35s），说明拒绝前已做了与仓库规模相关的工作，原因未查。
- 大量 `closure_owner_not_modeled` 诊断（闭包归属未建模）。

## 本轮未覆盖（P0 仍未完成）

- 20 个独立标注的改动任务：需要独立真值，不能由实现方自行标注后自证。
- 至少 2 个仓库的纯 CLI 人工对照（对照 IDE + rg）：需要人工参与。
- L3 语义索引：需要对第三方仓库授权执行 build.rs 与过程宏，未经用户确认不做。
- 语法层 P95 500ms 查询延迟：ripgrep/clap/tokio/rust-analyzer 均已测（见下），除 search 外多数未达标；每项仅 5 次，不是 P95。
- 百万行规模：最大样本 59 万行，未达 100 万行。
- 重复测量、固定机器、冷/热缓存区分。

## 语法层查询延迟（ripgrep，单仓库，2026-11-04）

方法：CLI 墙钟（含进程启动与 IPC），热 daemon，`semweft --format json <cmd> Searcher`，每项 4–6 次，本节只含 ripgrep（其他三个仓库见后文），无 P95 统计意义。

|查询|默认配置|`query_cache_bytes`=2GiB、`cursor_cache_bytes`=512MiB|
|---|---|---|
|search / symbol|约 150–220ms|约 175ms|
|refs|约 1,200ms|约 160ms|
|calls|约 1,200ms|约 150ms|
|impact|约 1,900ms，返回 `cursor_expired`|约 1,050ms，成功|

### 发现

- **默认查询缓存预算对 5.5 万行仓库已不够。** `status` 显示默认配置下 query_cache `estimated_bytes=0`、30 次 miss/30 次 eviction：单个视图的估算权重超过 64MiB 上限，每次查询后即被驱逐，`refs`/`calls`/`impact` 每次都重建约 27.5 万条关系的合并索引。调大后同一视图常驻，估算权重 526MiB。
- 视图估算权重 526MiB 对应 5.5 万行源码，但这个估算**不是虚高**：缓存该视图后 daemon 的实际 RSS 约 766MB（含持久化视图、语法事实和游标缓存，不能与估算一一对应，但量级一致）。内存大的原因是关系规模（L1 名称候选 27.5 万条）乘以每条关系的字符串开销：每条 `Relation` 带约 12 个独立 `String`（其中 `kind`、`freshness`、`runtime`、`source`、`indexer`、`derivation`、`file_hash` 在大量关系间完全相同），`RelationDeriver` 的按文件缓存（`Arc<Vec<Relation>>`）与合并后的 `IndexedView` 很可能各持一份关系数据，这一点只读了代码，未用实测验证。按此推算，百万行仓库的 L1 关系在当前布局下很难放进 64MiB 默认预算。已实施其中一个方向：对重复字符串做共享（`Arc<str>`，见下一节）；让索引与 deriver 缓存共享同一份 `Arc<Relation>` 未实施。
- 每个响应都携带全部 `known_gaps`（ripgrep 为 2,952 条、约 550KB，占 `search`/`refs`/`calls` 响应的约 80%）；`context` 已有按 kind 聚合的先例，其他查询没有。（后续实测更正：`cursor_expired` 并不是 `known_gaps` 造成的，见文末“缺口聚合”一节。）
- 缓存淘汰行为由现有测试 `cache_enforces_byte_budget_and_rechecks_lazy_index_growth` 固定，属有意的内存上限；默认预算与回收阈值在 roadmap 中仍是待决策项，未改默认值。
- 调大预算后 `impact` 仍约 1s，超过 500ms 目标，原因未查。

## 共享 Evidence 字符串后的内存变化（ripgrep）

`Evidence.indexer / derivation / file_hash` 由 `String` 改为 `Arc<str>`，L1 派生时每个文件只分配一次；序列化格式不变（serde `rc`）。同一流程（`query_cache_bytes`=2GiB，重启 daemon，索引后再 `refs`）：

|阶段|改动前 RSS|改动后 RSS|
|---|---|---|
|索引后|186MB|161MB|
|建立关系索引（`refs`）后|753MB|602MB|
|再次 `refs`|766MB|615MB|

关系索引新增部分约 567MB → 441MB（约 -22%）。`status` 的 `estimated_bytes` 仍为 526,548,194，因为它按 JSON 序列化字节估算，看不到共享，因此与真实内存的偏差变大，权重估算本身未调整。

## 另外三个仓库的查询延迟（2026-11-04）

条件：`query_cache_bytes`=2GiB、`cursor_cache_bytes`=512MiB；每项预热 1 次后测 5 次的 CLI 墙钟；歧义名称取非测试文件中限定名最短的候选，按 `symbol_id` 查询。`search` 用原名称。

|仓库|search(ms)|refs(ms)|calls(ms)|impact(ms)|索引后 RSS|查询后 RSS|
|---|---|---|---|---|---|---|
|ripgrep（上一节）|约175|约160|约150|约1,050|—|615MB|
|clap|约226|382–484|225–250|655–906|237MB|2,165MB|
|tokio|346–387|10,331–12,560|10,244–10,547|10,688–13,576|450MB|3,607MB|
|rust-analyzer|1,117–1,692|9,826–141,669|10,724–158,241|10,630–166,630|1,568MB|8,484MB|

- **clap**：视图常驻缓存，查询在 0.2–0.9s，`impact` 超过 500ms。查询后 RSS 2.1GB，对应 8.5 万行源码。
- **tokio**：视图估算权重超过 2GiB 上限，`status` 显示 18 次淘汰、缓存字节数为 0，每次查询都重建关系索引，`refs/calls/impact` 都在 10s 以上，查询后 RSS 3.6GB。
- **rust-analyzer**：`refs/calls/impact` 的 5 次测量在约 10–11s 与 128–166s 之间交替，这 5 次中至少有一次返回 `timeout` 错误（脚本只记录最后一个错误码，没有逐次对应），原因未查。查询后 RSS 8.5GB，`search` 也在 1.1–1.7s。
- 以上均为 L1 名称候选关系，不含语义索引。名称匹配会让每个同名 occurrence 与每个同名符号两两成关系（见 `engine.rs` 的 L1 派生循环），因此候选关系数与同名符号数相关；各仓库的关系总数本轮只在 ripgrep 和 clap 上取到（27.5 万、61.1 万），tokio 与 rust-analyzer 未取到，所以“不随行数线性增长”只是推断，未验证。
- 结论：在当前"全量物化 L1 候选关系"的设计下，clap（8.5 万行）以上的仓库在 `refs/calls/impact` 上已不满足 500ms 目标。继续调大缓存只是用内存换时间（tokio 已占 3.6GB，rust-analyzer 8.5GB），不是合理的出路。共享字符串只带来约 20% 的内存改善，不改变这个量级。可能的方向是按需派生关系或限制同名候选的展开，这是设计层面的变更，下一节已对点查询实施。

## 缺口聚合与建索引优化（ripgrep，2026-11-05）

**缺口聚合**：`known_gaps` 中同类型超过 3 条的条目折叠成 `{kind, count, samples[3], details_tool}`，不超过 3 条的保持原样；`doctor` 的 `analysis_health` 新增 `diagnostic_counts`、前 1000 条 `diagnostics` 和 `omitted_diagnostics` 作为完整明细入口。因协议未发布，没有保留旧格式。

|指标|聚合前|聚合后|
|---|---|---|
|`known_gaps` 条目数（`search Searcher`）|2,952|5|
|`search` 响应体积|约 770KB|约 111KB|
|`calls` 响应体积|约 665KB|约 5KB|
|`search`（默认配置）|约 150–220ms|约 110–130ms|

- **更正**：我先前预测聚合后 `impact` 的 `cursor_expired` 会消失，**这是错的**。默认配置下 `impact` 仍返回 `cursor_expired`。调大游标预算后看到 `impact` 响应约 1.2MB，其中 `nodes` 约 773KB、`relations` 约 145KB、`frontier`/`output` 各约 27KB，`known_gaps` 已不是大头；9 个游标合计约 290MB。原因是游标保存了整次遍历的结果（还剩 63,260 项），而不是只保存已返回的 100 项，未改。
- `refs`/`calls` 在默认配置下仍约 1.2s，因为视图缓存抖动（见前文），与响应体积无关。

**建索引（冷启动首次 `refs`，4 次，重启 daemon，2GiB 缓存预算）**：

|版本|首次 `refs`|之后 `refs`|daemon RSS|
|---|---|---|---|
|共享字符串后（基线）|1,155–1,255ms|111–118ms|约 700MB|
|本轮改动后|1,046–1,096ms|105–116ms|约 685MB|

约 -8% 的时间、-2% 的内存。改动：`IndexedView::new` 只在键首次出现时克隆符号和文件名；两处重复的 `BTreeMap` 合并逻辑改为同一个排序后折叠相邻同 id 的实现；`RelationDeriver::derive` 末尾不再把每条关系克隆进全局 `BTreeMap`；派生缓存键改为流式哈希（`digest_json`）。

**分阶段计时**（release，临时测试，已删除；ripgrep 275,464 条关系）：

|阶段|耗时|
|---|---|
|语法分析（tree-sitter）|约 530ms|
|`derive`（冷）|约 720–900ms|
|`derive`（第二次）|约 640–780ms，77 个文件中重建 77 个|
|克隆全部关系|约 65ms|
|按 id 排序|约 5ms|
|`merged_relations`|约 750–1,050ms|
|`IndexedView::new`|约 110–130ms|
|`estimated_heap_bytes`|约 80–105ms|
|其中 `identity()` 哈希（每条关系 2 次）|约 234ms|

- 上面的结论：**合并、排序、克隆都不是瓶颈**，我之前的几次判断（全局 map 键克隆、缓存键 JSON 序列化）都没有带来明显改善，已用计时排除。
- `RelationDeriver` 自带 64MiB 缓存对 ripgrep 形同虚设：第二次 `derive` 仍重建了全部 77 个文件，缓存里只留下 24 个。
- 冷派生本身约 700ms，其中 `identity()` 约占三分之一，其余是为每个“出现位置 × 同名符号”构造 `Relation` 的字符串分配。只要仍然物化全部 27.5 万条关系，这部分降不下来。
- `impact` 在关系索引已缓存时仍约 1s，原因未查，与上面的建索引成本无关。

## 点查询按需派生（2026-11-05）

`StoredView::index()` 改为返回新的 `QueryIndex`：只常驻已发布/引用层的少量关系，加上按名称、调用者、位置建立的紧凑表；`adjacent` / `relations_at` 在查询时才派生该符号或位置相关的语法关系，并按 id 叠加显式关系，叠加顺序与全量合并一致。全量扫描（服务聚合、diff 关系增减、模块地图、循环检测）和“按任意 id 取关系”改用 `full_index()`，仍会物化全部关系，这些路径没有变快。派生关系的构造抽成 `l1_relation`，全量派生与按需派生共用。遍历与路径枚举的节点/边改为拥有所有权的 `String`，关系在单次遍历内缓存。

**ripgrep（默认配置，不调缓存预算）**：

|指标|改动前|改动后|
|---|---|---|
|冷启动首次 `refs`|1,046–1,096ms|144–151ms|
|之后 `refs`|105–116ms|106–121ms|
|daemon RSS|约 685MB|183–206MB|
|`refs`（默认 64MiB 缓存）|约 1,200ms|约 107ms|
|`calls`（默认 64MiB 缓存）|约 1,200ms|约 100ms|
|`impact`（默认配置）|约 1,900ms，`cursor_expired`|约 970ms，仍 `cursor_expired`|
|缓存估算权重|526MiB|约 57MiB（上限 64MiB，余量很小）|

**clap（2GiB 缓存预算，5 次）**：search 约 226→134ms，refs 382–484→约 150ms，calls 225–250→约 135ms，impact 655–906→约 556ms，查询后 RSS 2,165→1,553MB。1.5GB 的构成未查，怀疑是游标缓存持有大型 `impact` 结果，未验证。

**正确性**：新增 2 个差分测试，用带歧义同名符号的多文件夹具对每个符号、三个方向、每个位置，比较按需结果与全量合并索引逐条相等，并覆盖已发布层与引用层叠加、叠加后带旧快照 origin 的关系不出现在当前位置。对其中两处做了变异检查（不再剔除带 origin 的关系；叠加时不覆盖 certainty），两个测试均报失败。另在 ripgrep 与 clap 上用旧版（上一个提交）与新版对 `refs`、`calls`（两个方向）、`impact`、`path` 共 35 个查询比较 JSON，去掉游标、耗时、存储统计和 `known_gaps` 后全部一致；其中末尾仓库 15 个响应有 13 个非空、0 个错误。该对照不比较 `known_gaps`，因为旧版没有缺口聚合。

**未覆盖**：`impact` 约 1s 与 `cursor_expired` 未解决，原因同前（遍历结果与游标整体保存）。tokio 与 rust-analyzer 的新版数据见下一节。

## tokio 与 rust-analyzer 在按需派生后（2026-11-05）

条件同前（`query_cache_bytes`=2GiB、`cursor_cache_bytes`=512MiB，每项预热 1 次后测 5 次，CLI 墙钟）。括号内为改动前。

|仓库|search(ms)|refs(ms)|calls(ms)|impact(ms)|索引后 RSS|查询后 RSS|缓存估算|
|---|---|---|---|---|---|---|---|
|tokio|208–216（346–387）|223–243（10,331–12,560）|513–533（10,244–10,547）|532–575（10,688–13,576）|457MB（450）|1,784MB（3,607）|186MB，0 次淘汰|
|rust-analyzer|664–722（1,117–1,692）|654–734（9,826–141,669）|646–702（10,724–158,241）|2,385–2,574（10,630–166,630）|1,229MB（1,568）|3,609MB（8,484）|670MB，0 次淘汰|

**更正前文的 tokio / rust-analyzer 旧数据。** 旧版 tokio 的 `calls`、`impact` 返回的是 1 个节点而不是关系，而新版返回 100 条关系（`calls` 被 `node_limit` 截断，还剩 17,890 项）。最可能的原因：旧版在视图被淘汰后重建关系索引要约 10s，超过默认 `timeout_ms` 的 5s，遍历在第一个检查点就因超时返回，只有根节点。因此旧表里这两列 10s 以上的延迟量的是“超时后的空结果”，不能与新版的真实结果逐项比较；`refs` 不走同一个检查点，旧版 `refs` 仍返回了 100 条。该解释没有用旧版二进制复现验证。

- rust-analyzer 的 `calls` 新版仍只返回 1 个节点。`Semantics` 是结构体，ripgrep 的 `Searcher` 也是 `calls` 只返回根节点，所以大概率是同一原因；rust-analyzer 这一项没有单独验证。
- 在 500ms 目标上：tokio 的 `search`/`refs` 达标，`calls`/`impact` 约 520–575ms 略超；rust-analyzer 全部超过，`impact` 约 2.5s。单次 5 个样本，不是 P95。
- 缓存估算 tokio 186MB、rust-analyzer 670MB，都超过默认 64MiB 上限；默认配置下这两个仓库仍会反复淘汰重建，本轮没有在默认配置下测。
- rust-analyzer 查询后 RSS 仍有 3.6GB，构成未查。

## 符号级 impact 游标按页编码（2026-11-05）

`impact_of` / `call_graph` 不再先把整条遍历结果构造成 JSON，再将完整 JSON 放进游标。遍历完成后，原始关系、深度和路径按 256 条 LZ4 压缩存入游标；分页时只解码相关块并生成当前页 JSON。游标同时缓存名称、测试范围和文件哈希，不再引用整个 `StoredView`。遍历边界、完整统计、返回字段及游标重放语义不变。实现与估算口径见 [query 层说明](../crates/semweft-query/README.md) 和 [基础设施预算](foundations.md)。Diff 影响报告及内部 `compute` 调用仍走原先的完整 JSON 路径。

**四仓延迟对照**：同一台机器、同一份仓库索引和解析后的唯一符号，旧 release 二进制与本次 release 二进制，`query_cache_bytes`=2GiB、`cursor_cache_bytes`=512MiB。每个仓库先预热 1 次，再测 5 次 CLI 墙钟；下表给中位数与实际最小–最大值，仅是 5 个观测值，不是 P95。耗时包含 CLI 启动和 IPC。

|仓库|改动前 impact(ms)|改动后 impact(ms)|impact 后 daemon RSS (前→后)|游标估算 (前→后)|
|---|---|---|---|---|
|ripgrep|997.5（988.6–1,067.7）|664.6（655.1–697.5）|2,202→529MB|532→275MB|
|clap|549.4（531.6–576.0）|434.4（416.8–442.9）|1,521→450MB|485→102MB|
|tokio|518.9（507.2–522.8）|450.1（420.7–494.0）|1,610→742MB|472→90MB|
|rust-analyzer|2,350（2,267.1–2,653.1）|2,005.4（1,852.6–2,419.6）|3,639→2,262MB|335→358MB|

耗时变化支持“完整 JSON 物化与游标持有大型图结果是主要成本之一”，但不证明它是唯一成本：rust-analyzer 仍约 2s，剩余约束未定位。该仓的图关系压缩率也低，估算游标体积略增；daemon 查询后 RSS 仍降低约 1.3GB。单次最大值仍可能受机器负载影响。

**默认 64MiB 缓存配置**：四仓的游标均可建立，没有再遇到 `cursor_expired`；impact 后游标估算分别约 46、51、60、59MB。ripgrep/clap/tokio/rust-analyzer 的 5 次 impact 中位数分别约 724、486、556、2,232ms。status 仍显示 tokio 和 rust-analyzer 的 64MiB 查询视图缓存被淘汰（impact 测量后 `query_cache` 为 0、累计 7 次淘汰），因此默认配置延迟仍包含关系查询视图重建；游标优化没有改变该项预算。

**行为对照与验证**：四仓首页完整 JSON（排除 cursor、耗时和存储计数）逐字段相同；另对每仓比较全部 impact 页、L1 过滤、空结果、callers/callees/both 方向及受限遍历，24 组 JSON 均相同。rust-analyzer `calls` 仍为 1 个节点，这一问题没有变化。新增测试覆盖实际多页拼接、游标重放、过滤和遍历限制、origin 坐标、源文件刷新后的固定结果、参数绑定、视图释放以及 64MiB 预算。query 单元测试 87 项、全 workspace 全 feature 测试 340 项通过；Clippy `-D warnings` 和格式检查通过。

## 按阶段继续优化查询与报告（2026-11-05）

本轮基线为上节符号级游标优化之后、此次顺序优化之前的 release 二进制。依次完成：未变化输入复用语法视图；查询缓存区分刷新常驻事实与查询独占分配，位置索引改用排序偏移；遍历内共享 ID、借用过滤关系、消费已缓存关系并减少路径复制；关系块使用字符串字典、小结果省略编码；测试推荐直接消费原始关系，diff 先去重再生成关系 JSON，报告列表按块压缩。实现细节与预算口径见 [query 层说明](../crates/semweft-query/README.md)。

**默认配置对照**：查询缓存与游标缓存均为默认 64 MiB；四仓使用原先相同源码、语法索引和唯一目标符号。每项预热 1 次后采样 5 次，计 CLI 墙钟，包含启动和 IPC；不是 P95。下表为整批对照的中位数与实际范围，MB 为十进制。

|仓库|本轮前 impact(ms)|本轮后 impact(ms)|daemon RSS (前→后)|本轮后查询缓存估算|
|---|---|---|---|---|
|ripgrep|635.9（619.3–645.5）|770.2（717.0–854.0）|407→355MB|5.23MB|
|clap|470.6（456.4–482.9）|373.4（366.8–393.4）|425→369MB|7.68MB|
|tokio|537.8（525.3–546.9）|338.1（331.4–338.8）|675→543MB|15.98MB|
|rust-analyzer|2,211.3（2,204.6–2,259.6）|1,443.3（1,398.4–1,457.3）|1,881→1,591MB|55.93MB|

ripgrep 整批结果明显变慢，因此追加同一最终二进制的独立复测：594.6ms（585.3、591.2、594.6、663.7、699.8），RSS 353MB。此前字典编码阶段的同条件 5 次中位数为 585.2ms。保留整批慢样本，不以复测替换它：当前波动尚大，ripgrep 没有达到 500ms，也不能据此宣称稳定提速。clap/tokio 此组热查询低于 500ms，rust-analyzer 仍超过 1s。

四仓查询缓存本轮均为 **0 次淘汰**，clap/tokio/rust-analyzer 在基线整批结束时均为 0 个视图、7 次淘汰。新估算排除了刷新协调器已经持有的事实，并真正减少了逐 occurrence 位置索引的树节点；它与此前把完整快照计入查询缓存的 186/670MB **不是相同计重口径**，不能把二者差值视为释放的内存，更不表示进程总内存受 64 MiB 约束。实际 RSS 另列于表中。

字符串字典使同一默认预算下的累计游标估算由 ripgrep 45.77→39.50MB、tokio 59.96→55.10MB、rust-analyzer 59.36→51.52MB；tokio 两侧均保留 4 个游标，其余两仓各保留 1 个。字典存在编码成本：分段样本中 ripgrep 的 graph_pack 从约 86–90ms 增至约 120–132ms，不能把体积缩小等同于首屏编码加速。最终 ripgrep 分段样本的 scan 约 39–40ms、traverse 249–256ms、graph_pack 129–132ms、page 约 2–3ms。rust-analyzer 字典阶段分段样本的 scan 约 150–159ms、traverse 647–684ms、graph_pack 176–184ms；它的主要已测成本仍在遍历。前台两次内容采集继续保留。

**Diff 与测试推荐补测**：另建固定语法样本，包含 200 个调用函数、100 个显式 Rust 测试；叶函数体从 `1` 改为 `2`。符号影响包含 1,700 条关系，双侧 diff 包含 3,400 条关系，测试推荐均为 100 个候选。两版各用独立持久化目录，默认双 64 MiB 缓存，通过同一 MCP 会话预热一次后测 5 次请求墙钟，故不与 CLI 四仓表混用。

|路径|本轮前中位数(ms)|本轮后中位数(ms)|完整结果对照|
|---|---|---|---|
|符号测试推荐|127.7（123.0–164.3）|114.4（108.6–118.3）|18 页相同|
|diff impact|93.9（91.4–98.9）|85.1（84.3–92.1）|35 页相同|
|diff 测试推荐|168.8（156.9–172.0）|164.9（153.2–177.6）|36 页相同|

第三项采样结束时，基线累计淘汰 63 个游标、保留 4 个、估算 61.03MB；新版累计淘汰 0 个、保留 67 个、估算 45.68MB。这是同一查询与续页序列的累计状态，不是单个报告大小。Diff 测试推荐耗时差异很小，范围重叠，主要收益为游标容量；diff 聚合仍需要完整关系 JSON，首屏仍需完成有界遍历和报告计算。

单独核查 rust-analyzer 的 `Semantics`：目标确为 `struct`，完整 refs 返回 463 条 L1 关系，其中 type 314、import 90、read 59，没有 call。因此 calls 仅返回根节点符合当前语法索引；这不表示已证明运行时或完整语义层没有调用。此核查还观察到，daemon 重启后直接用默认 5s 查询会超时；先完成 syntax-only 索引再查询可成功。上述延迟均为索引后的热请求，未解决冷启动首次分析成本，也未分解剩余约 1.6GB RSS 的全部构成。

**验证**：四仓的 impact 全页、L1/空结果、三个 calls 方向以及测试推荐全页/过滤共 32 组新旧 JSON 完全一致，比较保留 known_gaps，仅忽略游标身份、耗时及存储状态。合成报告的首页和全页列表也一致。新增覆盖共享事实计重、历史快照去重、未变输入绕过解析器、过滤谓词与证据归一化一致性、字典中的 L2/L3/Unicode/缺失端点、报告跨字段跨块分页与重放、原始关系和 JSON 测试证据等价性及重复边键的节点展开。最终全 workspace 全 feature 测试 345 项通过、11 项忽略；Clippy `-D warnings`、格式检查和差异空白检查通过。

## rust-analyzer impact 百次采样（2026-11-05）

补做符合样本数要求的单查询 P95：语法索引完成后，固定 `Semantics`（`hir::semantics::Semantics`）与 snapshot，默认 query/cursor 缓存各 64 MiB，连续执行 100 次 `impact` 首页 CLI 请求；墙钟包含进程启动、IPC 和标准输出传输，文件系统已预热。最近秩 P95 为 **1,692.1ms**，中位数 **1,483.4ms**，范围 **1,404.8–1,798.4ms**，仍明显未达 500ms。首次语法索引耗时 42.1s，采样时 daemon RSS 约 1.07GB；游标缓存记录 100 次淘汰，表明重复生成大游标仍有额外开销。结果文件：`target/desktop-preview/performance-rust-analyzer-latest.json`。这是单仓、单查询、单机的 100 次采样，不代表其他仓库或混合负载 P95；其余仓库仍需各自做 100 次采样。
