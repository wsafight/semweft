# Semweft

面向 Coding Agent 的本地代码事实与变更影响引擎：通过 MCP 和 CLI 提供带证据来源、快照和分析边界的符号关系、候选影响范围与预算上下文。

不接入 Agent 时，开发者也能直接用 CLI 的文本、Markdown 和依赖图输出看懂项目结构、复核改动；Semweft 自身不调用模型，不产生 Token 费用。

> **项目状态：P0 功能已实现，语言能力仍为实验性。** 已实现共享 daemon、持久化快照、CLI、MCP stdio、Rust 常驻语义与 SCIP 导入、导航、引用、调用图、diff 影响、结构地图和精确 Token 预算上下文；Go 通过可选 feature 启用。实现与回归对照见 [P0 清单](docs/p0.md)。文档中的正式语言支持、真实任务效果和性能门槛尚待独立评测，不等同于 MVP 功能验证通过。编辑期一致性原型仍可用，见[刷新原型进展](docs/prototype.md)。

## 安装与使用

```bash
cargo install --locked --path crates/semweft-cli

# 无需启动外部语义工具即可使用语法候选查询
semweft init --workspace /path/to/cargo-workspace
semweft index --syntax-only --workspace /path/to/cargo-workspace
semweft search Stock --workspace /path/to/cargo-workspace

# 语义索引需要 rust-analyzer、rust-src 和显式仓库授权
rustup component add rust-analyzer rust-src
semweft trust --accept-build-execution --workspace /path/to/cargo-workspace
semweft index --workspace /path/to/cargo-workspace --timeout-ms 60000
cd /path/to/cargo-workspace

semweft symbol stock --include-body
semweft symbol-at src/order.rs:12:8
semweft refs stock --source L3
semweft impls inventory::Deduct --path crates/inventory
semweft impls inventory::Deduct::deduct --source L3 --format json
semweft calls stock --direction callers --depth 3
semweft path entry stock --max-depth 5 --max-paths 100 --format markdown
semweft impact --diff WORKTREE --format markdown
semweft impact --diff origin/main...HEAD --format json
semweft verify --diff WORKTREE --format markdown
semweft map --dependencies crate --format mermaid
semweft context stock --budget 20k
semweft doctor
semweft status
semweft index --watch
semweft trust --revoke
```

命令默认选择当前目录所属的 Cargo workspace；`--workspace`、`--config` 可显式选择。`init` 生成 `semweft.toml`，已有文件保持不覆盖。未授权时可查询 L1 语法候选及已验证导入的 SCIP 证据；`trust` 授权可能执行 Cargo 构建脚本与过程宏的 L3 路径。自动连接或拉起 daemon 不申请授权。

CLI 与 MCP 共用每个工作区的 daemon 和索引。默认使用平台用户缓存目录；`SEMWEFT_CACHE_DIR`、`SEMWEFT_CONFIG_DIR` 可用于隔离运行环境，`SEMWEFT_RUST_ANALYZER` 可指定语义后端。Unix 存储目录须为当前用户的专用私有目录。daemon 空闲 60 秒退出，MCP 会话或 `index --watch` 保持监听；原生文件事件不可用时采用 500ms 元数据轮询，并在变化后重读源码；监听会话还会核对文件元数据以恢复丢失的通知。

查询可用 `--snapshot-id` 和 `--semantic-epoch` 固定分析视图，用 `--cursor` 接续分页；旧视图不会被后续编辑覆盖。历史引用用 `refs --include-stale --stale-from <snapshot_id> --stale-semantic-epoch <epoch>` 显式读取，与当前引用分开。预算不足时上下文逐项降为签名、大纲或位置，并提供省略清单游标。上下文 JSON 使用紧凑序列化，`estimated` 按完整响应的 UTF-8 字节数估算；支持离线精确 tokenizer `cl100k_base@tiktoken-rs-0.12.1`、`o200k_base@tiktoken-rs-0.12.1`，按最终完整 JSON（包括元数据和省略页）计数。当前最小预算为 4096，未知 tokenizer 回退到估算并说明原因。

stdout 是终端时默认 text，否则默认 JSON；还支持 Markdown，以及 `map` 的 Mermaid/DOT。JSON 字段中的缺口、pending 和截断均需保留在调用方的判断中。普通查询退出码 0 表示取得结果，partial 也可成功；`verify` 按核验状态返回 0（声明规则内未发现违规）、1（发现确定违规）或 3（证据不足）。2 表示参数或目标问题，4 表示需要仓库授权，5 表示索引、工具或服务失败，130 表示取消。

`verify` 复用 diff 两侧的候选影响，运行四条有版本的内置审查规则：配置变化、分析缺口、残留名称引用和候选影响关系。响应包含 `verification` 全量结论及逐规则状态，以及可分页的 `findings`；证据保留快照、来源、确定性与源码位置。当前规则只产生 warning / `insufficient_evidence`，不判断 API 兼容性，也不执行编译或测试。当前索引始终披露 L1 名称绑定缺口，因此通常返回 3；分页或展示裁剪不会把结论改为通过。

