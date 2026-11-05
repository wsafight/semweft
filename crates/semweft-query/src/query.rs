use crate::sync::MutexExt;
use crate::{
    engine::{Engine, StoredView},
    git,
    graph_page::GraphRelations,
    report_page::ReportList,
    suggestions, verify,
};
use anyhow::{Context, Result, bail};
use semweft_core::*;
use semweft_store::Direction;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
thread_local! {static REQUEST_CANCEL:RefCell<Option<Arc<AtomicBool>>>=const {RefCell::new(None)};}
thread_local! {static REQUEST_DEADLINE:RefCell<Option<Instant>>=const {RefCell::new(None)};}
pub fn bound_deadline(deadline: Instant) -> Instant {
    REQUEST_DEADLINE.with(|current| {
        current
            .borrow()
            .map_or(deadline, |request| deadline.min(request))
    })
}
pub fn check_cancelled() -> Result<()> {
    if REQUEST_CANCEL.with(|flag| {
        flag.borrow()
            .as_ref()
            .is_some_and(|f| f.load(Ordering::SeqCst))
    }) {
        bail!("cancelled: client cancelled this request");
    }
    Ok(())
}

pub(crate) fn check_deadline(deadline: Instant) -> Result<()> {
    check_cancelled()?;
    if Instant::now() >= deadline {
        bail!("timeout: query computation exceeded timeout_ms");
    }
    Ok(())
}

fn finish_computation(result: &mut Value, args: &Value, computed: Result<()>) -> Result<()> {
    if let Err(error) = computed {
        if error_value(&error)["error"]["code"] != "timeout"
            || ![
                "symbols",
                "modules",
                "entries",
                "dependencies",
                "core_symbols",
            ]
            .iter()
            .any(|key| {
                result[key]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
            })
        {
            return Err(error);
        }
        mark_timeout(result, args);
    }
    Ok(())
}

pub(crate) fn fixed_requery(result: &Value, args: &Value) -> Value {
    let mut suggested = args.clone();
    suggested.as_object_mut().unwrap().remove("cursor");
    if result.get("snapshots").is_some() {
        for side in ["base", "head"] {
            suggested["diff"][side] = json!({"kind":"snapshot",
                "snapshot_id":result["snapshots"][side]["snapshot_id"],
                "semantic_epoch":result["snapshots"][side]["epochs"]["semantic"]});
            if result["snapshots"][side]["commit"].is_string()
                && result["snapshots"][side]["commit"]
                    != result["snapshots"][side]["analysis_commit"]
            {
                suggested["diff"][side]["commit_context"] =
                    result["snapshots"][side]["commit"].clone();
            }
        }
    } else {
        suggested["snapshot_id"] = result["snapshot_id"].clone();
        suggested["semantic_epoch"] = result["epochs"]["semantic"].clone();
    }
    suggested["depth"] = json!((args["depth"].as_u64().unwrap_or(3) + 1).min(10));
    suggested["max_nodes"] = json!((args["max_nodes"].as_u64().unwrap_or(1000) * 2).min(100000));
    if result["output"]["truncation_reason"] == "timeout" {
        suggested["timeout_ms"] =
            json!((args["timeout_ms"].as_u64().unwrap_or(5000) * 2).min(60000));
    }
    suggested
}

fn mark_timeout(result: &mut Value, args: &Value) {
    result["evidence_summary"]["basis"] = json!("partial");
    result["evidence_summary"]["exact"] = json!(false);
    result["output"]["truncated"] = json!(true);
    result["output"]["truncation_reason"] = json!("timeout");
    result["output"]["remaining_relations"] = Value::Null;
    result["output"]["remaining_items"] = Value::Null;
    result["output"]["continuation"]["kind"] = json!("requery");
    result["output"]["continuation"]["suggested"] = json!({
        "snapshot_id":result["snapshot_id"],"semantic_epoch":result["epochs"]["semantic"],
        "timeout_ms":(args["timeout_ms"].as_u64().unwrap_or(5000)*2).min(60000)});
}
struct CancelGuard;
impl Drop for CancelGuard {
    fn drop(&mut self) {
        REQUEST_CANCEL.with(|flag| *flag.borrow_mut() = None);
        REQUEST_DEADLINE.with(|current| *current.borrow_mut() = None);
    }
}

#[cfg(test)]
use semweft_protocol::TOOL_NAMES;
pub use semweft_protocol::{MIN_BUDGET, error_value, failure, tool_schema, tools, validate};

pub fn capabilities() -> Value {
    json!({"schema_version":"0.1","capabilities_version":"0.1-p0","stage":"mvp_experimental","tools":tools(),
        "languages":{"rust":{"L1":{"status":"experimental","indexer":semweft_lang_rust::PARSER_VERSION,"binding":"name candidates; lexical scope unresolved"},"L2":{"status":"unsupported"},"batch_L3_scip":{"status":"experimental","format":"binary SCIP 0.10","source_manifest_required":true,"requires_build":false,"requires_trust":false,"producer":"external index with tool name and version; producer execution is outside import"},"resident_L3":{"status":"experimental","backend":"external rust-analyzer LSP","queries":["definition","implementation","references","diagnostic"],"requires_build":true,"requires_trust":true,"tool":"rust-analyzer","tool_available":tool_available(),"authorization":"semweft trust --accept-build-execution"}},"go":go_capabilities(),"typescript_javascript":web_capabilities()},
        "rules":verify::rules(),"tokenizers":[{"id":"estimated","mode":"estimated","method":"one token per UTF-8 byte of compact serialized JSON, including metadata; conservative estimate","min_budget":MIN_BUDGET},
            {"id":crate::tokens::CL100K,"mode":"exact","method":"offline BPE over complete compact JSON","min_budget":MIN_BUDGET},
            {"id":crate::tokens::O200K,"mode":"exact","method":"offline BPE over complete compact JSON","min_budget":MIN_BUDGET}],
        "limits":{"timeout_ms":5000,"max_timeout_ms":60000,"fresh_wait_ms":2000,"max_fresh_wait_fraction":0.5,"limit":100,"max_limit":1000,"depth":3,"max_depth":10,"max_nodes":100000,"dependency_depth":"nonnegative u32; offline Cargo resolution requires repository trust","snapshot_retention":"unlimited by default; configurable retention with durable pins and safe GC","max_cursors":256,"default_max_cursor_bytes":67108864,"cursor_limits_configurable":true},
        "path_queries":{"status":"experimental","endpoints":["indexed_symbol","source_file","configured_service"],"relations":["call"],"cycle_policy":"simple paths","max_depth":10,"max_paths":1000,"module_service_mapping":"explicit captured ownership; aggregate paths are not executable call chains"},
        "test_suggestions":{"status":"experimental","inputs":["symbol","diff"],"method":"reverse indexed call/reference paths","entrypoints":["Rust #[test]","Rust #[tokio::test]","Rust #[async_std::test]"],"coverage":"unavailable","executes_tests":false,"base_only_candidates":true},
        "risk_scoring":{"status":"experimental","inputs":["diff","optional base/head risk_inputs artifacts"],"model":crate::risk_inputs::MODEL,"calibrated":false,"coverage":"source-bound LCOV observations; no test execution or per-test attribution","historical_hotspots":"explicit frozen Git non-merge commit windows"},
        "public_api":{"method":"search_symbols api_scope=external","external_reachability":"candidate","reexports_expanded":true,"languages":["rust"],"semantic_validation":false},
        "cycle_detection":{"scope":["crate","source_file","configured_service"],"algorithm":"strongly connected components","service_mapping":true},
        "architecture":{"rule":"architecture_layer_direction","version":crate::architecture::RULE_VERSION,"scope":"indexed file-level dependencies","baseline":true},
        "agent_guards":{"tools":["agent_preflight","agent_verify"],"protection":"captured base/head policies; whole-file conservative symbol scope","compiler_and_tests_executed":false},
        "hooks":{"hosts":["claude","codex"],"events":["PreToolUse","PostToolUse"],"tool_input_scope":"explicit file edits and apply_patch headers; not shell writes","host_end_to_end_validated":false},
        "reviews":{"storage":"local immutable content-bound records and revision-checked decision events","requests":true,"finding_decisions":true,"source_annotations":true,"wait":"CLI polling with bounded timeout","approval_invalidated_by":["snapshot","semantic_epoch","rule_versions","later_decision"],"signed":false},
        "contracts":{"status":"experimental","formats":["protobuf"],"parser":"protox-parse@0.10.0","mapping":"explicit configuration; candidate only","rules":crate::contracts::rules(),"unmodeled":["thrift","openapi","enum compatibility","descriptor linking","runtime consumers"]},
        "ci_reports":{"formats":["json","markdown","sarif_2.1.0","html"],"sarif_scope":"verification findings","all_pages_commands":["verify","tests","impact","arch-check","guard","contract-impact","map","search","calls","path","refs","impls"],"offline_render":"report --input","executes_tests":false},
        "unsupported":["HTTP MCP","Thrift","OpenAPI"],"commands":["benchmark-storage","snapshots pin","snapshots unpin","snapshots gc","snapshots usage","snapshots compact","init","trust","index","doctor","capabilities","status","map","search","symbol-at","symbol","refs","impls","calls","impact","verify","path","tests","risk-inputs","arch-check","guard before","guard after","hook","review create","review open","review decide","review wait","review save","review get","contract-impact","report","context","mcp","prototype scan","prototype check"]})
}

fn go_capabilities() -> Value {
    if cfg!(feature = "go-experimental") {
        json!({"status":"experimental","formal_acceptance":"local source-reviewed samples and latency measurements available; independent review, broader build coverage and remote CI pending","L1":{"status":"experimental","binding":"syntax name candidates"},"batch_L3_scip":{"status":"experimental","source_manifest_required":true},"resident_L3":{"status":"experimental","backend":"scip-go","refresh":"full project","requires_trust":true},"build_systems":["go.mod","go.work"],"build_tags":{"config":"go.tags","environment":"frozen in snapshot; configured tags replace GOFLAGS"},"dependency_depth":0})
    } else {
        json!({"status":"unsupported","enable":"cargo install --features go-experimental"})
    }
}

fn web_capabilities() -> Value {
    json!({"status":if cfg!(feature="web-experimental") {"experimental"} else {"unsupported"},
        "formal_acceptance":"local TS and JS source-reviewed samples and latency measurements available; independent review, broader build coverage and remote CI pending",
        "enable":"--features web-experimental","L1":{"binding":"syntax candidates only","module_resolution":["relative imports","local JSONC tsconfig inheritance","paths/baseUrl candidates","literal import()/require()"],"extensions":["ts","tsx","js","jsx","mjs","cjs","mts","cts"]},
        "batch_L3_scip":{"source_manifest_required":true},"resident_L3":{"status":if cfg!(feature="web-experimental"){"experimental"}else{"unsupported"},"backend":"scip-typescript","refresh":"full project rebuild","requires_trust":true},"dependency_depth":0})
}

fn tool_available() -> bool {
    let executable =
        std::env::var_os("SEMWEFT_RUST_ANALYZER").unwrap_or_else(|| "rust-analyzer".into());
    let path = std::path::Path::new(&executable);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            dir.join(path).is_file()
                || cfg!(windows) && dir.join(format!("{}.exe", path.display())).is_file()
        })
    })
}

struct PageData {
    tool: String,
    args: Value,
    metadata: Value,
    fields: Vec<String>,
    lists: BTreeMap<String, Vec<Value>>,
    reports: BTreeMap<String, ReportList>,
    graph: Option<GraphRelations>,
    bytes: usize,
}
impl PageData {
    fn len(&self, field: &str) -> usize {
        if field == "relations"
            && let Some(graph) = &self.graph
        {
            return graph.len();
        }
        self.reports.get(field).map_or_else(
            || self.lists.get(field).map_or(0, Vec::len),
            ReportList::len,
        )
    }

    fn values(&self, field: &str, range: std::ops::Range<usize>) -> Result<Vec<Value>> {
        if field == "relations"
            && let Some(graph) = &self.graph
        {
            return graph.values(range);
        }
        if let Some(report) = self.reports.get(field) {
            return report.values(range);
        }
        Ok(self.lists.get(field).map(Vec::as_slice).unwrap_or(&[])[range].to_vec())
    }
}
#[derive(Clone)]
struct Page {
    data: Arc<PageData>,
    offset: usize,
    created: Instant,
    saved: Instant,
}
impl std::ops::Deref for Page {
    type Target = PageData;
    fn deref(&self) -> &PageData {
        &self.data
    }
}
impl Page {
    fn new(tool: String, args: Value, result: Value, fields: Vec<String>, offset: usize) -> Self {
        Self::build(tool, args, result, fields, offset, false).unwrap()
    }

