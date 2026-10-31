//! Minimal LSP probe transport, isolated from the eventual public query API.
use anyhow::{Context, Result, bail};
use semweft_core::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use url::Url;

pub fn version(executable: &Path, root: &Path) -> Result<String> {
    version_cancellable(executable, root, &Cancellation::default())
}

pub(crate) fn version_cancellable(
    executable: &Path,
    root: &Path,
    cancel: &Cancellation,
) -> Result<String> {
    cancel.check()?;
    let output = crate::process::output_cancellable(
        Command::new(executable).current_dir(root).arg("--version"),
        Instant::now() + Duration::from_secs(5),
        || cancel.is_cancelled(),
    )?
    .context("cancelled: semantic execution authorization revoked")?;
    if !output.status.success() {
        bail!("indexer_unavailable: rust-analyzer --version failed");
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

/// A generation of execution authorization. Revocation also interrupts blocked
/// stdin writes by terminating the producer independently of the worker thread.
#[derive(Default)]
pub(crate) struct Cancellation {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}
impl Cancellation {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.stop();
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("cancelled: semantic execution authorization revoked");
        }
        Ok(())
    }
    fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            crate::process::terminate_tree(&mut child);
        }
    }
}

pub struct LspClient {
    cancellation: Arc<Cancellation>,
    input: BufWriter<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
    root: PathBuf,
    source_manifest: Option<Manifest>,
    documents: BTreeMap<String, (String, i32)>,
    /// Hash of non-Rust inputs (Cargo manifests, lockfiles) last loaded.
    build_inputs: Option<String>,
    quiescent: bool,
}

impl LspClient {
    pub fn start(executable: &Path, root: &Path, deadline: Instant) -> Result<Self> {
        Self::start_cancellable(
            executable,
            root,
            deadline,
            Arc::new(Cancellation::default()),
        )
    }

