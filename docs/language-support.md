# Semweft 语言与 IDL 支持

> 本文：支持矩阵、各语言难点、IDL 映射与新语言接入规范。 [返回文档索引](../semweft.md)

## 1. 证据来源

tree-sitter 先提供 L1，批量语义索引与常驻语义服务提供 L3 证据；自研轻量名称解析和类型推断属于可选 L2，目前各语言均未排期，不进入 v0.1。不能把语义服务的结果改标为 L2。来源不可用时返回已有的当前证据，并披露能力缺口。以下为规划与待验证的适配范围，不表示已有实现；正式支持须满足本文 5.2 的标准，实际能力通过 capabilities 查询。

---

## 当前实验实现

Rust 的语法、SCIP 与 rust-analyzer 路径已经实现；Go 通过 `go-experimental` 启用，支持授权后的 scip-go 项目重建。`web-experimental` 提供 TS/JS/TSX、package.json 捕获、本地 JSONC tsconfig 单继承 paths/baseUrl 候选、literal import()/require()、导航和双侧 diff，新增授权后的 scip-typescript 项目重建。它不是常驻 tsserver，L1 模块候选不等于类型/词法绑定；真实 L3 仍需源清单匹配的 SCIP。Semweft 不自动下载项目依赖或安装 npm 工具，生成目录与 node_modules 排除。桌面构建同时启用两个 feature。正式验收仍未完成，详见 [语言验收状态](language-acceptance.md)。

Go 可在 `semweft.toml` 中设置 `[go] tags = ["purego"]`。未设置 tags 时继承进程的 GOFLAGS；显式空数组清空 GOFLAGS，非空数组排序去重后替换为 `-tags=a,b`，不会合并其他环境 GOFLAGS。GOFLAGS、GOOS、GOARCH、CGO_ENABLED、GOWORK、GOTOOLCHAIN、GOENV、GOPATH、GOROOT、GOEXPERIMENT 的有效值冻结进快照并用于生产器执行；历史语义复用也要求相同上下文与解析器版本。Git revision 查询按当前分析/构建环境尝试复用，不代表恢复历史机器环境。Go 模块内的汇编、头文件及常见 C/C++/Objective-C/Fortran 文本构建输入也参与捕获与监听。外部 GOENV 文件内容、工具链/依赖二进制、CGO 的其他环境和生成器输入仍未完整冻结。

TS/JS 的匿名默认导出函数保留 `default` 所有者，匿名回调/箭头函数获得独立的 `<anonymous@行:列>` 所有者，函数体内调用不会归入外围函数。`callback_invocation_not_modeled` 诊断披露回调的执行入口与动态流尚未建模，当前不会凭词法嵌套创建外围函数到回调的执行边。

真实 scip-typescript 0.6.0 的 JS 验证表明：匿名默认导出函数缺少定义/导入目标记录，使用方的别名只有未对齐的 local 符号。该函数体内的具名调用仍可提供 L3，导入方调用保持未解析；具名默认导出作为对照可对齐。Semweft 不用语法猜测补造这部分 L3，报告保留 `scip_unresolved_call` 等缺口。

以下矩阵保留正式支持的长期规划，不将实验实现视为语义完整支持。

## 2. 语言支持矩阵

|语言|L1 语法解析|L2|批量 L3（冷启动 / CI）|常驻 L3（编辑期）|构建与框架识别|版本|优先级|
|---|---|---|---|---|---|---|---|
|Rust|tree-sitter-rust|未排期|rust-analyzer scip|v0.1 rust-analyzer；LSP 与内嵌 ra_ap_* 的选择由原型决定|v0.1 Cargo workspace、bin/lib 目标与可定位的 main；build.rs、宏与生成物分项声明，框架入口后续适配|v0.1 主语言|P0|
|Go|tree-sitter-go|未排期|scip-go|v0.1 实验版先项目重建；v0.2 计划接 gopls 按需刷新|go.mod、go.work；框架入口随适配器验证|v0.1 实验；v0.2 正式|P1|
|TypeScript / JavaScript|tree-sitter-typescript、tree-sitter-javascript|未排期|scip-typescript|v0.3 计划接 tsserver，查询覆盖需验证|workspace、tsconfig paths；框架入口按版本逐项适配|v0.3|P1|
|Python|tree-sitter-python|未排期|scip-python|v0.4 计划接 pyright，查询覆盖需验证|pyproject、uv、poetry；动态机制与框架分项声明|v0.4|P1|
|Java / Kotlin|tree-sitter-java、tree-sitter-kotlin|未排期|scip-java；Kotlin 覆盖按工具版本独立验证|未定；首发允许项目重建，Java/Kotlin 分项验收|Maven、Gradle；Spring Boot|v1.0|P1|
|C / C++|tree-sitter-c、tree-sitter-cpp|未排期|scip-clang（需 compile_commands.json）|后续评估 clangd；未接入前项目重建|CMake、Bazel|后续|P2|
|C#|tree-sitter-c-sharp|未排期|scip-dotnet|未定；首发允许项目重建|MSBuild；ASP.NET Core|后续|P2|