    fn build(
        tool: String,
        args: Value,
        mut result: Value,
        fields: Vec<String>,
        offset: usize,
        compact: bool,
    ) -> Result<Self> {
        let mut lists: BTreeMap<_, _> = fields
            .iter()
            .map(|field| {
                let pointer = format!("/{}", field.replace('.', "/"));
                let values = result
                    .pointer_mut(&pointer)
                    .map(Value::take)
                    .and_then(|v| {
                        if let Value::Array(items) = v {
                            Some(items)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                (field.clone(), values)
            })
            .collect();
        let mut reports = BTreeMap::new();
        if compact
            && lists.values().map(Vec::len).sum::<usize>()
                > args["limit"].as_u64().unwrap_or(100) as usize
        {
            for (field, values) in &mut lists {
                if !values.is_empty() {
                    reports.insert(field.clone(), ReportList::new(std::mem::take(values))?);
                }
            }
        }
        let bytes = semweft_store::payload_bytes(&(&tool, &args, &result, &fields, &lists))
            .saturating_mul(3)
            .saturating_add(reports.values().map(ReportList::estimated_bytes).sum());
        Ok(Self {
            data: Arc::new(PageData {
                tool,
                args,
                metadata: result,
                fields,
                lists,
                reports,
                graph: None,
                bytes,
            }),
            offset,
            created: Instant::now(),
            saved: Instant::now(),
        })
    }

    fn with_graph(mut self, graph: Option<GraphRelations>) -> Self {
        if let Some(graph) = graph {
            let data = Arc::get_mut(&mut self.data).unwrap();
            data.bytes = data.bytes.saturating_add(graph.estimated_bytes());
            data.graph = Some(graph);
        }
        self
    }
}
#[derive(Default)]
struct PageCache {
    items: BTreeMap<String, Page>,
    evictions: u64,
}
impl PageCache {
    fn bytes(&self) -> usize {
        let unique: BTreeMap<_, _> = self
            .items
            .values()
            .map(|p| (Arc::as_ptr(&p.data) as usize, p.bytes))
            .collect();
        unique
            .values()
            .fold(self.items.len().saturating_mul(256), |sum, bytes| {
                sum.saturating_add(*bytes)
            })
    }
    fn trim(&mut self, max_count: usize, max_bytes: usize) {
        self.items
            .retain(|_, page| page.created.elapsed().as_secs() <= 600);
        while self.items.len() > max_count || self.bytes() > max_bytes {
            let Some(old) = self
                .items
                .iter()
                .min_by_key(|(_, p)| p.saved)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.items.remove(&old);
            self.evictions += 1;
        }
    }
    fn insert(
        &mut self,
        cursor: String,
        mut page: Page,
        max_count: usize,
        max_bytes: usize,
    ) -> Result<()> {
        self.trim(max_count, max_bytes);
        if max_count == 0 || page.bytes.saturating_add(256) > max_bytes {
            bail!(
                "cursor_expired: result exceeds configured cursor cache budget; increase index.cursor_cache_bytes or cursor_cache_count"
            );
        }
        page.saved = Instant::now();
        self.items.insert(cursor.clone(), page);
        self.trim(max_count, max_bytes);
        if !self.items.contains_key(&cursor) {
            bail!("cursor_expired: cursor cache capacity exceeded");
        }
        Ok(())
    }
}
pub struct QueryService {
    pub engine: Arc<Engine>,
    pages: Mutex<PageCache>,
    next: Mutex<u64>,
    cancellations: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
}
impl QueryService {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            pages: Mutex::new(PageCache::default()),
            next: Mutex::new(0),
            cancellations: Mutex::new(BTreeMap::new()),
        }
    }
    pub fn cancel(&self, id: &str) {
        let mut cancellations = self.cancellations.locked();
        if cancellations.len() > 256 {
            cancellations.retain(|_, flag| Arc::strong_count(flag) > 1);
        }
        cancellations
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .store(true, Ordering::SeqCst);
        self.engine.changed.notify_all();
    }
    pub fn execute_registered(
        &self,
        id: Option<&str>,
        tool: &str,
        args: Value,
        deadline: Instant,
    ) -> Result<Value> {
        let flag = id.map(|id| {
            self.cancellations
                .lock()
                .unwrap()
                .entry(id.to_owned())
                .or_insert_with(|| Arc::new(AtomicBool::new(false)))
                .clone()
        });
        REQUEST_CANCEL.with(|current| *current.borrow_mut() = flag);
        REQUEST_DEADLINE.with(|current| *current.borrow_mut() = Some(deadline));
        let _guard = CancelGuard;
        let result = self.execute(tool, args, deadline);
        if let Some(id) = id {
            self.cancellations.locked().remove(id);
        }
        result
    }
    pub fn execute(&self, tool: &str, mut args: Value, deadline: Instant) -> Result<Value> {
        check_cancelled()?;
        if tool == "index" {
            return self.engine.index(&args, deadline);
        }
        validate(tool, &args)?;
        if tool == "capabilities" {
            return Ok(capabilities());
        }
        if tool == "index_status" {
            let mut result = self.engine.status(&args)?;
            let (count, bytes) = self.page_limits();
            let mut pages = self.pages.locked();
            pages.trim(count, bytes);
            result["storage"]["cursor_cache"] = json!({"cursors":pages.items.len(),"estimated_bytes":pages.bytes(),"max_count":count,"max_bytes":bytes,"evictions":pages.evictions});
            return Ok(result);
        }
        if let Some(cursor) = args["cursor"].as_str() {
            return self.continue_page(tool, &args, cursor);
        }
        let mut graph_relations = None;
        let mut result = if matches!(
            tool,
            "impact_of"
                | "verify_change"
                | "suggest_tests"
                | "arch_check"
                | "agent_verify"
                | "contract_impact"
        ) && args.get("diff").is_some()
        {
            let impact = git::impact(self, &args, deadline)?;
            if tool == "verify_change" {
                verify::report(impact, &args)
            } else {
                impact
            }
        } else {
            if args.get("snapshot_id").is_none() {
                self.engine.scan(false)?;
            }
            let (stored, fresh) = self.engine.bind(&args, deadline)?;
            if tool == "context_pack" && args.get("tokenizer").is_none() {
                args["tokenizer"] = json!(
                    self.engine
                        .state
                        .lock()
                        .unwrap()
                        .resolved
                        .config
                        .context
                        .tokenizer
                );
            }
            if Instant::now() >= deadline {
                bail!("timeout: no query results within timeout_ms");
            }
            let compute_tool = if tool == "suggest_tests" {
                "impact_of"
            } else {
                tool
            };
            if matches!(tool, "call_graph" | "impact_of" | "suggest_tests") {
                let (result, graph) = graph_result(
                    envelope(&stored, fresh),
                    &stored,
                    &args,
                    compute_tool,
                    deadline,
                )?;
                let mut result = result;
                if tool == "suggest_tests" {
                    result = suggestions::report(self, result, &args, deadline, Some(&graph))?;
                }
                if graph.edges.len() <= args["limit"].as_u64().unwrap_or(100) as usize {
                    result["relations"] = json!(graph_values(&graph.edges, &stored));
                } else {
                    graph_relations = Some(GraphRelations::new(graph.edges, &stored)?);
                }
                result
            } else {
                self.compute(compute_tool, &args, &stored, fresh, deadline)?
            }
        };
        if tool == "suggest_tests" && args.get("diff").is_some() {
            result = suggestions::report(self, result, &args, deadline, None)?;
        }
        if tool == "agent_verify" {
            result = crate::guard::after(self, result, &args, deadline)?;
        }
        if tool == "contract_impact" {
            result = crate::contracts::report(self, result, &args, deadline)?;
        }
        if tool == "arch_check" {
            let head_metadata = if result.get("snapshots").is_some() {
                &result["snapshots"]["head"]
            } else {
                &result
            };
            let head = self.engine.historical(
                head_metadata["snapshot_id"].as_str().unwrap(),
                head_metadata["epochs"]["semantic"].as_u64(),
            )?;
            let base = result["snapshots"]["base"]["snapshot_id"]
                .as_str()
                .map(|snapshot| {
                    self.engine.historical(
                        snapshot,
                        result["snapshots"]["base"]["epochs"]["semantic"].as_u64(),
                    )
                })
                .transpose()?;
            crate::architecture::report(&mut result, &head, base.as_ref(), &args, deadline)?;
        }
        check_cancelled()?;
        if tool == "context_pack" {
            let omitted = result
                .as_object_mut()
                .unwrap()
                .remove("_all_omitted")
                .unwrap_or_else(|| json!([]));
            let shown = result["omitted"].as_array().unwrap().len();
            if omitted.as_array().unwrap().len() > shown {
                let mut page_result = result.clone();
                page_result["items"] = json!([]);
                page_result["omitted"] = omitted;
                let page = Page::new(
                    tool.into(),
                    args.clone(),
                    page_result,
                    vec!["omitted".into()],
                    shown,
                );
                let cursor = self.save_page(page)?;
                result["omitted_cursor"] = json!(cursor);
                result["output"]["continuation"]["omitted_cursor"] = json!(cursor);
            }
            count_context(&mut result)?;
            return Ok(result);
        }
        let mut fields = match tool {
            "contract_impact" => vec![
                "findings",
                "contract_changes",
                "contract_impacts",
                "relations",
                "relation_changes.added",
                "relation_changes.removed",
                "affected_modules",
            ],
            "agent_preflight" => vec!["findings", "symbols", "relations"],
            "agent_verify" => vec![
                "findings",
                "relations",
                "relation_changes.added",
                "relation_changes.removed",
                "affected_modules",
            ],
            "arch_check" if result.get("snapshots").is_some() => vec![
                "findings",
                "relations",
                "relation_changes.added",
                "relation_changes.removed",
                "affected_modules",
            ],
            "arch_check" => vec!["findings"],
            "search_symbols" | "symbol_at" => vec!["symbols"],
            "repo_map" => vec!["modules", "dependencies", "entries", "cycles"],
            "find_references" => {
                if result.get("history").is_some() {
                    vec!["relations", "history.relations"]
                } else {
                    vec!["relations"]
                }
            }
            "verify_change" => vec![
                "findings",
                "relations",
                "relation_changes.added",
                "relation_changes.removed",
                "affected_modules",
            ],
            "suggest_tests" if result.get("relation_changes").is_some() => vec![
                "test_candidates",
                "relations",
                "relation_changes.added",
                "relation_changes.removed",
                "affected_modules",
            ],
            "suggest_tests" => vec!["test_candidates", "relations"],
            "impact_of" if result.get("relation_changes").is_some() => vec![
                "relations",
                "relation_changes.added",
                "relation_changes.removed",
                "affected_modules",
            ],
            "find_paths" => vec!["paths"],
            "call_graph" | "impact_of" | "find_implementations" => vec!["relations"],
            _ => vec![],
        };
        if result.get("api_changes").is_some() {
            fields.push("api_changes");
        }
        if result.get("affected_services").is_some() {
            fields.push("affected_services");
        }
        if matches!(tool, "search_symbols" | "repo_map") && Instant::now() >= deadline {
            mark_timeout(&mut result, &args);
        }
        let mut response = self.page(
            Page::build(
                tool.into(),
                args.clone(),
                result,
                fields.into_iter().map(str::to_owned).collect(),
                0,
                args.get("diff").is_some() || tool == "suggest_tests",
            )?
            .with_graph(graph_relations),
        )?;
        if matches!(tool, "search_symbols" | "repo_map") && Instant::now() >= deadline {
            mark_timeout(&mut response, &args);
        }
        Ok(response)
    }
    fn page_limits(&self) -> (usize, usize) {
        let state = self.engine.state.locked();
        (
            state.resolved.config.index.cursor_cache_count,
            state.resolved.config.index.cursor_cache_bytes,
        )
    }
    fn continue_page(&self, tool: &str, args: &Value, cursor: &str) -> Result<Value> {
        let (max_count, max_bytes) = self.page_limits();
        let page = {
            let mut pages = self.pages.locked();
            pages.trim(max_count, max_bytes);
            pages.items.get(cursor).cloned()
        }
        .context("cursor_expired: cursor unavailable; restart query")?;
        if page.tool != tool || page.created.elapsed().as_secs() > 600 {
            bail!("cursor_expired: cursor expired or belongs to another tool");
        }
        for (key, value) in args.as_object().unwrap() {
            if ["cursor", "timeout_ms", "require_fresh", "fresh_wait_ms"].contains(&key.as_str()) {
                continue;
            }
            let expected = page
                .args
                .get(key)
                .cloned()
                .or_else(|| tool_schema(tool)["properties"][key].get("default").cloned());
            if expected.as_ref() != Some(value) {
                bail!("invalid_argument: cursor parameter {key} differs from first page");
            }
        }
        self.page(page)
    }
    fn page(&self, mut page: Page) -> Result<Value> {
        let _timing = crate::timing::Timing::new("page");
        let limit = page.args["limit"].as_u64().unwrap_or(100) as usize;
        let total: usize = page.fields.iter().map(|key| page.len(key)).sum();
        let start = page.offset;
        let mut end = (start + limit).min(total);
        let mut response = page.metadata.clone();
        let mut position = 0;
        for key in &page.fields {
            let length = page.len(key);
            let begin = start.saturating_sub(position).min(length);
            let finish = end.saturating_sub(position).min(length);
            set_list(&mut response, key, page.values(key, begin..finish)?);
            position += length;
        }
        if page.tool == "context_pack" {
            let budget = page.args["budget"].as_u64().unwrap();
            while crate::tokens::count(&response)? + 512 > budget && end > start {
                response["omitted"].as_array_mut().unwrap().pop();
                end -= 1;
            }
            if end == start && total > start {
                bail!("budget_too_small: omission metadata does not fit in this budget");
            }
        }
        let mut relation_position = 0;
        let mut remaining_relations = 0;
        for key in &page.fields {
            let length = page.len(key);
            if matches!(
                key.as_str(),
                "relations"
                    | "history.relations"
                    | "relation_changes.added"
                    | "relation_changes.removed"
            ) {
                remaining_relations += length.saturating_sub(end.saturating_sub(relation_position));
            }
            relation_position += length;
        }
        let remaining_path_relations =
            (page.tool == "find_paths").then(|| path_relation_count(&page.lists["paths"][end..]));
        if total > end {
            page.offset = end;
            let cursor = self.save_page(page)?;
            let reason = response["output"]["truncation_reason"]
                .as_str()
                .unwrap_or("page_limit")
                .to_owned();
            response["output"]["truncated"] = json!(true);
            response["output"]["truncation_reason"] = json!(reason);
            response["output"]["continuation"]["next_cursor"] = json!(cursor);
            if response["output"]["continuation"]["kind"].is_null() {
                response["output"]["continuation"]["kind"] = json!("cursor");
            }
        }
        response["output"]["returned_relations"] = json!(
            list_at(&response, "relations").len()
                + list_at(&response, "history.relations").len()
                + list_at(&response, "relation_changes.added").len()
                + list_at(&response, "relation_changes.removed").len()
        );
        if response["evidence_summary"]["exact"] == true {
            response["output"]["remaining_relations"] = json!(remaining_relations);
        }
        response["output"]["returned_items"] = json!(end.saturating_sub(start));
        response["output"]["remaining_items"] =
            if response["output"]["truncation_reason"] == "timeout" {
                Value::Null
            } else {
                json!(total.saturating_sub(end))
            };
        if let Some(remaining) = remaining_path_relations {
            let paths = list_at(&response, "paths");
            let returned = paths.len();
            let relations = path_relation_count(paths);
            response["output"]["returned_paths"] = json!(returned);
            response["output"]["returned_relations"] = json!(relations);
            let complete = response["path_summary"]["enumeration_complete"] == true;
            response["output"]["remaining_paths"] = if complete {
                json!(total.saturating_sub(end))
            } else {
                Value::Null
            };
            response["output"]["remaining_relations"] = if complete {
                json!(remaining)
            } else {
                Value::Null
            };
        }
        if response.get("history").is_some() {
            response["history"]["output"] = json!({"truncated":total>end,"returned_relations":list_at(&response,"history.relations").len(),"continuation":response["output"]["continuation"]});
        }
        if response.get("token_count").is_some() {
            count_context(&mut response)?;
        }
        Ok(response)
    }

    fn save_page(&self, page: Page) -> Result<String> {
        let (max_count, max_bytes) = self.page_limits();
        let mut next = self.next.locked();
        *next += 1;
        let cursor = identity(
            "cur",
            &[
                &std::process::id().to_string(),
                &next.to_string(),
                &page.metadata["snapshot_id"].to_string(),
            ],
        );
        self.pages
            .lock()
            .unwrap()
            .insert(cursor.clone(), page, max_count, max_bytes)?;
        Ok(cursor)
    }

    pub fn compute(
        &self,
        tool: &str,
        args: &Value,
        stored: &StoredView,
        fresh: Value,
        deadline: Instant,
    ) -> Result<Value> {
        let mut result = envelope(stored, fresh);
        match tool {
            "get_review_result" => {
                let resolved = self.engine.state.locked().resolved.clone();
                result["review"] =
                    crate::reviews::get(&resolved, stored, args["review_id"].as_str().unwrap())?;
            }
            "agent_preflight" => {
                return crate::guard::before(
                    stored,
                    result["fresh_requirement"].clone(),
                    args,
                    deadline,
                );
            }
            "arch_check" => {}
            "search_symbols" => {
                let query = args["query"].as_str().unwrap();
                let mode = args["match"].as_str().unwrap_or("fuzzy");
                let api = (args["api_scope"] == "external")
                    .then(|| stored.api_surface(deadline))
                    .transpose()?;
                let mut matches = BTreeMap::new();
                let computed = (|| -> Result<()> {
                    for s in &stored.view.snapshot.syntax.symbols {
                        check_deadline(deadline)?;
                        let exports = api.as_ref().and_then(|api| api.by_symbol.get(&s.symbol_id));
                        let alias_match = exports.is_some_and(|exports| {
                            exports.iter().any(|export| {
                                let path = export["path"].as_str().unwrap();
                                match mode {
                                    "exact" => {
                                        path == query || path.rsplit("::").next() == Some(query)
                                    }
                                    "prefix" => path.starts_with(query),
                                    _ => fuzzy(path, query),
                                }
                            })
                        });
                        if prefix(&s.file, args)
                            && kind_matches(s, args["kind"].as_str())
                            && (api.is_none() || exports.is_some())
                            && (api.is_some()
                                || args["visibility"].as_str().is_none_or(|visibility| {
                                    visibility == "all"
                                        || symbol_value(s, stored, false, false)["visibility"]
                                            == visibility
                                }))
                            && (query == "*"
                                || alias_match
                                || match mode {
                                    "exact" => {
                                        s.name == query
                                            || qualified_name(s, stored) == query
                                            || public_name(s) == query
                                            || s.qualified_name == query
                                    }
                                    "prefix" => {
                                        s.name.starts_with(query)
                                            || qualified_name(s, stored).starts_with(query)
                                            || public_name(s).starts_with(query)
                                    }
                                    _ => fuzzy(
                                        &format!("{} {}", qualified_name(s, stored), s.file),
                                        query,
                                    ),
                                })
                        {
                            let key = (
                                s.name != query,
                                public_name(s),
                                &s.file,
                                s.name_span,
                                &s.symbol_id,
                            );
                            let mut value = symbol_value(s, stored, false, true);
                            if let Some(api) = &api {
                                value["api_exposure"] = api.exposure(s);
                            }
                            matches.insert(key, value);
                            check_deadline(deadline)?;
                        }
                    }
                    Ok(())
                })();
                result["symbols"] = Value::Array(matches.into_values().collect());
                if let Some(api) = api {
                    result["api_summary"] =
                        api.summary(result["symbols"].as_array().unwrap().len());
                } else if args["visibility"] == "public" {
                    result["api_summary"] = json!({"declaration_count":result["symbols"].as_array().unwrap().len(),
                        "basis":"syntax_public_visibility","external_reachability":"not_established",
                        "includes_test_declarations":true,"reexports_expanded":false,
                        "limitations":["Private enclosing modules and types may hide public declarations", "Conditional compilation and generated declarations are not expanded", "Reexports and external consumers are not exhaustive"]});
                }
                finish_computation(&mut result, args, computed)?;
            }
            "get_symbol" => {
                let symbol = resolve(stored, args["symbol"].as_str().unwrap())?;
                result["symbol"] = symbol_value(
                    symbol,
                    stored,
                    args["include_body"].as_bool().unwrap_or(false),
                    args["include_docs"].as_bool().unwrap_or(true),
                );
            }
            "symbol_at" => symbol_at(&mut result, stored, args)?,
            "find_implementations" => {
                crate::implementations::find(&mut result, stored, args, deadline)?;
            }
            "repo_map" => {
                let computed = crate::structure::repo_map(&mut result, stored, args, deadline);
                finish_computation(&mut result, args, computed)?;
                if args["cycles"] == true {
                    let computed = crate::cycles::report(&mut result, deadline);
                    finish_computation(&mut result, args, computed)?;
                }
            }
            "find_references" => {
                let target = resolve(stored, args["symbol"].as_str().unwrap())?;
                let edges = filtered(
                    stored
                        .index()
                        .adjacent(&target.symbol_id, Direction::Incoming),
                    args,
                )
                .into_iter()
                .filter(|r| {
                    args["kinds"]
                        .as_array()
                        .is_none_or(|k| k.contains(&json!(r.kind)))
                })
                .collect::<Vec<_>>();
                result["target"] = symbol_value(target, stored, false, true);
                result["relations"] = json!(
                    edges
                        .iter()
                        .map(|r| relation_value(r, stored))
                        .collect::<Vec<_>>()
                );
                result["evidence_summary"] = summary(&edges, "complete");
                if args["include_stale"] == true {
                    let historical = self.engine.historical(
                        args["stale_from"]["snapshot_id"].as_str().unwrap(),
                        args["stale_from"]["semantic_epoch"].as_u64(),
                    )?;
                    if historical.view.epochs.syntax_epoch >= stored.view.epochs.syntax_epoch {
                        bail!(
                            "invalid_argument: stale source syntax epoch must precede the main snapshot"
                        );
                    }
                    let mut history = envelope(
                        &historical,
                        json!({"requested":false,"satisfied":historical.view.pending_units.is_empty(),"reason":null,"waited_ms":0}),
                    );
                    let matching = historical
                        .view
                        .snapshot
                        .syntax
                        .symbols
                        .iter()
                        .filter(|s| {
                            s.qualified_name == target.qualified_name && s.kind == target.kind
                        })
                        .collect::<Vec<_>>();
                    let mut old = vec![];
                    if matching.len() == 1 {
                        old = historical
                            .index()
                            .adjacent(&matching[0].symbol_id, Direction::Incoming)
                            .into_iter()
                            .filter(|r| r.freshness == "current")
                            .map(|mut r| {
                                r.freshness = "stale".into();
                                r
                            })
                            .collect();
                        let mut filters = args.clone();
                        if filters.get("freshness").is_none() {
                            filters["freshness"] = json!(["current", "pending", "stale"]);
                        }
                        old = filtered(old, &filters)
                            .into_iter()
                            .filter(|r| {
                                args["kinds"]
                                    .as_array()
                                    .is_none_or(|k| k.contains(&json!(r.kind)))
                            })
                            .collect();
                    } else {
                        history["completeness"]["known_gaps"].as_array_mut().unwrap().push(json!({"kind":"historical_symbol_not_unique","reason":"target is missing or ambiguous in the source view"}));
                        history["completeness"]["status"] = json!("partial");
                    }
                    history["relations"] = json!(
                        old.iter()
                            .map(|r| relation_value(r, &historical))
                            .collect::<Vec<_>>()
                    );
                    history["evidence_summary"] = summary(&old, "complete");
                    result["history"] = history;
                }
            }
            "call_graph" | "impact_of" => {
                let (metadata, graph) = graph_result(result, stored, args, tool, deadline)?;
                result = metadata;
                result["relations"] = json!(graph_values(&graph.edges, stored));
            }
            "find_paths" => return find_paths(result, stored, args, deadline),
            "context_pack" => return context_pack(result, stored, args, deadline),
            _ => bail!("capability_unsupported: {tool}"),
        }
        Ok(result)
    }
}

fn graph_values(edges: &[(Relation, u64, Vec<String>)], stored: &StoredView) -> Vec<Value> {
    edges.iter().map(|edge| graph_value(edge, stored)).collect()
}

pub(crate) fn graph_value(
    (relation, depth, path): &(Relation, u64, Vec<String>),
    stored: &StoredView,
) -> Value {
    let mut value = relation_value(relation, stored);
    value["depth"] = json!(depth);
    value["path"] = json!(path);
    value
}

fn graph_result(
    mut result: Value,
    stored: &StoredView,
    args: &Value,
    tool: &str,
    deadline: Instant,
) -> Result<(Value, Graph)> {
    let target = resolve(stored, args["symbol"].as_str().unwrap())?;
    let graph = traverse(
        stored,
        &target.symbol_id,
        args,
        tool == "impact_of",
        deadline,
    );
    result["target"] = symbol_value(target, stored, false, true);
    result["nodes"] = json!(
        graph
            .nodes
            .iter()
            .filter_map(|id| stored.index().symbol(id))
            .map(|symbol| symbol_value(symbol, stored, false, false))
            .collect::<Vec<_>>()
    );
    result["frontier"] = json!(graph.frontier);
    result["evidence_summary"] = summary_refs(
        graph.edges.iter().map(|(relation, _, _)| relation),
        graph.basis,
    );
    if let Some(reason) = graph.reason {
        result["output"] = json!({"truncated":true,"truncation_reason":reason,"remaining_relations":if graph.basis=="partial" {Value::Null} else {json!(0)},"continuation":{"kind":"requery","frontier":graph.frontier,"suggested":{"depth":(args["depth"].as_u64().unwrap_or(3)+1).min(10),"max_nodes":(args["max_nodes"].as_u64().unwrap_or(1000)*2).min(100000)}}});
    }
    if tool == "impact_of" {
        let callers: BTreeSet<_> = graph
            .edges
            .iter()
            .filter(|(relation, _, _)| {
                relation.kind == "call" && relation.from_symbol != target.symbol_id
            })
            .map(|(relation, _, _)| &relation.from_symbol)
            .collect();
        let direct: BTreeSet<_> = graph
            .edges
            .iter()
            .filter(|(relation, depth, _)| {
                *depth == 1 && relation.kind == "call" && relation.from_symbol != target.symbol_id
            })
            .map(|(relation, _, _)| &relation.from_symbol)
            .collect();
        result["summary"] = json!({"direct_callers":direct.len(),"transitive_callers":callers.len(),"services":null,"tests_to_run":null,"risk":null});
        result["changes"] = json!([]);
    }
    Ok((result, graph))
}

fn list_at<'a>(value: &'a Value, path: &str) -> &'a [Value] {
    let v = path.split('.').fold(value, |v, k| &v[k]);
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn path_relation_count(paths: &[Value]) -> usize {
    paths
        .iter()
        .flat_map(|path| list_at(path, "relations"))
        .filter_map(|relation| relation["relation_id"].as_str())
        .collect::<BTreeSet<_>>()
        .len()
}
fn set_list(value: &mut Value, path: &str, items: Vec<Value>) {
    if let Some((a, b)) = path.split_once('.') {
        value[a][b] = json!(items);
    } else {
        value[path] = json!(items);
    }
}
fn fuzzy(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    let mut chars = text.chars();
    query.to_lowercase().chars().all(|q| chars.any(|c| c == q))
}
fn prefix(file: &str, args: &Value) -> bool {
    args["path_prefix"].as_str().is_none_or(|p| {
        p == "." || file == p || file.starts_with(&format!("{}/", p.trim_end_matches('/')))
    })
}
fn kind_matches(symbol: &Symbol, kind: Option<&str>) -> bool {
    kind.is_none_or(|k| {
        k == symbol.kind
            || (k == "type" && ["struct", "enum", "trait", "type"].contains(&symbol.kind.as_str()))
    })
}