    pub(crate) fn start_cancellable(
        executable: &Path,
        root: &Path,
        deadline: Instant,
        cancellation: Arc<Cancellation>,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut slot = cancellation.child.lock().unwrap();
        cancellation.check()?;
        let mut child = command
            .spawn()
            .context("indexer_unavailable: cannot start rust-analyzer")?;
        let input = BufWriter::new(child.stdin.take().unwrap());
        let output = child.stdout.take().unwrap();
        *slot = Some(child);
        drop(slot);
        let (sender, messages) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut output = BufReader::new(output);
            loop {
                let result = read_message(&mut output).map_err(|error| error.to_string());
                let failed = result.is_err();
                if sender.send(result).is_err() || failed {
                    break;
                }
            }
        });
        let mut client = Self {
            cancellation,
            input,
            messages,
            reader: Some(reader),
            next_id: 1,
            root: root.into(),
            source_manifest: None,
            documents: BTreeMap::new(),
            build_inputs: None,
            quiescent: false,
        };
        let root_uri = Url::from_directory_path(root)
            .map_err(|_| anyhow::anyhow!("invalid_argument: invalid workspace URI"))?;
        let result = client.request(
            "initialize",
            json!({
                "processId":std::process::id(), "rootUri":root_uri,
                "workspaceFolders":[{"uri":root_uri,"name":"semweft-prototype"}],
                "capabilities":{
                    "general":{"positionEncodings":["utf-16"]},
                    "workspace":{"configuration":true,"workspaceFolders":true},
                    "textDocument":{"definition":{"linkSupport":true},"implementation":{"linkSupport":true},"diagnostic":{}},
                    "experimental":{"serverStatusNotification":true}
                },
                "initializationOptions":settings()
            }),
            deadline,
        )?;
        if result
            .pointer("/capabilities/positionEncoding")
            .and_then(Value::as_str)
            .is_some_and(|encoding| encoding != "utf-16")
        {
            bail!("capability_unsupported: semantic backend must negotiate UTF-16 positions");
        }
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    /// False once the backend process has exited, for example after a crash.
    pub fn alive(&mut self) -> bool {
        self.cancellation
            .child
            .lock()
            .unwrap()
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// Fault injection for the prototype: terminate the backend abruptly.
    pub fn kill(&mut self) {
        self.cancellation.stop();
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        self.cancellation.check()?;
        let body = serde_json::to_vec(message)?;
        write!(self.input, "Content-Length: {}\r\n\r\n", body.len())?;
        self.input.write_all(&body)?;
        self.input.flush()?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc":"2.0","method":method,"params":params}))
    }

    fn receive(&mut self, deadline: Instant) -> Result<Value> {
        loop {
            self.cancellation.check()?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("timeout: semantic refresh deadline exceeded")?;
            match self
                .messages
                .recv_timeout(remaining.min(Duration::from_millis(10)))
            {
                Ok(message) => {
                    self.cancellation.check()?;
                    return message.map_err(|e| anyhow::anyhow!("index_refresh_failed: {e}"));
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("index_refresh_failed: semantic backend disconnected")
                }
            }
        }
    }

    fn service(&mut self, message: &Value) -> Result<()> {
        if std::env::var_os("SEMWEFT_LSP_TRACE").is_some() {
            eprintln!(
                "LSP received method={:?} id={:?}",
                message.get("method"),
                message.get("id")
            );
            if message.get("method").and_then(Value::as_str) == Some("experimental/serverStatus") {
                eprintln!("LSP server status: {}", message["params"]);
            }
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        if method == "experimental/serverStatus" {
            self.quiescent = message
                .pointer("/params/quiescent")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        }
        if let Some(id) = message.get("id") {
            let result = match method {
                "workspace/configuration" => {
                    let values = message
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| {
                                    let settings = settings();
                                    let section =
                                        item.get("section").and_then(Value::as_str).unwrap_or("");
                                    if section.is_empty() || section == "rust-analyzer" {
                                        settings
                                    } else {
                                        section
                                            .strip_prefix("rust-analyzer.")
                                            .unwrap_or(section)
                                            .split('.')
                                            .fold(&settings, |v, key| &v[key])
                                            .clone()
                                    }
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    json!(values)
                }
                "workspace/workspaceFolders" => {
                    json!([{"uri":Url::from_directory_path(&self.root).unwrap(),"name":"semweft-prototype"}])
                }
                "workspace/applyEdit" => {
                    json!({"applied":false,"failureReason":"Semweft is read-only"})
                }
                "client/registerCapability"
                | "client/unregisterCapability"
                | "window/workDoneProgress/create"
                | "window/showMessageRequest" => Value::Null,
                _ => {
                    self.send(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported client request"}}))?;
                    return Ok(());
                }
            };
            self.send(&json!({"jsonrpc":"2.0","id":id,"result":result}))?;
        }
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value> {
        // rust-analyzer can cancel a query while its database is changing.
        for _ in 0..4 {
            let id = self.next_id;
            self.next_id += 1;
            self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
            loop {
                let message = self.receive(deadline)?;
                if message.get("id").and_then(Value::as_u64) == Some(id)
                    && message.get("method").is_none()
                {
                    if let Some(error) = message.get("error") {
                        if matches!(
                            error.get("code").and_then(Value::as_i64),
                            Some(-32801 | -32802)
                        ) {
                            break;
                        }
                        bail!("index_refresh_failed: {method}: {error}");
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                self.service(&message)?;
            }
        }
        bail!("index_refresh_failed: semantic backend repeatedly cancelled {method}")
    }

    fn uri(&self, path: &str) -> Result<Url> {
        let disk = self
            .source_manifest
            .as_ref()
            .map(|m| crate::dependencies::source_path(&self.root, m, path))
            .unwrap_or_else(|| self.root.join(path));
        Url::from_file_path(disk)
            .map_err(|_| anyhow::anyhow!("invalid_argument: cannot encode file URI"))
    }

    pub fn sync(&mut self, snapshot: &Snapshot, deadline: Instant) -> Result<()> {
        self.source_manifest = Some(snapshot.manifest.clone());
        let build_inputs = identity(
            "build",
            &[
                &serde_json::to_string(
                    &snapshot
                        .manifest
                        .files
                        .iter()
                        .filter(|(p, _)| !p.ends_with(".rs"))
                        .map(|(p, f)| (p, &f.file_hash))
                        .collect::<Vec<_>>(),
                )?,
                &serde_json::to_string(&snapshot.manifest.binary_files)?,
            ],
        );
        if self
            .build_inputs
            .replace(build_inputs.clone())
            .is_some_and(|previous| previous != build_inputs)
        {
            // Reload barrier: status notifications read before this response
            // describe the old workspace, so wait for a fresh quiescent report.
            self.request("rust-analyzer/reloadWorkspace", Value::Null, deadline)?;
            self.quiescent = false;
        }
        let mut changed = false;
        let removed: Vec<_> = self
            .documents
            .keys()
            .filter(|p| !snapshot.manifest.files.contains_key(*p))
            .cloned()
            .collect();
        let mut events = vec![];
        for path in removed {
            let uri = self.uri(&path)?;
            self.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}))?;
            events.push(json!({"uri":uri,"type":3}));
            self.documents.remove(&path);
            changed = true;
        }
        for (path, file) in &snapshot.manifest.files {
            if !path.ends_with(".rs") {
                continue;
            }
            let uri = self.uri(path)?;
            match self.documents.get(path).cloned() {
                Some((hash, _)) if hash == file.file_hash => continue,
                Some((_, version)) => {
                    self.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":version+1}, "contentChanges":[{"text":file.content}]}))?;
                    self.documents
                        .insert(path.clone(), (file.file_hash.clone(), version + 1));
                    events.push(json!({"uri":uri,"type":2}));
                }
                None => {
                    self.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"rust","version":1,"text":file.content}}))?;
                    self.documents
                        .insert(path.clone(), (file.file_hash.clone(), 1));
                    events.push(json!({"uri":uri,"type":1}));
                }
            }
            changed = true;
        }
        if changed {
            // serverStatus describes project loading, not a document-version
            // acknowledgment. Some edits emit no new status notification.
            self.notify("workspace/didChangeWatchedFiles", json!({"changes":events}))?;
        }
        Ok(())
    }

    /// Queries the callsites in `only`. Returns `None` when `cancel` asks to
    /// stop; partial outcomes are discarded rather than returned.
    pub fn refresh(
        &mut self,
        snapshot: &Snapshot,
        backend: &str,
        deadline: Instant,
        only: &BTreeSet<String>,
        cancel: &mut dyn FnMut() -> bool,
    ) -> Result<Option<SemanticBatch>> {
        if cancel() {
            return Ok(None);
        }
        self.sync(snapshot, deadline)?;
        while !self.quiescent {
            let message = self.receive(deadline)?;
            self.service(&message)?;
        }
        let mut resolutions = vec![];
        // Implementation sets are per trait method, shared by every callsite
        // in this refresh; a new snapshot always re-queries them.
        let mut implementations: BTreeMap<String, (Vec<Target>, Vec<String>)> = BTreeMap::new();
        let mut inactive: BTreeMap<String, Vec<(Span, String)>> = BTreeMap::new();
        for call in &snapshot.syntax.calls {
            if !only.contains(&call.callsite_id) {
                continue;
            }
            if cancel() {
                return Ok(None);
            }
            let mut diagnostics = vec![];
            if !inactive.contains_key(&call.file) {
                let ranges = self.inactive_ranges(snapshot, &call.file, deadline)?;
                inactive.insert(call.file.clone(), ranges);
            }
            let cfg_inactive = inactive[&call.file]
                .iter()
                .find(|(range, _)| range.start <= call.span.start && call.span.end <= range.end);
            if let Some((_, message)) = cfg_inactive {
                diagnostics.push(gap(call, "cfg_inactive", message));
            }
            let cfg_inactive = cfg_inactive.is_some();
            let targets = if call.kind == CallKind::Indirect {
                diagnostics.push(gap(
                    call,
                    "indirect_call_not_modeled",
                    "A definition binding does not identify this runtime call target",
                ));
                vec![]
            } else {
                let file = &snapshot.manifest.files[&call.file];
                let (line, character) =
                    to_utf16(&file.content, call.span.start).map_err(anyhow::Error::msg)?;
                let (definitions, errors) = self.locate(
                    snapshot,
                    "textDocument/definition",
                    &call.file,
                    (line, character),
                    TargetDerivation::Definition,
                    deadline,
                )?;
                for error in errors {
                    diagnostics.push(gap(call, "definition_mapping_gap", &error));
                }
                let mut targets: BTreeMap<_, _> = definitions
                    .iter()
                    .map(|t| (t.symbol_id.clone(), t.clone()))
                    .collect();
                let traits: Vec<_> = definitions
                    .iter()
                    .filter(|t| !t.concrete_function)
                    .collect();
                if call.kind == CallKind::Method && traits.is_empty() {
                    diagnostics.push(gap(call, "dispatch_not_enumerated", "Definition binding only; trait implementations and runtime dispatch targets are not enumerated"));
                }
                for declaration in traits {
                    if !implementations.contains_key(&declaration.symbol_id) {
                        let file = &snapshot.manifest.files[&declaration.file];
                        let position = to_utf16(&file.content, declaration.span.start)
                            .map_err(anyhow::Error::msg)?;
                        let found = self.locate(
                            snapshot,
                            "textDocument/implementation",
                            &declaration.file,
                            position,
                            TargetDerivation::Implementation,
                            deadline,
                        )?;
                        implementations.insert(declaration.symbol_id.clone(), found);
                    }
                    let (found, errors) = &implementations[&declaration.symbol_id];
                    for error in errors {
                        diagnostics.push(gap(call, "implementation_mapping_gap", error));
                    }
                    for target in found {
                        targets
                            .entry(target.symbol_id.clone())
                            .or_insert_with(|| target.clone());
                    }
                    diagnostics.push(gap(call, "dispatch_partially_enumerated", "Captured workspace implementations are candidates; inherited default methods, blanket or external impls and the runtime target are not proven"));
                }
                targets.into_values().collect()
            };
            resolutions.push(CallResolution {
                callsite_id: call.callsite_id.clone(),
                targets,
                diagnostics,
                cfg_inactive,
            });
        }
        Ok(Some(SemanticBatch {
            snapshot_id: snapshot.snapshot_id.clone(), syntax_epoch: snapshot.syntax_epoch,
            input_hash: snapshot.input_hash.clone(), backend: backend.into(), resolutions,
            diagnostics: vec![Diagnostic { code:"prototype_scope".into(), file:None,
                message:"Definition and trait-implementation queries only. LSP document-version barriers, cfg beyond backend-reported inactive code, references, macros, external dependencies and generated outputs are not validated by this prototype.".into() }],
        }))
    }

    /// Resolve references against the synchronized, immutable source snapshot.
    pub fn references(
        &mut self,
        snapshot: &Snapshot,
        backend: &str,
        deadline: Instant,
        cancel: &mut dyn FnMut() -> bool,
    ) -> Result<Option<(Vec<Relation>, Vec<Diagnostic>)>> {
        let mut relations = BTreeMap::new();
        let mut diagnostics = vec![];
        for symbol in &snapshot.syntax.symbols {
            if cancel() {
                return Ok(None);
            }
            let position = to_utf16(
                &snapshot.manifest.files[&symbol.file].content,
                symbol.name_span.start,
            )
            .map_err(anyhow::Error::msg)?;
            let result = self.request("textDocument/references", json!({"textDocument":{"uri":self.uri(&symbol.file)?},"position":{"line":position.0,"character":position.1},"context":{"includeDeclaration":false}}), deadline)?;
            for location in result.as_array().into_iter().flatten() {
                let mapped = (|| -> Result<(String, Span)> {
                    let uri =
                        Url::parse(location["uri"].as_str().context("missing reference URI")?)?;
                    let path = uri
                        .to_file_path()
                        .map_err(|_| anyhow::anyhow!("invalid URI"))?;
                    let relative =
                        crate::dependencies::relative_path(&self.root, &snapshot.manifest, &path)?;
                    let source = &snapshot
                        .manifest
                        .files
                        .get(&relative)
                        .context("reference outside indexed source scope")?
                        .content;
                    let point = |edge: &str| -> Result<Position> {
                        let line = location["range"][edge]["line"]
                            .as_u64()
                            .context("invalid line")?;
                        let column = location["range"][edge]["character"]
                            .as_u64()
                            .context("invalid character")?;
                        from_utf16(source, u32::try_from(line)?, u32::try_from(column)?)
                            .map_err(anyhow::Error::msg)
                    };
                    Ok((
                        relative,
                        Span {
                            start: point("start")?,
                            end: point("end")?,
                        },
                    ))
                })();
                let (file, span) = match mapped {
                    Ok(v) => v,
                    Err(error) => {
                        diagnostics.push(Diagnostic {
                            code: "reference_mapping_gap".into(),
                            file: Some(symbol.file.clone()),
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                if file == symbol.file && span == symbol.name_span {
                    continue;
                }
                let occurrence = snapshot
                    .syntax
                    .occurrences
                    .iter()
                    .find(|o| o.file == file && o.span == span);
                let enclosing = snapshot
                    .syntax
                    .symbols
                    .iter()
                    .filter(|s| s.file == file && s.span.contains(span.start))
                    .min_by_key(|s| {
                        (
                            s.span.end.line - s.span.start.line,
                            s.span.end.column.saturating_sub(s.span.start.column),
                        )
                    });
                let caller = occurrence
                    .and_then(|o| o.enclosing_symbol_id.clone())
                    .or_else(|| enclosing.map(|s| s.symbol_id.clone()))
                    .unwrap_or_else(|| format!("file:{file}"));
                let kind = occurrence.map(|o| o.kind.as_str()).unwrap_or("read");
                // Call certainty comes from the aligned definition/dispatch pipeline.
                if kind == "call" {
                    continue;
                }
                let id = identity(
                    "rel",
                    &[
                        kind,
                        &caller,
                        &symbol.symbol_id,
                        &file,
                        &serde_json::to_string(&span)?,
                    ],
                );
                let certainty = Certainty::Candidate;
                relations.insert(id.clone(), Relation { relation_id:id.clone(), kind:kind.into(), from_symbol:caller, to_symbol:symbol.symbol_id.clone(), file:file.clone(), span, certainty:certainty.clone(), freshness:"current".into(), runtime:"unavailable".into(), origin:None, evidence:vec![Evidence { evidence_id:identity("ev", &[&id, backend, "references"]), source:"L3".into(), certainty, indexer:backend.into(), derivation:"lsp_reference+syntax_occurrence; binding does not prove runtime dispatch or access behavior".into(), file_hash:snapshot.manifest.files[&file].file_hash.clone() }] });
            }
        }
        Ok(Some((relations.into_values().collect(), diagnostics)))
    }

    /// Code ranges the backend reports as compiled out by `#[cfg]`, pulled
    /// after synchronization so they describe the same inputs as the queries.
    fn inactive_ranges(
        &mut self,
        snapshot: &Snapshot,
        path: &str,
        deadline: Instant,
    ) -> Result<Vec<(Span, String)>> {
        let report = self.request(
            "textDocument/diagnostic",
            json!({"textDocument":{"uri":self.uri(path)?}}),
            deadline,
        )?;
        if report.get("kind").and_then(Value::as_str) != Some("full") {
            bail!("index_refresh_failed: expected a full diagnostic report for {path}");
        }
        let content = &snapshot.manifest.files[path].content;
        let point = |range: &Value, edge: &str| -> Result<Position> {
            let line = range
                .pointer(&format!("/{edge}/line"))
                .and_then(Value::as_u64);
            let character = range
                .pointer(&format!("/{edge}/character"))
                .and_then(Value::as_u64);
            let (Some(line), Some(character)) = (line, character) else {
                bail!("index_refresh_failed: malformed diagnostic range in {path}");
            };
            from_utf16(content, u32::try_from(line)?, u32::try_from(character)?)
                .map_err(anyhow::Error::msg)
        };
        let mut ranges = vec![];
        for item in report["items"].as_array().into_iter().flatten() {
            if item.get("code").and_then(Value::as_str) != Some("inactive_code") {
                continue;
            }
            let range = &item["range"];
            let message = item["message"]
                .as_str()
                .unwrap_or("inactive code")
                .to_string();
            ranges.push((
                Span {
                    start: point(range, "start")?,
                    end: point(range, "end")?,
                },
                message,
            ));
        }
        Ok(ranges)
    }

    /// Runs a location query and maps every result to a captured callable;
    /// unmappable locations are returned as messages instead of dropped.
    fn locate(
        &mut self,
        snapshot: &Snapshot,
        method: &str,
        path: &str,
        (line, character): (u32, u32),
        derivation: TargetDerivation,
        deadline: Instant,
    ) -> Result<(Vec<Target>, Vec<String>)> {
        let result = self.request(method, json!({"textDocument":{"uri":self.uri(path)?}, "position":{"line":line,"character":character}}), deadline)?;
        let locations = match result {
            Value::Array(values) => values,
            Value::Null => vec![],
            other => vec![other],
        };
        let mut targets = BTreeMap::new();
        let mut errors = vec![];
        for location in locations {
            match self.target(snapshot, &location, derivation) {
                Ok(target) => {
                    targets.insert(target.symbol_id.clone(), target);
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        Ok((targets.into_values().collect(), errors))
    }

    fn target(
        &self,
        snapshot: &Snapshot,
        location: &Value,
        derivation: TargetDerivation,
    ) -> Result<Target> {
        let uri = location
            .get("targetUri")
            .or_else(|| location.get("uri"))
            .and_then(Value::as_str)
            .context("missing target URI")?;
        let path = Url::parse(uri)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("non-file target URI"))?;
        let relative = crate::dependencies::relative_path(&self.root, &snapshot.manifest, &path)?;
        let file = snapshot
            .manifest
            .files
            .get(&relative)
            .context("target file not captured")?;
        let range = location
            .get("targetSelectionRange")
            .or_else(|| location.get("range"))
            .context("missing target range")?;
        let line = range
            .pointer("/start/line")
            .and_then(Value::as_u64)
            .context("missing target line")?;
        let column = range
            .pointer("/start/character")
            .and_then(Value::as_u64)
            .context("missing target column")?;
        let pos = from_utf16(&file.content, u32::try_from(line)?, u32::try_from(column)?)
            .map_err(anyhow::Error::msg)?;
        let symbol = snapshot
            .syntax
            .symbols
            .iter()
            .find(|s| s.file == relative && (s.name_span.contains(pos) || s.span.start == pos))
            .context("target does not align with a captured declaration")?;
        if !matches!(symbol.kind.as_str(), "function" | "trait_method") {
            bail!("binding is not a modeled callable declaration");
        }
        Ok(Target {
            symbol_id: symbol.symbol_id.clone(),
            file: relative,
            span: symbol.name_span,
            concrete_function: symbol.kind == "function",
            derivation,
        })
    }
}

fn settings() -> Value {
    json!({"cargo":{"offline":true,"buildScripts":{"enable":true}}, "procMacro":{"enable":true},
        "checkOnSave":false, "check":{"enable":false}, "cachePriming":{"enable":false}, "files":{"watcher":"client"}})
}

fn gap(call: &CallSite, code: &str, message: &str) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        file: Some(call.file.clone()),
        message: format!("{}: {message}", call.expression),
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Bounded cleanup even if the backend stopped responding to LSP.
        self.cancellation.stop();
        if let Some(reader) = self.reader.take() {
            // A wrapper may hand a pipe to a process outside its group. Cleanup
            // must still finish; ordinary descendants are killed above.
            let deadline = Instant::now() + Duration::from_millis(200);
            while !reader.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> Result<Value> {
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("semantic backend closed stdout");
        }
        header_bytes += line.len();
        if header_bytes > 8192 {
            bail!("LSP headers exceed 8 KiB");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            if length.is_some() {
                bail!("duplicate LSP Content-Length");
            }
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("missing LSP Content-Length")?;
    if length > 16 * 1024 * 1024 {
        bail!("LSP message exceeds 16 MiB");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_handles_unicode_and_consecutive_messages() {
        let body = r#"{"result":"中文😀"}"#;
        let wire = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{body}Content-Length: 4\r\n\r\nnull",
            body.len()
        );
        let mut input = Cursor::new(wire.into_bytes());
        assert_eq!(read_message(&mut input).unwrap()["result"], "中文😀");
        assert!(read_message(&mut input).unwrap().is_null());
    }

    #[test]
    fn malformed_or_truncated_messages_fail() {
        for wire in [
            "Content-Length: 99\r\n\r\n{}",
            "\r\n{}",
            "Content-Length: 999999999\r\n\r\n",
            "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
        ] {
            assert!(read_message(&mut Cursor::new(wire)).is_err());
        }
    }

    #[test]
    fn headers_are_case_insensitive_and_allow_extra_fields() {
        let body = r#"{"ok":true}"#;
        let wire = format!(
            "content-length: {}\nContent-Type: application/json\n\n{body}",
            body.len()
        );
        let mut input = Cursor::new(wire.into_bytes());
        assert_eq!(read_message(&mut input).unwrap()["ok"], true);
    }
}
