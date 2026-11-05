# GPUI + gpui-rsx 桌面工作台

使用 **GPUI 0.2.2 + gpui-rsx 0.11.0** 原生渲染。布局、节点、列表、矩阵、工具栏和交互使用 `rsx!`；有向边使用 GPUI canvas，长列表使用原生虚拟列表。中文输入法通过 `EntityInputHandler` 接入。依赖升级和实际问题记录见 [gpui-rsx 反馈](../../docs/gpui-rsx-feedback.md)。

## 构建与启动

构建机需要 macOS、Rust、Python 3 和 Xcode Command Line Tools：

```bash
python3 crates/semweft-desktop/scripts/preview.py
open "target/desktop-preview/Semweft Preview.app"
# 优化构建，或指定初始工作区
python3 crates/semweft-desktop/scripts/preview.py --release --workspace /path/to/project
```

输出应用包和 `target/desktop-preview/Semweft-macOS.zip`。包内包含桌面程序、`semweft` 引擎、隔离的 Git 示例和许可；移出仓库后可独立运行，运行时不需要 Python、Rust 或项目源码仓库。构建脚本生成版本元数据、SHA-256，并验证签名。本地安装器支持校验包、安装、升级和回滚；生产分发仍需 Developer ID、公证及可信渠道提供的摘要。自动更新和其他桌面平台尚未接入。

首次启动把示例复制到可写数据目录，再通过本地服务捕获。后续启动恢复上次视图，可用 `⌘O` 打开项目、工作区标签切换已打开项目、`⌘R` 刷新。数据默认保存在 `~/Library/Application Support/Semweft`；开发二进制使用 `target/desktop-preview`。可用 `SEMWEFT_DESKTOP_DATA_DIR` 隔离桌面数据目录；`SEMWEFT_ENGINE` 可指定引擎。缓存与配置仍遵循引擎配置目录约定。

桌面构建启用 Go 和 Web 实验特性。工作区识别 Cargo.toml、go.mod/go.work、package.json；不会自动执行项目构建、npm 脚本或测试。首次构建下载依赖，使用 GPUI runtime_shaders，无需完整 Xcode Metal 工具链。

## 已实现

| 页面 | 功能 |
| --- | --- |
| 源码浏览 | Rust/Go/TS/TSX/JS/JSX 语法高亮、目录折叠、大纲、标识符定义候选、行定位、复制和书签；Base/Head 文件标签、双侧对照；源码和定义候选虚拟滚动 |
| 符号导航 | 名称、路径、类型过滤；签名、文档、可见性、作用域；跳转源码、调用图和引用 |
| 全文搜索 | 捕获的 Head 源码中搜索；展示前 500 个匹配行与总数，点击定位 |
| 调用与引用 | 固定快照/语义 epoch；调用方、被调用方、双向及 1–5 层；按页显示关联和定义候选，可取消 |
| 交互图 | 横向/纵向/网格布局、节点拖动、缩放、拖动/滚轮平移、文件聚合、节点折叠、相邻边高亮、鸟瞰图、源码跳转和 SVG 导出；最多 160 个可达节点，候选/过期边保留虚线 |
| 模块依赖 | 行列独立翻页的文件矩阵、SCC 候选循环、依赖依据和包声明；可切换交互依赖图 |
| 度量与热点 | 总行、非空行、函数/符号数量，历史增删行和触及次数，最长函数 |
| 我的书签 | 按工作区和 Base/Head 保存文件行；跨启动读取、删除 |
| 仓库概览 | 项目、模块、核心符号和分支 |
| 变更审查 | Git 双侧变更、候选影响、覆盖缺口；Base/Head 行对齐 diff、hunk 导航、本地源码批注；作用域/未阅筛选和 JSON 导出 |
| 验证与测试 | 固定 Base/Head 的 verify_change、suggest_tests、arch_check；分页发现、测试候选、证据与覆盖范围；逐项结论及人工审查记录 |
| 深入分析 | 关系路径、Trait 实现、公开 API、变更风险、Protobuf 契约影响、服务依赖图和修改前检查；证据跳转、分页、取消和 JSON 导出 |

图明确显示隐藏节点数量；展开已加载图与继续加载服务分页是不同操作。布局、缩放、平移、分组和节点位置按图身份保存到工作区会话。依赖图展示当前根文件所在的连通部分，SVG 导出当前可见图。行对齐 diff 对中等变更使用有界 LCS，大块变更退化为粗粒度替换行并明确标记；它是只读审查视图，不是编辑器。分析工具的可用能力受当前索引及项目配置约束，例如未配置服务映射时服务依赖查询会返回不支持状态。

Git 热点统计最近 90 天、最多 200 条提交，按当前路径计数，不跟踪重命名。非空行包含注释，函数跨度包含空白；不等同于圈复杂度。循环、调用和测试建议可能来自语法候选，应结合证据核查；推荐测试未执行，也不代表覆盖率。

