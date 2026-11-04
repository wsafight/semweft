# 架构规则、Agent 护栏与审查归档

本页记录产品清单 9–17 的当前实现。所有查询固定分析视图；候选依赖不能提升为确定违规，缺口不能由人工批准消除。

## 分层规则

在 `semweft.toml` 配置文件级分层，方向为依赖方 → 被依赖方。同层依赖允许，跨层依赖只允许出现在 `allow` 中的目标。路径是相对工作区的 glob，多个层匹配同一依赖文件时拒绝检查，避免依赖声明顺序决定结果。

```toml
[[architecture.layers]]
name = "api"
paths = ["src/api/**"]
allow = ["service"]

[[architecture.layers]]
name = "service"
paths = ["src/service/**"]
allow = ["domain", "infra"]

[[architecture.layers]]
name = "domain"
paths = ["src/domain/**"]

[[architecture.layers]]
name = "infra"
paths = ["src/infra/**"]
allow = ["domain"]
```

`architecture.deny_unassigned=true` 可要求依赖两端都属于配置层。默认未分层的文件不会产生违规，仍在 `unassigned_dependency_files` 中披露。规则针对已索引源文件依赖，不将 Rust `mod` 文件归属边当作业务依赖，不推断服务级或跨进程依赖。

```sh
semweft arch-check --all-pages
semweft arch-check --diff WORKTREE --format markdown
semweft arch-check --write-baseline .semweft/baseline.json
semweft arch-check --baseline .semweft/baseline.json --all-pages
```

`arch_check` MCP 接收可选 `diff`、`policy` 和 `baseline` 对象。默认规则来自选中 head 分析视图冻结的已解析配置（包括外置 --config），Git 历史视图回退到其捕获的 semweft.toml，而不是查询时的新磁盘内容；policy 可显式覆盖。保护规则和契约映射采用同样的冻结策略。基线只由显式 CLI 操作写入；MCP 查询不写文件。

每个 finding 保留禁止的层方向、文件级依赖路径、关系位置和逐条证据。当前 definite/current 的依赖可产生确定策略违规；名称候选依赖只产生 warning / insufficient_evidence。退出码 0/1/3 分别为声明范围内未发现新违规、确定新违规、证据不足。未配置策略不会解释为检查通过。

## 基线

基线记录规则版本摘要、生成时的 snapshot/semantic epoch 和全部 finding 指纹。指纹绑定源/目标文件和层，不依赖易移动的行号。历史 finding 标为 existing，新增标为 new，消失的列入 resolved_fingerprints。确定违规退出码只由新增确定违规触发；分析缺口和新增候选仍可返回 3。

规则摘要不匹配时拒绝静默复用。变更层或 allow 后必须显式重建基线。写基线前收齐分页；计算未完成时拒绝写入。指纹压缩了同一文件对的多个调用点，全部当前证据仍保留在 finding 中。

## 修改前与修改后

```sh
semweft guard before --file src/domain/stock.rs --all-pages
semweft guard before --symbol inventory::Stock::deduct --format markdown
semweft guard after --diff WORKTREE --baseline .semweft/baseline.json --all-pages
```

`agent_preflight` 接受 `files[]` / `symbols[]`、depth 与 max_nodes。返回公开声明观察量、Rust 模块/重导出 API 候选、受限影响关系、未确认风险和保护结果。可传 `risk_inputs.head`（CLI `--risk-inputs FILE`）复用[源码绑定 LCOV/历史](risk-inputs.md)，在 guard.risk_observations 返回所选现有声明的行观测和所选捕获文件的冻结提交频率。过期/错工作区/错 revision 拒绝，分页冻结结果；不接受 base artifact。不提供输入时保持 unavailable，部分输入保持 partial；不预测未来修改的覆盖率，不执行测试。新建/未捕获文件没有历史观察，不能解释为没有热点或无需测试。

`agent_verify` 必须接收双侧 `diff`，执行内置 diff 核验、已配置架构规则及保护规则，合并逐规则结论与全量计数；不执行编译、测试或任意自定义命令。guard.decision 为 allow/review/block；存在确定保护违规则 block，缺口或候选警告为 review。

## 保护区域

```toml
[[guard.protected]]
name = "protocol"
paths = ["proto/**"]
reason = "协议只允许人工修改"

[[guard.protected]]
name = "core_symbol"
symbols = ["*::Ledger::settle"]
reason = "核心结算需人工复核"
```

路径和符号模式分别匹配：命中任一即保护。文件级操作保守地保护含目标符号的整个文件，不声称能限制到精确编辑范围。修改后检查同时使用 base/head 捕获策略，不能通过同一修改删除保护配置来绕过旧策略。匹配保护路径的 UTF-8 文本纳入捕获；显式排除、gitignore、符号链接与缓存边界仍有效，不是文件系统沙箱。非 UTF-8 保护输入报错而不静默漏过。

## Claude Code / Codex Hooks

