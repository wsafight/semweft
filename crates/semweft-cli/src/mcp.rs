use crate::{config::Resolved, daemon, query};
use anyhow::Result;
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    model::*,
    service::{RequestContext, RoleServer},
};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Bridge {
    resolved: Arc<Resolved>,
    session: String,
    lease: Arc<Mutex<daemon::Lease>>,
}
impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(Implementation::new("semweft",env!("CARGO_PKG_VERSION"))).with_instructions("Local workspace evidence. Read completeness and freshness. Impact results are candidates. Trust and reindex are CLI controls.")
    }
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        let tools = query::tools()
            .into_iter()
            .map(|t| {
                Tool::new(
                    t["name"].as_str().unwrap().to_owned(),
                    t["description"].as_str().unwrap().to_owned(),
                    Arc::new(t["input_schema"].as_object().unwrap().clone()),
                )
            })
            .collect();
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let resolved = self.resolved.clone();
        let name = request.name.into_owned();
        let args = Value::Object(request.arguments.unwrap_or_default());
        if let Err(error) = query::validate(&name, &args) {
            return Ok(CallToolResult::structured_error(query::error_value(&error)).into());
        }
        let id = format!("{}:{:?}", self.session, context.id);
        let worker_id = id.clone();
        let lease = self.lease.clone();
        let task = tokio::task::spawn_blocking(move || {
            daemon::call_with_id(&resolved, &lease, &name, args, &worker_id)
        });
        let value = tokio::select! {
            result=task=>match result {Ok(Ok(value))=>value,Ok(Err(error))=>query::error_value(&error),Err(error)=>query::error_value(&anyhow::anyhow!("internal_error: {error}"))},
            _=context.ct.cancelled()=>{let resolved=self.resolved.clone();tokio::task::spawn_blocking(move ||daemon::call(&resolved,"__cancel",serde_json::json!({"id":id})));serde_json::json!({"schema_version":"0.1","error":{"code":"cancelled","message":"client cancelled this query"}})},
        };
        Ok(if value.get("error").is_some() {
            CallToolResult::structured_error(value)
        } else {
            CallToolResult::structured(value)
        }
        .into())
    }
}
pub async fn serve(resolved: Resolved) -> Result<()> {
    let lease = Arc::new(Mutex::new(daemon::lease(&resolved)?));
    let resolved = Arc::new(resolved);
    let monitor_resolved = resolved.clone();
    let monitor_lease = lease.clone();
    let monitor = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let resolved = monitor_resolved.clone();
            let lease = monitor_lease.clone();
            let _ = tokio::task::spawn_blocking(move || lease.lock().unwrap().reconnect(&resolved))
                .await;
        }
    });
    let result = async {
        Bridge {
            resolved,
            session: format!("{}-{:?}", std::process::id(), std::time::SystemTime::now()),
            lease,
        }
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
        Ok(())
    }
    .await;
    monitor.abort();
    result
}
