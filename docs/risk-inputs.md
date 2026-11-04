# 风险评分输入与六维模型

风险是审查优先级启发式，不是漏洞概率、兼容性结论或安全证明。`heuristic_v2` 在已有范围、公开声明、调用影响、证据缺口之外，加入可选的源码绑定 LCOV 行观测和冻结 Git 历史窗口。所有查询仍然只读，不执行测试、包脚本或构建。

## 生成与使用

在工作区外保存输入和报告，避免它们被保护规则作为源码反复捕获：

```sh
semweft index --syntax-only --export-manifest /tmp/head-sources.json
# 使用自己的测试/覆盖率工具，在相同输入上生成 /tmp/head-coverage.info
semweft risk-inputs --coverage /tmp/head-coverage.info \
  --source-manifest /tmp/head-sources.json --history-commits 100 \
  --format json > /tmp/head-risk.json
semweft impact --diff WORKTREE --risk-inputs /tmp/head-risk.json --all-pages
semweft verify --diff WORKTREE --risk-inputs /tmp/head-risk.json --format sarif
```

仅需热点时可省略 coverage/source-manifest，只提供 `--history-commits 1..1000`。没有 Git 历史的目录可以只导入 LCOV。工具不下载依赖、不启动测试，也不因为读取 Git 历史获得构建授权。

修改前可运行 `semweft guard before --file src/lib.rs --risk-inputs /tmp/head-risk.json --all-pages`。MCP agent_preflight 只接受 risk_inputs.head。复用同一哈希/revision 校验，观测范围改为所选现有声明/捕获文件，结果在 guard.risk_observations；不把它称为未来修改的覆盖率或测试执行证明。

可在修改前以相同流程保存 `/tmp/base-risk.json`，之后同时传 `--base-risk-inputs /tmp/base-risk.json`。base/head 源码不同就必须分别采集；不能把 head 覆盖率套到删除符号或历史版本上。base artifact 应与实际 diff base 对应，而非“开始编辑时”的任意其他工作区状态。Git revision 及所有语言的新工作区快照在 Git 上下文可用时记录 revision，历史窗口锚点须匹配请求上下文。相同源码的 L3 重用保留原分析视图，不把原视图提交冒充请求提交；固定重查通过经过文件内容验证的 commit_context 保留两者，见接口规格 1.4。

MCP 的 diff 查询 `impact_of`、`verify_change`、`suggest_tests`、`agent_verify`、`arch_check`、`contract_impact` 接受同一结构：

```json
{
  "diff": {"base": {"kind": "revision", "revision": "HEAD"}, "head": {"kind": "workspace"}},
  "risk_inputs": {"head": {"schema_version": "0.1", "workspace_id": "WORKSPACE_ID", "files": []}}
}
```

示意中的空 files 不产生覆盖或热点观测。实际使用 `risk-inputs` 的完整输出，不填写推测数值。`risk_inputs` 必须和 diff 一起使用；分页游标继续使用原请求输入，不重新加载磁盘文件或 Git。

## Artifact 与绑定

