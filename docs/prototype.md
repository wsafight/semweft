# Semweft 编辑期语义刷新原型：进展与测量

> 本文：[语言与 IDL 支持](language-support.md)第 5.4 节前置原型的实现范围、验证方法与本地测量。 [返回文档索引](../semweft.md)

**状态（2026-10-30）**：LSP 路径的刷新查询集、pending→current 发布、连续编辑、取消与崩溃恢复、Cargo 配置变更已有可运行原型，并在 3 个真实 Rust 仓库上通过增量与冷启动一致性对比（未解释差异为零）；内嵌 ra_ap_* 尚未比较，references 刷新未实现，测量未重复，因此存储与调度方案仍不定稿。下文数字均为单机本地观测，不是产品性能承诺。

## 1. 运行方式

```bash
semweft prototype scan --workspace <dir>                 # 仅 L1 语法事实，不启动外部工具
semweft prototype check --workspace <dir> --edits <scenario.json> \
  --accept-build-execution [--rust-analyzer <path>] [--timeout-ms 30000]
```

check 将工作区复制到临时目录，按场景逐步编辑；每步用常驻 rust-analyzer 增量刷新，再用独立的解析器、协调器和 LSP 进程冷启动分析同一快照，按 relation_id 比较关系集合与 known_gaps。不一致时退出码为 5。原仓库不被修改（报告字段 `source_inputs_unchanged`）。

场景格式：

```json
{"steps":[{"name":"...","fault":"interrupt_refresh | crash_backend（可选）",
  "edits":[{"path":"src/lib.rs","content":"整文件内容；null 表示删除"},
           {"path":"Cargo.toml","replace":{"old":"恰好出现一次的原文","new":"替换文本"}}]}]}
```

只接受 `.rs` 与 `Cargo.toml` 编辑；`interrupt_refresh` 不能是最后一步。

**后端版本**：rustup 代理按目录选择工具链，`--version` 在临时副本中执行，与实际后端一致。仓库用 rust-toolchain.toml 固定了未安装 rust-analyzer 组件的工具链时返回 `indexer_unavailable`；可用 `--rust-analyzer` 传入其他工具链 rust-analyzer 的绝对路径，sysroot 仍取仓库固定的工具链。

## 2. 对 5.4 节问题的原型回答

|问题|原型做法|验证|
|---|---|---|
|刷新查询集|以最近一次已发布语义视图为基线做规划。语法层为每个文件计算“函数体省略后的条目骨架”哈希：只有函数体变化时（`body_local`），仅重查变化文件内的调用点，其他调用点复用上次结果，目标若位于变化文件则按（文件、限定名、种类）唯一映射到新符号，映射不唯一则改为重查；骨架、Rust 文件集合、Cargo 等非 Rust 输入或分析版本变化时全量重查（`full`）并记录原因。带非白名单属性（可能是过程宏）的函数体计入骨架。方法调用的定义落在 trait 方法上时，再查 `textDocument/implementation` 列出工作区内实现|复用结果与新查询结果走同一套发布校验；每步与冷启动对比|
|pending 转为 current|后端同步全部文档、等待 quiescent 后，计划内调用点全部得到结果才整体发布；`complete` 要求每个调用点恰有一个结果|核心单测覆盖不完整、重复、错配批次|
|连续编辑|被新编辑打断的刷新调用 `cancel_refresh`，其部分结果被拒绝（`superseded`）；下一次规划仍以最后的已发布视图为基线，失效文件自然取并集|fixture 与真实仓库各含一次打断|
|取消与崩溃|查询之间检查取消；后端退出时以固定快照重新启动并同步（每步最多 2 次），计划与复用结果不依赖后端状态|注入 `crash_backend`，重启后结果与冷启动一致|
|Cargo 配置|非 Rust 输入变化时发送 `rust-analyzer/reloadWorkspace`，以响应之后的新 quiescent 状态为屏障|启用 feature 后 cfg 代码中的关系由 candidate 变为 definite，与冷启动一致|
|接入方式|仅实现外部 LSP 进程|内嵌 ra_ap_* 未比较，见第 4 节|

**证据口径补充**：

- 方法调用、多目标与实现枚举得到的目标一律为 candidate；实现枚举的证据推导标为 `lsp_implementation+syntax_callsite`，并记录 `dispatch_partially_enumerated` 缺口（继承的默认方法、blanket 与外部 impl、运行时目标未证明）

- rust-analyzer 会解析 cfg 关闭代码中的路径。原型用 pull 诊断（`textDocument/diagnostic`）中的 `inactive_code` 范围标记调用点，这类绑定保持 candidate 并记录 `cfg_inactive` 缺口；否则未编译代码中的调用会被误标为 definite

## 3. 本地测量

环境：macOS（Apple Silicon），rust-analyzer 1.99.1（release 构建的 semweft），后两个仓库的 sysroot 为其固定的 1.99.0。时间为单次观测，含本机缓存状态，未做重复与置信区间。增量刷新指编辑后到语义视图发布，不含冷启动对照。

