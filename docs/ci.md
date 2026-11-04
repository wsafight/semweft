# CI 证据报告

`semweft verify --diff <selector> --all-pages --format json` 导出分页前判定对应的全部 findings、关系及模块。`--all-pages` 也适用于 tests 和 impact；它只读取固定视图的后续页，不扩大分析预算。深度、节点或时间截断仍保留，不能通过导出全部分页消除缺口。`--cursor` 与 `--all-pages` 不能同时传入。

先保存 JSON，再离线渲染可保证不同格式来自同一分析结果。`report --format sarif` 默认把当前目录作为源码根；从其他目录渲染时传 `--workspace <repo>`：

```sh
semweft verify --diff HEAD~1..HEAD --all-pages --format json > verify.json
semweft report --input verify.json --format markdown > verify.md
semweft report --input verify.json --format sarif > verify.sarif
semweft --workspace /path/to/repo report --input verify.json --format sarif > verify.sarif
```

verify 和 report 均保留 0/1/3 判定退出码，错误仍用通用退出码。证据不足返回 3 并不表示报告生成失败；脚本应保存退出码后再渲染。`examples/ci/semweft-ci.sh` 已处理这些状态，保存 JSON、Markdown、SARIF 和原始退出码。默认使证据不足的 CI 作业失败；显式设置 `SEMWEFT_ALLOW_INSUFFICIENT=1` 可以接受该状态，但报告本身仍保留证据不足。确定违规和执行错误不能被此选项掩盖。

`semweft verify --format sarif` 自动收集全部页。SARIF 只支持核验 findings，离线输入若缺页或 finding_count 不一致会拒绝导出；不允许 max_chars 裁剪。SARIF 2.1.0 的 rules/results 保留规则版本、证据、双侧快照、缺口、状态和截断信息。候选与证据不足以 warning 表示，只有 violations_found 对应 error；缺口另有 invocation notification。执行错误生成 executionSuccessful=false 的 SARIF，退出码保持原错误。

head 位置使用相对 URI 和行号；原始列坐标保留在 evidence，避免把不同来源的字节/编码列误映射成 SARIF 列。base 位置放入 historicalLocation 属性并在消息中注明，不把历史行号错误锚定到当前源码。无当前位置的 findings 仍保存在文件中，扫描平台可能不展示这些结果。该格式不宣称运行覆盖、测试执行、漏洞识别或行为安全。

GitHub Actions 和 GitLab 示例位于 [examples/ci](../examples/ci)。示例以本仓库构建为前提，未自动启用；其他仓库需预装固定版本 Semweft，并调整构建步骤、脚本路径和对比范围。示例不启用 trust，不运行被分析项目的 Cargo 构建脚本；构建分析器本身是 CI 中显式的步骤。GitHub 示例先上传完整工件，再尝试上传 SARIF；fork PR 保留工件并跳过需要写权限的扫描上传。仓库仍需具备目标扫描服务的访问权限。

GitLab 示例将 SARIF 作为普通工件保存，未将其声明为 Code Quality 或 SAST 报告，因为这些入口有不同的格式与接入要求；不要将 SARIF 文件直接改名后传入其他格式的 reports 字段。

本仓库自身的 `semantic-producers` 作业还固定 Go/Node 与两种 SCIP 工具，执行真实生产器/LCOV、标签切换与 JS 默认导出边界、九仓库源码样本的 Release 质量与冷/热一致性、真实 CLI 监听三步编辑测试，保存 JSON 和日志。`hook-hosts` 固定 Claude Code 2.1.108 / Codex CLI 0.166.0，通过回环脚本模型验证真实宿主前后回执和保护拒绝；不依赖线上模型或修改用户全局信任配置。这些作业的本机等价测试已通过，本次更改尚无远程 CI 运行记录。语言质量样本边界见[语言验收](language-acceptance.md)，宿主配置边界见[护栏指南](guardrails.md)。

`stability` 作业在 Linux/macOS 上重复 10 轮并发启动、重启恢复、文件监听、Go 撤权、私有目录竞争和进程取消测试，保存 `stability-<os>` 日志工件；任一轮失败会使作业失败。测试使用临时工作区和假生产器，保持单轮内部并行，不需要预装真实语义工具。常规三平台 `checks` 作业也会运行新增的 26 项测试；本机结果与尚未完成的远程验收见[稳定性回归](quality.md#偶发失败回归2026-11-04)。

格式与 CI 字段参考：

- SARIF 2.1.0 schema：`https://docs.oasis-open.org/sarif/sarif/v2.1.0/os/schemas/sarif-schema-2.1.0.json`
- GitHub SARIF 支持：`https://docs.github.com/en/code-security/code-scanning/integrating-with-code-scanning/sarif-support-for-code-scanning`
- GitLab job artifacts：`https://docs.gitlab.com/ci/jobs/job_artifacts/`