- 顶层为 schema_version、workspace_id、files[]，可选 coverage 和 history。每个文件为 path、BLAKE3 file_hash，可选 lines[]（line、hits）及 history_changes；缺失字段不能使用 null 代替。
- coverage 为 format=lcov、report_digest、source_manifest_digest、omitted_records。LCOV `DA` 计数必须非负、行号从 1 开始且在所绑定源码内；重复测试记录的同一行合并计数，未结束、非法数值和溢出拒绝。提供行 checksum 时核对 LCOV 的 MD5/Base64 校验（仅用于输入完整性，不作密码学身份）；可选 statement end 核对范围，计分只计 DA 观测锚点，不把 statement span 的每行伪装成独立观测。函数/分支记录不冒充行观测。
- coverage 的 source manifest 必须来自覆盖率运行前的显式导出，导入时要求全部文件哈希、分析配置、工作区身份一致。这能检查内容匹配，但不能独立证明外部覆盖率工具确实执行过测试；输入仍是调用方提供的证据。
- SF 只精确映射当前工作区已捕获的路径，不按 basename 猜测；未捕获或工作区外记录计入 omitted_records，不读取对应外部源码。绝对 SF 须使用工作区规范路径；不能映射的记录不作为覆盖率。
- history 包括 anchor_commit、commits_observed、limit、window_limited、shallow。读取锚点之前、限定工作区范围的非 merge 提交，每个文件按触及提交数计数；不跟随重命名，不以未观察的过去填零。浅克隆返回 partial。窗口达到显式 limit 是范围约定，window_limited 仍披露还有更早历史。
- 生成前后重新捕获输入并检查 HEAD，途中源码或 HEAD 改变则拒绝。查询再次逐文件验证哈希、范围、工作区和可用 revision；过期输入返回 content_mismatch，不静默降级为“低风险”。符号关系仍绑定各自不可变视图。
- 单个 artifact 至多 10,000 文件、100,000 行观测，输入 JSON 至多 16 MiB；原始 LCOV 至多 32 MiB、源清单至多 8 MiB。超限明确拒绝，Git 历史采集可取消并受超时约束。

报告 sources 保存 artifact 内容摘要与覆盖/历史来源。需要长期复核时，除了审查记录也应保留原始 artifact、覆盖报告及源清单；报告摘要不等于原始外部文件的自动归档或执行证明。

## 计分与不确定性

|维度|最高分|公式与范围|
|---|---|---|
|影响范围|20|min(变更符号数×3 + 受影响源文件数×2, 20)|
|公开声明|20|min(语法公开变更数×10, 20)，不代表外部可达 API|
|调用影响|15|min(调用者数×2, 9) + min(完整全调用 witness 深度×2, 6)|
|行覆盖观测|15|ceil(未命中行数 / 已报告行数 ×15)，仅统计变更声明 span 内、两侧分别去重的已报告可执行行|
|历史热点|15|ceil(变更文件在各自显式窗口中的最大触及提交频率×15)，包含仅配置/IDL 内容变化，不限于声明变更|
|证据缺口|15|min(known_gaps×2,6) + min(pending,3) + 截断×5 + min(配置变更,3) + min(残留引用,3) + 覆盖缺失或部分×3 + 历史缺失或部分×3，再封顶15|

没有覆盖观测或历史窗口时对应观测分为 0，但状态为 unavailable，比例为 null，并增加缺口分；不是把它解释为已证实零风险。仅部分声明有行观测、仅一侧提供输入或浅历史时保留 partial；分母只包含已报告行，不能用缺失声明抬高覆盖率。没有任何真实变更时总 score 为 null。

等级为 low 0–19、moderate 20–44、high 45–69、critical 70–100。存在静态缺口、pending、截断或不完整覆盖/历史时 confidence=low，否则最多 medium；模型尚未做独立真实任务校准。旧 `heuristic_v1` 的人工批准不自动适用于新模型。

格式核对（2026-11-03）：LCOV 上游 [tracefile 说明](https://github.com/linux-test-project/lcov/blob/master/docs/man/geninfo.rst)及 [checksum 实现](https://github.com/linux-test-project/lcov/blob/master/lib/lcovutil.pm)。这里声明的支持范围仅为源码记录、DA 行锚点及上述可选字段，不宣称执行覆盖率生产或分支/函数覆盖率分析。

2026-11-03 本机使用真实 Node 24.5.0 内置 test coverage/LCOV reporter，在临时 JS 测试项目验证报告生成、源清单绑定、覆盖/未覆盖行导入及 diff 评分；该 ignored 集成用例已显式通过，并纳入固定生产器 CI 作业。产品查询不执行这一测试命令，临时集成验证也不等于用户仓库已执行测试。