pub fn public_name(symbol: &Symbol) -> String {
    let local = symbol
        .qualified_name
        .strip_prefix(&format!("{}::", symbol.file))
        .unwrap_or(&symbol.qualified_name);
    if crate::web::is_source(&symbol.file) {
        return format!("{}::{local}", symbol.file);
    }
    if symbol.file.ends_with(".go") {
        return symbol
            .file
            .rsplit_once('/')
            .map(|(dir, _)| format!("{}::{local}", dir.replace('/', "::")))
            .unwrap_or_else(|| local.into());
    }
    let file = symbol
        .file
        .rsplit_once("/src/")
        .map(|(_, p)| p)
        .unwrap_or_else(|| symbol.file.strip_prefix("src/").unwrap_or(&symbol.file));
    let module = file.strip_suffix(".rs").unwrap_or(file).replace('/', "::");
    let module = module.strip_suffix("::mod").unwrap_or(&module);
    if ["lib", "main", "mod"].contains(&module) {
        local.into()
    } else {
        format!("{module}::{local}")
    }
}
pub fn resolve<'a>(stored: &'a StoredView, name: &str) -> Result<&'a Symbol> {
    let symbols = &stored.view.snapshot.syntax.symbols;
    let matches = symbols
        .iter()
        .filter(|s| {
            s.symbol_id == name
                || qualified_name(s, stored) == name
                || s.qualified_name == name
                || public_name(s) == name
                || public_name(s).ends_with(&format!("::{name}"))
                || s.name == name
        })
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(failure(
            "symbol_not_found",
            "symbol not found",
            json!({"symbol":name}),
        )),
        1 => Ok(matches[0]),
        _ => Err(failure(
            "ambiguous_symbol",
            "choose a symbol_id or qualified name",
            json!({"candidates":matches.iter().map(|s|json!({"symbol_id":s.symbol_id,"qualified_name":public_name(s),"file":s.file,"line":s.name_span.start.line})).collect::<Vec<_>>()}),
        )),
    }
}
pub(crate) fn qualified_name(symbol: &Symbol, stored: &StoredView) -> String {
    let namespace = stored
        .namespaces
        .iter()
        .filter(|(dir, _)| dir.is_empty() || symbol.file.starts_with(&format!("{dir}/")))
        .max_by_key(|(dir, _)| dir.len());
    if symbol.file.ends_with(".go")
        && let Some((dir, name)) = namespace
    {
        let local = symbol
            .qualified_name
            .strip_prefix(&format!("{}::", symbol.file))
            .unwrap_or(&symbol.name);
        let file = symbol
            .file
            .strip_prefix(&format!("{dir}/"))
            .unwrap_or(&symbol.file);
        let package = file
            .rsplit_once('/')
            .map(|(d, _)| format!("{name}/{d}"))
            .unwrap_or_else(|| name.clone());
        return format!("{package}::{local}");
    }
    namespace
        .map(|(_, name)| format!("{name}::{}", public_name(symbol)))
        .unwrap_or_else(|| public_name(symbol))
}
fn source_span(text: &str, span: Span) -> &str {
    let a = byte_offset(text, span.start).unwrap_or(0);
    let b = byte_offset(text, span.end).unwrap_or(a);
    &text[a..b]
}
pub fn symbol_value(symbol: &Symbol, stored: &StoredView, body: bool, docs: bool) -> Value {
    let text = &stored.view.snapshot.manifest.files[&symbol.file].content;
    let info = stored
        .view
        .snapshot
        .syntax
        .declarations
        .get(&symbol.symbol_id);
    let mut lines = vec![];
    if docs && info.is_none() {
        let start = byte_offset(text, symbol.span.start).unwrap_or(0);
        for line in text[..start].lines().rev() {
            let line = line.trim();
            if let Some(comment) = line
                .strip_prefix("///")
                .or_else(|| line.strip_prefix("//!"))
            {
                lines.push(comment.trim());
            } else if line.starts_with("#[") || line.is_empty() {
                continue;
            } else {
                break;
            }
        }
        lines.reverse();
    }
    json!({"symbol_id":symbol.symbol_id,"name":symbol.name,"qualified_name":qualified_name(symbol,stored),"syntax_qualified_name":symbol.qualified_name,"kind":symbol.kind,"signature":symbol.signature,"docs":if docs {Some(info.map(|i|i.docs.clone()).unwrap_or_else(||lines.join("\n")))} else {None},"body":if body {Some(source_span(text,symbol.span))} else {None},"visibility":info.map(|i|i.visibility.as_str()).unwrap_or_else(||if symbol.signature.starts_with("pub ") {"public"} else if symbol.signature.starts_with("pub(") {"restricted"} else {"private"}),"module":symbol.file,"location":{"file":symbol.file,"line":symbol.name_span.start.line,"column":symbol.name_span.start.column,"file_hash":symbol.file_hash},"span":symbol.span,"file_hash":symbol.file_hash,"scope":test_scope(symbol,stored)})
}
pub fn test_scope(symbol: &Symbol, stored: &StoredView) -> &'static str {
    if let Some(info) = stored
        .view
        .snapshot
        .syntax
        .declarations
        .get(&symbol.symbol_id)
    {
        return if info.test_only { "test" } else { "production" };
    }
    let text = &stored.view.snapshot.manifest.files[&symbol.file].content;
    let start = byte_offset(text, symbol.span.start).unwrap_or(0);
    let prefix = &text[..start];
    if symbol.file.starts_with("tests/")
        || symbol.file.contains("/tests/")
        || symbol.qualified_name.contains("::tests::")
        || prefix
            .lines()
            .rev()
            .take(3)
            .any(|l| l.contains("#[test]") || l.contains("#[cfg(test)]"))
    {
        "test"
    } else {
        "production"
    }
}
pub fn relation_value(relation: &Relation, stored: &StoredView) -> Value {
    let from = stored.index().symbol(&relation.from_symbol);
    let to = stored.index().symbol(&relation.to_symbol);
    relation_value_with_context(
        relation,
        &from
            .map(public_name)
            .unwrap_or_else(|| relation.from_symbol.clone()),
        to.map(public_name).as_deref(),
        from.map(|symbol| test_scope(symbol, stored))
            .unwrap_or("production"),
        stored
            .view
            .snapshot
            .manifest
            .files
            .get(&relation.file)
            .map(|file| file.file_hash.as_str()),
        &stored.view.snapshot.snapshot_id,
    )
}

pub(crate) fn relation_value_with_context(
    relation: &Relation,
    from_name: &str,
    to_name: Option<&str>,
    scope: &str,
    file_hash: Option<&str>,
    snapshot_id: &str,
) -> Value {
    json!({"relation_id":relation.relation_id,"kind":relation.kind,"from":relation.from_symbol,"to":relation.to_symbol,"from_name":from_name,"to_name":to_name,"location":{"file":relation.file,"line":relation.span.start.line,"column":relation.span.start.column,"file_hash":relation.origin.as_ref().map(|origin|origin.file_hash.as_str()).or(file_hash),"coordinate_snapshot_id":relation.origin.as_ref().map(|origin|origin.snapshot_id.as_str()).unwrap_or(snapshot_id)},"span":relation.span,"certainty":relation.certainty,"freshness":relation.freshness,"runtime":relation.runtime,"evidence":relation.evidence,"origin":relation.origin,"scope":scope})
}
pub fn filtered(relations: Vec<Relation>, args: &Value) -> Vec<Relation> {
    relations
        .into_iter()
        .filter_map(|r| filter_relation(r, args))
        .collect()
}
fn filter_relation(mut r: Relation, args: &Value) -> Option<Relation> {
    if let Some(sources) = args["source"].as_array() {
        r.evidence.retain(|e| {
            sources
                .iter()
                .any(|source| source.as_str() == Some(e.source.as_str()))
        });
        if r.evidence.is_empty() {
            return None;
        }
        if r.origin.is_some() && r.evidence.iter().all(|e| e.source == "L1") {
            r.origin = None;
            r.freshness = "current".into();
        }
        r.certainty = if r.origin.is_none()
            && r.evidence
                .iter()
                .any(|e| e.certainty == Certainty::Definite)
        {
            Certainty::Definite
        } else {
            Certainty::Candidate
        };
    }
    relation_matches(&r, args).then_some(r)
}

/// Whether the relation is definite and its effective freshness after the
/// `source` filter; `None` when no evidence survives.
fn effective_filter<'a>(relation: &'a Relation, sources: &[Value]) -> Option<(bool, &'a str)> {
    let mut definite = false;
    let mut freshness = relation.freshness.as_str();
    let mut count = 0;
    let mut only_syntax = true;
    for evidence in &relation.evidence {
        if !sources
            .iter()
            .any(|source| source.as_str() == Some(evidence.source.as_str()))
        {
            continue;
        }
        count += 1;
        only_syntax &= evidence.source == "L1";
        definite |= evidence.certainty == Certainty::Definite;
    }
    if count == 0 {
        return None;
    }
    if relation.origin.is_some() {
        if only_syntax {
            freshness = "current";
        } else {
            definite = false;
        }
    }
    Some((definite, freshness))
}