示例分别在 `examples/hooks/claude.settings.json`、`examples/hooks/codex.hooks.json`。合并到宿主受支持的配置中，先确认安装版本确实支持对应事件；不会自动修改用户全局设置。2026-11-03 官方 Codex 文档说明支持该能力的当前版本默认启用 hooks，可用 features.hooks=false 关闭；旧版本的开关和支持范围仍按所安装版本核对。

两者通过 `semweft hook --host claude|codex` 从 stdin 接收事件 JSON。适配 PreToolUse/PostToolUse：前者输出 hookSpecificOutput 的 deny 或审查上下文，后者输出上下文，存在阻断结果时附 block。静态示例仅匹配 Claude 的 Edit/Write 和 Codex 的 apply_patch；宿主工具别名或封装改变时须调整 matcher 并重验。

前置事件保存 session_id/tool_use_id 对应的固定快照回执，后置事件使用该快照到当前捕获输入的 diff。回执在本地私有索引目录；缺失回执、快照回收、查询错误或路径越界都明确阻断而不是声称核验通过。后置阻断不能撤销已执行的修改。上下文说明没有执行编译或测试。

只处理显式文件路径或 apply_patch 文件头（包括移动目标），支持 Codex 官方 tool_input.command 的完整补丁载荷及既有 patch/input 字段；command 必须以补丁开始标志开头，普通 shell 文本不当成补丁。不解析、执行或猜测 shell 命令的写入效果。拒绝符号链接目标和工作区外路径。Bash、exec_command、网络写入和间接写入不在这组 matcher 的保护范围，应显式调用 guard；Hooks 不能替代权限隔离。保存快照只覆盖已捕获输入，也可能包括工具运行期间的其他编辑。

协议核对日期：2026-11-03。依据官方文档：

- Claude Code Hooks：<https://code.claude.com/docs/en/hooks>
- Codex Hooks：<https://developers.openai.com/codex/hooks>

2026-11-03 使用 Claude Code 2.1.108 与临时安装的 Codex CLI 0.166.0，通过两个真实宿主闭环：Claude Read/Edit、Codex apply_patch 实际调用，PreToolUse/PostToolUse 相同 session/tool ID、快照回执核验，以及保护规则拒绝后文件保持原状。测试连接回环地址上的脚本模型响应，**不是手工注入事件 JSON，也不是线上模型能力验收**。Claude 使用会话级 settings；Codex 使用本次调用的 TOML Hooks 参数、忽略用户配置和持久会话，仅对测试自己构造的命令使用 invocation 级 hook-trust bypass，不修改用户信任或全局 Hooks。Codex 的模型名称从已安装二进制的 bundled catalog 选取以启用真实 apply_patch 工具，响应仍完全由本地脚本生成。

可显式执行 `cargo test --locked -p semweft-cli --all-features --test mvp hosts:: -- --ignored --nocapture`，需 Claude Code、Codex 和 Node，允许 SEMWEFT_CLAUDE/SEMWEFT_CODEX 指定路径，不需线上模型请求。一次线上 Claude 模型尝试未在 180 秒内完成，不记作通过。`.github/workflows/ci.yml` 的 hook-hosts 作业固定上述版本并执行同组用例；当前仅有本机实跑结果，远程 CI 尚未运行。项目配置的发现/信任方式仍需按实际宿主设置，不能把 invocation 级测试解释为自动启用所有目录的 Hooks。

## 审查记录

```sh
semweft guard after --diff WORKTREE --all-pages > review.json
semweft review save --input review.json --conclusion approve --note '人工核查完成'
semweft review get REVIEW_ID
```

可保存 approve/reject/comment。记录包括固定报告、commit（来源可用时）、双侧快照/epochs、捕获文件内容差异摘要、规则版本摘要、报告摘要、时间和人工结论。没有 commit 来源时不编造提交身份；工作区内容由 snapshot 和文件 hash 绑定。输入必须来自本工作区、包含全部 findings，不能对缺页报告归档为完整审查。

`get_review_result` 只读查询返回记录及针对选中分析视图的 validity。输入、semantic epoch 或规则版本变更使批准失效；固定旧快照重查仍可复核旧结论。记录以内容身份校验篡改，但只是本地人工记录，不是签名、远程认证或行为安全证明；不会提升关系 certainty。

## 静态 HTML

```sh
semweft guard after --diff WORKTREE --all-pages --format html > review.html
semweft report --input review.json --format html > review.html
```

单文件自包含，不加载脚本、字体或外部资源。展示摘要、关系/路径、变更、风险依据、缺口、规则和原始固定 JSON。所有外来文本转义，源码链接只允许工作区相对路径；历史 base 坐标不错误链接到 head。将报告放在工作区根时，相对源码链接指向当前文件，不声称提供归档源码。缺页/计算截断仍显式披露，不通过导出消除。HTML 不支持 max_chars 裁剪。
