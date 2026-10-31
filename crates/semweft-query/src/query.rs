use crate::{
    engine::{Engine, StoredView},
    git,
};
use anyhow::{Context, Result, bail};
use semweft_core::*;
use semweft_store::{Direction, IndexedView};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
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
        "languages":{"rust":{"L1":{"status":"experimental","indexer":semweft_lang_rust::PARSER_VERSION,"binding":"name candidates; lexical scope unresolved"},"L2":{"status":"unsupported"},"batch_L3_scip":{"status":"experimental","format":"binary SCIP 0.10","source_manifest_required":true,"requires_build":false,"requires_trust":false,"producer":"external index with tool name and version; producer execution is outside import"},"resident_L3":{"status":"experimental","backend":"external rust-analyzer LSP","queries":["definition","implementation","references","diagnostic"],"requires_build":true,"requires_trust":true,"tool":"rust-analyzer","tool_available":tool_available(),"authorization":"semweft trust --accept-build-execution"}},"go":go_capabilities(),"typescript_javascript":{"status":if cfg!(feature="web-experimental") {"experimental"} else {"unsupported"},"enable":"--features web-experimental","L1":{"binding":"syntax name candidates","extensions":["ts","tsx","js","jsx","mjs","cjs","mts","cts"]},"batch_L3_scip":{"source_manifest_required":true},"resident_L3":{"status":"unsupported"},"dependency_depth":0}},
        "rules":[],"tokenizers":[{"id":"estimated","mode":"estimated","method":"one token per UTF-8 byte of compact serialized JSON, including metadata; conservative estimate","min_budget":MIN_BUDGET},
            {"id":crate::tokens::CL100K,"mode":"exact","method":"offline BPE over complete compact JSON","min_budget":MIN_BUDGET},
            {"id":crate::tokens::O200K,"mode":"exact","method":"offline BPE over complete compact JSON","min_budget":MIN_BUDGET}],
        "limits":{"timeout_ms":5000,"max_timeout_ms":60000,"fresh_wait_ms":2000,"max_fresh_wait_fraction":0.5,"limit":100,"max_limit":1000,"depth":3,"max_depth":10,"max_nodes":100000,"dependency_depth":"nonnegative u32; offline Cargo resolution requires repository trust","snapshot_retention":"unlimited by default; configurable retention with durable pins and safe GC","max_cursors":256,"default_max_cursor_bytes":67108864,"cursor_limits_configurable":true},
        "unsupported":["verify_change","GUI","IDL","HTTP MCP"],"commands":["benchmark-storage","snapshots pin","snapshots unpin","snapshots gc","snapshots usage","snapshots compact","init","trust","index","doctor","capabilities","status","map","search","symbol-at","symbol","refs","calls","impact","context","mcp","prototype scan","prototype check"]})
}