fn relation_matches(relation: &Relation, args: &Value) -> bool {
    if !prefix(&relation.file, args) {
        return false;
    }
    let (definite, freshness) = match args["source"].as_array() {
        Some(sources) => match effective_filter(relation, sources) {
            Some(value) => value,
            None => return false,
        },
        None => (relation.certainty == Certainty::Definite, relation.freshness.as_str()),
    };
    let certainty = if definite { "definite" } else { "candidate" };
    !args["certainty"]
        .as_array()
        .is_some_and(|values| !values.iter().any(|value| value.as_str() == Some(certainty)))
        && !args["freshness"]
            .as_array()
            .is_some_and(|values| !values.iter().any(|value| value.as_str() == Some(freshness)))
}

pub fn summary(relations: &[Relation], basis: &str) -> Value {
    summary_refs(relations.iter(), basis)
}

pub(crate) fn summary_refs<'a>(
    relations: impl Iterator<Item = &'a Relation>,
    basis: &str,
) -> Value {
    let mut relation_count = 0usize;
    let mut evidence_count = 0usize;
    let mut source = BTreeMap::from([("L1", 0), ("L2", 0), ("L3", 0)]);
    let mut certainty = BTreeMap::from([("definite", 0), ("candidate", 0)]);
    let mut freshness = BTreeMap::from([("current", 0), ("pending", 0), ("stale", 0)]);
    let mut runtime = BTreeMap::from([("observed", 0), ("not_observed", 0), ("unavailable", 0)]);
    for r in relations {
        relation_count += 1;
        evidence_count += r.evidence.len();
        for e in &r.evidence {
            *source.entry(e.source.as_str()).or_default() += 1;
        }
        *certainty
            .entry(if r.certainty == Certainty::Definite {
                "definite"
            } else {
                "candidate"
            })
            .or_default() += 1;
        *freshness.entry(&r.freshness).or_default() += 1;
        *runtime.entry(&r.runtime).or_default() += 1;
    }
    json!({"basis":basis,"exact":basis!="partial","relation_count":relation_count,"evidence_count":evidence_count,"source":source,"certainty":certainty,"freshness":freshness,"runtime":runtime})
}
/// Collapse many gaps of one kind (and side) into a single counted record with a
/// few samples, so a response does not repeat thousands of per-file diagnostics.
/// Small groups stay as individual entries. Consumers sum `count` (default 1);
/// the full list is available from `doctor`.
pub fn aggregate_gaps(gaps: Vec<Value>) -> Vec<Value> {
    const KEEP: usize = 3;
    let mut order: Vec<(String, String)> = vec![];
    let mut groups: BTreeMap<(String, String), Vec<Value>> = BTreeMap::new();
    for gap in gaps {
        let key = (
            gap["kind"].as_str().unwrap_or("unknown").to_string(),
            gap["side"].as_str().unwrap_or("").to_string(),
        );
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(gap);
    }
    order
        .into_iter()
        .flat_map(|key| {
            let items = groups.remove(&key).unwrap_or_default();
            if items.len() <= KEEP {
                return items;
            }
            let samples = items
                .iter()
                .take(KEEP)
                .map(|gap| json!({"file":gap["file"],"reason":gap["reason"]}))
                .collect::<Vec<_>>();
            let mut record =
                json!({"kind":key.0,"count":items.len(),"samples":samples,"details_tool":"doctor"});
            if !key.1.is_empty() {
                record["side"] = json!(key.1);
            }
            vec![record]
        })
        .collect()
}

/// Number of underlying gaps, counting aggregated records by their `count`.
pub fn gap_total(gaps: &Value) -> usize {
    gaps.as_array().map_or(0, |items| {
        items
            .iter()
            .map(|gap| gap["count"].as_u64().unwrap_or(1) as usize)
            .sum()
    })
}

pub fn envelope(stored: &StoredView, fresh: Value) -> Value {
    let view = &stored.view;
    let mut gaps = view
        .known_gaps
        .iter()
        .chain(stored.reference_gaps.iter())
        .map(|d| json!({"kind":d.code,"file":d.file,"reason":d.message}))
        .collect::<Vec<_>>();
    gaps.push(json!({"kind":"syntax_binding_candidates","reason":"L1 name matching does not resolve lexical scope, shadowing or external consumers; candidate sets may contain false positives"}));
    let gaps = aggregate_gaps(gaps);
    let languages = view
        .snapshot
        .manifest
        .files
        .keys()
        .filter_map(|p| {
            if p.ends_with(".rs") {
                Some("rust")
            } else if crate::web::is_source(p) {
                Some("typescript_javascript")
            } else if p.ends_with(".go") {
                Some("go")
            } else {
                None
            }
        })
        .collect::<BTreeSet<_>>();
    json!({"schema_version":"0.1","snapshot_id":view.snapshot.snapshot_id,"epochs":{"syntax":view.epochs.syntax_epoch,"semantic":view.epochs.semantic_epoch},"fresh_requirement":fresh,"evidence_summary":summary(&[],"complete"),"completeness":{"status":if gaps.is_empty() && view.pending_units.is_empty() {"within_scope"} else {"partial"},"scope":{"workspace_id":view.snapshot.manifest.workspace_id,"languages":languages,"build_config_hash":view.snapshot.manifest.analysis_versions.get("analysis_config"),"external_consumers_included":false},"known_gaps":gaps,"pending_units":view.pending_units.iter().map(|p|json!({"unit":p,"reason":"semantic refresh not published for this input"})).collect::<Vec<_>>()},"output":{"truncated":false,"truncation_reason":null,"returned_relations":0,"remaining_relations":0,"continuation":{}}})
}

fn symbol_at(result: &mut Value, stored: &StoredView, args: &Value) -> Result<()> {
    let path = args["file"].as_str().unwrap();
    crate::workspace::safe_relative(path)?;
    let file = stored
        .view
        .snapshot
        .manifest
        .files
        .get(path)
        .context("file_not_found: file is outside indexed snapshot")?;
    if args["file_hash"]
        .as_str()
        .is_some_and(|hash| hash != file.file_hash)
    {
        return Err(failure(
            "content_mismatch",
            "snapshot file differs from expected contents",
            json!({"expected":args["file_hash"],"actual":file.file_hash}),
        ));
    }
    let pos = Position {
        line: args["line"].as_u64().unwrap() as u32,
        column: args["column"].as_u64().unwrap() as u32,
    };
    byte_offset(&file.content, pos).map_err(|e| anyhow::anyhow!("invalid_argument: {e}"))?;
    let declarations = stored
        .index()
        .symbols_in_file(path)
        .filter(|s| s.name_span.contains(pos))
        .collect::<Vec<_>>();
    let enclosing = stored
        .index()
        .symbols_in_file(path)
        .filter(|s| s.span.contains(pos))
        .min_by_key(|s| s.span.end.line - s.span.start.line);
    result["enclosing_symbol_id"] = json!(enclosing.map(|s| &s.symbol_id));
    if !declarations.is_empty() {
        result["occurrence"] = json!({"kind":"declaration","span":declarations[0].name_span,"file_hash":file.file_hash});
        result["symbols"] = json!(
            declarations
                .iter()
                .map(|s| {
                    let mut v = symbol_value(s, stored, false, true);
                    v["certainty"] = json!("definite");
                    v["evidence"] = json!([{"source":"L1","derivation":"syntax_declaration"}]);
                    v
                })
                .collect::<Vec<_>>()
        );
        return Ok(());
    }
    let occurrence = stored
        .index()
        .occurrences_in_file(path)
        .find(|o| o.span.contains(pos));
    result["occurrence"] = json!(occurrence);
    let edges = occurrence
        .map(|o| stored.index().relations_at(path, o.span))
        .unwrap_or_default();
    result["symbols"] = json!(
        edges
            .iter()
            .filter_map(|r| stored.index().symbol(&r.to_symbol).map(|s| {
                let mut v = symbol_value(s, stored, false, true);
                v["certainty"] = json!(r.certainty);
                v["evidence"] = json!(r.evidence);
                v
            }))
            .collect::<Vec<_>>()
    );
    result["evidence_summary"] = summary(&edges, "complete");
    if occurrence.is_some() && edges.is_empty() {
        result["completeness"]["known_gaps"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind":"unresolved_occurrence","file":path,"position":pos}));
    }
    Ok(())
}

pub struct Graph {
    pub edges: Vec<(Relation, u64, Vec<String>)>,
    pub nodes: BTreeSet<String>,
    pub frontier: Vec<String>,
    pub basis: &'static str,
    pub reason: Option<&'static str>,
}
pub fn truncation_reason<'a>(previous: Option<&'a str>, next: Option<&'a str>) -> Option<&'a str> {
    let priority = |reason| match reason {
        Some("cancelled") => 5,
        Some("timeout") => 4,
        Some("node_limit") => 3,
        Some("depth_limit") => 2,
        Some(_) => 1,
        None => 0,
    };
    if priority(next) > priority(previous) {
        next
    } else {
        previous
    }
}
/// Edges are relation ids; the relations behind them are derived on demand and
/// remembered for the length of one traversal so filters and result assembly
/// never have to look an id up again.
struct RelationGraph<'a> {
    index: &'a crate::query_index::QueryIndex,
    direction: Direction,
    seen: std::cell::RefCell<HashMap<Rc<str>, Relation>>,
    symbols: std::cell::RefCell<HashSet<Rc<str>>>,
}
impl<'a> RelationGraph<'a> {
    fn new(index: &'a crate::query_index::QueryIndex, direction: Direction) -> Self {
        Self {
            index,
            direction,
            seen: Default::default(),
            symbols: Default::default(),
        }
    }

    /// A relation produced by an earlier `neighbors` call.
    fn relation(&self, id: &str) -> std::cell::Ref<'_, Relation> {
        std::cell::Ref::map(self.seen.borrow(), |seen| seen.get(id).unwrap())
    }
}
impl semweft_algo::graph::GraphView for RelationGraph<'_> {
    type Node = Rc<str>;
    type Edge = Rc<str>;

    fn neighbors(&self, node: &Rc<str>) -> impl Iterator<Item = (Rc<str>, Rc<str>)> {
        let direction = self.direction;
        let edges = self.index.adjacent(node, direction);
        let mut seen = self.seen.borrow_mut();
        let mut symbols = self.symbols.borrow_mut();
        let mut out = Vec::with_capacity(edges.len());
        for r in edges {
            let next = match direction {
                Direction::Incoming => r.from_symbol.as_str(),
                Direction::Outgoing => r.to_symbol.as_str(),
                Direction::Both if r.to_symbol == node.as_ref() => r.from_symbol.as_str(),
                Direction::Both => r.to_symbol.as_str(),
            };
            let next = symbols.get(next).cloned().unwrap_or_else(|| {
                let value: Rc<str> = next.into();
                symbols.insert(value.clone());
                value
            });
            let edge = seen
                .get_key_value(r.relation_id.as_str())
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| {
                    let id: Rc<str> = r.relation_id.as_str().into();
                    seen.insert(id.clone(), r);
                    id
                });
            out.push((edge, next));
        }
        out.into_iter()
    }
}

pub fn traverse(
    stored: &StoredView,
    target: &str,
    args: &Value,
    impact: bool,
    deadline: Instant,
) -> Graph {
    let _timing = crate::timing::Timing::new("traverse");
    use semweft_algo::graph::{Interrupt, Limits, StopReason, breadth_first_filtered};
    let direction = match args["direction"].as_str().unwrap_or("callers") {
        "callees" => Direction::Outgoing,
        "both" => Direction::Both,
        _ => Direction::Incoming,
    };
    let graph = RelationGraph::new(stored.index(), direction);
    let traversal = breadth_first_filtered(
        &graph,
        Rc::from(target),
        Limits {
            max_depth: args["depth"].as_u64().unwrap_or(3),
            max_nodes: args["max_nodes"].as_u64().unwrap_or(1000) as usize,
        },
        || {
            if check_cancelled().is_err() {
                Err(Interrupt::Cancelled)
            } else if Instant::now() >= deadline {
                Err(Interrupt::Timeout)
            } else {
                Ok(())
            }
        },
        |id| {
            let relation = graph.relation(id);
            (impact || relation.kind == "call") && relation_matches(&relation, args)
        },
    );
    let reason = traversal.reason.map(|reason| match reason {
        StopReason::DepthLimit => "depth_limit",
        StopReason::NodeLimit => "node_limit",
        StopReason::Timeout => "timeout",
        StopReason::Cancelled => "cancelled",
    });
    let basis = match traversal.reason {
        Some(StopReason::DepthLimit) => "within_limits",
        Some(_) => "partial",
        None => "complete",
    };
    let mut relations = graph.seen.into_inner();
    let edges = traversal
        .edges
        .into_iter()
        .map(|(id, visit)| {
            let mut path = visit.path;
            if direction != Direction::Outgoing {
                path.reverse();
            }
            (
                filter_relation(relations.remove(id.as_ref()).unwrap(), args).unwrap(),
                visit.depth,
                path.into_iter().map(|id| id.to_string()).collect(),
            )
        })
        .collect();
    Graph {
        edges,
        nodes: traversal
            .nodes
            .into_iter()
            .map(|id| id.to_string())
            .collect(),
        frontier: traversal
            .frontier
            .into_iter()
            .map(|id| id.to_string())
            .collect(),
        basis,
        reason,
    }
}

fn find_paths(
    mut result: Value,
    stored: &StoredView,
    args: &Value,
    deadline: Instant,
) -> Result<Value> {
    if args["scope"]
        .as_str()
        .is_some_and(|scope| scope != "symbol")
    {
        return crate::services::find_paths(result, stored, args, deadline);
    }
    use semweft_algo::{
        graph::Interrupt,
        paths::{Limits, StopReason, simple_paths_filtered},
    };
    let from = resolve(stored, args["from"].as_str().unwrap())?;
    let to = resolve(stored, args["to"].as_str().unwrap())?;
    let index = stored.index();
    let graph = RelationGraph::new(index, Direction::Outgoing);
    let limits = Limits {
        max_depth: args["max_depth"].as_u64().unwrap_or(5),
        max_paths: args["max_paths"].as_u64().unwrap_or(100) as usize,
        max_nodes: args["max_nodes"].as_u64().unwrap_or(1000) as usize,
    };
    let traversal = simple_paths_filtered(
        &graph,
        Rc::from(from.symbol_id.as_str()),
        Rc::from(to.symbol_id.as_str()),
        limits,
        || {
            if check_cancelled().is_err() {
                Err(Interrupt::Cancelled)
            } else if Instant::now() >= deadline {
                Err(Interrupt::Timeout)
            } else {
                Ok(())
            }
        },
        |id| {
            let relation = graph.relation(id);
            relation.kind == "call" && relation_matches(&relation, args)
        },
    );
    if traversal.reason == Some(StopReason::Cancelled) {
        bail!("cancelled: client cancelled path enumeration");
    }
    if traversal.reason == Some(StopReason::Timeout) && traversal.paths.is_empty() {
        bail!("timeout: no paths computed within timeout_ms");
    }
    let reason = traversal.reason.map(|reason| match reason {
        StopReason::PathLimit => "path_limit",
        StopReason::DepthLimit => "depth_limit",
        StopReason::NodeLimit => "node_limit",
        StopReason::Timeout => "timeout",
        StopReason::Cancelled => "cancelled",
    });
    let basis = match traversal.reason {
        Some(StopReason::DepthLimit | StopReason::PathLimit) => "within_limits",
        Some(_) => "partial",
        None => "complete",
    };
    let mut relations = BTreeMap::new();
    let paths = traversal.paths.iter().map(|path| {
        let edges = path.edges.iter().map(|id| filter_relation(graph.relation(id).clone(), args).unwrap()).collect::<Vec<_>>();
        for edge in &edges {
            relations.insert(edge.relation_id.clone(), edge.clone());
        }
        json!({"path_id":identity("path", &[&stored.view.snapshot.snapshot_id,
                &stored.view.epochs.semantic_epoch.to_string(), &from.symbol_id, &to.symbol_id, &path.edges.join("\n")]),
            "symbol_ids":path.nodes,"length":edges.len(),
            "symbols":path.nodes.iter().filter_map(|id| index.symbol(id)).map(|symbol| symbol_value(symbol,stored,false,false)).collect::<Vec<_>>(),
            "certainty":if edges.iter().all(|edge| edge.certainty == Certainty::Definite) { "definite" } else { "candidate" },
            "freshness":if edges.iter().any(|edge| edge.freshness == "pending") { "pending" } else { "current" },
            "relations":edges.iter().map(|edge| relation_value(edge,stored)).collect::<Vec<_>>()})
    }).collect::<Vec<_>>();
    result["from"] = symbol_value(from, stored, false, false);
    result["to"] = symbol_value(to, stored, false, false);
    result["evidence_summary"] = summary(&relations.into_values().collect::<Vec<_>>(), basis);
    result["path_summary"] = json!({"path_count":paths.len(),"enumeration_complete":reason.is_none(),
        "reason":reason,"visited_nodes":traversal.visited.len(),"max_depth":limits.max_depth,
        "max_paths":limits.max_paths,"max_nodes":limits.max_nodes,"cycle_policy":"simple paths; repeated symbols excluded",
        "scope":"indexed symbol-to-symbol call paths; module and service mapping unsupported"});
    result["paths"] = json!(paths);
    result["frontier"] = json!(traversal.frontier);
    if let Some(reason) = reason {
        let mut suggested = args.clone();
        suggested["snapshot_id"] = result["snapshot_id"].clone();
        suggested["semantic_epoch"] = result["epochs"]["semantic"].clone();
        suggested["max_depth"] = json!((limits.max_depth + 1).min(10));
        suggested["max_paths"] = json!((limits.max_paths * 2).min(1000));
        suggested["max_nodes"] = json!((limits.max_nodes * 2).min(100000));
        suggested["timeout_ms"] =
            json!((args["timeout_ms"].as_u64().unwrap_or(5000) * 2).min(60000));
        result["output"] = json!({"truncated":true,"truncation_reason":reason,"remaining_relations":null,
            "continuation":{"kind":"requery","frontier":traversal.frontier,"suggested":suggested}});
    }
    Ok(result)
}