**编辑期降级**：Go v0.1 的编辑先发布新语法快照并使受影响语义 pending，再以锁定输入重建受影响项目；成功且调用点对齐完成后发布 current 视图。失败保留 pending 并记录 refresh_failed，后续编辑或 CLI semweft index 可重试，具体恢复提示见[接口规格](interfaces.md)第 1.5 节。未授权、缺少工具等阻断另列 known_gaps 和 index_status。该实验路径单独报告重建时间与 pending 持续时间；其他语言的常驻路径未达门槛时采用相同降级，并在 capabilities 标明实际刷新方式。

## 3. 各语言难点

无法唯一确定目标时返回 candidate；无法可靠枚举候选时返回未解析调用点和原因，不制造候选集合。下表描述适配方向。

|语言|主要难点|Semweft 的处理|只能给到 candidate 的关系|
|---|---|---|---|
|Rust|宏展开、trait object、泛型、build.rs 生成的代码|复用 rust-analyzer 的可用语义信息；按已分析类型列候选；生成物仅在可定位且版本匹配时纳入|dyn Trait、未确定的泛型实现和函数指针；不可映射的宏位置列为缺口|
|Go|interface 隐式实现、结构体嵌入、反射|按工作区方法集计算候选实现；处理嵌入字段的方法提升|interface 方法调用（已索引实现不保证包含外部实现）、reflect 调用、函数类型变量|
|TypeScript / JavaScript|any 和 unknown、复杂类型运算、动态 import；JS 没有类型|优先用 tsserver 的类型；JS 借助 JSDoc 和导入关系推断|any 上的方法调用、obj[key] 动态访问、动态 import 的模块|
|Python|动态属性、monkey patch、装饰器、缺少类型注解|利用类型注解和 pyright 推断；按 FastAPI、Django 等框架的约定为装饰器建模|getattr 和 setattr、importlib 动态导入、无注解参数上的调用|
|Java / Kotlin|反射、依赖注入、注解处理器|按 Spring 注解为依赖注入建模；纳入注解处理器生成的代码|反射调用、按名称注入的 Bean|
|C / C++|预处理宏、多套编译配置、模板、虚调用、函数指针与生成头文件|按 compile_commands.json 固定翻译单元与宏；借助语义索引映射展开位置；配置与位置映射失败单列诊断|未确定的虚调用与函数指针目标；跨配置目标分开呈现，不能映射的宏列为缺口|
|C#|泛型、虚调用、dynamic、反射、源生成器与条件编译|按 MSBuild target 与条件固定输入；记录源生成器及生成物；利用语义索引解析静态绑定|dynamic、反射、委托和无法确定实现的接口/虚调用；缺失生成物列为缺口|

## 4. IDL 支持

|IDL|解析|生成代码映射|版本|优先级|
|---|---|---|---|---|
|Thrift|semweft-idl-thrift（优先评估复用 pilota 解析器）|按锁定的 thriftgo / Kitex / Volo 生成器版本适配|v0.4|P1|
|Protobuf|semweft-idl-proto（评估 protox）|按锁定的 protoc-gen-go / prost / tonic 生成器版本适配|v0.4|P1|
|OpenAPI|后续新增 semweft-idl-openapi|openapi-generator 生成的客户端和服务端|后续|P2|
|GraphQL|后续新增 semweft-idl-graphql|graphql-codegen 等|后续|P2|