fn go_capabilities() -> Value {
    if cfg!(feature = "go-experimental") {
        json!({"status":"experimental","L1":{"status":"experimental","binding":"syntax name candidates"},"batch_L3_scip":{"status":"experimental","source_manifest_required":true},"resident_L3":{"status":"experimental","backend":"scip-go","refresh":"full project","requires_trust":true},"build_systems":["go.mod","go.work"],"dependency_depth":0})
    } else {
        json!({"status":"unsupported","enable":"cargo install --features go-experimental"})
    }
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
    bytes: usize,
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
    fn new(
        tool: String,
        args: Value,
        mut result: Value,
        fields: Vec<String>,
        offset: usize,
    ) -> Self {
        let bytes =
            semweft_store::payload_bytes(&(&tool, &args, &result, &fields)).saturating_mul(3);
        let lists = fields
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
        Self {
            data: Arc::new(PageData {
                tool,
                args,
                metadata: result,
                fields,
                lists,
                bytes,
            }),
            offset,
            created: Instant::now(),
            saved: Instant::now(),
        }
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
        let mut cancellations = self.cancellations.lock().unwrap();
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
            self.cancellations.lock().unwrap().remove(id);
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
            let mut pages = self.pages.lock().unwrap();
            pages.trim(count, bytes);
            result["storage"]["cursor_cache"] = json!({"cursors":pages.items.len(),"estimated_bytes":pages.bytes(),"max_count":count,"max_bytes":bytes,"evictions":pages.evictions});
            return Ok(result);
        }
        if let Some(cursor) = args["cursor"].as_str() {
            return self.continue_page(tool, &args, cursor);
        }
        let mut result = if tool == "impact_of" && args.get("diff").is_some() {
            git::impact(self, &args, deadline)?
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
            self.compute(tool, &args, &stored, fresh, deadline)?
        };
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
        let fields = match tool {
            "search_symbols" | "symbol_at" => vec!["symbols"],
            "repo_map" => vec!["modules", "dependencies", "entries"],
            "find_references" => {
                if result.get("history").is_some() {
                    vec!["relations", "history.relations"]
                } else {
                    vec!["relations"]
                }
            }
            "impact_of" if result.get("relation_changes").is_some() => vec![
                "relations",
                "relation_changes.added",
                "relation_changes.removed",
            ],
            "call_graph" | "impact_of" => vec!["relations"],
            _ => vec![],
        };
        if matches!(tool, "search_symbols" | "repo_map") && Instant::now() >= deadline {
            mark_timeout(&mut result, &args);
        }
        let mut response = self.page(Page::new(
            tool.into(),
            args.clone(),
            result,
            fields.into_iter().map(str::to_owned).collect(),
            0,
        ))?;
        if matches!(tool, "search_symbols" | "repo_map") && Instant::now() >= deadline {
            mark_timeout(&mut response, &args);
        }
        Ok(response)
    }
    fn page_limits(&self) -> (usize, usize) {
        let state = self.engine.state.lock().unwrap();
        (
            state.resolved.config.index.cursor_cache_count,
            state.resolved.config.index.cursor_cache_bytes,
        )
    }
    fn continue_page(&self, tool: &str, args: &Value, cursor: &str) -> Result<Value> {
        let (max_count, max_bytes) = self.page_limits();
        let page = {
            let mut pages = self.pages.lock().unwrap();
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
        let limit = page.args["limit"].as_u64().unwrap_or(100) as usize;
        let total: usize = page
            .fields
            .iter()
            .map(|key| page.lists.get(key).map(Vec::as_slice).unwrap_or(&[]).len())
            .sum();
        let start = page.offset;
        let mut end = (start + limit).min(total);
        let mut response = page.metadata.clone();
        let mut position = 0;
        for key in &page.fields {
            let values = page.lists.get(key).map(Vec::as_slice).unwrap_or(&[]);
            let a = start.saturating_sub(position).min(values.len());
            let b = end.saturating_sub(position).min(values.len());
            set_list(&mut response, key, values[a..b].to_vec());
            position += values.len();
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
            let length = page.lists.get(key).map(Vec::as_slice).unwrap_or(&[]).len();
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
        let mut next = self.next.lock().unwrap();
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
            "search_symbols" => {
                let query = args["query"].as_str().unwrap();
                let mode = args["match"].as_str().unwrap_or("fuzzy");
                let mut matches = BTreeMap::new();
                let computed = (|| -> Result<()> {
                    for s in &stored.view.snapshot.syntax.symbols {
                        check_deadline(deadline)?;
                        if prefix(&s.file, args)
                            && kind_matches(s, args["kind"].as_str())
                            && match mode {
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
                            }
                        {
                            let key = (
                                s.name != query,
                                public_name(s),
                                &s.file,
                                s.name_span,
                                &s.symbol_id,
                            );
                            let value = symbol_value(s, stored, false, true);
                            matches.insert(key, value);
                            check_deadline(deadline)?;
                        }
                    }
                    Ok(())
                })();
                result["symbols"] = Value::Array(matches.into_values().collect());
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
            "repo_map" => {
                let computed = crate::structure::repo_map(&mut result, stored, args, deadline);
                finish_computation(&mut result, args, computed)?;
            }
            "find_references" => {
                let target = resolve(stored, args["symbol"].as_str().unwrap())?;
                let edges = filtered(
                    stored
                        .index()
                        .adjacent(&target.symbol_id, Direction::Incoming)
                        .cloned()
                        .collect(),
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
                            .filter(|r| r.freshness == "current")
                            .cloned()
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
                let target = resolve(stored, args["symbol"].as_str().unwrap())?;
                let graph = traverse(
                    stored,
                    &target.symbol_id,
                    args,
                    tool == "impact_of",
                    deadline,
                );
                result["target"] = symbol_value(target, stored, false, true);
                result["relations"] = json!(
                    graph
                        .edges
                        .iter()
                        .map(|(r, depth, path)| {
                            let mut v = relation_value(r, stored);
                            v["depth"] = json!(depth);
                            v["path"] = json!(path);
                            v
                        })
                        .collect::<Vec<_>>()
                );
                result["nodes"] = json!(
                    graph
                        .nodes
                        .iter()
                        .filter_map(|id| stored.index().symbol(id))
                        .map(|s| symbol_value(s, stored, false, false))
                        .collect::<Vec<_>>()
                );
                result["frontier"] = json!(graph.frontier);
                result["evidence_summary"] = summary(
                    &graph
                        .edges
                        .iter()
                        .map(|(r, _, _)| r.clone())
                        .collect::<Vec<_>>(),
                    graph.basis,
                );
                if let Some(reason) = graph.reason {
                    result["output"] = json!({"truncated":true,"truncation_reason":reason,"remaining_relations":if graph.basis=="partial" {Value::Null} else {json!(0)},"continuation":{"kind":"requery","frontier":graph.frontier,"suggested":{"depth":(args["depth"].as_u64().unwrap_or(3)+1).min(10),"max_nodes":(args["max_nodes"].as_u64().unwrap_or(1000)*2).min(100000)}}});
                }
                if tool == "impact_of" {
                    let callers: BTreeSet<_> = graph
                        .edges
                        .iter()
                        .filter(|(r, _, _)| r.kind == "call")
                        .map(|(r, _, _)| r.from_symbol.clone())
                        .filter(|id| id != &target.symbol_id)
                        .collect();
                    let direct: BTreeSet<_> = graph
                        .edges
                        .iter()
                        .filter(|(r, d, _)| {
                            *d == 1 && r.kind == "call" && r.from_symbol != target.symbol_id
                        })
                        .map(|(r, _, _)| r.from_symbol.clone())
                        .collect();
                    result["summary"] = json!({"direct_callers":direct.len(),"transitive_callers":callers.len(),"services":null,"tests_to_run":null,"risk":null});
                    result["changes"] = json!([]);
                }
            }
            "context_pack" => return context_pack(result, stored, args, deadline),
            _ => bail!("capability_unsupported: {tool}"),
        }
        Ok(result)
    }
}

fn list_at<'a>(value: &'a Value, path: &str) -> &'a [Value] {
    let v = path.split('.').fold(value, |v, k| &v[k]);
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
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
fn qualified_name(symbol: &Symbol, stored: &StoredView) -> String {
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
    json!({"relation_id":relation.relation_id,"kind":relation.kind,"from":relation.from_symbol,"to":relation.to_symbol,"from_name":from.map(public_name).unwrap_or_else(||relation.from_symbol.clone()),"to_name":to.map(public_name),"location":{"file":relation.file,"line":relation.span.start.line,"column":relation.span.start.column,"file_hash":relation.origin.as_ref().map(|o|&o.file_hash).or_else(||stored.view.snapshot.manifest.files.get(&relation.file).map(|f|&f.file_hash)),"coordinate_snapshot_id":relation.origin.as_ref().map(|o|&o.snapshot_id).unwrap_or(&stored.view.snapshot.snapshot_id)},"span":relation.span,"certainty":relation.certainty,"freshness":relation.freshness,"runtime":relation.runtime,"evidence":relation.evidence,"origin":relation.origin,"scope":from.map(|s|test_scope(s,stored)).unwrap_or("production")})
}
pub fn filtered(relations: Vec<Relation>, args: &Value) -> Vec<Relation> {
    relations
        .into_iter()
        .filter_map(|r| filter_relation(r, args))
        .collect()
}
fn filter_relation(mut r: Relation, args: &Value) -> Option<Relation> {
    if !prefix(&r.file, args) {
        return None;
    }
    if let Some(sources) = args["source"].as_array() {
        r.evidence.retain(|e| sources.contains(&json!(e.source)));
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
    if args["certainty"]
        .as_array()
        .is_some_and(|c| !c.contains(&serde_json::to_value(&r.certainty).unwrap()))
        || args["freshness"]
            .as_array()
            .is_some_and(|c| !c.contains(&json!(r.freshness)))
    {
        return None;
    }
    Some(r)
}

pub fn summary(relations: &[Relation], basis: &str) -> Value {
    let mut source = BTreeMap::from([("L1", 0), ("L2", 0), ("L3", 0)]);
    let mut certainty = BTreeMap::from([("definite", 0), ("candidate", 0)]);
    let mut freshness = BTreeMap::from([("current", 0), ("pending", 0), ("stale", 0)]);
    let mut runtime = BTreeMap::from([("observed", 0), ("not_observed", 0), ("unavailable", 0)]);
    for r in relations {
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
    json!({"basis":basis,"exact":basis!="partial","relation_count":relations.len(),"evidence_count":relations.iter().map(|r|r.evidence.len()).sum::<usize>(),"source":source,"certainty":certainty,"freshness":freshness,"runtime":runtime})
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
        .map(|o| {
            stored
                .index()
                .relations_at(path, o.span)
                .cloned()
                .collect::<Vec<_>>()
        })
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
pub fn truncation_reason(
    previous: Option<&'static str>,
    next: Option<&'static str>,
) -> Option<&'static str> {
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
struct RelationGraph<'a> {
    index: &'a IndexedView,
    direction: Direction,
}
impl<'a> semweft_algo::graph::GraphView for RelationGraph<'a> {
    type Node = &'a str;
    type Edge = &'a str;

    fn neighbors(&self, node: &&'a str) -> impl Iterator<Item = (&'a str, &'a str)> {
        let index = self.index;
        let direction = self.direction;
        let node = *node;
        index.adjacent(node, direction).map(move |r| {
            let next = match direction {
                Direction::Incoming => r.from_symbol.as_str(),
                Direction::Outgoing => r.to_symbol.as_str(),
                Direction::Both if r.to_symbol == node => r.from_symbol.as_str(),
                Direction::Both => r.to_symbol.as_str(),
            };
            (r.relation_id.as_str(), next)
        })
    }
}

pub fn traverse(
    stored: &StoredView,
    target: &str,
    args: &Value,
    impact: bool,
    deadline: Instant,
) -> Graph {
    use semweft_algo::graph::{Interrupt, Limits, StopReason, breadth_first_filtered};
    let direction = match args["direction"].as_str().unwrap_or("callers") {
        "callees" => Direction::Outgoing,
        "both" => Direction::Both,
        _ => Direction::Incoming,
    };
    let index = stored.index();
    let graph = RelationGraph { index, direction };
    let traversal = breadth_first_filtered(
        &graph,
        target,
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
            let relation = index.relation(id).unwrap();
            (impact || relation.kind == "call") && filter_relation(relation.clone(), args).is_some()
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
    let edges = traversal
        .edges
        .into_iter()
        .map(|(id, visit)| {
            let mut path = visit.path;
            if direction != Direction::Outgoing {
                path.reverse();
            }
            (
                filter_relation(index.relation(id).unwrap().clone(), args).unwrap(),
                visit.depth,
                path.into_iter().map(str::to_owned).collect(),
            )
        })
        .collect();
    Graph {
        edges,
        nodes: traversal.nodes.into_iter().map(str::to_owned).collect(),
        frontier: traversal.frontier.into_iter().map(str::to_owned).collect(),
        basis,
        reason,
    }
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
            .or_default() += 1;
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
        let relations = stored
            .index()
            .adjacent(&root.symbol_id, Direction::Both)
            .collect::<Vec<_>>();
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
    use semweft_lang_rust::RustParser;
    use std::time::Duration;

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
        assert_eq!(tools().len(), 10);
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
        assert_eq!(indexed.index().relations()[0].evidence.len(), 2);
        assert_eq!(
            indexed.index().relations()[0].certainty,
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