fn context_pack(
    mut result: Value,
    stored: &StoredView,
    args: &Value,
    deadline: Instant,
) -> Result<Value> {
    let budget = args["budget"].as_u64().unwrap();
    if budget < MIN_BUDGET {
        return Err(failure(
            "budget_too_small",
            "budget cannot hold response metadata and an omission page",
            json!({"requested":budget,"min_budget":MIN_BUDGET,"token_count_mode":"estimated"}),
        ));
    }
    // Context retains gap counts instead of copying an unbounded diagnostic list.
    let gaps = result["completeness"]["known_gaps"].as_array().unwrap();
    let mut counts = BTreeMap::<String, usize>::new();
    for gap in gaps {
        *counts
            .entry(gap["kind"].as_str().unwrap_or("unknown").into())
            .or_default() += gap["count"].as_u64().unwrap_or(1) as usize;
    }
    result["completeness"]["known_gaps"] = json!(
        counts
            .into_iter()
            .map(|(kind, count)| json!({"kind":kind,"count":count,"details_tool":"index_status"}))
            .collect::<Vec<_>>()
    );
    let pending = result["completeness"]["pending_units"]
        .as_array()
        .unwrap()
        .len();
    result["completeness"]["pending_units"] = if pending > 0 {
        json!([{"count":pending,"details_tool":"index_status"}])
    } else {
        json!([])
    };
    let include = args["include"].as_array();
    let enabled = |kind: &str| include.is_none_or(|values| values.contains(&json!(kind)));
    let mut roots = if let Some(symbol) = args["symbol"].as_str() {
        vec![resolve(stored, symbol)?]
    } else {
        let task = args["task"].as_str().unwrap();
        stored
            .view
            .snapshot
            .syntax
            .symbols
            .iter()
            .filter(|s| {
                task.split_whitespace()
                    .any(|q| fuzzy(&format!("{} {}", public_name(s), s.file), q))
            })
            .collect()
    };
    let total_roots = roots.len();
    if args.get("task").is_some() {
        roots.sort_by_key(|s| (public_name(s), s.file.clone(), s.name_span));
    }
    result["retrieval"] = json!({"method":if args.get("task").is_some() {"symbol name and path fuzzy matching"} else {"explicit symbol"},"candidate_targets":total_roots,"selected_targets":roots.len(),"omitted_targets":total_roots.saturating_sub(roots.len()),"target_limit":null});
    let mut candidates = vec![];
    let mut seen = BTreeSet::new();
    if enabled("definition") {
        for root in &roots {
            if seen.insert(root.symbol_id.clone()) {
                candidates.push(((*root).clone(), "definition"));
            }
        }
    }
    for root in &roots {
        check_cancelled()?;
        if Instant::now() >= deadline {
            bail!("timeout: context retrieval exceeded query deadline");
        }
        let relations = stored.index().adjacent(&root.symbol_id, Direction::Both);
        for edge in &relations {
            let reason = if edge.to_symbol == root.symbol_id && enabled("callers") {
                Some("callers")
            } else if edge.from_symbol == root.symbol_id && enabled("callees") {
                Some("callees")
            } else {
                None
            };
            if let Some(reason) = reason {
                let id = if reason == "callers" {
                    &edge.from_symbol
                } else {
                    &edge.to_symbol
                };
                if let Some(s) = stored.index().symbol(id)
                    && seen.insert(id.clone())
                {
                    candidates.push((s.clone(), reason));
                }
            }
        }
        if enabled("types") {
            for s in &stored.view.snapshot.syntax.symbols {
                if ["struct", "enum", "trait", "type"].contains(&s.kind.as_str())
                    && root
                        .signature
                        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .any(|part| part == s.name)
                    && seen.insert(s.symbol_id.clone())
                {
                    candidates.push((s.clone(), "types"));
                }
            }
        }
        if enabled("tests") {
            for s in &stored.view.snapshot.syntax.symbols {
                if test_scope(s, stored) == "test"
                    && (s.file == root.file
                        || relations
                            .iter()
                            .any(|r| r.to_symbol == root.symbol_id && r.from_symbol == s.symbol_id))
                    && seen.insert(s.symbol_id.clone())
                {
                    candidates.push((s.clone(), "tests"));
                }
            }
        }
    }
    result["items"] = json!([]);
    result["omitted"] = json!([]);
    result["omitted_count"] = json!(0);
    result["token_count"] =
        crate::tokens::metadata(args["tokenizer"].as_str().unwrap_or("estimated"), budget);
    let mut omitted = vec![];
    candidates.sort_by_key(|(_, reason)| match *reason {
        "definition" => 0,
        "callers" => 1,
        "callees" => 2,
        "types" => 3,
        _ => 4,
    });
    let mut timed_out = false;
    for (symbol, reason) in candidates {
        check_cancelled()?;
        let mut accepted = false;
        for granularity in if reason == "definition" {
            vec!["full", "signature", "outline", "location"]
        } else {
            vec!["signature", "outline", "location"]
        } {
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            let content = match granularity {
                "full" => source_span(
                    &stored.view.snapshot.manifest.files[&symbol.file].content,
                    symbol.span,
                )
                .to_owned(),
                "signature" => symbol.signature.clone(),
                "outline" => format!("{} {}", symbol.kind, public_name(&symbol)),
                _ => String::new(),
            };
            let item = json!({"symbol_id":symbol.symbol_id,"qualified_name":qualified_name(&symbol,stored),"location":{"file":symbol.file,"line":symbol.name_span.start.line,"column":symbol.name_span.start.column,"file_hash":symbol.file_hash},"granularity":granularity,"reason":reason,"content":content});
            result["items"].as_array_mut().unwrap().push(item);
            if crate::tokens::count(&result)? + 512 <= budget {
                accepted = true;
                break;
            }
            result["items"].as_array_mut().unwrap().pop();
        }
        if !accepted {
            omitted.push(json!({"symbol_id":symbol.symbol_id,"location":{"file":symbol.file,"line":symbol.name_span.start.line},"granularity":"location","query":{"tool":"get_symbol","symbol":symbol.symbol_id,"snapshot_id":result["snapshot_id"],"semantic_epoch":result["epochs"]["semantic"]}}));
        }
    }
    if enabled("config") {
        for (path, file) in &stored.view.snapshot.manifest.files {
            if !(path.ends_with("Cargo.toml")
                || path.ends_with("semweft.toml")
                || path.ends_with("rust-toolchain.toml")
                || path.ends_with("rust-toolchain")
                || matches!(
                    path.rsplit('/').next(),
                    Some("go.mod" | "go.work" | "go.sum" | "go.work.sum")
                ))
            {
                continue;
            }
            let related = roots.iter().any(|s| {
                let dir = crate::structure::package(stored, &s.file)
                    .map(|(d, _)| d)
                    .unwrap_or("");
                !path.contains('/')
                    || path
                        .rsplit_once('/')
                        .is_some_and(|(parent, _)| parent == dir)
            });
            if !related {
                continue;
            }
            result["items"].as_array_mut().unwrap().push(json!({"location":{"file":path,"file_hash":file.file_hash},"granularity":"full","reason":"explicit owning project configuration","content":file.content}));
            if Instant::now() >= deadline || crate::tokens::count(&result)? + 512 > budget {
                timed_out |= Instant::now() >= deadline;
                result["items"].as_array_mut().unwrap().pop();
                omitted.push(json!({"location":{"file":path,"file_hash":file.file_hash},"granularity":"location","query":{"tool":"index_status","snapshot_id":result["snapshot_id"]}}));
            }
        }
    }
    result["omitted_count"] = json!(omitted.len());
    if !omitted.is_empty() {
        let mut retry = args.clone();
        retry["snapshot_id"] = result["snapshot_id"].clone();
        retry["semantic_epoch"] = result["epochs"]["semantic"].clone();
        retry["budget"] = json!((budget * 2).min(10_000_000));
        retry["tool"] = json!("context_pack");
        if timed_out {
            retry["timeout_ms"] =
                json!((args["timeout_ms"].as_u64().unwrap_or(5000) * 2).min(60000));
        }
        result["output"] = json!({"truncated":true,"truncation_reason":if timed_out {"timeout"} else {"budget"},"continuation":{"kind":"omitted","query":retry}});
    }
    if timed_out {
        if result["items"].as_array().unwrap().is_empty() {
            bail!("timeout: no context selected within query deadline");
        }
        result["evidence_summary"]["basis"] = json!("partial");
        result["evidence_summary"]["exact"] = json!(false);
    }
    let all_omitted = omitted.clone();
    for item in omitted {
        result["omitted"].as_array_mut().unwrap().push(item);
        if crate::tokens::count(&result)? + 512 > budget {
            result["omitted"].as_array_mut().unwrap().pop();
            break;
        }
    }
    count_context(&mut result)?;
    result["_all_omitted"] = json!(all_omitted);
    Ok(result)
}

