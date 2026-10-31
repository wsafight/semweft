# GPUI + gpui-rsx 桌面工作台

使用 **GPUI 0.2.2 + gpui-rsx 0.10.2** 原生渲染。页面、节点卡片、列表行、矩阵与布局使用 `rsx!`。中文输入法通过 GPUI `EntityInputHandler` 接入，长文件通过 GPUI 虚拟列表渲染可见行。没有引入 WebView 或 gpui-kit。

通用按钮、图标按钮、SVG 图标、文件/符号标识、面板标题、徽标、字段、度量卡、空状态、Section 和源码交互文本在 [src/ui/components.rs](src/ui/components.rs) 使用 `#[component]`，覆盖函数/struct 组件、props、children、回调 trait 和条件样式。事件、快捷键、焦点及可选内容也使用 RSX；原生虚拟列表通过 `base` 接入。后续发现的 gpui-rsx 问题与复现统一记录于 [反馈记录](../../docs/gpui-rsx-feedback.md)。

**开发约定**：优先并尽量多使用 gpui-rsx；仅在必要兼容处使用原生 GPUI API。遇到 gpui-rsx 的编译、兼容性、功能限制或渲染问题，应及时说明具体问题、影响与处理方案，不静默更换界面技术。见根目录 [AGENTS.md](../../AGENTS.md)。

## 启动

macOS 安装 Rust、Python 3、Xcode Command Line Tools 后，在仓库根目录运行：

```bash
# 默认生成一个隔离的 Rust 变更示例
python3 crates/semweft-desktop/scripts/preview.py
open "target/desktop-preview/Semweft Preview.app"

# 或分析自己的 Rust / Go / TypeScript / JavaScript 项目
python3 crates/semweft-desktop/scripts/preview.py --workspace /path/to/project
```

应用中也可用 `⌘O` 切换项目。重新启动会读取上次捕获的项目，刷新继续分析该项目。工作区来自 Cargo.toml、go.mod/go.work 或 package.json；桌面构建会启用 `go-experimental,web-experimental`。不会执行项目的构建、npm 脚本或包安装。

首次构建需要下载依赖。使用 GPUI `runtime_shaders`，无需完整 Xcode Metal 工具链。应用依赖当前源码目录、Python 与本地 CLI，目前是本地开发应用，尚未打包为独立安装包。桌面 crate 与引擎 workspace 分别锁定依赖，当前验证平台为 macOS。

## 已实现

| 页面 | 功能 |
| --- | --- |
| 源码浏览 | Rust/Go/TS/TSX/JS/JSX 语法高亮；点击标识符查询定义/同名候选并联动调用与引用；可折叠目录树与路径过滤、完整文件、行号定位、可收起大纲、复制、前进后退、书签；源码与定义候选均虚拟滚动 |
| 符号导航 | 按名称、路径、类型关键词过滤；签名、文档、可见性、作用域；跳转源码、调用图、引用 |
| 全文搜索 | 在捕获的源码中搜索，不区分大小写；显示文件、原始行号和匹配行，点击定位；前 500 条与总匹配行数分别显示 |
| 调用与引用 | 按快照及语义 epoch 查询；调用方/被调用方/双向、1–5 层；直接邻接节点可继续导航；完整关联位置列表保留 read/write/call/type/import 与 certainty |
| 模块依赖 | 按文件的依赖矩阵，行列独立翻页；候选循环按强连通分量分组；显示依赖依据及包声明依赖 |
| 度量与热点 | 物理行、非空行、函数和符号数量；文件增删行与提交触及次数；按源码跨度排列的最长函数 |
| 我的书签 | 收藏文件行；按项目保存并跨启动读取；支持删除；源码改动后需核查原行号 |
| 仓库概览 | 项目、模块、核心符号与分支 |
| 变更审查 | Git 双侧变更、候选影响、缺口、Base/Head 证据和局部源码；作用域/未阅筛选、本地标记、JSON 导出 |

Git 热点范围为最近 90 天、最多 200 条提交，按当前文件路径统计，不追踪重命名。非空行包含注释，函数跨度包含空白；这些指标没有冒充圈复杂度。循环和调用关系可能来自 L1 名称候选，应结合证据核查。

变更符号切到 Base 时展示该侧捕获的局部源码。完整源码、调用图和引用导航目前仅支持 Head，Base 的这三个按钮会禁用；Head 跳转先校验文件内容哈希与坐标。

