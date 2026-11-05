use crate::model::{Preview, Relation, Symbol};
use anyhow::{Context, Result, bail, ensure};
use interprocess::local_socket::{Stream, prelude::*};
use semweft_protocol::{DAEMON_PROTOCOL, DaemonRequest, DesktopEndpoint};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);

fn field_pointer(field: &str) -> String {
    format!("/{}", field.replace('.', "/"))
}

pub fn page_values<'a>(page: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    page.pointer(&field_pointer(field))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

pub fn merge_page(result: &mut Value, page: Value, fields: &[&str]) {
    for field in fields {
        let values = page_values(&page, field).cloned().collect::<Vec<_>>();
        if let Some(target) = result
            .pointer_mut(&field_pointer(field))
            .and_then(Value::as_array_mut)
        {
            target.extend(values);
        }
    }
    result["output"] = page["output"].clone();
}

#[derive(Clone)]
pub struct Client(Arc<Connection>);

struct Connection {
    engine: PathBuf,
    repository: PathBuf,
    workspace: PathBuf,
    development: bool,
    connection: Mutex<Option<(DesktopEndpoint, Stream)>>,
}

#[derive(Clone, Default)]
pub struct Cancellation(Arc<Control>);

#[derive(Default)]
struct Control {
    cancelled: AtomicBool,
    active: Mutex<BTreeMap<String, DesktopEndpoint>>,
    progress: Mutex<String>,
}

impl Cancellation {
    pub fn progress(&self) -> String {
        self.0.progress.lock().unwrap().clone()
    }

    fn stage(&self, stage: &str) -> Result<()> {
        self.check()?;
        *self.0.progress.lock().unwrap() = stage.into();
        Ok(())
    }

    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.0.cancelled.load(Ordering::SeqCst),
            "cancelled: query cancelled"
        );
        Ok(())
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        let active = self.0.active.lock().unwrap().clone();
        if !active.is_empty() {
            std::thread::spawn(move || {
                for (id, endpoint) in active {
                    let _ = request(&endpoint, "__cancel", json!({"id":id}), None);
                }
            });
        }
    }
}

fn socket(endpoint: &DesktopEndpoint) -> Result<Stream> {
    #[cfg(not(windows))]
    let name = endpoint
        .endpoint
        .as_str()
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()?;
    #[cfg(windows)]
    let name = endpoint
        .endpoint
        .as_str()
        .to_ns_name::<interprocess::local_socket::GenericNamespaced>()?;
    Stream::connect(name).context("daemon_unavailable: 无法连接本地分析服务")
}

fn exchange(
    stream: &mut Stream,
    endpoint: &DesktopEndpoint,
    method: &str,
    args: Value,
    id: Option<String>,
) -> Result<Value> {
    serde_json::to_writer(
        &mut *stream,
        &DaemonRequest {
            protocol: DAEMON_PROTOCOL.into(),
            workspace_id: endpoint.workspace_id.clone(),
            config_hash: endpoint.config_hash.clone(),
            method: method.into(),
            args,
            request_id: id,
        },
    )?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    ensure!(
        !line.is_empty(),
        "daemon_unavailable: 分析服务已断开，请刷新项目"
    );
    let value: Value = serde_json::from_str(&line).context("daemon_incompatible: 无效分析响应")?;
    if let Some(error) = value.get("error") {
        bail!("{error}");
    }
    Ok(value)
}

fn request(
    endpoint: &DesktopEndpoint,
    method: &str,
    args: Value,
    id: Option<String>,
) -> Result<Value> {
    exchange(&mut socket(endpoint)?, endpoint, method, args, id)
}