fn count_context(result: &mut Value) -> Result<()> {
    crate::tokens::finalize(result)?;
    if crate::tokens::count(result)? > result["token_count"]["budget"].as_u64().unwrap() {
        return Err(failure(
            "budget_too_small",
            "snapshot metadata exceeds this budget",
            json!({"requested":result["token_count"]["budget"],"min_budget":crate::tokens::count(result)?}),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gap(kind: &str, file: &str) -> Value {
        json!({"kind":kind,"file":file,"reason":format!("{kind} in {file}")})
    }

    #[test]
    fn small_gap_groups_stay_individual_and_large_groups_become_counted_samples() {
        let mut gaps = vec![gap("rare", "a.rs"), gap("rare", "b.rs")];
        gaps.extend((0..500).map(|i| gap("closure", &format!("f{i}.rs"))));
        gaps.push(gap("other", "z.rs"));
        let out = aggregate_gaps(gaps);
        // First-seen order is kept: rare (2 raw), closure (1 record), other (1 raw).
        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["file"], "a.rs");
        assert_eq!(out[1]["file"], "b.rs");
        assert_eq!(out[2]["kind"], "closure");
        assert_eq!(out[2]["count"], 500);
        assert_eq!(out[2]["samples"].as_array().unwrap().len(), 3);
        assert_eq!(out[2]["samples"][0]["file"], "f0.rs");
        assert_eq!(out[2]["details_tool"], "doctor");
        assert_eq!(out[3]["kind"], "other");
        // The total is preserved across aggregation.
        assert_eq!(gap_total(&json!(out)), 2 + 500 + 1);
    }

    #[test]
    fn aggregation_keeps_sides_separate_and_threshold_is_inclusive() {
        let mut gaps = vec![];
        for side in ["base", "head"] {
            gaps.extend(
                (0..3)
                    .map(|i| json!({"kind":"k","side":side,"file":format!("{i}.rs"),"reason":"r"})),
            );
        }
        // Exactly three per side: nothing is collapsed.
        assert_eq!(aggregate_gaps(gaps.clone()).len(), 6);
        gaps.push(json!({"kind":"k","side":"base","file":"x.rs","reason":"r"}));
        let out = aggregate_gaps(gaps);
        let base = out
            .iter()
            .find(|g| g["side"] == "base" && g["count"].is_u64());
        assert_eq!(base.unwrap()["count"], 4);
        assert_eq!(out.iter().filter(|g| g["side"] == "head").count(), 3);
        assert_eq!(gap_total(&json!(out)), 7);
    }

    #[test]
    fn gap_total_handles_missing_and_empty_gaps() {
        assert_eq!(gap_total(&json!([])), 0);
        assert_eq!(gap_total(&Value::Null), 0);
        assert_eq!(gap_total(&json!([{"kind":"a"},{"kind":"b","count":9}])), 10);
        assert!(aggregate_gaps(vec![]).is_empty());
    }

    #[test]
    fn fixed_requery_preserves_selected_commit_context_with_reused_semantic_views() {
        let selected = "a".repeat(40);
        let analysis = "b".repeat(40);
        let result = json!({"snapshots":{"base":{"snapshot_id":"base","epochs":{"semantic":3},"commit":selected,"analysis_commit":analysis},"head":{"snapshot_id":"head","epochs":{"semantic":4},"commit":analysis,"analysis_commit":analysis}}});
        let suggested = fixed_requery(
            &result,
            &json!({"diff":{"base":{"kind":"revision","revision":selected},"head":{"kind":"workspace"}}}),
        );
        assert_eq!(suggested["diff"]["base"]["kind"], "snapshot");
        assert_eq!(suggested["diff"]["base"]["semantic_epoch"], 3);
        assert_eq!(suggested["diff"]["base"]["commit_context"], selected);
        assert!(suggested["diff"]["head"].get("commit_context").is_none());
        semweft_protocol::validate("impact_of", &suggested).unwrap();
    }
    use semweft_lang_rust::RustParser;
    use std::time::Duration;

    fn query_service() -> (tempfile::TempDir, QueryService) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"implementations-query-test\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let config_path = temp.path().join("semweft.toml");
        std::fs::write(
            &config_path,
            "schema_version = \"0.1\"\n[index]\ndirectory = \".semweft/index\"\n",
        )
        .unwrap();
        let resolved = crate::config::resolve(Some(temp.path()), Some(&config_path)).unwrap();
        let service = QueryService::new(Engine::new(resolved).unwrap());
        (temp, service)
    }

    #[test]
    fn compact_graph_pages_match_eager_pages_with_filters_bounds_and_replay() {
        let (_temp, service) = query_service();
        let stored = stored(&[
            (
                "src/lib.rs",
                "pub fn target() {}\npub fn left() { target(); entry(); }\npub fn right() { target(); }\npub fn entry() { left(); right(); target(); }\n",
            ),
            ("tests/calls.rs", "#[test]\nfn check() { target(); }\n"),
        ]);
        for tool in ["call_graph", "impact_of"] {
            for direction in ["callers", "callees", "both"] {
                for filter in [
                    json!({}),
                    json!({"source":["L1"],"certainty":["candidate"],"freshness":["current"]}),
                    json!({"source":["L3"]}),
                    json!({"path_prefix":"tests"}),
                    json!({"depth":1}),
                    json!({"max_nodes":2}),
                ] {
                    let mut args = json!({"symbol":"target","direction":direction,"limit":2});
                    args.as_object_mut()
                        .unwrap()
                        .extend(filter.as_object().unwrap().clone());
                    let eager = service
                        .compute(tool, &args, &stored, json!({}), deadline())
                        .unwrap();
                    let (metadata, graph) = graph_result(
                        envelope(&stored, json!({})),
                        &stored,
                        &args,
                        tool,
                        deadline(),
                    )
                    .unwrap();
                    let mut expected = service
                        .page(Page::new(
                            tool.into(),
                            args.clone(),
                            eager,
                            vec!["relations".into()],
                            0,
                        ))
                        .unwrap();
                    let mut actual = service
                        .page(
                            Page::new(
                                tool.into(),
                                args.clone(),
                                metadata,
                                vec!["relations".into()],
                                0,
                            )
                            .with_graph(Some(GraphRelations::new(graph.edges, &stored).unwrap())),
                        )
                        .unwrap();
                    let mut seen = BTreeSet::new();
                    loop {
                        let expected_cursor = expected["output"]["continuation"]
                            .as_object_mut()
                            .unwrap()
                            .remove("next_cursor");
                        let actual_cursor = actual["output"]["continuation"]
                            .as_object_mut()
                            .unwrap()
                            .remove("next_cursor");
                        assert_eq!(actual, expected, "{tool} {args}");
                        for relation in actual["relations"].as_array().unwrap() {
                            assert!(
                                seen.insert(relation["relation_id"].as_str().unwrap().to_owned())
                            );
                        }
                        let Some(cursor) = actual_cursor else {
                            assert!(expected_cursor.is_none());
                            break;
                        };
                        let cursor_args = json!({"cursor":cursor});
                        let replay = service
                            .execute(tool, cursor_args.clone(), deadline())
                            .unwrap();
                        actual = service.execute(tool, cursor_args, deadline()).unwrap();
                        let mut replay = replay;
                        replay["output"]["continuation"]
                            .as_object_mut()
                            .unwrap()
                            .remove("next_cursor");
                        let mut replayed = actual.clone();
                        replayed["output"]["continuation"]
                            .as_object_mut()
                            .unwrap()
                            .remove("next_cursor");
                        assert_eq!(replayed, replay);
                        expected = service
                            .execute(tool, json!({"cursor":expected_cursor.unwrap()}), deadline())
                            .unwrap();
                    }
                }
            }
        }
        service.engine.shutdown();
    }

    #[test]
    fn compact_graph_cursor_fits_without_pinning_the_view_and_keeps_origin_coordinates() {
        let (_temp, service) = query_service();
        let mut source = "pub fn target() {}\n".to_owned();
        for caller in 0..129 {
            source.push_str(&format!(
                "pub fn caller_{caller}() {{ {} }}\n",
                "target();".repeat(8)
            ));
        }
        let stored = stored(&[("src/lib.rs", &source)]);
        let view = Arc::downgrade(&stored.view);
        let args = json!({"symbol":"target","limit":100});
        let (metadata, mut graph) = graph_result(
            envelope(&stored, json!({})),
            &stored,
            &args,
            "impact_of",
            deadline(),
        )
        .unwrap();
        let total = graph.edges.len();
        assert!(total >= 1032);
        graph.edges[0].0.origin = Some(RelationOrigin {
            snapshot_id: "original-snapshot".into(),
            epochs: stored.view.epochs.clone(),
            file_hash: "original-file-hash".into(),
            reason: "retained".into(),
        });
        for source in ["L2", "L3"] {
            graph.edges[0].0.evidence.push(Evidence {
                evidence_id: format!("evidence-{source}-中文"),
                source: source.into(),
                indexer: "indexer-中文".into(),
                derivation: "derived-调用".into(),
                file_hash: "hash-中文".into(),
                certainty: Certainty::Definite,
            });
        }
        graph.edges[1].0.to_symbol = "missing-目标".into();
        graph.edges[2].0.file = "missing-文件.rs".into();
        let mut eager = metadata.clone();
        eager["relations"] = json!(
            graph
                .edges
                .iter()
                .map(|(relation, depth, path)| {
                    let mut value = relation_value(relation, &stored);
                    value["depth"] = json!(depth);
                    value["path"] = json!(path);
                    value
                })
                .collect::<Vec<_>>()
        );
        let eager = Page::new(
            "impact_of".into(),
            args.clone(),
            eager,
            vec!["relations".into()],
            0,
        );
        let compact = Page::new(
            "impact_of".into(),
            args,
            metadata,
            vec!["relations".into()],
            0,
        )
        .with_graph(Some(GraphRelations::new(graph.edges, &stored).unwrap()));
        assert!(
            compact.bytes * 2 < eager.bytes,
            "compact={} eager={}",
            compact.bytes,
            eager.bytes
        );
        assert_eq!(
            compact.values("relations", 0..total).unwrap(),
            eager.values("relations", 0..total).unwrap()
        );
        assert_eq!(
            compact.values("relations", 0..1).unwrap()[0]["location"]["coordinate_snapshot_id"],
            "original-snapshot"
        );
        drop(stored);
        assert!(view.upgrade().is_none());
        let budget = compact.bytes + 512;
        service
            .engine
            .state
            .locked()
            .resolved
            .config
            .index
            .cursor_cache_bytes = budget;
        assert_eq!(code(service.page(eager).unwrap_err()), "cursor_expired");
        let first = service.page(compact).unwrap();
        let mut next = first.clone();
        let mut seen = BTreeSet::new();
        loop {
            assert_eq!(next["snapshot_id"], first["snapshot_id"]);
            for relation in next["relations"].as_array().unwrap() {
                assert!(seen.insert(relation["relation_id"].as_str().unwrap().to_owned()));
            }
            let Some(cursor) = next["output"]["continuation"]["next_cursor"].as_str() else {
                assert_eq!(next["relations"].as_array().unwrap().len(), total % 100);
                break;
            };
            next = service
                .execute("impact_of", json!({"cursor":cursor}), deadline())
                .unwrap();
        }
        assert_eq!(seen.len(), total);
        service.engine.shutdown();
    }

    #[test]
    fn report_pages_match_eager_pages_across_fields_blocks_and_replay() {
        let (_temp, service) = query_service();
        let args = json!({"symbol":"target","limit":127});
        let mut result = json!({"snapshot_id":"frozen","epochs":{"syntax":1,"semantic":0},
            "output":{"continuation":{}},"evidence_summary":{"exact":true},
            "test_summary":{"candidate_count":260},"completeness":{"known_gaps":[]}});
        let fields = [
            "test_candidates",
            "relations",
            "relation_changes.added",
            "relation_changes.removed",
            "affected_modules",
        ]
        .map(str::to_owned)
        .to_vec();
        for (index, field) in fields.iter().enumerate() {
            let length = if index == 3 { 0 } else { 260 };
            set_list(&mut result, field, (0..length).map(|item| json!({
                "relation_id":format!("{field}-{item}"),"name":"固定结果-中文",
                "path":["first","second"],"location":{"file":"src/lib.rs","line":item+1},
                "evidence":[{"source":"L3","certainty":"definite","indexer":"repeated-indexer"}]
            })).collect());
        }
        let eager = Page::new(
            "suggest_tests".into(),
            args.clone(),
            result.clone(),
            fields.clone(),
            0,
        );
        let compact = Page::build(
            "suggest_tests".into(),
            args,
            result,
            fields.clone(),
            0,
            true,
        )
        .unwrap();
        assert_eq!(compact.reports.len(), 4);
        assert!(
            compact.bytes * 2 < eager.bytes,
            "compact={} eager={}",
            compact.bytes,
            eager.bytes
        );
        for field in &fields {
            assert_eq!(compact.len(field), eager.len(field));
            assert_eq!(
                compact.values(field, 0..compact.len(field)).unwrap(),
                eager.values(field, 0..eager.len(field)).unwrap()
            );
        }
        let mut expected = service.page(eager).unwrap();
        let mut actual = service.page(compact).unwrap();
        loop {
            let expected_cursor = expected["output"]["continuation"]
                .as_object_mut()
                .unwrap()
                .remove("next_cursor");
            let actual_cursor = actual["output"]["continuation"]
                .as_object_mut()
                .unwrap()
                .remove("next_cursor");
            assert_eq!(actual, expected);
            let Some(cursor) = actual_cursor else { break };
            let request = json!({"cursor":cursor});
            actual = service
                .execute("suggest_tests", request.clone(), deadline())
                .unwrap();
            let mut replay = service
                .execute("suggest_tests", request, deadline())
                .unwrap();
            let mut frozen = actual.clone();
            replay["output"]["continuation"]
                .as_object_mut()
                .unwrap()
                .remove("next_cursor");
            frozen["output"]["continuation"]
                .as_object_mut()
                .unwrap()
                .remove("next_cursor");
            assert_eq!(frozen, replay);
            expected = service
                .execute(
                    "suggest_tests",
                    json!({"cursor":expected_cursor.unwrap()}),
                    deadline(),
                )
                .unwrap();
        }
        service.engine.shutdown();
    }

    #[test]
    fn typed_test_suggestions_match_json_paths_with_filters_and_truncation() {
        let (temp, service) = query_service();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"),
            "pub fn target() {}\nfn helper() { target(); }\n#[test]\nfn direct() { target(); }\n#[test]\n#[ignore]\nfn indirect() { helper(); }\n#[test]\nfn reference() { let callback = target; }\n").unwrap();
        service.engine.scan(false).unwrap();
        let (stored, fresh) = service.engine.bind(&json!({}), deadline()).unwrap();
        for filter in [
            json!({}),
            json!({"depth":1}),
            json!({"max_nodes":2}),
            json!({"source":["L3"]}),
            json!({"path_prefix":"src"}),
        ] {
            let mut args = json!({"symbol":"target","limit":2});
            args.as_object_mut()
                .unwrap()
                .extend(filter.as_object().unwrap().clone());
            let (metadata, graph) = graph_result(
                envelope(&stored, fresh.clone()),
                &stored,
                &args,
                "impact_of",
                deadline(),
            )
            .unwrap();
            let relations = json!(graph_values(&graph.edges, &stored));
            let mut eager = metadata.clone();
            eager["relations"] = relations.clone();
            let expected = suggestions::report(&service, eager, &args, deadline(), None).unwrap();
            let mut actual =
                suggestions::report(&service, metadata, &args, deadline(), Some(&graph)).unwrap();
            actual["relations"] = relations;
            assert_eq!(actual, expected, "{args}");
        }
        service.engine.shutdown();
    }

    #[test]
    fn borrowed_relation_filters_match_normalized_evidence_and_origins() {
        let stored = stored(&[(
            "src/lib.rs",
            "pub fn target() {}\nfn caller() { target(); }\n",
        )]);
        let template = stored
            .index()
            .adjacent(
                &stored
                    .view
                    .snapshot
                    .syntax
                    .symbols
                    .iter()
                    .find(|symbol| symbol.name == "target")
                    .unwrap()
                    .symbol_id,
                Direction::Incoming,
            )
            .pop()
            .unwrap();
        for mask in 0..8 {
            for origin in [false, true] {
                for certainty in [Certainty::Candidate, Certainty::Definite] {
                    for freshness in ["current", "pending", "stale"] {
                        let mut relation = template.clone();
                        relation.certainty = certainty.clone();
                        relation.freshness = freshness.into();
                        relation.origin = origin.then(|| RelationOrigin {
                            snapshot_id: "old".into(),
                            epochs: stored.view.epochs.clone(),
                            file_hash: "old-hash".into(),
                            reason: "retained".into(),
                        });
                        relation.evidence = ["L1", "L2", "L3"]
                            .into_iter()
                            .enumerate()
                            .filter(|(index, _)| mask & (1 << index) != 0)
                            .map(|(index, source)| Evidence {
                                evidence_id: source.into(),
                                source: source.into(),
                                indexer: "indexer".into(),
                                derivation: "derivation".into(),
                                file_hash: "hash".into(),
                                certainty: if index == 1 {
                                    Certainty::Candidate
                                } else {
                                    Certainty::Definite
                                },
                            })
                            .collect();
                        for source in [
                            Value::Null,
                            json!([]),
                            json!(["L1"]),
                            json!(["L2"]),
                            json!(["L3"]),
                            json!(["L1", "L3"]),
                        ] {
                            for requested_certainty in
                                [Value::Null, json!(["candidate"]), json!(["definite"])]
                            {
                                for requested_freshness in
                                    [Value::Null, json!(["current"]), json!(["pending", "stale"])]
                                {
                                    for path in ["src", "tests"] {
                                        let args = json!({"source":source,"certainty":requested_certainty,"freshness":requested_freshness,"path_prefix":path});
                                        let mut expected = relation.clone();
                                        if let Some(sources) = source.as_array() {
                                            expected.evidence.retain(|evidence| {
                                                sources.contains(&json!(evidence.source))
                                            });
                                            if expected.origin.is_some()
                                                && expected
                                                    .evidence
                                                    .iter()
                                                    .all(|evidence| evidence.source == "L1")
                                            {
                                                expected.origin = None;
                                                expected.freshness = "current".into();
                                            }
                                            expected.certainty = if expected.origin.is_none()
                                                && expected.evidence.iter().any(|evidence| {
                                                    evidence.certainty == Certainty::Definite
                                                }) {
                                                Certainty::Definite
                                            } else {
                                                Certainty::Candidate
                                            };
                                        }
                                        let accepted = prefix(&expected.file, &args)
                                            && (source.is_null() || !expected.evidence.is_empty())
                                            && !requested_certainty.as_array().is_some_and(
                                                |values| {
                                                    !values.contains(&json!(expected.certainty))
                                                },
                                            )
                                            && !requested_freshness.as_array().is_some_and(
                                                |values| {
                                                    !values.contains(&json!(expected.freshness))
                                                },
                                            );
                                        assert_eq!(
                                            relation_matches(&relation, &args),
                                            accepted,
                                            "{args}"
                                        );
                                        assert_eq!(
                                            serde_json::to_value(filter_relation(
                                                relation.clone(),
                                                &args
                                            ))
                                            .unwrap(),
                                            json!(accepted.then_some(expected)),
                                            "{args}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn impact_cursor_keeps_frozen_results_after_source_refresh_and_rejects_changed_args() {
        let (temp, service) = query_service();
        let source = temp.path().join("src/lib.rs");
        std::fs::create_dir(source.parent().unwrap()).unwrap();
        std::fs::write(
            &source,
            "pub fn target() {}\npub fn caller() { target(); target(); target(); }\n",
        )
        .unwrap();
        let first = service
            .execute(
                "impact_of",
                json!({"symbol":"target","limit":1}),
                deadline(),
            )
            .unwrap();
        let cursor = first["output"]["continuation"]["next_cursor"]
            .as_str()
            .unwrap();
        let before = service
            .execute("impact_of", json!({"cursor":cursor}), deadline())
            .unwrap();
        std::fs::write(&source, "pub fn unrelated() {}\n").unwrap();
        service.engine.scan(false).unwrap();
        let after = service
            .execute("impact_of", json!({"cursor":cursor}), deadline())
            .unwrap();
        assert_eq!(after["relations"], before["relations"]);
        assert_eq!(after["snapshot_id"], first["snapshot_id"]);
        assert_eq!(after["epochs"], first["epochs"]);
        assert_eq!(after["evidence_summary"], first["evidence_summary"]);
        assert_eq!(
            code(
                service
                    .execute(
                        "impact_of",
                        json!({"cursor":cursor,"max_nodes":1}),
                        deadline()
                    )
                    .unwrap_err()
            ),
            "invalid_argument"
        );
        assert_eq!(
            code(
                service
                    .execute("call_graph", json!({"cursor":cursor}), deadline())
                    .unwrap_err()
            ),
            "cursor_expired"
        );
        service.engine.shutdown();
    }

    #[test]
    fn implementation_pages_keep_complete_metadata_and_bind_cursor_arguments() {
        let (_temp, service) = query_service();
        let args = json!({"symbol":"Draw::draw","path_prefix":"src","source":["L1"],
            "certainty":["candidate"],"freshness":["current"],"snapshot_id":"fixed",
            "semantic_epoch":42,"limit":1});
        let original = json!({"schema_version":"0.1","snapshot_id":"fixed",
            "epochs":{"syntax":7,"semantic":42},"output":{"continuation":{}},
            "target":{"symbol_id":"draw"},"trait":{"symbol_id":"Draw"},"mode":"trait_method",
            "relations":[{"relation_id":"first"},{"relation_id":"second"},{"relation_id":"third"}],
            "implementation_counts":{"total":3,"production":2,"test":1,"inherited_defaults":2,"excluded":2},
            "inherited_defaults":[{"impl_id":"default-first"},{"impl_id":"default-second"}],
            "excluded_candidates":[{"impl_id":"excluded-first"},{"impl_id":"excluded-second"}],
            "evidence_summary":{"basis":"complete","exact":true,"total":3},
            "completeness":{"status":"partial","known_gaps":[{"kind":"external_implementations_not_indexed"}]}});
        let first = service
            .page(Page::new(
                "find_implementations".into(),
                args,
                original.clone(),
                vec!["relations".into()],
                0,
            ))
            .unwrap();
        let first_cursor = first["output"]["continuation"]["next_cursor"]
            .as_str()
            .unwrap()
            .to_owned();
        for (parameter, value) in [
            ("symbol", json!("Other::draw")),
            ("path_prefix", json!("tests")),
            ("source", json!(["L3"])),
            ("certainty", json!(["definite"])),
            ("freshness", json!(["pending"])),
            ("snapshot_id", json!("different")),
            ("semantic_epoch", json!(43)),
            ("limit", json!(2)),
        ] {
            let mut changed = json!({"cursor":first_cursor});
            changed[parameter] = value;
            let error = service
                .execute("find_implementations", changed, deadline())
                .unwrap_err();
            assert_eq!(code(error), "invalid_argument", "{parameter}");
        }
        assert_eq!(
            code(
                service
                    .execute(
                        "find_references",
                        json!({"cursor":first_cursor}),
                        deadline()
                    )
                    .unwrap_err()
            ),
            "cursor_expired"
        );
        let mut response = first;
        let mut ids = vec![];
        loop {
            for field in [
                "schema_version",
                "snapshot_id",
                "epochs",
                "target",
                "trait",
                "mode",
                "implementation_counts",
                "inherited_defaults",
                "excluded_candidates",
                "evidence_summary",
                "completeness",
            ] {
                assert_eq!(response[field], original[field], "{field}");
            }
            let relations = response["relations"].as_array().unwrap();
            assert_eq!(relations.len(), 1);
            assert_eq!(response["output"]["returned_relations"], 1);
            assert_eq!(response["output"]["returned_items"], 1);
            ids.push(relations[0]["relation_id"].as_str().unwrap().to_owned());
            assert_eq!(response["output"]["remaining_relations"], 3 - ids.len());
            assert_eq!(response["output"]["remaining_items"], 3 - ids.len());
            let Some(cursor) = response["output"]["continuation"]["next_cursor"].as_str() else {
                break;
            };
            response = service
                .execute(
                    "find_implementations",
                    json!({"cursor":cursor,"snapshot_id":"fixed","semantic_epoch":42,
                        "timeout_ms":1000,"require_fresh":true,"fresh_wait_ms":500}),
                    deadline(),
                )
                .unwrap();
        }
        assert_eq!(ids, ["first", "second", "third"]);
        let replay = service
            .execute(
                "find_implementations",
                json!({"cursor":first_cursor}),
                deadline(),
            )
            .unwrap();
        assert_eq!(replay["relations"][0]["relation_id"], "second");
        assert_eq!(replay["epochs"], original["epochs"]);
        assert_eq!(
            replay["implementation_counts"],
            original["implementation_counts"]
        );
    }

    #[test]
    fn implementation_queries_page_a_bound_version_after_new_publications() {
        let (temp, service) = query_service();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        let source = "pub trait Draw {}\npub struct First;\npub struct Second;\nimpl Draw for First {}\nimpl Draw for Second {}\n";
        let path = temp.path().join("src/lib.rs");
        std::fs::write(&path, source).unwrap();
        let view = service.engine.scan(true).unwrap();
        let args = json!({"symbol":"Draw","path_prefix":"src","source":["L1"],
            "certainty":["candidate"],"freshness":["current"],"limit":1,
            "snapshot_id":view.snapshot.snapshot_id,"semantic_epoch":view.epochs.semantic_epoch});
        let first = service
            .execute("find_implementations", args.clone(), deadline())
            .unwrap();
        assert_eq!(first["snapshot_id"], view.snapshot.snapshot_id);
        assert_eq!(first["epochs"]["semantic"], view.epochs.semantic_epoch);
        assert_eq!(first["mode"], "trait");
        assert_eq!(first["implementation_counts"]["total"], 2);
        assert_eq!(first["relations"].as_array().unwrap().len(), 1);
        assert_eq!(first["relations"][0]["kind"], "implements");
        assert_eq!(first["output"]["returned_relations"], 1);
        assert_eq!(first["output"]["remaining_relations"], 1);
        let cursor = first["output"]["continuation"]["next_cursor"]
            .as_str()
            .unwrap()
            .to_owned();
        std::fs::write(
            &path,
            format!("{source}pub struct Third;\nimpl Draw for Third {{}}\n"),
        )
        .unwrap();
        let newer = service.engine.scan(true).unwrap();
        assert_ne!(newer.snapshot.snapshot_id, view.snapshot.snapshot_id);
        let second = service
            .execute("find_implementations", json!({"cursor":cursor}), deadline())
            .unwrap();
        for field in [
            "snapshot_id",
            "epochs",
            "target",
            "trait",
            "mode",
            "implementation_counts",
            "inherited_defaults",
            "excluded_candidates",
            "evidence_summary",
            "completeness",
        ] {
            assert_eq!(second[field], first[field], "{field}");
        }
        assert_eq!(second["relations"].as_array().unwrap().len(), 1);
        assert_ne!(
            second["relations"][0]["relation_id"],
            first["relations"][0]["relation_id"]
        );
        assert_eq!(second["output"]["remaining_relations"], 0);
        assert!(second["output"]["continuation"]["next_cursor"].is_null());
        let historical = service
            .execute("find_implementations", args.clone(), deadline())
            .unwrap();
        assert_eq!(historical["relations"], first["relations"]);
        assert_eq!(
            historical["implementation_counts"],
            first["implementation_counts"]
        );
        let mut unpublished = args.clone();
        unpublished["semantic_epoch"] = json!(u64::MAX);
        assert_eq!(
            code(
                service
                    .execute("find_implementations", unpublished, deadline())
                    .unwrap_err()
            ),
            "invalid_argument"
        );
        let mut unavailable = args;
        unavailable["snapshot_id"] = json!("unavailable-snapshot");
        assert_eq!(
            code(
                service
                    .execute("find_implementations", unavailable, deadline())
                    .unwrap_err()
            ),
            "snapshot_expired"
        );
    }

    #[test]
    fn cursor_cache_shares_payload_and_enforces_count_bytes_expiry_and_oversized_policy() {
        let result = json!({"snapshot_id":"fixed", "relations":[{"id":1},{"id":2}], "history":{"relations":[{"id":3}]}});
        let mut page = Page::new(
            "find_references".into(),
            json!({"limit":1}),
            result.clone(),
            vec!["relations".into(), "history.relations".into()],
            0,
        );
        assert!(page.metadata["relations"].is_null());
        assert!(page.metadata["history"]["relations"].is_null());
        assert_eq!(page.lists["history.relations"][0]["id"], 3);
        let mut cache = PageCache::default();
        let budget = page.bytes + 3 * 256;
        cache.insert("a".into(), page.clone(), 3, budget).unwrap();
        page.offset = 1;
        cache.insert("b".into(), page.clone(), 3, budget).unwrap();
        assert!(Arc::ptr_eq(&cache.items["a"].data, &cache.items["b"].data));
        assert_eq!(cache.bytes(), page.bytes + 512);
        cache.trim(1, budget);
        assert_eq!(cache.items.len(), 1);
        cache
            .insert("newest".into(), page.clone(), 1, budget)
            .unwrap();
        assert!(cache.items.contains_key("newest"));
        cache.trim(10, page.bytes + 255);
        assert!(cache.items.is_empty());
        assert!(
            cache
                .insert("too-large".into(), page.clone(), 10, page.bytes)
                .is_err()
        );
        page.created = Instant::now() - Duration::from_secs(601);
        assert!(cache.insert("expired".into(), page, 10, budget).is_err());
        assert!(cache.items.is_empty());
    }

    #[test]
    fn nested_relation_pages_cover_each_item_and_replay_the_same_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let resolved = crate::config::Resolved {
            root: temp.path().into(),
            config_path: None,
            config: crate::config::Config::default(),
            workspace_id: "page-test".into(),
            cache: temp.path().join("cache"),
            index_dir: temp.path().join("index"),
            config_dir: temp.path().join("config"),
        };
        let service = QueryService::new(Engine::new(resolved).unwrap());
        let original = json!({"snapshot_id":"fixed", "semantic_epoch":42,"output":{},"relations":[{"id":1},{"id":2}], "history":{"relations":[{"id":3},{"id":4}]}});
        let mut response = service
            .page(Page::new(
                "find_references".into(),
                json!({"limit":1}),
                original,
                vec!["relations".into(), "history.relations".into()],
                0,
            ))
            .unwrap();
        let first_cursor = response["output"]["continuation"]["next_cursor"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut ids = vec![];
        loop {
            assert_eq!(response["snapshot_id"], "fixed");
            assert_eq!(response["semantic_epoch"], 42);
            ids.extend(
                list_at(&response, "relations")
                    .iter()
                    .chain(list_at(&response, "history.relations"))
                    .map(|v| v["id"].as_u64().unwrap()),
            );
            let Some(cursor) = response["output"]["continuation"]["next_cursor"].as_str() else {
                break;
            };
            response = service
                .continue_page("find_references", &json!({"cursor":cursor}), cursor)
                .unwrap();
        }
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert_eq!(
            service
                .continue_page(
                    "find_references",
                    &json!({"cursor":first_cursor}),
                    &first_cursor
                )
                .unwrap()["relations"][0]["id"],
            2
        );
        service
            .engine
            .state
            .lock()
            .unwrap()
            .resolved
            .config
            .index
            .cursor_cache_count = 0;
        assert!(
            service
                .continue_page(
                    "find_references",
                    &json!({"cursor":first_cursor}),
                    &first_cursor
                )
                .is_err()
        );
    }

    fn stored(files: &[(&str, &str)]) -> StoredView {
        stored_with_namespaces(files, BTreeMap::new())
    }

    fn stored_with_namespaces(
        files: &[(&str, &str)],
        namespaces: BTreeMap<String, String>,
    ) -> StoredView {
        let manifest = Manifest {
            workspace_id: "ws".into(),
            files: files
                .iter()
                .map(|(path, text)| ((*path).to_string(), SourceFile::new(*text)))
                .collect(),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        };
        let syntax = RustParser::new().unwrap().analyze(&manifest).unwrap();
        let view = RefreshCoordinator::default()
            .publish_inputs(manifest, syntax)
            .unwrap();
        let mut stored = StoredView::new(view, vec![], vec![]);
        stored.namespaces = Arc::new(namespaces);
        stored
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn code(error: anyhow::Error) -> String {
        error_value(&error)["error"]["code"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn bare(name: &str, kind: &str, file: &str, qualified_name: &str) -> Symbol {
        Symbol {
            symbol_id: format!("{file}::{name}"),
            name: name.into(),
            qualified_name: qualified_name.into(),
            kind: kind.into(),
            file: file.into(),
            file_hash: "hash".into(),
            span: Span {
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            },
            name_span: Span {
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            },
            signature: String::new(),
        }
    }

    fn relation(
        id: &str,
        evidence: &[(&str, Certainty)],
        certainty: Certainty,
        freshness: &str,
    ) -> Relation {
        Relation {
            relation_id: id.into(),
            kind: "call".into(),
            from_symbol: "a".into(),
            to_symbol: "b".into(),
            file: "src/lib.rs".into(),
            span: Span {
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            },
            certainty: certainty.clone(),
            freshness: freshness.into(),
            runtime: "unavailable".into(),
            evidence: evidence
                .iter()
                .map(|(source, certainty)| Evidence {
                    evidence_id: format!("{id}-{source}"),
                    source: (*source).into(),
                    certainty: certainty.clone(),
                    indexer: "test".into(),
                    derivation: "test".into(),
                    file_hash: "hash".into(),
                })
                .collect(),
            origin: None,
        }
    }

    #[test]
    fn validate_enforces_cross_parameter_rules() {
        assert!(validate("capabilities", &json!({})).is_ok());
        assert!(validate("search_symbols", &json!({"query":"x"})).is_ok());
        assert!(validate("call_graph", &json!({"symbol":"a","cursor":"c"})).is_ok());
        assert_eq!(
            code(validate("search_symbols", &json!({})).unwrap_err()),
            "invalid_argument"
        );
        assert!(
            format!(
                "{}",
                validate("search_symbols", &json!({"query":"x","bogus":1})).unwrap_err()
            )
            .contains("unknown parameter")
        );
        assert!(
            validate("search_symbols", &json!({"query":"x","semantic_epoch":1})).is_err(),
            "semantic_epoch needs snapshot_id"
        );
        assert!(
            validate(
                "search_symbols",
                &json!({"query":"x","timeout_ms":1000,"fresh_wait_ms":600})
            )
            .is_err(),
            "fresh_wait_ms must fit in half the timeout"
        );
        assert!(validate("impact_of", &json!({})).is_err());
        assert!(
            validate(
                "impact_of",
                &json!({"symbol":"a","diff":{"base":{"kind":"workspace"},"head":{"kind":"workspace"}}})
            )
            .is_err()
        );
        assert!(validate("context_pack", &json!({"budget":4096})).is_err());
        assert!(
            validate(
                "context_pack",
                &json!({"budget":4096,"symbol":"a","task":"b"})
            )
            .is_err()
        );
        assert!(
            validate(
                "find_references",
                &json!({"symbol":"a","include_stale":true})
            )
            .is_err()
        );
        assert!(
            validate(
                "find_references",
                &json!({"symbol":"a","freshness":["stale"]})
            )
            .is_err()
        );
        assert!(
            validate(
                "find_references",
                &json!({"symbol":"a","freshness":["stale"],"include_stale":true,
                    "stale_from":{"snapshot_id":"s","semantic_epoch":0}})
            )
            .is_ok()
        );
        assert_eq!(
            code(validate("no_such_tool", &json!({})).unwrap_err()),
            "capability_unsupported"
        );
    }

    #[test]
    fn schemas_keep_required_fields_and_cursor_alternatives() {
        assert_eq!(TOOL_NAMES.len(), tools().len());
        let search = tool_schema("search_symbols");
        assert_eq!(search["anyOf"][1]["required"], json!(["query"]));
        assert_eq!(search["required"], json!([]));
        assert_eq!(search["properties"]["match"]["default"], "fuzzy");
        let context = tool_schema("context_pack");
        assert_eq!(context["anyOf"][1]["required"], json!(["budget"]));
        assert_eq!(context["properties"]["budget"]["minimum"], 1);
        assert_eq!(context["properties"]["budget"]["maximum"], 10_000_000);
        let calls = tool_schema("call_graph");
        assert_eq!(calls["properties"]["direction"]["default"], "callers");
        assert_eq!(calls["properties"]["depth"]["maximum"], 10);
        assert!(calls["properties"]["source"]["items"]["enum"].is_array());
        let impact = tool_schema("impact_of");
        assert!(impact["properties"]["diff"]["required"].is_array());
        assert_eq!(tools().len(), 19);
        let implementations = tool_schema("find_implementations");
        assert_eq!(implementations["anyOf"][1]["required"], json!(["symbol"]));
        let advertised = capabilities();
        assert_eq!(advertised["tools"], json!(tools()));
        assert!(
            advertised["commands"]
                .as_array()
                .unwrap()
                .contains(&json!("impls"))
        );
        assert!(
            advertised["commands"]
                .as_array()
                .unwrap()
                .contains(&json!("verify"))
        );
        assert!(
            advertised["commands"]
                .as_array()
                .unwrap()
                .contains(&json!("report"))
        );
        assert_eq!(
            advertised["ci_reports"]["sarif_scope"],
            "verification findings"
        );
        assert_eq!(advertised["rules"].as_array().unwrap().len(), 4);
        assert!(
            !advertised["unsupported"]
                .as_array()
                .unwrap()
                .contains(&json!("verify_change"))
        );
    }

    #[test]
    fn name_matching_and_path_prefixes_are_literal() {
        assert!(fuzzy("shop::stock", "stk"));
        assert!(fuzzy("shop::stock", "STK"));
        assert!(!fuzzy("shop::stock", "kcots"));
        assert!(!fuzzy("st", "stock"));
        assert!(prefix("src/lib.rs", &json!({})));
        assert!(prefix("src/lib.rs", &json!({"path_prefix":"."})));
        assert!(prefix("src/lib.rs", &json!({"path_prefix":"src"})));
        assert!(!prefix("source/lib.rs", &json!({"path_prefix":"src"})));
        assert!(prefix("src/lib.rs", &json!({"path_prefix":"src/"})));
        let function = bare("f", "function", "src/lib.rs", "f");
        let structure = bare("S", "struct", "src/lib.rs", "S");
        assert!(kind_matches(&structure, Some("type")));
        assert!(kind_matches(&function, None));
        assert!(!kind_matches(&structure, Some("function")));
        assert!(kind_matches(&function, Some("function")));
    }

    #[test]
    fn public_names_depend_on_the_module_layout() {
        assert_eq!(
            public_name(&bare(
                "alpha",
                "function",
                "src/lib.rs",
                "src/lib.rs::alpha"
            )),
            "alpha"
        );
        assert_eq!(
            public_name(&bare(
                "create",
                "function",
                "src/order.rs",
                "src/order.rs::create"
            )),
            "order::create"
        );
        assert_eq!(
            public_name(&bare(
                "Widget",
                "struct",
                "src/foo/mod.rs",
                "src/foo/mod.rs::Widget"
            )),
            "foo::Widget"
        );
        assert_eq!(
            public_name(&bare(
                "Thing",
                "struct",
                "crates/a/src/lib.rs",
                "crates/a/src/lib.rs::Thing"
            )),
            "Thing"
        );
        let namespaced = stored_with_namespaces(
            &[("src/lib.rs", "pub fn alpha() {}\n")],
            BTreeMap::from([(String::new(), "shop".to_string())]),
        );
        let alpha = resolve(&namespaced, "alpha").unwrap();
        assert_eq!(qualified_name(alpha, &namespaced), "shop::alpha");
        assert!(resolve(&namespaced, "shop::alpha").is_ok());
    }

    #[test]
    fn resolve_reports_not_found_and_ambiguity() {
        let stored = stored(&[
            ("src/lib.rs", "pub fn create() {}\n"),
            ("src/order.rs", "pub fn create() {}\n"),
        ]);
        assert!(resolve(&stored, "order::create").is_ok());
        assert!(resolve(&stored, "src/order.rs::create").is_ok());
        assert_eq!(
            code(resolve(&stored, "missing").unwrap_err()),
            "symbol_not_found"
        );
        let ambiguous = resolve(&stored, "create").unwrap_err();
        assert_eq!(code(ambiguous), "ambiguous_symbol");
        // The payload names both candidates so a caller can disambiguate.
        let details =
            error_value(&resolve(&stored, "create").unwrap_err())["error"]["details"]["candidates"]
                .as_array()
                .unwrap()
                .len();
        assert_eq!(details, 2);
    }

    #[test]
    fn source_filters_recompute_certainty_and_drop_empty_evidence() {
        let relations = vec![
            relation(
                "r1",
                &[("L1", Certainty::Candidate), ("L3", Certainty::Definite)],
                Certainty::Definite,
                "current",
            ),
            relation(
                "r2",
                &[("L1", Certainty::Candidate)],
                Certainty::Candidate,
                "pending",
            ),
        ];
        let filtered_l3 = filtered(relations.clone(), &json!({"source":["L3"]}));
        assert_eq!(filtered_l3.len(), 1);
        assert_eq!(filtered_l3[0].relation_id, "r1");
        assert_eq!(filtered_l3[0].evidence.len(), 1);
        assert_eq!(filtered_l3[0].certainty, Certainty::Definite);

        let current = filtered(relations.clone(), &json!({"freshness":["current"]}));
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].relation_id, "r1");

        let candidates = filtered(relations, &json!({"certainty":["candidate"]}));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].relation_id, "r2");
    }

    #[test]
    fn summaries_count_evidence_sources_and_dimensions() {
        let relations = vec![
            relation(
                "r1",
                &[("L3", Certainty::Definite)],
                Certainty::Definite,
                "current",
            ),
            relation(
                "r2",
                &[("L1", Certainty::Candidate), ("L3", Certainty::Definite)],
                Certainty::Candidate,
                "stale",
            ),
        ];
        let summary = summary(&relations, "partial");
        assert_eq!(summary["basis"], "partial");
        assert_eq!(summary["exact"], false);
        assert_eq!(summary["relation_count"], 2);
        assert_eq!(summary["evidence_count"], 3);
        assert_eq!(summary["source"]["L1"], 1);
        assert_eq!(summary["source"]["L3"], 2);
        assert_eq!(summary["certainty"]["definite"], 1);
        assert_eq!(summary["certainty"]["candidate"], 1);
        assert_eq!(summary["freshness"]["current"], 1);
        assert_eq!(summary["freshness"]["stale"], 1);
        assert_eq!(summary["runtime"]["unavailable"], 2);
    }

    #[test]
    fn traversal_respects_direction_depth_and_node_limits() {
        let stored = stored(&[(
            "src/lib.rs",
            "pub fn a() { b(); }\npub fn b() { c(); }\npub fn c() {}\n",
        )]);
        let id = |name: &str| {
            stored
                .view
                .snapshot
                .syntax
                .symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap()
                .symbol_id
                .clone()
        };
        let graph = traverse(
            &stored,
            &id("a"),
            &json!({"direction":"callees","depth":1,"max_nodes":1000}),
            false,
            deadline(),
        );
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].0.to_symbol, id("b"));
        assert_eq!(graph.edges[0].1, 1);
        assert_eq!(graph.frontier, vec![id("b")]);
        assert_eq!(graph.reason, Some("depth_limit"));
        assert_eq!(graph.basis, "within_limits");

        let graph = traverse(
            &stored,
            &id("a"),
            &json!({"direction":"callees","depth":5,"max_nodes":2}),
            false,
            deadline(),
        );
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.reason, Some("node_limit"));
        assert_eq!(graph.basis, "partial");
        assert!(graph.frontier.contains(&id("b")));

        let graph = traverse(
            &stored,
            &id("b"),
            &json!({"direction":"callers","depth":1}),
            false,
            deadline(),
        );
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].0.from_symbol, id("a"));
        assert!(graph.frontier.is_empty());
        assert_eq!(graph.reason, None);
        assert_eq!(graph.basis, "complete");
    }

    #[test]
    fn graph_cycles_keep_shortest_paths_and_stronger_truncation_reasons() {
        let stored = stored(&[(
            "src/lib.rs",
            "pub fn a() { b(); c(); }\npub fn b() { c(); }\npub fn c() { a(); }\n",
        )]);
        let a = resolve(&stored, "a").unwrap();
        let graph = traverse(
            &stored,
            &a.symbol_id,
            &json!({"direction":"callees","depth":5,"max_nodes":10}),
            false,
            deadline(),
        );
        assert_eq!(graph.nodes.len(), 3);
        assert!(graph.reason.is_none());
        assert!(
            graph
                .edges
                .iter()
                .filter(|(r, _, _)| r.from_symbol == a.symbol_id)
                .all(|(_, depth, path)| *depth == 1 && path.len() == 1)
        );
        assert_eq!(
            truncation_reason(Some("node_limit"), Some("depth_limit")),
            Some("node_limit")
        );
        assert_eq!(
            truncation_reason(Some("depth_limit"), Some("timeout")),
            Some("timeout")
        );
    }

    #[test]
    fn caller_paths_follow_call_direction_and_interruption_keeps_the_frontier() {
        let stored = stored(&[(
            "src/lib.rs",
            "pub fn a() { b(); }\npub fn b() { c(); }\npub fn c() {}\n",
        )]);
        let c = resolve(&stored, "c").unwrap();
        let args = json!({"direction":"callers","depth":5,"max_nodes":10});
        let graph = traverse(&stored, &c.symbol_id, &args, false, deadline());
        let (edge, _, path) = graph
            .edges
            .iter()
            .find(|(_, depth, _)| *depth == 2)
            .unwrap();
        assert_eq!(path[0], edge.relation_id);
        let last = graph
            .edges
            .iter()
            .find(|(r, _, _)| r.relation_id == path[1])
            .unwrap();
        assert_eq!(edge.to_symbol, last.0.from_symbol);
        assert_eq!(last.0.to_symbol, c.symbol_id);

        let timed_out = traverse(&stored, &c.symbol_id, &args, false, Instant::now());
        assert_eq!(timed_out.reason, Some("timeout"));
        assert_eq!(timed_out.basis, "partial");
        assert_eq!(timed_out.frontier, std::slice::from_ref(&c.symbol_id));

        let _guard = CancelGuard;
        REQUEST_CANCEL.with(|flag| *flag.borrow_mut() = Some(Arc::new(AtomicBool::new(true))));
        let cancelled = traverse(&stored, &c.symbol_id, &args, false, deadline());
        assert_eq!(cancelled.reason, Some("cancelled"));
        assert_eq!(cancelled.basis, "partial");
        assert_eq!(cancelled.frontier, std::slice::from_ref(&c.symbol_id));
    }

    #[test]
    fn indexed_queries_filter_evidence_without_mutating_shared_facts_or_spending_limits() {
        let syntax = stored(&[("src/lib.rs", "pub fn a() { b(); }\npub fn b() {}\n")]);
        let mut semantic = crate::engine::syntax_relations(&syntax.view).remove(0);
        semantic.certainty = Certainty::Definite;
        semantic.evidence[0].source = "L3".into();
        semantic.evidence[0].certainty = Certainty::Definite;
        let indexed = StoredView::new(syntax.view.clone(), vec![semantic], vec![]);
        let a = resolve(&indexed, "a").unwrap();
        let graph = |source, certainty| {
            traverse(
                &indexed,
                &a.symbol_id,
                &json!({"direction":"callees","depth":1,"max_nodes":2,"source":[source],"certainty":[certainty]}),
                false,
                deadline(),
            )
        };
        let l1 = graph("L1", "candidate");
        assert_eq!(l1.edges.len(), 1);
        assert_eq!(l1.edges[0].0.evidence.len(), 1);
        assert_eq!(l1.edges[0].0.evidence[0].source, "L1");
        let l3 = graph("L3", "definite");
        assert_eq!(l3.edges.len(), 1);
        assert_eq!(l3.edges[0].0.evidence[0].source, "L3");
        assert_eq!(indexed.full_index().relations()[0].evidence.len(), 2);
        assert_eq!(
            indexed.full_index().relations()[0].certainty,
            Certainty::Definite
        );
        let excluded = traverse(
            &indexed,
            &a.symbol_id,
            &json!({"direction":"both","depth":0,"max_nodes":1,"source":["L2"]}),
            false,
            deadline(),
        );
        assert!(excluded.edges.is_empty());
        assert!(excluded.frontier.is_empty());
        assert_eq!(excluded.basis, "complete");
        assert_eq!(excluded.reason, None);
    }

    #[test]
    fn context_pack_respects_budget_and_include_filters() {
        let stored = stored(&[(
            "src/lib.rs",
            "pub fn alpha() { beta(); }\npub fn beta() {}\n",
        )]);
        let args = json!({"budget":8192,"symbol":"beta","include":["definition","callers"],
            "tokenizer":"estimated"});
        let value = context_pack(envelope(&stored, json!({})), &stored, &args, deadline()).unwrap();
        assert_eq!(value["token_count"]["mode"], "estimated");
        assert!(value["token_count"]["total"].as_u64().unwrap() <= 8192);
        let items = value["items"].as_array().unwrap();
        assert!(
            items
                .iter()
                .any(|i| i["reason"] == "definition" && i["qualified_name"] == "beta")
        );
        assert!(
            items
                .iter()
                .any(|i| i["reason"] == "callers" && i["qualified_name"] == "alpha")
        );
        assert!(!items.iter().any(|i| i["reason"] == "tests"));

        let error = context_pack(
            envelope(&stored, json!({})),
            &stored,
            &json!({"budget":10,"symbol":"beta"}),
            deadline(),
        )
        .unwrap_err();
        assert_eq!(code(error), "budget_too_small");
    }

    #[test]
    fn symbol_at_resolves_declarations_and_references() {
        let stored = stored(&[(
            "src/lib.rs",
            "pub fn alpha() {}\npub fn caller() { alpha(); }\n",
        )]);
        let mut result = envelope(&stored, json!({}));
        symbol_at(
            &mut result,
            &stored,
            &json!({"file":"src/lib.rs","line":1,"column":8}),
        )
        .unwrap();
        assert_eq!(result["symbols"][0]["name"], "alpha");
        assert_eq!(result["symbols"][0]["certainty"], "definite");
        assert_eq!(result["occurrence"]["kind"], "declaration");

        let mut result = envelope(&stored, json!({}));
        symbol_at(
            &mut result,
            &stored,
            &json!({"file":"src/lib.rs","line":2,"column":20}),
        )
        .unwrap();
        assert_eq!(result["occurrence"]["kind"], "call");
        assert_eq!(result["symbols"][0]["name"], "alpha");
        assert_eq!(result["symbols"][0]["certainty"], "candidate");

        let mut result = envelope(&stored, json!({}));
        let error = symbol_at(
            &mut result,
            &stored,
            &json!({"file":"src/lib.rs","line":1,"column":8,"file_hash":"wrong"}),
        )
        .unwrap_err();
        assert_eq!(code(error), "content_mismatch");
        let error = symbol_at(
            &mut result,
            &stored,
            &json!({"file":"outside.rs","line":1,"column":1}),
        )
        .unwrap_err();
        assert_eq!(code(error), "file_not_found");
        let error = symbol_at(
            &mut result,
            &stored,
            &json!({"file":"../escape.rs","line":1,"column":1}),
        )
        .unwrap_err();
        assert_eq!(code(error), "invalid_argument");
    }

    #[test]
    fn repo_map_lists_auto_targets_and_declared_dependencies() {
        let stored = stored(&[
            (
                "Cargo.toml",
                "[package]\nname = \"shop\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
            ),
            ("src/lib.rs", "pub fn alpha() {}\n"),
            (
                "src/main.rs",
                "fn main() {}\nmod helper_0 { fn main() {} }\nmod helper_1 { fn main() {} }\nmod helper_2 { fn main() {} }\nmod helper_3 { fn main() {} }\nmod helper_4 { fn main() {} }\nmod helper_5 { fn main() {} }\n",
            ),
            ("src/bin/demo.rs", "fn main() {}\n"),
        ]);
        let mut result = envelope(&stored, json!({}));
        crate::structure::repo_map(
            &mut result,
            &stored,
            &json!({"dependencies":"crate","max_symbols_per_module":10}),
            deadline(),
        )
        .unwrap();
        let entries = result["entries"].as_array().unwrap();
        let main = entries
            .iter()
            .find(|e| e["target"] == "shop" && e["kind"] == "bin")
            .unwrap();
        let symbol = stored
            .index()
            .symbol(main["main_symbol_id"].as_str().unwrap())
            .unwrap();
        assert_eq!(symbol.qualified_name, "src/main.rs::main");
        assert_eq!(symbol.name_span.start.line, 1);
        assert!(
            entries
                .iter()
                .any(|e| e["target"] == "shop" && e["kind"] == "lib" && e["file"] == "src/lib.rs")
        );
        assert!(
            entries
                .iter()
                .any(|e| e["target"] == "shop" && e["kind"] == "bin" && e["file"] == "src/main.rs")
        );
        assert!(
            entries
                .iter()
                .any(|e| e["target"] == "demo" && e["kind"] == "bin")
        );
        let dependencies = result["dependencies"].as_array().unwrap();
        assert!(dependencies.iter().any(|d| {
            d["dependency"] == "serde" && d["to"] == "external dependencies" && d["source"] == "L1"
        }));
        assert_eq!(result["dependency_kind"], "crate");
        assert!(!result["modules"].as_array().unwrap().is_empty());
        assert!(result["evidence_summary"]["relation_count"].is_u64());
    }

    #[test]
    fn symbol_values_carry_docs_bodies_visibility_and_scope() {
        let stored = stored(&[(
            "src/lib.rs",
            "//! crate docs\n/// Alpha docs.\n#[inline]\npub fn alpha(x: u8) -> u8 { x }\n#[test]\nfn check() {}\n",
        )]);
        let alpha = resolve(&stored, "alpha").unwrap();
        let value = symbol_value(alpha, &stored, true, true);
        assert!(value["docs"].as_str().unwrap().contains("Alpha docs."));
        assert_eq!(value["visibility"], "public");
        assert!(value["body"].as_str().unwrap().starts_with("pub fn alpha"));
        assert_eq!(value["scope"], "production");
        assert_eq!(
            test_scope(resolve(&stored, "check").unwrap(), &stored),
            "test"
        );
        let hidden = symbol_value(alpha, &stored, false, false);
        assert!(hidden["body"].is_null());
        assert!(hidden["docs"].is_null());
    }
}