基础查询与刷新服务已提取为独立 [semweft-query](crates/semweft-query/README.md)，可直接嵌入其他 Rust 程序。存储按需压缩，支持 `snapshots usage`、`snapshots gc --dry-run --max-bytes N` 和 `snapshots compact`；分页缓存同时限制游标数量和共享载荷字节。独立 `semweft-bench` 支持冻结输入、多轮新进程及同输入对照，见[第二轮基础层记录](docs/foundations.md#第二轮基础层开发记录)。

## 桌面工作台（macOS · GPUI + gpui-rsx）

```bash
python3 crates/semweft-desktop/scripts/preview.py
open "target/desktop-preview/Semweft Preview.app"
```

使用 gpui-rsx 声明的原生 GPUI 工作台支持完整源码与文件大纲、符号及全文搜索、调用与引用导航、模块依赖矩阵与候选循环、代码度量、Git 热点、书签和变更审查。默认打开隔离示例；`--workspace /path/to/project` 可分析自己的 Rust、Go、TS/JS 项目。当前以语法候选为基础，具体语言边界与运行方式见[桌面工作台说明](crates/semweft-desktop/README.md)。

## SCIP 导入与 Go 实验支持

```bash
# 先固定源文件哈希和分析配置，再在同一份输入上运行外部索引器
semweft index --syntax-only --export-manifest inputs.json
# 在此运行对应语言的 SCIP 生产工具，得到 index.scip
semweft index --scip index.scip --scip-manifest inputs.json
semweft refs stock --source L3
semweft context stock --budget 8k --tokenizer cl100k_base@tiktoken-rs-0.12.1

# Go 为可选编译功能，识别 go.mod / go.work
cargo install --locked --path crates/semweft-cli --features go-experimental
cd /path/to/go-project
semweft init
semweft index --syntax-only
# 安装外部 scip-go 后，授权并执行项目级语义重建
semweft trust --accept-build-execution
semweft index
```

SCIP 导入本身不运行生产工具，无需构建执行授权；生产者名称和版本必填，文件与配置哈希不一致时拒绝导入。UTF-8/16/32 坐标按文档声明映射，失败处保留诊断。工具参数和项目根元数据可从状态诊断复核；导入不重建生产者的条件编译配置。

Go 语法与结构查询不需要 Go 工具链；语义刷新运行外部 `scip-go --output <file>`，可用 `SEMWEFT_SCIP_GO` 指定可执行文件。项目重建使用离线依赖；源码编辑先发布语法视图，失败保留缺口与可重试状态，撤销信任会停止在途生产进程。Go 暂只支持依赖深度 0，build tags、生成代码与穷尽的 interface 分派不在实验模型内。

## MCP

在支持 stdio MCP 的客户端配置中加入：

```json
{
  "mcpServers": {
    "semweft": {
      "command": "semweft",
      "args": ["mcp", "--workspace", "/path/to/cargo-workspace"]
    }
  }
}
```

提供 19 个只读工具，入参 schema 随 `capabilities` 和 MCP tools/list 返回。已有导航、实现查询、diff 核验、调用路径和测试候选；新增 `arch_check`、`agent_preflight`、`agent_verify`、`get_review_result`、`contract_impact`。CLI/MCP 还提供 [Rust 库 API 候选](docs/public-api.md)、[显式服务依赖/路径/循环](docs/services.md)、架构基线、Hooks 适配、审查归档与自包含 HTML。使用与边界见 [护栏指南](docs/guardrails.md)、[Protobuf 分析](docs/contracts.md)及 [20 项清单](docs/product-next.md)；Go/TS 正式验收仍未完成。信任、重建和写入归档走 CLI 控制通道。MCP 使用 rmcp stdio，协议流不包含 daemon 日志。

`semweft tests <symbol>` 或 `semweft tests --suggested --diff WORKTREE` 根据反向调用/引用路径推荐显式 Rust 测试入口，支持 JSON/text/Markdown、过滤、预算与固定视图分页。报告区分当前、ignored 和仅旧版本的测试，保留每条证据路径；不会执行测试，也没有逐测试覆盖率。候选绑定确定性不表示运行覆盖，零候选不能证明无需测试。自定义/生成测试入口及其他语言入口尚未支持。

Diff 报告同时提供 `diff_summary`、`affected_modules`、`affected_services` 和 `risk_summary`：按两侧源文件及显式服务映射聚合变更、影响关系与残留引用，摘要保持全量计数，列表可分页。`heuristic_v2` 提供六维风险分数，可导入源码绑定的 LCOV 和冻结 Git 热点，详见[风险输入](docs/risk-inputs.md)。公开语法声明不等于外部可达 API；Rust 外部 API 候选另见[公开 API](docs/public-api.md)。内联模块尚未拆成独立依赖节点。

CI 可用 `semweft verify --diff <selector> --all-pages --format json` 固定并导出完整核验结果，再用 `semweft report --input verify.json --format markdown|sarif` 离线生成报告。SARIF 2.1.0 仅映射核验 findings；JSON、Markdown 和 SARIF 保留证据不足语义与核验退出码。GitHub Actions / GitLab 示例和权限边界见 [CI 报告说明](docs/ci.md)。

`path <from> <to>` 枚举已索引符号间的简单调用路径，保留逐边来源、确定性与源码位置，支持深度、路径数量、节点和时间预算。环中的同一符号不会重复，独立调用点形成不同路径；同一符号到自身返回一条零边路径。`--scope module|service` 查询源文件或显式服务的聚合依赖路径，每跳保留调用见证，相邻跳不保证来自同一函数。路径枚举完成只针对已索引图，完整性缺口仍保留，详见[服务映射](docs/services.md)。原始开发顺序与进度见 [20 项产品功能清单](docs/product-next.md)。

`impls` 接收 Rust trait 或 trait 方法，`--path` 过滤实现所在路径，并支持关系查询的来源、确定性、新鲜度与通用快照、分页参数。它返回已索引的显式实现，L1 名称候选与 L3 语义证据分开披露，测试作用域单独标注；默认方法继承单独计数。L3 可独立识别 use 别名，只有语义绑定而无语法名称匹配的别名实现不附 L1 name_match 证据，`--source L1` 不返回它。仅 `relations` 分页，过滤后的完整计数、`inherited_defaults` 和 `excluded_candidates` 随每页返回。宏与 derive 生成实现、blanket impl 覆盖类型枚举、未索引的依赖和下游实现仍有缺口；无语义证据时不解析导入和遮蔽，不能把同名候选当作已绑定实现。字段与缺口见[接口契约](docs/interfaces.md#find_implementationsrust-trait-实现查询v02-增量)。

## 验证与当前边界

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace
cargo test --locked --workspace --all-features
# 需要 rust-analyzer 与 rust-src；仅对临时生成的工作区授权
cargo test --locked -p semweft-cli --test mvp trusted_daemon_publishes_l3_and_refreshes_references -- --ignored
# 编辑、故障恢复与增量/冷启动一致性原型
cargo test --locked -p semweft-cli --test cli lsp_refresh_matches_cold_with_annotated_targets -- --ignored
```

当前 Rust L1 名称匹配是候选证据，未解析词法作用域或遮蔽；L3 提供工作区内定义、实现枚举及引用，方法分派候选不宣称穷尽。`index.dependency_depth=n` 可纳入离线 Cargo 已解析依赖图上至多 n 跳的源码，默认 0；正值需要仓库信任，解析失败会列出缺口。Git 提交侧优先复用完整输入与配置相同的已保留 L3 视图，否则使用 L1 并披露缺口。桌面端已有 GPUI 交互预览；规则核验、正式桌面审查闭环、IDL 和 HTTP MCP 按路线图后置。存储按内容哈希复用源码、单文件语法事实、快照和视图，以增量日志和检查点提交目录；历史按需加载，支持缓存双预算、持久化版本固定和安全回收，默认保留全部历史。分页游标在 daemon 重启后失效。独立 `semweft-bench` 提供真实输入测量，CLI 使用 `benchmark-storage --revisions N` 运行。命令、配置、格式兼容及测量边界见[底层基础实现](docs/foundations.md)。

Diff 分析先保留两侧唯一的精确匹配，再按双向唯一的语法指纹推断移动和重命名；歧义按删除、新增处理。删除、重命名和移动后的旧名称出现位置列入 `residual_references`，作为待复核的 L1 候选，不据此断言仍绑定旧符号或迁移失败。匹配规则与边界见[Diff 变更模型](docs/features.md#d-变更影响分析)。

## 文档

从[文档总览](semweft.md)开始，其中有完整的文档地图与阅读顺序。常用入口：

- [产品需求](docs/product.md)：问题、定位、竞品与差异化

- [核心概念与证据模型](docs/evidence-model.md)：证据维度、快照与编辑一致性

- [底层基础实现](docs/foundations.md)：共享哈希与坐标、受限图遍历、快照查询索引及测量边界

- [接口规格](docs/interfaces.md)：MCP 工具、响应格式与已接入的工具契约

- [路线图、风险与决策](docs/roadmap.md)：版本门槛与待决策事项

协议设计与前置原型门槛见[路线图](docs/roadmap.md)，决策状态统一维护于该文第 4 节。

## 风险输入

变更风险可选接入快照绑定的 LCOV 行观测与冻结 Git 历史：使用 `semweft risk-inputs` 生成 artifact，再传给 diff 命令的 `--risk-inputs` / `--base-risk-inputs`，或给 `guard before --risk-inputs` 提供修改前观测。输入缺失或过期不会伪装成已证实低风险，不自动执行测试。用法与 `heuristic_v2` 六维公式见 [风险输入指南](docs/risk-inputs.md)。

## 许可

引擎和 CLI 计划采用 Apache-2.0（暂定）。