|仓库|规模|步骤|一致性|
|---|---|---|---|
|fixtures/rust-refresh|4–5 个文件，4–8 个调用点|10 步：函数体、签名、trait 实现增删、打断、崩溃、cfg 模块、Cargo feature、纯行移|发布的 9 步全部一致|
|semweft（3 个 crate）|约 1,450 个调用点，约 240 条工作区内调用关系|8 步|发布的 7 步全部一致|
|velin（15 个 crate，约 4.5 万行）|15,552 个调用点，约 5,190 条关系|9 步：函数体 ×3（含跨 crate 新调用、纯行移）、打断、新增导出方法、崩溃中增加 trait 实现、Cargo feature、删除 trait 实现|发布的 8 步全部一致|
|app-struct（10 个 crate，约 6.9 万行）|23,871 个调用点，约 4,550 条关系|9 步：函数体 ×4（含跨 crate 新调用、崩溃中编辑、纯行移）、打断、新增 trait 实现与动态调用方、Cargo feature、新增导出函数|发布的 8 步全部一致|

三个仓库均未改动原目录（`source_inputs_unchanged`）；被打断的刷新均以 `superseded` 拒绝发布，下一步规划覆盖其编辑；崩溃后各重启一次。

**增量刷新耗时**（ms；括号内为查询/复用调用点）：

|改动类型|semweft|velin|app-struct|
|---|---|---|---|
|初始（含首次构建脚本与过程宏）|15,487|16,077|44,567|
|函数体编辑（body_local）|94–175（98–255 / 1,189–1,352）|415–495（8–217 / 15,341–15,544）|507–596（36–62 / 23,811–23,839）|
|结构变化（full）|565|2,289–3,420|7,527–12,932|
|Cargo feature（full，含工作区重载）|2,053|3,503|19,181|
|崩溃后重启并刷新|4,905|9,169|10,501|
|冷启动对照（每步）|约 5,900|约 9,500–10,100|约 25,800–28,200|

velin 进程峰值 RSS 约 1.3 GB，app-struct 约 3.4 GB（`time -l` 观测，未拆分 Semweft 与 rust-analyzer）。

**body_local 耗时拆分**（velin，复测）：后端同步与查询 41–46 ms，发布 30–44 ms，发布前全量重扫输入以确认后端未改写 Cargo.lock 等 228–415 ms；语法侧全量重扫与抽取另需 430–606 ms。主要成本来自原型每步重读整个工作区，而不是语义刷新；常驻 daemon 应改为文件事件驱动与按文件缓存，并只核验后端可能写入的构建输入，语法延迟按[质量目标](quality.md)第 1 节单独验收。

**初步结论**：

- 函数体编辑占 Agent 改动的多数；`body_local` 只重查变化文件内的调用点，后端开销在百毫秒以内，与冷启动相比有数量级差距

- 结构与 Cargo 变化在 7 万行规模的全量重查达到 8–19 秒，超出“一次编辑-编译循环内回到 current”的期望；需要按导出指纹与反向依赖缩小 full 范围，或在 full 期间继续对未失效单元提供 current 证据

- 冷启动对照与增量结果在 4 个工作区、共 32 个发布步骤中逐关系一致，未发现需要解释的差异

**新发现的证据问题**：

- **cfg 关闭代码**：rust-analyzer 会解析 cfg 关闭代码中的路径，已用 `inactive_code` 诊断降级（见第 2 节）

- **测试专用实现**：`#[cfg(test)]` 模块与 tests/ 中的 trait 实现会进入分派候选（rust-analyzer 默认启用 test cfg），例如 app-struct 的 `observe_all` 列出 2 个测试观察者。候选不错误但会扩大阅读成本，接口需标注测试作用域或允许按作用域过滤

- **工作区外目标**：约 60–70% 的调用点目标在标准库或依赖中，记为 `definition_mapping_gap` / `unresolved_call`；能力清单需将其披露为范围外而非解析失败

## 4. 未完成项

- **内嵌 ra_ap_* 对比**：依赖体量大、版本须与工具链锁定，需单独评估构建时间、内存与故障隔离后再接入；路线图第 4 节“接入方式与进程边界”仍待决策

- **正式门槛**：真实仓库的一致性已覆盖 3 个仓库，但编辑序列各 8 步、单次运行；5.2 节要求的独立标注准确率/召回率尚未开始

- **references 刷新**：导出变化后重查引用尚未实现；当前只有调用关系

- **更细的失效**：结构变化一律全量重查；按导出指纹与反向依赖缩小范围待大仓库测量后决定

- **飞行中取消**：故障在查询之间注入，尚未验证对进行中请求发送 `$/cancelRequest`

- **重复测量**：P50/P95/P99 需要固定机器、重复运行与更多编辑样本

- **测试作用域**：测试专用 trait 实现尚未与生产实现区分