## 查询与持久化

桌面仅启动一次 CLI 引导服务，随后通过本地 IPC 查询，并持有 watcher lease。刷新显示六阶段进度，可取消；失败保留已显示视图。调用/引用、定义候选、分析发现先显示一页，再按需加载；后台结果通过任务代次与快照/语义版本校验，避免旧请求覆盖新视图。

调用/引用、定义候选、验证与测试、深入分析统一使用 `PageState<T>` 管理查询结果、忙碌状态、错误、取消、任务代次与分页游标去重。验证与测试、深入分析复用 `QueryToolbar` 和 `EvidencePanel`，各页提供工具操作、虚拟列表、证据源码跳转及审查内容；共享布局使用 `rsx!`。

源码从不可变索引 manifest 分页读取，校验内容哈希，不读取变化中的工作目录冒充固定源码。Base/Head 各自保存完整源码和符号；定义、调用和引用绑定各自快照，跳转校验 UTF-8 字节位置及对应内容哈希。旧导出 JSON 没有完整 Base 符号时，请刷新后使用 Base 导航。语法着色不代表语义绑定；超过 2 MiB 的文件保留纯文本虚拟浏览并显示高亮/标识符导航限制。

文件标签、活动侧/行、导航历史、目录/大纲宽度、大纲可见性、双侧对照和书签保存在 `sessions/<workspace_key>/session.json`。工作区标签列表保存在 `workspaces.json`。新快照只恢复仍存在的文件，原行号需核查。全文搜索与度量仍针对 Head。

逐项审阅标记保存在 `local-marks/<review_key>.json`，绑定快照和语义 epoch。正式请求通过 `semweft review create` 创建并固定报告；`review open <review_id>` 打开请求，逐项决定和整体批准通过带 revision 的事件记录保存。批准要求逐项确认发现，报告/规则或源码变化会使旧批准失效。Agent 可用只读 `get_review_result(review_id)` 获取记录；`review wait` 提供有界轮询，不是推送通知。人工结论不会改变候选确定性，也不是密码学签名。

## 安装与回滚

本地预览包可以显式允许 ad-hoc 签名并安装到临时目录：

```bash
python3 crates/semweft-desktop/scripts/install.py \
  --archive target/desktop-preview/Semweft-macOS.zip \
  --sha256 "$(python3 -c 'import json; print(json.load(open("target/desktop-preview/release.json"))["sha256"])')" \
  --allow-ad-hoc --destination "/tmp/Semweft Preview.app"
```

正式安装默认要求可信 Developer ID Team ID 和可信来源提供的预期摘要；`release.json` 与应用包同在一个未认证渠道时，不能单独作为信任根。升级保留一个上一版本，`--rollback` 可交换当前与上一版本。安装器不负责下载更新或后台更新检查。

语言能力沿用引擎。TS/JS 的完整类型绑定、tsconfig 别名、动态导入、CommonJS 导出和 tsserver，Go 构建标签与动态分派等仍有缺口；以每条证据和 completeness 为准。架构检查使用工作区配置的策略，未配置策略不能视为架构合规。没有补齐 Understand 的多语言语义、控制/数据流、复杂度指标和工业规则库。

## 开发检查

```bash
cargo fmt --manifest-path crates/semweft-desktop/Cargo.toml -- --check
cargo clippy --locked --manifest-path crates/semweft-desktop/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path crates/semweft-desktop/Cargo.toml
# 原生检查十一页、交互图与对照状态、IME/action、异步定义与分析查询；完成后自动退出
crates/semweft-desktop/target/debug/semweft-desktop --check-ui --data target/desktop-preview/data.json
# 同一回归检查后停留在工作区概览，窗口保持打开
crates/semweft-desktop/target/debug/semweft-desktop --check-ui-keep-open --data target/desktop-preview/data.json
# 本地 IPC、双侧历史源码/定义、完整报告分页、lease 与取消
crates/semweft-desktop/target/debug/semweft-desktop --check-service target/desktop-preview/data.json
crates/semweft-desktop/target/debug/semweft-desktop --check-source target/desktop-preview/data.json
crates/semweft-desktop/target/debug/semweft-desktop --check-navigation target/desktop-preview/data.json
```

消费端回归覆盖动态 key、数值默认值、异构片段、Tailwind 比例、InteractiveText 回调，另有图布局边界/循环/聚合、报告分页合并、双侧源码身份和会话兼容测试。引擎端测试历史源码分页，不允许新文件内容或错误 epoch 混入旧快照。

输入组件改编自 GPUI 0.2.2 Apache-2.0 示例，见 [许可](licenses/GPUI-APACHE-2.0.txt)。