快捷键：`⌘P` 搜索文件、`⌘T` 搜索符号、`⌘F` 全文搜索、`⌘L` 输入行号并按 Enter 定位、`⌘D` 书签、`⌥←/→` 导航、`⌘O` 打开项目、`⌘R` 刷新、`⌘W` 关闭、`⌘Q` 退出。

点击标识符会查询当前固定快照的 UTF-8 字节位置，并校验所显示源码的内容哈希。同名候选保留来源与确定性，供选择后跳转；跳转目标也检查内容哈希，前进/后退保留调用位置。刷新、切换文件/行或收起面板会使在途旧查询失效。语法着色使用各语言 tree-sitter 查询，不代表语义绑定；超过 2 MiB 的单文件保留纯文本虚拟浏览，并显示高亮与标识符导航未启用的说明。

## 界面与导航

九个页面共用深色分组导航、浅色内容区、页面标题、工具栏及卡片样式。导航按「探索 / 分析 / 个人」分组；图标使用内嵌的统一 SVG，无需下载字体或外部资源。概览使用模块卡片，关系图使用分栏节点，依赖矩阵、度量表与证据详情各有独立的内容区域。

文件目录按目录优先排序，支持折叠与收起全部；搜索时保留父目录并自动展开匹配路径。打开源码或跳转定义会展开目标文件的祖先目录。源码区区分文件标签、路径面包屑、行号栏和代码正文，右上角可收起文件大纲；图标操作悬停显示说明及已有快捷键。全局状态栏保留当前语言、行号和截断提示，完整证据与来源仍在详情和报告中。

## 分析与持久化

导出器通过真实 CLI 查询捕获固定快照与源码；符号、模块、依赖、影响及按需调用/引用查询会继续读取分页。引擎的深度、节点、超时等范围限制仍保留，截断状态不会被分页合并掩盖。异步查询绑定快照及语义 epoch，刷新或后续查询使旧任务结果失效。刷新失败保留现有视图。

Rust、Go 与 TS/JS 均可使用语法候选导航。TS/JS 支持 ts/tsx/js/jsx/mjs/cjs/mts/cts，包含函数、命名箭头函数、类方法、类型、ES 导入及相对模块依赖。node_modules、常见生成目录、符号链接与忽略路径不会捕获。TS/JS 的词法/类型绑定、tsconfig 别名、动态导入、CommonJS 导出、匿名回调归属和 tsserver 尚未接入；Go 构建标签与动态分派也属于分析缺口。语义证据仍以 CLI 能力与每条关系的来源为准。桌面源码捕获目前限于所选目录；go.work 引用目录外成员时，CLI 可返回其符号，桌面会提示没有捕获对应源码。

示例与缓存位于 `target/desktop-preview/`。本地审阅标记保存于 `local-marks/<review_key>.json`，绑定两侧快照及语义 epoch，不迁移到新输入；书签保存于 `sessions/<workspace_key>/session.json`。这些标记不会提升候选关系的确定性。正式 review 控制接口、Agent 回传、规则核验和商业 Understand 的全部功能尚未完成。

macOS 必须开启 GPUI `font-kit`；否则 GPUI 会使用无字形输出的 `NoopTextSystem`。构建脚本先运行系统/中文字形检查与数据检查，再打包。

## 开发检查

`cargo test` 包含动态字符串 key、数值默认值、异构根片段、Tailwind 间距比例，以及 InteractiveText 双参数/普通 Div 单参数 onClick 的消费端回归。源码交互文本直接使用 RSX 的 `onClick={(ranges, listener)}`。gpui-rsx 升级时先改造对应代码并验证，再清理反馈记录中已解决的条目。

```bash
cargo fmt --manifest-path crates/semweft-desktop/Cargo.toml -- --check
cargo clippy --locked --manifest-path crates/semweft-desktop/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path crates/semweft-desktop/Cargo.toml
# 原生检查九页、十种条件状态、IME/action 路由、异步定义查询、跳转与行定位
crates/semweft-desktop/target/debug/semweft-desktop --check-ui
# 校验真实定义查询与内容不匹配拒绝，可追加 file:line:column 测试调用位置
crates/semweft-desktop/target/debug/semweft-desktop --check-source target/desktop-preview/data.json
# 校验固定快照的实际调用与引用查询
crates/semweft-desktop/target/debug/semweft-desktop --check-navigation target/desktop-preview/data.json
```

输入组件改编自 GPUI 0.2.2 Apache-2.0 示例，许可见 [licenses/GPUI-APACHE-2.0.txt](licenses/GPUI-APACHE-2.0.txt)。
