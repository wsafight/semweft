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