**生成代码映射回 IDL**：记录生成器版本、源 IDL、字段编号或身份与生成符号的对应关系。直接字段访问、getter/setter、序列化和整对象透传分别建模；无法证明读写某个字段的透传只标为契约或类型级潜在影响。没有映射证据的字段列为缺口。对生成客户端的引用证明契约依赖；识别实际服务实例还需显式配置、服务发现或部署映射。

---

## 5. 新语言接入规范

下面是接口草图，不是已实现 API。语言适配器先实现 extract 提供实验性 L1，其余能力必须显式声明；Unsupported 与“分析完成但结果为空”分别表达。诊断、取消、快照身份和失效依赖是首版接口要求，Rust 的 semantic_backend 也必须在 v0.1 接入。

```rust
pub struct AnalysisContext<'a> {
    /// 不可变输入身份，包含 snapshot_id、syntax_epoch 与构建输入哈希
    pub snapshot: &'a SnapshotManifest,
    pub cancel: &'a CancellationToken,
}

pub struct FileAnalysis {
    /// snapshot_id、syntax_epoch、文件路径与 file_hash；调度器校验后才能发布
    pub source: SourceIdentity,
    pub facts: FileFacts,
    /// 导出身份、签名、类型及实现集合指纹，用于判断跨文件失效
    pub exports: Vec<ExportFact>,
    /// 导入、符号引用、生成物和构建输入依赖，含依赖目标及作用域
    pub dependencies: Vec<DependencyFact>,
    pub diagnostics: Vec<Diagnostic>,
}

pub enum CapabilityResult<T> {
    Unsupported,
    Complete(T),
}

/// 接入方式与进程边界分别声明；具体字段由前置原型确定。
pub enum SemanticBackendSpec {
    ExternalProcess(ProcessBackendSpec),
    Embedded(EmbeddedBackendSpec),
}

pub trait LanguageFrontend: Send + Sync {
    fn id(&self) -> LangId;
    fn capabilities(&self) -> FrontendCapabilities;
    fn file_matches(&self, path: &Path, project: &ProjectUnit) -> bool;
    fn grammar(&self) -> tree_sitter::Language;

    /// L1：返回事实、输入身份、导出、依赖和诊断（必须实现）
    fn extract(
        &self, ctx: &AnalysisContext<'_>, file: &SourceFile, tree: &Tree,
    ) -> Result<FileAnalysis, FrontendError>;

    /// L2：轻量名称解析和类型推断
    fn resolve(
        &self, _ctx: &AnalysisContext<'_>, _ws: &WorkspaceView,
        _analysis: &mut FileAnalysis,
    ) -> Result<CapabilityResult<()>, FrontendError> {
        Ok(CapabilityResult::Unsupported)
    }

    /// L3：批量 SCIP 索引器的启动与输出协议
    fn semantic_indexer(&self) -> Option<IndexerSpec> { None }

    /// L3：常驻语义后端；运行器统一处理输入同步、查询与取消
    fn semantic_backend(&self) -> Option<SemanticBackendSpec> { None }

    /// 构建系统识别
    fn detect_projects(
        &self, ctx: &AnalysisContext<'_>, root: &Path,
    ) -> Result<CapabilityResult<Vec<ProjectUnit>>, FrontendError>;

    /// 基本入口/目标与框架入口分项声明；Rust v0.1 为 bin/lib 与 main
    fn entry_points(&self, _facts: &FileFacts) -> CapabilityResult<Vec<EntryPoint>> {
        CapabilityResult::Unsupported
    }

    /// 生成代码识别并关联回 IDL（thriftgo、prost 等）
    fn generated_from(&self, _file: &SourceFile) -> CapabilityResult<Option<GeneratedFrom>> {
        CapabilityResult::Unsupported
    }
}
```

**首版配套契约（具体类型在详细设计中细化）**

- **诊断输出**：FileAnalysis.diagnostics 包含解析错误、未解析引用及位置映射失败；错误须区分输入失败、取消和能力不支持，供解析健康报告与 completeness 使用

