# 公开 API 候选清单

```sh
semweft search --api-scope external --all-pages
semweft search my_crate::Facade --api-scope external --match exact
semweft search --api-scope external --kind type --path crates/domain
semweft impact --diff WORKTREE --all-pages --format markdown
```

MCP 使用 `search_symbols {query:"*", api_scope:"external"}`。默认 `api_scope=declarations` 保留原搜索语义，`--visibility public` 仍是语法公开声明清单。

external 模式从捕获的 Cargo 库入口出发，使用显式 `[lib].path/name` 或默认 src/lib.rs，处理内联模块、常规模块文件和字符串 `#[path]`、本地 `pub use`（别名、分组、glob、self/super/crate 和多跳重导出）。私有父模块内未重导出的声明不出现在清单；公开类型通过别名重导出后，保留其公开字段、固有方法、trait 成员与 enum variant 的路径。二进制和测试入口不当成库 API；不运行 Cargo、编译器或过程宏。

每个符号携带 `api_exposure.paths[]`：导出路径、candidate、conditional、原始 cfg 条件、声明及逐步模块/重导出源码位置和哈希。别名可用于精确搜索；路径过滤按声明文件执行。`api_summary` 统计过滤后的声明及整个捕获 API 图的候选数量、路径和 known_gaps，分页绑定同一快照/epoch，不因后续编辑重算。

diff 同时返回 `api_changes[]` 和固定全量 `api_surface_summary`。按导出路径比较声明身份、签名和条件，能发现仅修改 pub use、隐藏模块或 cfg 的外部候选变化，即使 changed_symbols=0。代码实现体变化不等于 API 签名变化。条目分 added/removed/changed，保留 base_exports/head_exports 与各侧证据，纳入全分页和静态报告；不判定 SemVer 或行为兼容性。

边界：当前仅 Rust 捕获图，全部结果为候选。cfg 未求值，条件分支不会擅自选一条；生成物、宏、仓库外重导出、trait impl 的派生成员、类型别名继承和复杂类型求解不完整。未解析 reexport、模块缺失、解析错误与预算均显式记录。固定上限为 10,000 binding/import/export、64 层模块/重导出展开；预算到达会列 known_gaps，不能把缺失路径理解为已证明私有。捕获语法不是 rustdoc/compiler 的正式可达性证明。

测试覆盖私有父模块、别名/glob/分组、固有方法/字段、trait/enum、条件标记、自定义库入口、二进制排除、重导出独立 diff、过滤和编辑后的固定分页。外部可达性最终仍需编译器或独立语义证据确认。
