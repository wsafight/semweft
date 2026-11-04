# 真实仓库源码样本评测

该工具用于检查语言适配器，并保存可复查的测量。它不自动把语言改为正式支持，也不将 SCIP 输出当作真值。源码标注与结果边界见 [语言验收状态](../../docs/language-acceptance.md)，最近一次本机 Release 结果见 [JSON 摘要](../../docs/language-acceptance-results.json)。

`corpus.json` 固定九个公共仓库的提交、生产源码投影、Git blob 身份、调用/声明文本锚点和编辑，Go、TypeScript、JavaScript 各三个。Go 选择包根目录的生产文件及构建输入，TS/JS 选择 src 生产源码；测试、示例、依赖和生成构建产物不纳入投影。目标是源码声明；虚方法声明绑定与可确定执行目标分开评测，参数回调/动态构造没有伪造的唯一目标。当前标注由 Codex 根据源码编写，JS 标注先于这些仓库的首次索引，仍未做独立人工复核和随机抽样。

先安装仓库 CI 固定的 scip-go 0.2.9、scip-typescript 0.6.0、TypeScript 5.9.3、Go 1.27.2、Node 24.5.0。工具须位于 PATH，或显式设置 `SEMWEFT_SCIP_GO` / `SEMWEFT_SCIP_TYPESCRIPT`。评测不会安装被测项目依赖、运行其 npm scripts 或修改原始下载目录。

在仓库根目录执行（macOS/Linux，Python 3.9+ 和 Git）：

```sh
python3 examples/language-acceptance/fetch.py --destination /tmp/semweft-language-corpus
SEMWEFT_ACCEPTANCE_ROOT=/tmp/semweft-language-corpus \
SEMWEFT_ACCEPTANCE_REPORT=/tmp/semweft-language-acceptance.json \
cargo test --release --locked -p semweft-cli --all-features --test mvp \
  acceptance::source_annotated_go_and_typescript_repositories -- --ignored --nocapture
```

下载器只获取固定提交并验证文件身份，禁用仓库 Git hooks；已有目录只有 revision 与文件身份均匹配时才复用，不覆盖不匹配目录。评测先逐文件验证 blob，再在临时副本中运行索引；独立子进程通过环境变量隔离配置、trust 和缓存，不写用户全局设置。TS/JS 使用报告中记录的 ES2018 配置，只包含 src，JS 开启 allowJs/checkJs，不声称原始仓库完整构建通过。仓库外导入仍保留为缺口。

每个文件使用新解析器执行十次全文件分析，统计包含 tree-sitter 与事实提取的 P50/P95/P99；不把缓存命中或单个 tree-sitter 调用耗时当作该指标。索引后依次应用固定函数体编辑、构建输入编辑，并与同一路径、同一源码、全新索引目录的冷启动结果比较 relation_id、确定性及证据来源/确定性。生产器原始输出哈希可以因构建元数据变化而不同，仍保留在来源中，不把它误判为调用图变化。

`semantic_current_ms` 从显式扫描开始，包含语法发布和项目级语义重建，未经过文件监听器与 debounce；`syntax_publish_ms` 是同一路径的语法可查时间。每种语言只有六个显式编辑样本，P95/P99 等于最大值，不能外推生产系统尾延迟。Go 修改 go.mod 的语言版本为 1.22；TS/JS 修改 strict 配置。uuid 的函数体编辑会替换跨文件调用目标，其余为固定表达式/字面量编辑。

另一个 ignored 测试启动真实 `index --watch` 客户端，每仓库连续进行函数体、跨文件辅助函数重命名、构建配置三步编辑。Go 配置切换到 purego 标签，TS/JS 的 target 切换到 ES2020。计时期间只轮询 status，要求预期文件哈希、新快照、存活 watcher、有效 lease 和语义 epoch 追上语法 epoch；不通过查询的隐式扫描推动刷新。观测到 current 后，固定快照查询所选根的三级调用关系，再与另一临时路径全新索引比较。它只验证所选子图，不宣称全图相等。Go 投影最终另外执行离线 `go test ./...`；没有把缺少原依赖的 TS/JS 投影称为完整项目构建通过。

```sh
SEMWEFT_ACCEPTANCE_ROOT=/tmp/semweft-language-corpus \
SEMWEFT_WATCH_ACCEPTANCE_REPORT=/tmp/semweft-language-watch-acceptance.json \
cargo test --release --locked -p semweft-cli --all-features --test mvp \
  acceptance::repository_watchers_match_cold_after_body_rename_and_build_changes \
  -- --ignored --nocapture
```

watcher 耗时从写文件开始，包含 50ms 轮询间隔、CLI/IPC 与调度开销；语法和 current 可在同一次轮询中观测到，数值是观测上界。另有 `producers::real_go_build_tags_change_bindings_and_preserve_pinned_views` 使用互斥标签的双定义夹具，验证真实绑定切换、旧快照保留与冷启动一致性。

评测输出每个标注的实际目标、关系/证据身份、快照、版本、缺口、各来源准确率/召回、候选错误率、未解析率和 Wilson 95% 区间。零预测或零候选时相应比率为 null。候选错误率只评估有确定源码目标的 direct 标注；虚方法另保留声明绑定观测，参数回调的候选集合未判定真值。缺失调用点仍计入召回分母，不能通过少返回结果提高召回。

ignored 测试会对当前样本的 L3 direct 准确率 ≥99%、召回 ≥95%、不安全 definite 为零，以及编辑后冷/热一致性进行断言。它没有把这些断言当成总体质量证明；完整报告即使质量断言失败也会写入指定文件。常规测试覆盖统计口径、源码坐标匹配、按证据来源重算确定性和过期证据排除。