impl Client {
    pub fn new(repository: &Path, workspace: &Path) -> Self {
        let bundled = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|parent| parent.join("semweft")));
        let development = !bundled.as_ref().is_some_and(|path| path.is_file())
            && repository.join("Cargo.toml").is_file();
        let engine = std::env::var_os("SEMWEFT_ENGINE")
            .map(PathBuf::from)
            .or_else(|| bundled.filter(|path| path.is_file()))
            .unwrap_or_else(|| {
                let release = repository.join("target/release/semweft");
                if release.is_file() {
                    release
                } else {
                    repository.join("target/debug/semweft")
                }
            });
        Self(Arc::new(Connection {
            engine,
            repository: repository.into(),
            workspace: workspace.into(),
            development,
            connection: Mutex::new(None),
        }))
    }

    fn endpoint(&self) -> Result<DesktopEndpoint> {
        let mut connection = self.0.connection.lock().unwrap();
        if let Some((endpoint, stream)) = connection.as_mut() {
            stream.set_nonblocking(true)?;
            let alive = matches!(stream.read(&mut [0]), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock);
            stream.set_nonblocking(false)?;
            if alive {
                return Ok(endpoint.clone());
            }
        }
        let mut command = Command::new(&self.0.engine);
        if self.0.development {
            command
                .env(
                    "SEMWEFT_CACHE_DIR",
                    self.0.repository.join("target/desktop-preview/cache"),
                )
                .env(
                    "SEMWEFT_CONFIG_DIR",
                    self.0.repository.join("target/desktop-preview/config"),
                );
        }
        let output = command
            .arg("--workspace")
            .arg(&self.0.workspace)
            .args(["--format", "json", "__desktop"])
            .output()
            .context("无法启动本地分析服务")?;
        ensure!(
            output.status.success(),
            "无法启动分析服务：{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let endpoint: DesktopEndpoint = serde_json::from_slice(&output.stdout)?;
        ensure!(
            endpoint.protocol == DAEMON_PROTOCOL,
            "daemon_incompatible: 桌面与引擎版本不兼容"
        );
        let mut stream = socket(&endpoint)?;
        exchange(&mut stream, &endpoint, "__lease", json!({}), None)?;
        *connection = Some((endpoint.clone(), stream));
        Ok(endpoint)
    }

    pub fn call(&self, tool: &str, args: Value, cancellation: &Cancellation) -> Result<Value> {
        cancellation.check()?;
        let endpoint = self.endpoint()?;
        let id = format!(
            "desktop-{}-{}",
            std::process::id(),
            NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
        );
        cancellation
            .0
            .active
            .lock()
            .unwrap()
            .insert(id.clone(), endpoint.clone());
        let result = cancellation
            .check()
            .and_then(|()| request(&endpoint, tool, args, Some(id.clone())));
        cancellation.0.active.lock().unwrap().remove(&id);
        cancellation.check()?;
        result
    }

    pub fn page(
        &self,
        tool: &str,
        args: Value,
        snapshot: &str,
        epoch: u64,
        cancellation: &Cancellation,
    ) -> Result<Value> {
        let value = self.call(tool, args, cancellation)?;
        ensure!(
            value["snapshot_id"].as_str() == Some(snapshot),
            "引擎返回了不同的快照"
        );
        ensure!(
            value["epochs"]["semantic"].as_u64() == Some(epoch),
            "引擎返回了不同的语义版本"
        );
        Ok(value)
    }

    pub fn collect(
        &self,
        tool: &str,
        args: Value,
        fields: &[&str],
        cancellation: &Cancellation,
    ) -> Result<Value> {
        let mut result = self.call(tool, args, cancellation)?;
        let snapshot = result["snapshot_id"]
            .as_str()
            .context("缺少快照")?
            .to_owned();
        let epoch = result["epochs"]["semantic"]
            .as_u64()
            .context("缺少语义版本")?;
        let mut seen = BTreeSet::new();
        while let Some(cursor) = result["output"]["continuation"]["next_cursor"]
            .as_str()
            .map(str::to_owned)
        {
            ensure!(seen.insert(cursor.clone()), "引擎重复了分页游标");
            let page = self.page(
                tool,
                json!({"cursor":cursor,"timeout_ms":60000}),
                &snapshot,
                epoch,
                cancellation,
            )?;
            merge_page(&mut result, page, fields);
        }
        Ok(result)
    }

    pub fn sources(
        &self,
        snapshot: &str,
        epoch: u64,
        cancellation: &Cancellation,
    ) -> Result<BTreeMap<String, String>> {
        let mut offset = 0;
        let mut sources = BTreeMap::new();
        loop {
            let value = self.page(
                "__desktop_sources",
                json!({"snapshot_id":snapshot,"semantic_epoch":epoch,"offset":offset,"limit":32}),
                snapshot,
                epoch,
                cancellation,
            )?;
            for (path, file) in value["files"].as_object().context("缺少固定源码")? {
                let content = file["content"].as_str().context("无效源码")?;
                ensure!(
                    file["file_hash"] == semweft_shared::digest(content),
                    "content_mismatch: 捕获源码哈希不一致"
                );
                sources.insert(path.clone(), content.into());
            }
            let Some(next) = value["next_offset"].as_u64() else {
                break;
            };
            ensure!(next > offset, "无效源码分页");
            offset = next;
        }
        Ok(sources)
    }

    pub fn capture(&self, example: bool, cancellation: &Cancellation) -> Result<Preview> {
        self.capture_bound(example, None, cancellation)
    }

    pub fn capture_review(
        &self,
        id: &str,
        cancellation: &Cancellation,
    ) -> Result<(Preview, Value)> {
        let result = self.call(
            "get_review_result",
            json!({"review_id":id,"timeout_ms":60000}),
            cancellation,
        )?;
        let review = result["review"].clone();
        ensure!(review["kind"] == "request", "此 ID 不是审查请求");
        let preview = self.capture_bound(false, Some(review["report"].clone()), cancellation)?;
        Ok((preview, review))
    }

    fn capture_bound(
        &self,
        example: bool,
        report: Option<Value>,
        cancellation: &Cancellation,
    ) -> Result<Preview> {
        cancellation.stage("1/6 正在捕获固定快照…")?;
        let status = if let Some(report) = &report {
            report.clone()
        } else {
            self.call(
                "index",
                json!({"syntax_only":true,"timeout_ms":60000}),
                cancellation,
            )?
        };
        let snapshot = status["snapshot_id"].as_str().context("缺少索引快照")?;
        let epoch = status["epochs"]["semantic"]
            .as_u64()
            .context("缺少索引版本")?;
        let pinned =
            json!({"snapshot_id":snapshot,"semantic_epoch":epoch,"limit":1000,"timeout_ms":60000});
        let mut search = pinned.clone();
        search["query"] = json!("::");
        cancellation.stage("2/6 正在读取符号…")?;
        let symbols = self.collect("search_symbols", search, &["symbols"], cancellation)?;
        let mut map_args = pinned.clone();
        map_args["depth"] = json!(10);
        map_args["max_symbols_per_module"] = json!(0);
        cancellation.stage("3/6 正在读取模块与依赖…")?;
        let map = self.collect(
            "repo_map",
            map_args.clone(),
            &["modules", "dependencies", "entries"],
            cancellation,
        )?;
        map_args["dependencies"] = json!("module");
        let module_map = self.collect(
            "repo_map",
            map_args,
            &["modules", "dependencies", "entries"],
            cancellation,
        )?;
        let context = if report.is_some() {
            json!({"churn":{},"churn_scope":"审查固定版本未采集 Git 热点"})
        } else {
            self.call(
                "__desktop_context",
                json!({"timeout_ms": 20000}),
                cancellation,
            )?
        };
        let mut impact_error = None;
        cancellation.stage("4/6 正在分析变更影响…")?;
        let impact = if let Some(report) = report {
            report
        } else if context["diff"].is_object() {
            let mut diff = context["diff"].clone();
            diff["head"] = json!({"kind":"snapshot","snapshot_id":snapshot,"semantic_epoch":epoch});
            self.collect(
                "impact_of",
                json!({"diff":diff,"depth":5,"max_nodes":5000,"limit":1000,"timeout_ms":60000}),
                &[
                    "relations",
                    "relation_changes.added",
                    "relation_changes.removed",
                    "affected_modules",
                    "api_changes",
                    "affected_services",
                ],
                cancellation,
            )
            .unwrap_or_else(|error| {
                impact_error = Some(format!("{error:#}"));
                json!({})
            })
        } else {
            impact_error = context["git_error"].as_str().map(str::to_owned);
            json!({})
        };
        cancellation.check()?;
        if let Some(head) = impact["snapshots"]["head"]["snapshot_id"].as_str() {
            ensure!(head == snapshot, "工作区输入已变化，请重新刷新");
            ensure!(
                impact["snapshots"]["head"]["epochs"]["semantic"].as_u64() == Some(epoch),
                "语义版本已变化，请重新刷新"
            );
        }
        cancellation.stage("5/6 正在读取 Head 固定源码…")?;
        let sources = self.sources(snapshot, epoch, cancellation)?;
        cancellation.stage("6/6 正在读取 Base 固定源码与历史…")?;
        let base_sources = if let Some(base) = impact["snapshots"]["base"]["snapshot_id"].as_str() {
            self.sources(
                base,
                impact["snapshots"]["base"]["epochs"]["semantic"]
                    .as_u64()
                    .unwrap_or(0),
                cancellation,
            )?
        } else {
            BTreeMap::new()
        };
        let base_symbols = if let Some(base) = impact["snapshots"]["base"]["snapshot_id"].as_str() {
            self.collect("search_symbols", json!({"query":"::","snapshot_id":base,"semantic_epoch":impact["snapshots"]["base"]["epochs"]["semantic"],"limit":1000,"timeout_ms":60000}), &["symbols"], cancellation)?
        } else {
            json!({})
        };
        let binding = json!({"head":snapshot,"epoch":epoch,"snapshots":impact["snapshots"]});
        let raw = json!({"name":self.0.workspace.file_name().unwrap_or_default().to_string_lossy(),"example":example,
            "workspace":self.0.workspace,"generated_at":format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()),
            "review_key":semweft_shared::digest_json(&binding)?,"symbols":symbols,"map":map,"module_map":module_map,
            "status":status,"impact":impact,"impact_error":impact_error,"sources":sources,"base_sources":base_sources,
            "base_symbols":base_symbols,
            "branch":context["branch"],"churn":context["churn"],"git_error":context["git_error"],
            "churn_scope":context["churn_scope"].as_str().unwrap_or("最近 90 天、最多 200 条提交；按当前路径统计，未追踪重命名")});
        let mut preview: Preview = serde_json::from_value(raw.clone())?;
        preview.raw = raw;
        Ok(preview)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Navigation {
    pub target: Symbol,
    pub nodes: Vec<Symbol>,
    pub relations: Vec<Relation>,
    pub output: Value,
}

pub struct Query {
    pub symbol: String,
    pub snapshot: String,
    pub epoch: u64,
    pub direction: String,
    pub depth: usize,
    pub references: bool,
}

#[derive(Clone)]
pub struct PositionQuery {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub file_hash: String,
    pub snapshot: String,
    pub epoch: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Definition {
    #[serde(flatten)]
    pub symbol: Symbol,
    pub certainty: String,
    pub evidence: Vec<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Definitions {
    pub symbols: Vec<Definition>,
    pub occurrence: Value,
    pub completeness: Value,
    pub output: Value,
}

impl Definitions {
    pub fn append(&mut self, page: Self) {
        self.symbols.extend(page.symbols);
        self.output = page.output;
        self.deduplicate();
    }

    fn deduplicate(&mut self) {
        let mut candidates = BTreeMap::<String, Definition>::new();
        for candidate in std::mem::take(&mut self.symbols) {
            let existing = candidates
                .entry(candidate.symbol.symbol_id.clone())
                .or_insert_with(|| candidate.clone());
            if candidate.certainty == "definite" {
                existing.certainty = candidate.certainty;
            }
            for evidence in candidate.evidence {
                if !existing.evidence.contains(&evidence) {
                    existing.evidence.push(evidence);
                }
            }
        }
        self.symbols = candidates.into_values().collect();
    }
}

impl Client {
    pub fn definitions_page(
        &self,
        query: &PositionQuery,
        cursor: Option<String>,
        cancellation: &Cancellation,
    ) -> Result<Definitions> {
        ensure!(!query.file_hash.is_empty(), "缺少捕获源码的内容哈希");
        let args = if let Some(cursor) = cursor {
            json!({"cursor":cursor,"timeout_ms":60000})
        } else {
            json!({"file":query.file,"line":query.line,"column":query.column,"file_hash":query.file_hash,"snapshot_id":query.snapshot,"semantic_epoch":query.epoch,"limit":100,"timeout_ms":60000})
        };
        let mut result: Definitions = serde_json::from_value(self.page(
            "symbol_at",
            args,
            &query.snapshot,
            query.epoch,
            cancellation,
        )?)?;
        result.deduplicate();
        Ok(result)
    }

    pub fn definitions(
        &self,
        query: &PositionQuery,
        cancellation: &Cancellation,
    ) -> Result<Definitions> {
        ensure!(!query.file_hash.is_empty(), "缺少捕获源码的内容哈希");
        let value = self.collect("symbol_at", json!({"file":query.file,"line":query.line,"column":query.column,"file_hash":query.file_hash,
            "snapshot_id":query.snapshot,"semantic_epoch":query.epoch,"limit":1000,"timeout_ms":60000}), &["symbols"], cancellation)?;
        ensure!(
            value["snapshot_id"] == query.snapshot && value["epochs"]["semantic"] == query.epoch,
            "定义版本不一致"
        );
        let mut result: Definitions = serde_json::from_value(value)?;
        result.deduplicate();
        Ok(result)
    }

    pub fn navigate_page(
        &self,
        query: &Query,
        cursor: Option<String>,
        cancellation: &Cancellation,
    ) -> Result<Navigation> {
        let args = if let Some(cursor) = cursor {
            json!({"cursor":cursor,"timeout_ms":60000})
        } else {
            let mut args = json!({"symbol":query.symbol,"snapshot_id":query.snapshot,"semantic_epoch":query.epoch,"limit":100,"timeout_ms":60000});
            if !query.references {
                args["direction"] = json!(query.direction);
                args["depth"] = json!(query.depth);
                args["max_nodes"] = json!(5000);
            }
            args
        };
        let tool = if query.references {
            "find_references"
        } else {
            "call_graph"
        };
        Ok(serde_json::from_value(self.page(
            tool,
            args,
            &query.snapshot,
            query.epoch,
            cancellation,
        )?)?)
    }
}

pub fn definitions(
    repository: &Path,
    workspace: &Path,
    query: &PositionQuery,
) -> Result<Definitions> {
    Client::new(repository, workspace).definitions(query, &Cancellation::default())
}

pub fn navigate(repository: &Path, workspace: &Path, query: &Query) -> Result<Navigation> {
    let client = Client::new(repository, workspace);
    let cancellation = Cancellation::default();
    let mut result = client.navigate_page(query, None, &cancellation)?;
    let mut seen = BTreeSet::new();
    while let Some(cursor) = result.output["continuation"]["next_cursor"]
        .as_str()
        .map(str::to_owned)
    {
        ensure!(seen.insert(cursor.clone()), "重复分页游标");
        let page = client.navigate_page(query, Some(cursor), &cancellation)?;
        result.relations.extend(page.relations);
        let mut ids = result
            .nodes
            .iter()
            .map(|node| node.symbol_id.clone())
            .collect::<BTreeSet<_>>();
        result.nodes.extend(
            page.nodes
                .into_iter()
                .filter(|node| ids.insert(node.symbol_id.clone())),
        );
        result.output = page.output;
    }
    Ok(result)
}

pub fn check_service(repository: &Path, data: &Preview) -> Result<()> {
    let client = Client::new(repository, &data.workspace);
    let cancellation = Cancellation::default();
    let endpoint = client.endpoint()?;
    ensure!(
        endpoint.workspace.canonicalize()? == data.workspace.canonicalize()?,
        "服务工作区错误"
    );
    for side in ["head", "base"] {
        let symbols = data.symbols_on(side);
        if symbols.snapshot_id.is_empty() {
            continue;
        }
        let epoch = symbols.epochs["semantic"].as_u64().context("缺少 epoch")?;
        ensure!(
            client.sources(&symbols.snapshot_id, epoch, &cancellation)? == *data.sources_on(side),
            "固定源码不同"
        );
        let symbol = symbols
            .symbols
            .iter()
            .find(|symbol| symbol.kind == "function")
            .context("缺少函数")?;
        let location = &symbol.location;
        let definitions = client.definitions(
            &PositionQuery {
                file: location.file.clone(),
                line: location.line,
                column: location.column,
                file_hash: location.file_hash.clone(),
                snapshot: symbols.snapshot_id.clone(),
                epoch,
            },
            &cancellation,
        )?;
        ensure!(!definitions.symbols.is_empty(), "缺少定义候选");
        for definition in definitions.symbols {
            data.verify_location_on(&definition.symbol.location, side)?;
        }
        println!("{side}: historical source and definition identity verified");
    }
    if let Some(diff) = data.fixed_diff() {
        for tool in ["verify_change", "suggest_tests", "arch_check"] {
            let mut args = json!({"diff":diff,"limit":1,"timeout_ms":60000});
            if tool != "arch_check" {
                args["depth"] = json!(5);
                args["max_nodes"] = json!(5000);
            }
            let report = client.collect(
                tool,
                args,
                &[
                    "findings",
                    "test_candidates",
                    "relations",
                    "relation_changes.added",
                    "relation_changes.removed",
                    "affected_modules",
                    "api_changes",
                    "affected_services",
                ],
                &cancellation,
            )?;
            ensure!(
                report["snapshot_id"] == data.symbols.snapshot_id,
                "分析快照不同"
            );
            if let Some(count) = report["verification"]["finding_count"].as_u64() {
                ensure!(
                    report["findings"].as_array().map(Vec::len) == Some(count as usize),
                    "遗漏发现分页"
                );
            }
            println!("{tool}: all fixed-view pages verified");
            if tool == "verify_change" {
                let mut review = client.call("__review_request", json!({"action":"create","report":report,"title":"Native service review check"}), &cancellation)?;
                ensure!(review["state"] == "pending", "新请求状态错误");
                let id = review["review_id"]
                    .as_str()
                    .context("缺少审查 ID")?
                    .to_owned();
                let (captured, reopened) = client.capture_review(&id, &cancellation)?;
                ensure!(
                    captured.sources == data.sources
                        && captured.base_sources == data.base_sources
                        && reopened["report_digest"] == review["report_digest"],
                    "按请求打开了不同源码"
                );
                for item in review["items"].as_array().cloned().unwrap_or_default() {
                    review = client.call("__review_request", json!({"action":"decide","review_id":id,"revision":review["revision"],"item_id":item["item_id"],"conclusion":"accept","reviewer":"native service regression","note":"Diagnostic fixture only"}), &cancellation)?;
                }
                review = client.call("__review_request", json!({"action":"decide","review_id":id,"revision":review["revision"],"conclusion":"approve","reviewer":"native service regression","note":"Diagnostic fixture only; not behavioral proof"}), &cancellation)?;
                ensure!(review["state"] == "approved", "请求没有完成");
                let latest =
                    client.call("get_review_result", json!({"review_id":id}), &cancellation)?;
                ensure!(
                    latest["review"]["revision"] == review["revision"],
                    "审查意见未回传"
                );
                println!(
                    "Review request: fixed source reopening, finding decisions, revision and agent retrieval verified"
                );
            }
        }
    }
    let id = format!("desktop-check-cancel-{}", std::process::id());
    request(&endpoint, "__cancel", json!({"id":id}), None)?;
    ensure!(
        request(&endpoint, "status", json!({}), Some(id))
            .unwrap_err()
            .to_string()
            .contains("cancelled"),
        "取消信号未生效"
    );
    let clone = client.clone();
    ensure!(
        clone.endpoint()?.endpoint == endpoint.endpoint,
        "服务连接未复用"
    );
    println!("Resident lease reuse and cancellation verified");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_pages_merge_evidence_and_keep_definite_bindings() {
        let mut first: Definitions = serde_json::from_value(json!({"symbols":[{"symbol_id":"a","certainty":"candidate","evidence":[{"source":"syntax"}]}],"occurrence":{"name":"same"}})).unwrap();
        let second = serde_json::from_value(json!({"symbols":[{"symbol_id":"a","certainty":"definite","evidence":[{"source":"lsp"}]},{"symbol_id":"b","certainty":"candidate"}],"output":{"continuation":{}}})).unwrap();
        first.append(second);
        assert_eq!(first.symbols.len(), 2);
        assert_eq!(first.symbols[0].certainty, "definite");
        assert_eq!(first.symbols[0].evidence.len(), 2);
        assert_eq!(first.symbols[1].certainty, "candidate");
        assert_eq!(first.occurrence["name"], "same");
    }

    #[test]
    fn report_merge_preserves_nested_evidence_and_latest_continuation() {
        let mut report = json!({"findings":[1],"relation_changes":{"added":[2],"removed":[]},"verification":{"finding_count":2},"output":{"continuation":{"next_cursor":"next"}}});
        merge_page(
            &mut report,
            json!({"findings":[3],"relation_changes":{"added":[4],"removed":[5]},"output":{"truncated":true,"truncation_reason":"node_limit","continuation":{}}}),
            &[
                "findings",
                "relation_changes.added",
                "relation_changes.removed",
            ],
        );
        assert_eq!(report["findings"], json!([1, 3]));
        assert_eq!(report["relation_changes"]["added"], json!([2, 4]));
        assert_eq!(report["relation_changes"]["removed"], json!([5]));
        assert_eq!(report["verification"]["finding_count"], 2);
        assert_eq!(report["output"]["truncation_reason"], "node_limit");
        assert!(report["output"]["continuation"]["next_cursor"].is_null());
    }

    #[test]
    fn cancellation_is_shared_and_rejects_before_engine_start() {
        let cancellation = Cancellation::default();
        let clone = cancellation.clone();
        cancellation.cancel();
        assert!(clone.check().is_err());
        assert!(
            Client::new(
                Path::new("/missing-engine"),
                Path::new("/missing-workspace")
            )
            .call("status", json!({}), &clone)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
        );
    }
}
