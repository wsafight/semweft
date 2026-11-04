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
- 每个响应都携带全部 `known_gaps`（ripgrep 为 2,952 条、约 550KB，占 `search`/`refs`/`calls` 响应的约 80%）；`context` 已有按 kind 聚合的先例，其他查询没有。`cursor_cache` 因此在默认配置下被单个结果占用约 54/64MiB。
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
- 结论：在当前"全量物化 L1 候选关系"的设计下，clap（8.5 万行）以上的仓库在 `refs/calls/impact` 上已不满足 500ms 目标。继续调大缓存只是用内存换时间（tokio 已占 3.6GB，rust-analyzer 8.5GB），不是合理的出路。共享字符串只带来约 20% 的内存改善，不改变这个量级。可能的方向是按需派生关系或限制同名候选的展开，这是设计层面的变更，本轮未实施，也未评估收益。
