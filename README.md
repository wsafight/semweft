# Semweft

面向 Coding Agent 的本地代码事实与变更影响引擎。通过 MCP 与 CLI 提供带证据来源、快照和分析边界的符号关系、候选影响范围与预算上下文；引擎自身不调用模型，也不产生 Token 费用。不接入 Agent 时，也可以直接用 CLI 的文本、Markdown 和依赖图输出复核改动。

> **状态：P0 功能已实现，语言能力仍为实验性。** daemon、持久化快照、CLI、MCP stdio、Rust 常驻语义与 SCIP 导入、导航/引用/调用图、diff 影响、结构地图和 Token 预算上下文均可用；Go 需可选 feature。正式语言支持、真实任务效果与性能门槛尚待独立评测。见 [P0 清单](docs/p0.md)。

## 安装与快速开始

```bash
cargo install --locked --path crates/semweft-cli

# 语法候选，无需外部语义工具
semweft init --workspace /path/to/cargo-workspace
semweft index --syntax-only --workspace /path/to/cargo-workspace
semweft search Stock --workspace /path/to/cargo-workspace

# 语义索引需要 rust-analyzer、rust-src 和仓库授权
rustup component add rust-analyzer rust-src
semweft trust --accept-build-execution --workspace /path/to/cargo-workspace
semweft index --workspace /path/to/cargo-workspace --timeout-ms 60000
```

命令默认选择当前目录所属的 Cargo workspace；`--workspace`、`--config` 可显式选择。未授权时可查询 L1 语法候选及已验证导入的 SCIP 证据；`trust` 授权的 L3 路径可能执行构建脚本与过程宏。自动连接或拉起 daemon 不申请授权。

## 常用命令

```bash
semweft symbol stock --include-body
semweft symbol-at src/order.rs:12:8
semweft refs stock --source L3
semweft impls inventory::Deduct --path crates/inventory
semweft calls stock --direction callers --depth 3
semweft path entry stock --max-depth 5 --max-paths 100 --format markdown
semweft impact --diff WORKTREE --format markdown
semweft verify --diff WORKTREE --format markdown
semweft tests --suggested --diff WORKTREE
semweft map --dependencies crate --format mermaid
semweft context stock --budget 20k
semweft status
semweft doctor
```

- `verify` 复用 diff 两侧候选影响并运行四条有版本的内置规则，返回 findings 与逐规则状态；当前只产生 warning / `insufficient_evidence`，不判断 API 兼容性，也不执行编译或测试。
- `context` 在预算不足时逐项降为签名、大纲或位置，并提供省略清单游标；支持离线精确 tokenizer（`cl100k_base`、`o200k_base`）。
- 查询可用 `--snapshot-id`、`--semantic-epoch` 固定分析视图，用 `--cursor` 接续分页；历史引用需显式指定旧快照。
- stdout 是终端时默认 text，否则默认 JSON，也支持 Markdown 与 Mermaid/DOT。

退出码：0 成功（partial 也算成功）；`verify` 为 0 通过 / 1 违规 / 3 证据不足；2 参数或目标问题；4 需要仓库授权；5 索引、工具或服务失败；130 取消。

daemon、索引与 MCP 共用每个工作区。默认使用平台用户缓存目录，`SEMWEFT_CACHE_DIR`、`SEMWEFT_CONFIG_DIR`、`SEMWEFT_RUST_ANALYZER` 可覆盖。daemon 空闲 60 秒退出，MCP 会话或 `index --watch` 保持监听；监听会话会核对文件元数据以恢复丢失的通知。

## 桌面工作台（macOS · GPUI + gpui-rsx）

```bash
python3 crates/semweft-desktop/scripts/preview.py
open "target/desktop-preview/Semweft Preview.app"
```

支持源码与大纲、符号/全文搜索、调用与引用导航、关系与实现分析、diff 与图视图、依赖矩阵、度量、Git 热点、书签和审查流程。`semweft review create --open` 可创建审查请求。默认打开隔离示例；`--workspace /path/to/project` 可分析自己的项目。运行方式与语言边界见[桌面工作台说明](crates/semweft-desktop/README.md)。

## MCP

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

提供 19 个只读工具，入参 schema 随 `capabilities` 和 tools/list 返回。信任、重建和写入归档走 CLI 控制通道，MCP 协议流不包含 daemon 日志。使用与边界见[护栏指南](docs/guardrails.md)、[接口规格](docs/interfaces.md)与 [Protobuf 分析](docs/contracts.md)。

## SCIP 导入与 Go（实验）

```bash
# 固定源文件哈希与分析配置后，在同一份输入上运行外部索引器
semweft index --syntax-only --export-manifest inputs.json
semweft index --scip index.scip --scip-manifest inputs.json

# Go 需可选 feature；语义刷新依赖外部 scip-go
cargo install --locked --path crates/semweft-cli --features go-experimental
```

SCIP 导入本身不运行生产工具，无需构建执行授权；生产者名称和版本必填，文件与配置哈希不一致时拒绝导入。Go 语义刷新使用 `scip-go --output <file>`（可用 `SEMWEFT_SCIP_GO` 指定），目前只支持依赖深度 0。

## 验证

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
```

需要 rust-analyzer / rust-src 的忽略测试与编辑期一致性原型见 [P0 清单](docs/p0.md) 与[刷新原型](docs/prototype.md)。

## 当前边界

- Rust L1 名称匹配是候选证据，未解析词法作用域或遮蔽；L3 提供工作区内定义、实现枚举及引用，不宣称穷尽。
- `index.dependency_depth=n` 可纳入离线依赖图上至多 n 跳源码，默认 0，正值需要仓库授权。
- 存储按内容哈希复用源码、语法事实与视图，支持 `snapshots usage|gc|compact`、缓存双预算和安全回收。
- 分页游标在 daemon 重启后失效；历史按需加载，默认保留全部历史。
- 独立 `semweft-bench` 提供真实输入测量，见[基础层记录](docs/foundations.md)。

## 风险输入

变更风险可选接入快照绑定的 LCOV 行观测与冻结 Git 历史：用 `semweft risk-inputs` 生成 artifact，再传给 diff 命令的 `--risk-inputs` / `--base-risk-inputs`，或给 `guard before` 使用。输入缺失或过期不会伪装成低风险，也不自动执行测试。见[风险输入指南](docs/risk-inputs.md)。

## 文档

从[文档总览](semweft.md)开始。常用入口：

- [产品需求](docs/product.md) · [核心概念与证据模型](docs/evidence-model.md)
- [底层基础实现](docs/foundations.md) · [接口规格](docs/interfaces.md)
- [路线图、风险与决策](docs/roadmap.md) · [20 项功能清单](docs/product-next.md)
- [CI 报告说明](docs/ci.md) · [服务映射](docs/services.md) · [公开 API 候选](docs/public-api.md)

## 许可

引擎和 CLI 计划采用 Apache-2.0（暂定）。
