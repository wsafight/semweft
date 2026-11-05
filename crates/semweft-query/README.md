# semweft-query

可直接嵌入的查询服务，负责采集、刷新、快照绑定、语言适配、证据合并、关系索引、分页与预算。CLI、daemon 和 MCP 共用此实现；本库不依赖命令行解析、IPC、文件监听或 MCP 运行时。Go/Web 可通过同名 feature 启用。

```rust,no_run
use semweft_query::{config, Engine, QueryService};
use serde_json::json;
use std::time::{Duration, Instant};
# fn main() -> anyhow::Result<()> {
let resolved = config::resolve(Some(std::path::Path::new("/path/to/workspace")), None)?;
// 宿主须先取得 index_dir 的进程级独占写锁，并持有到 shutdown 完成。
let engine = Engine::new(resolved)?;
let service = QueryService::new(engine.clone());
let result = service.execute("search_symbols", json!({"query":"Stock"}),
    Instant::now() + Duration::from_secs(5));
engine.shutdown();
println!("{}", result?);
# Ok(())
# }
```

`Engine::new` 启动后台刷新线程；宿主退出时必须调用 `shutdown`，并在此之后释放工作区和索引目录写锁。单个进程共享一个 `Arc<Engine>`，请求共享一个 `QueryService` 以保留游标；可用 `execute_registered` 和 `cancel` 关联取消。配置解析、仓库信任、构建授权、固定快照和错误协议与 CLI 相同。首次事实查询会采集源码；已有信任允许语义后端执行构建，宿主应遵循相同的授权约定。

独立库集成测试直接验证查询、编辑刷新、固定快照续页和历史恢复，不启动 CLI 或传输层。

前台查询仍进行两次内容采集及配置复核；输入清单和采集诊断均未变化时复用不可变语法视图，跳过事实复制、排序、校验及输入身份重算。位置索引按文件保存排序的 occurrence 偏移，用二分查找代替逐位置的树节点。

`index.query_cache_bytes` 默认 64 MiB，约束查询独占分配、延迟索引及独立持有的历史事实。刷新协调器已经常驻的分析视图不再重复计重；非驻留快照在多个语义版本间只计一次。`storage.query_cache.accounting` 说明此口径，`shared_resident_views` 给出刷新常驻视图数。这不是进程总内存上限，刷新根、活跃请求、语法缓存及游标都有独立生命周期。

符号级 `impact_of`、`call_graph` 和测试推荐的影响关系在遍历结束后，按 256 条分块、块内字符串字典和 LZ4 编码保存原始关系、深度和证据路径；只解码当前页涉及的块并生成该页 JSON。关系数不超过请求页容量时直接生成 JSON，省略编码。游标保存展示所需的符号名称、测试范围和文件哈希，不持有整个分析视图。测试推荐直接检查原始关系，仅为候选测试构造证据路径 JSON。Diff 报告先按关系身份去重，再生成一次关系 JSON；后续报告计算完成后，跨页列表按 128 条 LZ4 压缩，包括候选测试和嵌套增删关系。普通列表和内部 `compute` 仍返回完整 JSON。

这些路径保持完整统计、遍历限制、固定版本重放、游标预算及过期规则。首屏之前仍会完成有界遍历和所需的报告计算；压缩分页不等于增量遍历。设置 `SEMWEFT_QUERY_TIMING=1` 可向 stderr（daemon 的 `daemon.log`）输出 scan、capture、parse、verify_capture、bind、traverse、graph_pack、page 的耗时；未发生的阶段不输出，阶段可嵌套，不能直接相加。