- **取消**：extract、resolve、项目识别接收 AnalysisContext.cancel；批量索引与常驻查询由运行器传入同一任务的取消令牌。再次编辑可取消旧刷新；调用方取消等待不取消共享刷新

- **快照身份**：AnalysisContext 传入固定输入，FileAnalysis 和语义运行器的结果都携带 SourceIdentity。文件内容、构建输入或文档版本不匹配时拒绝发布，不能仅凭任务完成时间推断 current；semantic_epoch 的推进由调度器统一完成

- **失效依赖**：exports 提供导出签名、类型与实现集合的变化指纹；dependencies 提供构建反向依赖图所需的目标与作用域，包含尚未解析的依赖及原因。前端输出依赖事实，调度器维护反向图；导出、类型或实现变化沿反向边失效，文件删除也使用旧依赖图传播，构建或生成器变化按项目失效。范围不明时扩大失效范围，不以空依赖代表没有依赖

- **增量解析**：解析调度器传入旧树与编辑区间，并按[质量目标与评测](quality.md)第 1 节的固定工作负载测量；extract 消费调度器生成的树，不负责自行重建

- **文件归属冲突**：.h 可能属于 C 或 C++，file_matches 按 ProjectUnit 中的 compile_commands.json 或项目配置消歧，多个前端同时匹配时由注册表按显式项目配置和优先级选择，仍无法确定则报告诊断

- **常驻语义路径**：semantic_backend 返回 SemanticBackendSpec；共同契约是固定输入同步、定义/引用/实现/调用关系查询、取消和诊断。ExternalProcess 描述可执行文件及 LSP 或语言原生协议，LSP 路径另有能力协商与文档同步；Embedded 描述后端注册身份、数据库快照与查询映射，Rust 候选为 ra_ap_*，抽象类型不依赖其 crate。运行器分别映射查询集，返回事实须携带匹配的输入身份，未覆盖的关系保持 pending。Rust v0.1 使用常驻路径，Go v0.1 的项目重建例外见矩阵；刷新验收见本文 5.4 节

- **进程边界**：批量 SCIP 始终由独立子进程执行。常驻 LSP 由 daemon 监督外部进程；内嵌 ra_ap_* 可链接 daemon 或独立语义 worker，位置由原型比较决定并在 capabilities 公布。同进程内嵌的原生崩溃可能导致 daemon 重启，不能沿用外部进程的崩溃隔离保证；独立 worker 才由监督器重启。各方案分别报告重启、视图恢复和丢失游标的行为

- **能力声明**：FrontendCapabilities 按 L1/L2、批量 L3、常驻 L3、项目与框架能力分项声明。None 或 Unsupported 映射为能力缺口；只有显式完成的空结果才表示“在声明范围内未找到”。框架入口和生成代码映射未实现时也须标不支持

```toml
[features]
default = ["rust"]
rust   = ["dep:semweft-lang-rust"]
go     = ["dep:semweft-lang-go"]
ts     = ["dep:semweft-lang-ts"]
python = ["dep:semweft-lang-python"]
java   = ["dep:semweft-lang-java"]
cpp    = ["dep:semweft-lang-cpp"]
csharp = ["dep:semweft-lang-csharp"]
```

安装名和 feature 尚待实现验证。v0.1 预编译二进制默认启用 Rust；Go 需显式启用实验 feature，尚未实现的语言不进入发布包。L3 索引器与工具链须按能力清单另行安装或预置。

### 5.1 证据接入路径

1. **L1**：实现 extract，输出语法事实与启发式关系；目标不确定时标 candidate，不能解析时记录缺口

2. **L2**：后续实现 resolve 时补充名称绑定与类型信息，保留 certainty、诊断与依赖失效信息；未实现前应返回 Unsupported，不以空实现宣称支持

3. **L3**：实现 semantic_indexer，调用该语言的 SCIP 索引器（如 scip-go、scip-typescript）生成索引，由 semweft-scip 导入；调用边需与语法调用点按源码区间对齐，保留来源与推导，冲突按证据模型诊断。常驻路径通过 semantic_backend 按需查询受影响符号；批量路径用于冷启动、后续 CI 共享索引与无常驻能力时的项目重建。两条路径按同一证据模型合并，实际刷新策略在 capabilities 公布

4. **框架和契约**：实现 detect_projects、entry_points、generated_from，接入构建系统、服务入口和 IDL 映射

### 5.2 正式支持的标准

新语言先以“实验性”状态发布，满足以下全部条件后才标为“正式支持”：

- 在至少 3 个真实仓库上做独立标注：声明支持的直接调用 L3 definite 准确率目标 ≥99%、召回率目标 ≥95%；同时报告各来源的召回、候选错误率、未解析率与置信区间，不能把 SCIP 当完整调用图真值

- 有可用的 L3 来源，并在 CI 中自动验证 SCIP 导入

- 能识别主流构建系统和语言基本入口/目标；Rust v0.1 识别 Cargo workspace 成员、bin/lib 目标及可定位的 main，lib 作为库目标而非执行入口。Web/RPC 等框架入口随适配器分项验收

- 列出候选与未解析机制、仓库外依赖、构建条件和生成物限制，并验证跨文件与项目级失效

- 固定文件规模和编辑集下测解析 P50/P95/P99；普通文件解析 P99 50ms 为目标，端到端语法可查按[质量目标与评测](quality.md)第 1 节单独验收

- 语法 fixture 测试和 golden 测试齐全；golden 指固定输入与期望输出对比，不是产品的 Snapshot 快照

### 5.3 第三方语言插件（P2）

不进主仓库的语言，可以用 WASM 插件接入：Semweft 用 wasmtime 加载插件，插件携带 tree-sitter 的 WASM 语法并实现同一套接口。插件运行在沙箱中，崩溃不会影响主进程；插件执行形式不决定证据等级；来源、确定性与能力须按同一标准验证。WASM tree-sitter 与 wasmtime 的接口和 ABI 需原型验证，不承诺直接复用原生 Rust trait。

### 5.4 编辑期语义刷新（v0.1 前置原型）

常驻语义路径决定 v0.1 编辑期结果是否可信，先以原型验证，再确定存储与调度方案。原型需回答以下问题，表中候选做法待测量后定稿：

|问题|原型需回答|候选做法|
|---|---|---|
|刷新查询集|编辑后向语义后端发哪些查询，才能覆盖失效关系|失效单元内的调用点重新解析定义；导出变化的符号重查 references 与调用关系；trait 实现集合变化时重查 implementations。LSP 和内嵌后端分别映射查询集，未覆盖的关系保持 pending|
|pending 转为 current|何时可判定语义证据已对应某个 syntax_epoch|后端已同步该 epoch 的全部源码与构建输入，且失效单元的查询全部成功；中间结果暂存，完成后原子发布新的 semantic_epoch 视图。失败或超时保持 pending 并记录原因|
|连续编辑|刷新未完成时又发生编辑|通过取消令牌中止旧 epoch 未完成的查询，失效集合取并集后按最新 epoch 重排；debounce 窗口与合并策略由测量确定；旧 epoch 的部分结果不发布为 current|
|取消与崩溃|调用方取消查询，或语义后端崩溃、重启|取消只结束该查询的等待；外部进程或 worker 崩溃后保留 pending，重启后重新刷新，超过上限退回项目重建；同进程内嵌导致 daemon 退出时按架构第 1.1 节恢复|
|接入方式|走 LSP 还是内嵌 ra_ap_* crate，以及内嵌代码运行在哪个进程|同一编辑集下比较覆盖、延迟、合计内存与故障恢复；验证查询抽象适配两条路径，结论回填[路线图](roadmap.md)第 4 节|

**一致性验证**：固定 3 个真实 Rust 仓库与编辑序列，覆盖函数体修改、签名变化、trait 实现增删、跨 crate 导出变化和 Cargo feature 变化。每步编辑后，把增量刷新得到的 current 关系集合与同一源码冷启动全量分析的结果（同一语义路径）按 relation_id 比较，SCIP 重建结果另作对照。差异逐例分为可解释差异（须在能力清单披露）与缺陷；未解释差异为零，且 pending 持续时间的 P50/P95/P99 已测量，才进入存储与调度方案的定稿。原型当前进展与测量见[刷新原型进展](prototype.md)。
