use crate::frontend::Frontends;
use crate::sync::MutexExt;
use crate::{
    config::{self, Resolved},
    lsp::{self, LspClient},
    workspace,
};
use anyhow::{Context, Result, bail};
use semweft_core::*;
use semweft_store::{IndexedView, PublicationRef, SnapshotStore};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct StoredView {
    pub view: Arc<AnalysisView>,
    pub references: Arc<Vec<Relation>>,
    pub reference_gaps: Arc<Vec<Diagnostic>>,
    pub namespaces: Arc<BTreeMap<String, String>>,
    index: Arc<OnceLock<IndexedView>>,
    api_surface: Arc<OnceLock<crate::api_surface::Inventory>>,
    weight: Arc<AtomicUsize>,
    deriver: Arc<Mutex<RelationDeriver>>,
}
impl StoredView {
    pub(crate) fn new(
        view: Arc<AnalysisView>,
        references: Vec<Relation>,
        reference_gaps: Vec<Diagnostic>,
    ) -> Self {
        let mut namespaces: BTreeMap<String, String> = view
            .snapshot
            .manifest
            .files
            .iter()
            .filter(|(p, _)| p.ends_with("Cargo.toml"))
            .filter_map(|(p, f)| {
                let value: toml::Value = toml::from_str(&f.content).ok()?;
                let name = value
                    .get("lib")
                    .and_then(|l| l.get("name"))
                    .and_then(toml::Value::as_str)
                    .or_else(|| {
                        value
                            .get("package")
                            .and_then(|v| v.get("name"))
                            .and_then(toml::Value::as_str)
                    })?;
                Some((
                    p.trim_end_matches("Cargo.toml")
                        .trim_end_matches('/')
                        .to_owned(),
                    name.replace('-', "_"),
                ))
            })
            .collect();
        for (path, file) in &view.snapshot.manifest.files {
            if path.ends_with("package.json")
                && let Ok(package) = serde_json::from_str::<Value>(&file.content)
                && let Some(name) = package["name"].as_str()
            {
                namespaces.insert(
                    path.trim_end_matches("package.json")
                        .trim_end_matches('/')
                        .into(),
                    name.into(),
                );
            }
            if (path == "go.mod" || path.ends_with("/go.mod"))
                && let Some(module) = crate::go::module(&file.content)
            {
                namespaces.insert(
                    path.trim_end_matches("go.mod").trim_end_matches('/').into(),
                    module,
                );
            }
        }
        let weight =
            semweft_store::payload_bytes(&(&view, &references, &reference_gaps, &namespaces))
                .saturating_mul(2);
        Self {
            view,
            references: Arc::new(references),
            reference_gaps: Arc::new(reference_gaps),
            namespaces: Arc::new(namespaces),
            index: Arc::new(OnceLock::new()),
            api_surface: Arc::new(OnceLock::new()),
            weight: Arc::new(AtomicUsize::new(weight)),
            deriver: Arc::new(Mutex::new(RelationDeriver::default())),
        }
    }

    fn with_deriver(mut self, deriver: Arc<Mutex<RelationDeriver>>) -> Self {
        self.deriver = deriver;
        self
    }

    pub fn index(&self) -> &IndexedView {
        self.index.get_or_init(|| {
            let index = IndexedView::new(self.view.clone(), merged_relations(self));
            self.weight
                .fetch_add(index.estimated_heap_bytes(), Ordering::Relaxed);
            index
        })
    }

    pub(crate) fn api_surface(&self, deadline: Instant) -> Result<&crate::api_surface::Inventory> {
        if let Some(inventory) = self.api_surface.get() {
            return Ok(inventory);
        }
        let inventory = crate::api_surface::build(self, deadline)?;
        let bytes = inventory.estimated_heap_bytes();
        if self.api_surface.set(inventory).is_ok() {
            self.weight.fetch_add(bytes, Ordering::Relaxed);
        }
        Ok(self.api_surface.get().unwrap())
    }
}

// Derived indexes can be reconstructed. Keep only eight recently queried views;
// eviction never drops a persisted publication or invalidates an active query.
const QUERY_VIEW_CAPACITY: usize = 8;
struct QueryViews {
    items: BTreeMap<String, StoredView>,
    recent: VecDeque<String>,
    max_views: usize,
    max_bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}
impl Default for QueryViews {
    fn default() -> Self {
        Self {
            items: BTreeMap::new(),
            recent: VecDeque::new(),
            max_views: QUERY_VIEW_CAPACITY,
            max_bytes: 64 * 1024 * 1024,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }
}
impl QueryViews {
    fn bytes(&self) -> usize {
        self.items.values().fold(0usize, |sum, item| {
            sum.saturating_add(item.weight.load(Ordering::Relaxed))
        })
    }
    fn trim(&mut self) {
        while self.items.len() > self.max_views || self.bytes() > self.max_bytes {
            let Some(key) = self.recent.pop_front() else {
                break;
            };
            self.items.remove(&key);
            self.evictions += 1;
        }
    }
    fn set_limits(&mut self, views: usize, bytes: usize) {
        self.max_views = views;
        self.max_bytes = bytes;
        self.trim();
    }
    fn get(&mut self, key: &str) -> Option<StoredView> {
        let stored = self.items.get(key).cloned();
        if stored.is_some() {
            self.hits += 1;
            self.recent.retain(|old| old != key);
            self.recent.push_back(key.into());
        } else {
            self.misses += 1;
        }
        self.trim();
        stored
    }

    fn insert(&mut self, stored: StoredView) {
        let key = view_key(&stored.view);
        self.remove(&key);
        self.recent.push_back(key.clone());
        self.items.insert(key, stored);
        self.trim();
    }

    fn remove(&mut self, key: &str) {
        self.items.remove(key);
        self.recent.retain(|old| old != key);
    }
}

pub struct State {
    pub resolved: Resolved,
    pub coordinator: RefreshCoordinator,
    pub extra: BTreeMap<String, (Vec<Relation>, Vec<Diagnostic>)>,
    pub backend: String,
    pub refresh_error: Option<String>,
    pub refreshing: bool,
    pub last_refresh_ms: u64,
    pub recovery: String,
    pub authorized: bool,
    force_refresh: bool,
    index_requested: u64,
    index_completed: u64,
    query_views: QueryViews,
    store: SnapshotStore,
    dirty_extra: BTreeSet<String>,
    deriver: Arc<Mutex<RelationDeriver>>,
}

impl State {
    fn load_version(&mut self, version: &(String, u64)) -> Result<Arc<AnalysisView>> {
        let cache_key = format!("{}:{}", version.0, version.1);
        if let Some(stored) = self.query_views.get(&cache_key) {
            return Ok(stored.view);
        }
        if let Some(view) = self.coordinator.view(&version.0, version.1) {
            return Ok(view);
        }
        let publication = self.store.load(version)?;
        let view = publication.view;
        self.query_views.insert(
            StoredView::new(
                view.clone(),
                publication.references,
                publication.reference_gaps,
            )
            .with_deriver(self.deriver.clone()),
        );
        Ok(view)
    }

    fn choose_view(&mut self, args: &Value) -> Result<Arc<AnalysisView>> {
        let Some(id) = args["snapshot_id"].as_str() else {
            return self
                .coordinator
                .latest()
                .context("index_store_error: index not initialized");
        };
        let epoch = args["semantic_epoch"].as_u64();
        let disk = self.store.find_key(id, epoch);
        let memory = self
            .coordinator
            .retained()
            .iter()
            .rev()
            .find(|v| {
                v.snapshot.snapshot_id == id
                    && epoch.is_none_or(|epoch| v.epochs.semantic_epoch == epoch)
            })
            .map(|v| (id.to_owned(), v.epochs.semantic_epoch));
        let key = disk.into_iter().chain(memory).max_by_key(|key| key.1);
        let key = match key {
            Some(key) => key,
            None if self.store.find_key(id, None).is_some() => {
                bail!("invalid_argument: semantic_epoch not published for snapshot")
            }
            None => bail!("snapshot_expired: snapshot unavailable; query latest again"),
        };
        self.load_version(&key)
    }

    fn stored_view(&mut self, view: Arc<AnalysisView>) -> Result<StoredView> {
        let key = view_key(&view);
        if let Some(stored) = self.query_views.get(&key) {
            return Ok(stored);
        }
        let (references, gaps) = if let Some(extra) = self.extra.get(&key) {
            extra.clone()
        } else if self
            .store
            .find_key(&view.snapshot.snapshot_id, Some(view.epochs.semantic_epoch))
            .is_some()
        {
            let publication = self.store.load(&(
                view.snapshot.snapshot_id.clone(),
                view.epochs.semantic_epoch,
            ))?;
            (publication.references, publication.reference_gaps)
        } else {
            (vec![], vec![])
        };
        let stored = StoredView::new(view, references, gaps).with_deriver(self.deriver.clone());
        self.query_views.insert(stored.clone());
        Ok(stored)
    }

    fn set_extra(&mut self, view: &AnalysisView, references: Vec<Relation>, gaps: Vec<Diagnostic>) {
        let key = view_key(view);
        self.query_views.remove(&key);
        self.dirty_extra.insert(key.clone());
        self.extra.insert(key, (references, gaps));
    }
}
pub struct Engine {
    pub state: Mutex<State>,
    pub changed: Condvar,
    wake: mpsc::Sender<WorkerControl>,
    execution: Mutex<Arc<lsp::Cancellation>>,
    trust_control: Mutex<()>,
    executable: PathBuf,
    worker_handle: Mutex<Option<thread::JoinHandle<()>>>,
    semantic_gate: Mutex<()>,
    writer: Mutex<()>,
    parser: Mutex<Frontends>,
    capture_cache: Mutex<crate::capture_cache::CaptureCache>,
}
enum WorkerControl {
    Refresh,
    Revoke(mpsc::Sender<()>),
    Shutdown,
}
fn view_key(view: &AnalysisView) -> String {
    format!(
        "{}:{}",
        view.snapshot.snapshot_id, view.epochs.semantic_epoch
    )
}

#[derive(Debug)]
struct InputsChanged;

impl std::fmt::Display for InputsChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .write_str("index_refresh_failed: inputs changed during syntax analysis; retry refresh")
    }
}

impl std::error::Error for InputsChanged {}

fn retry_input_changes<Output>(
    deadline: Instant,
    mut scan: impl FnMut() -> Result<Output>,
) -> Result<Output> {
    loop {
        crate::query::check_cancelled()?;
        if Instant::now() >= deadline {
            bail!("timeout: inputs did not stabilize within timeout_ms");
        }
        match scan() {
            Err(error) if error.is::<InputsChanged>() => {
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            result => return result,
        }
    }
}

impl Engine {
    pub fn new(resolved: Resolved) -> Result<Arc<Self>> {
        let authorized = resolved.trusted()?;
        let (store, recovered) =
            SnapshotStore::open_lazy(&resolved.index_dir, &resolved.workspace_id)?;
        let (syntax_high_water, semantic_high_water) = store.high_water();
        let mut views = vec![];
        let mut extra = BTreeMap::new();
        let mut recovery = "new".to_owned();
        let mut latest = None;
        if let Some(recovered) = recovered {
            latest = recovered.latest;
            for item in recovered.publications {
                extra.insert(view_key(&item.view), (item.references, item.reference_gaps));
                views.push(item.view);
            }
            recovery = "restored; latest queries recheck disk inputs".into();
        }
        let backend = views
            .last()
            .and_then(|v: &Arc<AnalysisView>| v.snapshot.manifest.analysis_versions.get("semantic"))
            .cloned()
            .unwrap_or_else(|| "not_enabled".into());
        let mut coordinator =
            RefreshCoordinator::restore_roots(views, syntax_high_water, semantic_high_water)
                .map_err(anyhow::Error::msg)?;
        if let Some((id, epoch)) = latest {
            coordinator
                .select_latest(&id, epoch)
                .map_err(anyhow::Error::msg)?;
        }
        let (wake, receiver) = mpsc::channel();
        let mut query_views = QueryViews::default();
        query_views.set_limits(
            resolved.config.index.query_cache_views,
            resolved.config.index.query_cache_bytes,
        );
        let engine = Arc::new(Self {
            state: Mutex::new(State {
                resolved,
                coordinator,
                extra,
                backend,
                refresh_error: None,
                refreshing: false,
                last_refresh_ms: 0,
                recovery,
                authorized,
                force_refresh: false,
                index_requested: 0,
                index_completed: 0,
                query_views,
                store,
                dirty_extra: BTreeSet::new(),
                deriver: Arc::new(Mutex::new(RelationDeriver::default())),
            }),
            changed: Condvar::new(),
            wake,
            execution: Mutex::new(Arc::new(lsp::Cancellation::default())),
            trust_control: Mutex::new(()),
            executable: std::env::var_os("SEMWEFT_RUST_ANALYZER")
                .map(PathBuf::from)
                .unwrap_or_else(|| "rust-analyzer".into()),
            worker_handle: Mutex::new(None),
            semantic_gate: Mutex::new(()),
            writer: Mutex::new(()),
            parser: Mutex::new(Frontends::new().map_err(anyhow::Error::msg)?),
            capture_cache: Mutex::new(crate::capture_cache::CaptureCache::default()),
        });
        let worker = engine.clone();
        let handle = thread::spawn(move || worker.worker(receiver));
        *engine.worker_handle.locked() = Some(handle);
        Ok(engine)
    }

    /// The caller holds `writer` before locking state, so no other publication
    /// can overtake the captured batch while filesystem work runs without state.
    fn persist<'a>(&'a self, state: MutexGuard<'a, State>) -> Result<MutexGuard<'a, State>> {
        self.persist_using(state, |store, latest, updates| {
            store.commit(latest, updates).map(|_| ())
        })
    }

    fn persist_using<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        commit: impl FnOnce(
            &mut SnapshotStore,
            Option<semweft_store::ViewKey>,
            &[PublicationRef<'_>],
        ) -> Result<()>,
    ) -> Result<MutexGuard<'a, State>> {
        let latest = state
            .coordinator
            .latest()
            .map(|v| (v.snapshot.snapshot_id.clone(), v.epochs.semantic_epoch));
        let updates: Vec<_> = state
            .coordinator
            .retained()
            .iter()
            .filter(|view| {
                !state.store.is_persisted(view) || state.dirty_extra.contains(&view_key(view))
            })
            .map(|view| {
                let extra = state
                    .extra
                    .get(&view_key(view))
                    .cloned()
                    .unwrap_or_default();
                (view.clone(), extra)
            })
            .collect();
        if updates.is_empty() && &latest == state.store.latest_key() {
            return Ok(state);
        }
        let mut store = state.store.clone();
        drop(state);
        let borrowed: Vec<_> = updates
            .iter()
            .map(|(view, (references, gaps))| PublicationRef {
                view,
                references,
                reference_gaps: gaps,
            })
            .collect();
        let result = commit(&mut store, latest, &borrowed);
        state = self.state.locked();
        // Failed commits retain reusable object caches and all dirty publications.
        state.store = store;
        result?;
        state.dirty_extra.clear();
        state.coordinator.retain_refresh_roots();
        let resident: BTreeSet<_> = state
            .coordinator
            .retained()
            .iter()
            .map(|view| view_key(view))
            .collect();
        state.extra.retain(|key, _| resident.contains(key));
        let keep_last = state.resolved.config.index.retain_snapshots;
        let max_disk_bytes = state.resolved.config.index.max_disk_bytes;
        if keep_last.is_some() || max_disk_bytes.is_some() {
            drop(borrowed);
            drop(updates);
            let mut store = state.store.clone();
            drop(state);
            let result = store.gc_with_budget(keep_last.unwrap_or(usize::MAX), max_disk_bytes);
            state = self.state.locked();
            state.store = store;
            result?;
        }
        Ok(state)
    }

    /// Single writer for capture, syntax publication and persisted views.
    pub fn scan(&self, force: bool) -> Result<Arc<AnalysisView>> {
        self.scan_inner(force, true)
    }
    fn scan_inner(&self, force: bool, semantic: bool) -> Result<Arc<AnalysisView>> {
        self.scan_using(force, semantic, |resolved, backend| {
            self.capture_cache
                .lock()
                .unwrap()
                .capture(resolved, backend, true)
        })
    }
    pub fn invalidate_inputs(&self, paths: &[PathBuf], full: bool) {
        self.capture_cache.locked().invalidate(paths, full);
    }
    pub fn scan_incremental(&self) -> Result<Arc<AnalysisView>> {
        self.scan_using(false, true, |resolved, backend| {
            self.capture_cache
                .lock()
                .unwrap()
                .capture(resolved, backend, false)
        })
    }
    fn scan_using(
        &self,
        force: bool,
        semantic: bool,
        mut capture: impl FnMut(&Resolved, &str) -> Result<workspace::Capture>,
    ) -> Result<Arc<AnalysisView>> {
        let _writer = self.writer.locked();
        let (previous_config, backend) = {
            let state = self.state.locked();
            (state.resolved.clone(), state.backend.clone())
        };
        let updated = config::resolve(
            Some(&previous_config.root),
            previous_config.config_path.as_deref(),
        )?;
        if updated.index_dir != previous_config.index_dir {
            bail!("config_conflict: index.directory changed; stop the daemon before reconnecting");
        }
        let captured = capture(&updated, &backend)?;
        let mut syntax = self
            .parser
            .lock()
            .unwrap()
            .analyze(&captured.manifest)
            .map_err(anyhow::Error::msg)?;
        let verified = capture(&updated, &backend)?;
        let checked_config = config::resolve(Some(&updated.root), updated.config_path.as_deref())?;
        if verified.manifest != captured.manifest
            || checked_config.analysis_hash() != updated.analysis_hash()
        {
            return Err(InputsChanged.into());
        }
        let trusted = updated.trusted()?;
        let mut state = self.state.locked();
        state.resolved = updated;
        let limits = (
            state.resolved.config.index.query_cache_views,
            state.resolved.config.index.query_cache_bytes,
        );
        state.query_views.set_limits(limits.0, limits.1);
        if !trusted {
            self.execution.locked().cancel();
        } else if !state.authorized {
            *self.execution.locked() = Arc::new(lsp::Cancellation::default());
        }
        state.authorized = trusted;
        syntax.diagnostics.extend(captured.diagnostics);
        if !trusted {
            syntax.diagnostics.push(Diagnostic { code:"semantic_untrusted".into(), file:None, message:"Resident semantic tool execution is blocked; verified imported evidence remains queryable. Authorize execution with semweft trust --accept-build-execution".into() });
        }
        let previous = state
            .coordinator
            .latest()
            .map(|v| v.snapshot.snapshot_id.clone());
        let semantic_base = state.coordinator.latest_current();
        let view = state
            .coordinator
            .publish_inputs(captured.manifest, syntax)
            .map_err(anyhow::Error::msg)?;
        let changed = previous.as_deref() != Some(&view.snapshot.snapshot_id);
        if changed {
            state.refresh_error = None;
            if let Some(base) = semantic_base {
                let extra = state
                    .extra
                    .get(&view_key(&base))
                    .cloned()
                    .unwrap_or_default()
                    .0;
                let old_symbols = &base.snapshot.syntax.symbols;
                let map_symbol = |id: &str| -> Option<String> {
                    if id.starts_with("file:") {
                        return view
                            .snapshot
                            .manifest
                            .files
                            .contains_key(id.strip_prefix("file:")?)
                            .then(|| id.to_owned());
                    }
                    let old = old_symbols.iter().find(|s| s.symbol_id == id)?;
                    let candidates = view
                        .snapshot
                        .syntax
                        .symbols
                        .iter()
                        .filter(|s| s.qualified_name == old.qualified_name && s.kind == old.kind)
                        .collect::<Vec<_>>();
                    (candidates.len() == 1).then(|| candidates[0].symbol_id.clone())
                };
                let pending = base.relations.iter().chain(&extra).filter(|r|r.freshness=="current").filter_map(|r| {
                    let mut r=r.clone();r.from_symbol=map_symbol(&r.from_symbol)?;r.to_symbol=map_symbol(&r.to_symbol)?;
                    r.relation_id=identity("rel", &[&r.kind,&r.from_symbol,&r.to_symbol,&r.file,&serde_json::to_string(&r.span).unwrap()]);
                    r.freshness="pending".into();r.certainty=Certainty::Candidate;
                    r.origin=Some(RelationOrigin {snapshot_id:base.snapshot.snapshot_id.clone(),epochs:base.epochs.clone(),file_hash:base.snapshot.manifest.files.get(&r.file)?.file_hash.clone(),reason:"conservative workspace semantic invalidation; coordinates belong to origin snapshot".into()});Some(r)
                }).collect::<Vec<_>>();
                state.set_extra(&view, pending, vec![]);
            }
            self.changed.notify_all();
        }
        // Also retries an earlier failed publication. An unchanged successful
        // scan performs no object or catalog writes.
        state = self.persist(state)?;
        let trusted = state.resolved.trusted()?;
        if force && trusted {
            state.force_refresh = true;
            state.index_requested += 1;
            state.refresh_error = None;
        }
        if !trusted
            || (semantic
                && (changed || force || !view.pending_units.is_empty())
                && state.refresh_error.is_none())
        {
            let _ = self.wake.send(WorkerControl::Refresh);
        }
        Ok(view)
    }

    /// A successful revoke acknowledges that the worker has released its LSP
    /// client and reaped the process tree, including an in-flight initialization.
    pub fn trust_changed(&self) -> Result<()> {
        let _control = self.trust_control.locked();
        let resolved = self.state.locked().resolved.clone();
        if !resolved.trusted()? {
            self.execution.locked().cancel();
            {
                let mut state = self.state.locked();
                state.authorized = false;
                state.force_refresh = false;
                state.index_completed = state.index_requested;
                self.changed.notify_all();
            }
            let (done, stopped) = mpsc::channel();
            self.wake
                .send(WorkerControl::Revoke(done))
                .context("index_refresh_failed: semantic worker stopped before revocation")?;
            stopped
                .recv_timeout(Duration::from_secs(5))
                .context("timeout: semantic worker did not acknowledge revocation")?;
        }
        self.scan(false)?;
        Ok(())
    }

    pub fn shutdown(&self) {
        self.execution.locked().cancel();
        self.state.locked().authorized = false;
        let _ = self.wake.send(WorkerControl::Shutdown);
        if let Some(handle) = self.worker_handle.locked().take() {
            let _ = handle.join();
        }
    }

    fn worker(self: Arc<Self>, receiver: mpsc::Receiver<WorkerControl>) {
        let mut client: Option<LspClient> = None;
        while let Ok(control) = receiver.recv() {
            match control {
                WorkerControl::Shutdown => return,
                WorkerControl::Revoke(done) => {
                    client = None;
                    let _ = done.send(());
                    continue;
                }
                WorkerControl::Refresh => {}
            }
            let execution = self.execution.locked().clone();
            let (root, trusted) = {
                let state = self.state.locked();
                (
                    state.resolved.root.clone(),
                    state.resolved.trusted().unwrap_or(false),
                )
            };
            if !trusted || execution.is_cancelled() {
                client = None;
                continue;
            }
            {
                let state = self.state.locked();
                if !state.force_refresh
                    && state
                        .coordinator
                        .latest()
                        .is_none_or(|v| v.pending_units.is_empty())
                {
                    continue;
                }
            }
            self.state.locked().refreshing = true;
            let started = Instant::now();
            let result = (|| -> Result<()> {
                if crate::web::enabled(&root) {
                    return self.refresh_project_scip(true);
                }
                if crate::go::enabled(&root) {
                    return self.refresh_go();
                }
                let _semantic = self.semantic_gate.locked();
                {
                    let state = self.state.locked();
                    if !state.force_refresh
                        && state
                            .coordinator
                            .latest()
                            .is_none_or(|v| v.pending_units.is_empty())
                    {
                        return Ok(());
                    }
                }
                if client.is_none() || !client.as_mut().unwrap().alive() {
                    client = None;
                    let backend = lsp::version_cancellable(&self.executable, &root, &execution)?;
                    let changed = {
                        let mut state = self.state.locked();
                        let changed = state.backend != backend;
                        state.backend = backend;
                        changed
                    };
                    if changed {
                        self.scan(false)?;
                    }
                    client = Some(LspClient::start_cancellable(
                        &self.executable,
                        &root,
                        Instant::now() + Duration::from_secs(60),
                        execution.clone(),
                    )?);
                }
                let (view, plan, ticket, backend, index_goal) = {
                    let mut state = self.state.locked();
                    let Some(view) = state.coordinator.latest() else {
                        return Ok(());
                    };
                    if view.pending_units.is_empty() && !state.force_refresh {
                        return Ok(());
                    }
                    let force = std::mem::take(&mut state.force_refresh);
                    let index_goal = state.index_requested;
                    state.refreshing = true;
                    let mut plan = state
                        .coordinator
                        .plan_refresh()
                        .map_err(anyhow::Error::msg)?;
                    if force {
                        plan.query = view
                            .snapshot
                            .syntax
                            .calls
                            .iter()
                            .map(|c| c.callsite_id.clone())
                            .collect();
                        plan.reused.clear();
                        plan.scope = RefreshScope::Full {
                            reason: "explicit_full_rebuild".into(),
                        };
                    }
                    (
                        view,
                        plan,
                        state
                            .coordinator
                            .begin_refresh()
                            .map_err(anyhow::Error::msg)?,
                        state.backend.clone(),
                        index_goal,
                    )
                };
                let snapshot = &view.snapshot;
                let mut cancel = || {
                    let state = self.state.locked();
                    !state.authorized
                        || state
                            .coordinator
                            .latest()
                            .is_none_or(|v| v.snapshot.snapshot_id != snapshot.snapshot_id)
                };
                let deadline = Instant::now() + Duration::from_secs(60);
                let Some(mut batch) = client.as_mut().unwrap().refresh(
                    snapshot,
                    &backend,
                    deadline,
                    &plan.query,
                    &mut cancel,
                )?
                else {
                    return Ok(());
                };
                batch.resolutions.extend(plan.reused);
                batch.diagnostics = vec![Diagnostic { code:"semantic_scope".into(), file:None, message:"Workspace bindings only; external consumers, generated-source mapping, macro bodies and exhaustive runtime dispatch remain outside this model".into() }];
                let Some((references, reference_gaps)) = client.as_mut().unwrap().references(
                    snapshot,
                    &backend,
                    deadline,
                    &mut cancel,
                )?
                else {
                    return Ok(());
                };
                // Cargo can write lockfiles while loading; never publish against changed inputs.
                let disk = {
                    let state = self.state.locked();
                    workspace::capture_config(&state.resolved, &backend)?
                };
                if disk.manifest.input_hash() != snapshot.input_hash {
                    self.scan(false)?;
                    return Ok(());
                }
                let _writer = self.writer.locked();
                let mut state = self.state.locked();
                if !state.authorized || execution.is_cancelled() {
                    return Ok(());
                }
                if let Some(existing) = state.extra.get(&format!(
                    "{}:{}",
                    snapshot.snapshot_id, snapshot.syntax_epoch
                )) && existing != &(references.clone(), reference_gaps.clone())
                {
                    bail!(
                        "index_refresh_failed: inconsistent reference rebuild; published view retained"
                    );
                }
                match state.coordinator.complete(&ticket, batch) {
                    Ok(view) => {
                        state.set_extra(&view, references, reference_gaps);
                        state = self.persist(state)?;
                        state.index_completed = index_goal;
                    }
                    Err(reason) if reason == "superseded" => {}
                    Err(reason) => bail!("index_refresh_failed: {reason}"),
                }
                Ok(())
            })();
            let mut state = self.state.locked();
            state.refreshing = false;
            state.last_refresh_ms = started.elapsed().as_millis() as u64;
            if execution.is_cancelled() {
                state.refresh_error = None;
                client = None;
            } else if let Err(error) = result {
                state.refresh_error = Some(format!("{error:#}"));
                client = None;
            }
            self.changed.notify_all();
            // Coalesce duplicate requests; retain a wake for an unpublished newer snapshot.
            let mut stop = false;
            while let Ok(control) = receiver.try_recv() {
                match control {
                    WorkerControl::Shutdown => stop = true,
                    WorkerControl::Revoke(done) => {
                        client = None;
                        let _ = done.send(());
                    }
                    WorkerControl::Refresh => {}
                }
            }
            if stop {
                return;
            }
            if state.index_requested > state.index_completed {
                state.force_refresh = true;
            }
            if state.authorized
                && !execution.is_cancelled()
                && state.refresh_error.is_none()
                && (state.force_refresh
                    || state
                        .coordinator
                        .latest()
                        .is_some_and(|v| !v.pending_units.is_empty()))
            {
                let _ = self.wake.send(WorkerControl::Refresh);
            }
        }
    }

    pub fn bind(&self, args: &Value, deadline: Instant) -> Result<(StoredView, Value)> {
        let requested = args["require_fresh"].as_bool().unwrap_or(false);
        let pinned = args.get("semantic_epoch").is_some();
        let wait_ms = args["fresh_wait_ms"]
            .as_u64()
            .unwrap_or(2_000.min(args["timeout_ms"].as_u64().unwrap_or(5_000) / 2));
        let started = Instant::now();
        let wait_deadline = deadline.min(started + Duration::from_millis(wait_ms));
        let mut state = self.state.locked();
        let mut view = state.choose_view(args)?;
        let mut reason = None;
        if requested && !view.pending_units.is_empty() {
            if pinned {
                reason = Some("version_pinned");
            } else {
                loop {
                    crate::query::check_cancelled()?;
                    if state.refresh_error.is_some() || !state.authorized {
                        reason = Some("refresh_failed");
                        break;
                    }
                    if args.get("snapshot_id").is_some()
                        && state
                            .coordinator
                            .latest()
                            .is_some_and(|v| v.snapshot.snapshot_id != view.snapshot.snapshot_id)
                    {
                        reason = Some("superseded");
                        break;
                    }
                    let Some(remaining) = wait_deadline.checked_duration_since(Instant::now())
                    else {
                        reason = Some("timeout");
                        break;
                    };
                    state = self
                        .changed
                        .wait_timeout(state, remaining.min(Duration::from_millis(50)))
                        .unwrap()
                        .0;
                    view = state.choose_view(args)?;
                    if view.pending_units.is_empty() {
                        break;
                    }
                }
            }
        }
        let fresh = json!({"requested":requested,"satisfied":view.pending_units.is_empty(),"reason":reason,"waited_ms":if requested && !pinned {started.elapsed().as_millis() as u64} else {0}});
        Ok((state.stored_view(view)?, fresh))
    }

    pub fn snapshots(&self, args: &Value) -> Result<Value> {
        let _writer = self.writer.locked();
        let state = self.state.locked();
        let max_disk_bytes =
            args["max_bytes"]
                .as_u64()
                .or(state.resolved.config.index.max_disk_bytes);
        let mut store = state.store.clone();
        drop(state);
        let result = (|| -> Result<Value> {
            match args["action"].as_str() {
                Some("compact") => {
                    Ok(json!({"schema_version":"0.1","reclaimed_bytes":store.compact_objects()?}))
                }
                Some("usage") => Ok(
                    json!({"schema_version":"0.1","usage":store.disk_usage()?,"max_disk_bytes":max_disk_bytes}),
                ),
                Some("gc") => {
                    let keep_last = args["keep_last"]
                        .as_u64()
                        .context("invalid_argument: keep_last required")?
                        as usize;
                    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
                    let stats = if dry_run {
                        store.preview_gc(keep_last, max_disk_bytes)?
                    } else {
                        store.gc_with_budget(keep_last, max_disk_bytes)?
                    };
                    Ok(json!({"schema_version":"0.1","dry_run":dry_run,"gc":stats}))
                }
                Some(action @ ("pin" | "unpin")) => {
                    let id = args["snapshot_id"]
                        .as_str()
                        .context("invalid_argument: snapshot required")?;
                    let version = store
                        .find_key(id, args["semantic_epoch"].as_u64())
                        .context("snapshot_expired: publication unavailable")?;
                    store.pin(&version, action == "pin")?;
                    Ok(
                        json!({"schema_version":"0.1","snapshot_id":version.0,"semantic_epoch":version.1,"pinned":action == "pin"}),
                    )
                }
                _ => bail!("invalid_argument: unknown snapshot operation"),
            }
        })();
        self.state.locked().store = store;
        result
    }

    pub fn status(&self, args: &Value) -> Result<Value> {
        let mut state = self.state.locked();
        let view = if let Some(id) = args["snapshot_id"].as_str() {
            Some(state.choose_view(&json!({"snapshot_id":id}))?)
        } else {
            state.coordinator.latest()
        };
        let trusted = state.resolved.trusted()?;
        state.query_views.trim();
        let storage = json!({"runtime":state.store.metrics(),"max_disk_bytes":state.resolved.config.index.max_disk_bytes,"retained_publications":state.store.entries().count(),"pins":state.store.pins(),"epoch_high_water":state.store.high_water(),"journal_sequence":state.store.journal_sequence(),
            "query_cache":{"views":state.query_views.items.len(),"estimated_bytes":state.query_views.bytes(),"max_views":state.query_views.max_views,"max_bytes":state.query_views.max_bytes,"hits":state.query_views.hits,"misses":state.query_views.misses,"evictions":state.query_views.evictions}});
        let store = state.store.clone();
        let mut result = json!({"storage":storage,"schema_version":"0.1","workspace_id":state.resolved.workspace_id,"snapshot_id":view.as_ref().map(|v| &v.snapshot.snapshot_id),"epochs":view.as_ref().map(|v| json!({"syntax":v.epochs.syntax_epoch,"semantic":v.epochs.semantic_epoch})),
            "manifest":view.as_ref().map(|v| json!({"input_hash":v.snapshot.input_hash,"scope":v.snapshot.manifest.scope,"analysis_versions":v.snapshot.manifest.analysis_versions,"files":v.snapshot.manifest.files.iter().map(|(path,file)| json!({"path":path,"file_hash":file.file_hash})).collect::<Vec<_>>(),"binary_files":v.snapshot.manifest.binary_files})),
            "retained_views":state.store.entries().map(|(key, info)| json!({"snapshot_id":key.0,"epochs":{"syntax":info.syntax_epoch,"semantic":key.1}})).collect::<Vec<_>>(),
            "refresh":{"state":if state.refresh_error.is_some() {"failed"} else if state.refreshing {"refreshing"} else if view.as_ref().is_some_and(|v| !v.pending_units.is_empty()) {"pending"} else if view.is_some() {"current"} else {"not_indexed"},"pending_units":view.as_ref().map(|v| &v.pending_units),"failure_reason":state.refresh_error,"elapsed_ms":state.last_refresh_ms,"recovery":state.recovery},
            "trust":{"state":if trusted {"trusted"} else {"untrusted"},"L3_blocked_reason":if !trusted {Some("trust_required")} else {state.refresh_error.as_deref()},"hint":"semweft trust --accept-build-execution"},"diagnostics":view.as_ref().map(|v| &v.known_gaps )});
        drop(state);
        let usage = store.disk_usage()?;
        result["storage"]["disk_budget_unmet"] = json!(
            result["storage"]["max_disk_bytes"]
                .as_u64()
                .is_some_and(|max| usage.total_bytes > max)
        );
        result["storage"]["disk"] = serde_json::to_value(usage)?;
        Ok(result)
    }

    pub fn index(&self, args: &Value, deadline: Instant) -> Result<Value> {
        if args["scip"].is_string() {
            return self.import_scip(args, deadline);
        }
        let syntax_only = args["syntax_only"].as_bool() == Some(true);
        let initial =
            retry_input_changes(deadline, || self.scan_inner(!syntax_only, !syntax_only))?;
        if args["syntax_only"].as_bool() == Some(true) {
            let mut result = self.status(&json!({"snapshot_id":initial.snapshot.snapshot_id}))?;
            if args["export_manifest"] == true {
                result["source_manifest"] = crate::scip::source_manifest(&initial.snapshot);
            }
            return Ok(result);
        }
        if !self.state.locked().resolved.trusted()? {
            bail!(
                "trust_required: index L3 may execute Cargo build scripts and procedural macros; run semweft trust --accept-build-execution or index --syntax-only"
            );
        }
        let mut state = self.state.locked();
        let goal = state.index_requested;
        loop {
            crate::query::check_cancelled()?;
            if !state.authorized {
                bail!("trust_required: semantic execution authorization was revoked");
            }
            if let Some(error) = &state.refresh_error {
                bail!("index_refresh_failed: {error}");
            }
            if state
                .coordinator
                .latest()
                .is_some_and(|v| v.pending_units.is_empty())
                && state.index_completed >= goal
            {
                drop(state);
                let mut result = self.status(&json!({}))?;
                if args["export_manifest"] == true {
                    let state = self.state.locked();
                    result["source_manifest"] =
                        crate::scip::source_manifest(&state.coordinator.latest().unwrap().snapshot);
                }
                return Ok(result);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("timeout: index task did not complete within timeout_ms")?;
            state = self
                .changed
                .wait_timeout(state, remaining.min(Duration::from_millis(50)))
                .unwrap()
                .0;
        }
    }

    fn import_scip(&self, args: &Value, deadline: Instant) -> Result<Value> {
        let _semantic = loop {
            crate::query::check_cancelled()?;
            if Instant::now() >= deadline {
                bail!("timeout: waiting for semantic writer");
            }
            if let Ok(guard) = self.semantic_gate.try_lock() {
                break guard;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let index_goal = args["index_goal"]
            .as_u64()
            .unwrap_or_else(|| self.state.locked().index_requested);
        let scip = args["scip"]
            .as_str()
            .context("invalid_argument: SCIP index path required")?;
        let (index, backend) = crate::scip::load(Path::new(scip))?;
        let expected: Value = serde_json::from_slice(&fs::read(
            args["scip_manifest"]
                .as_str()
                .context("invalid_argument: SCIP source manifest required")?,
        )?)?;
        let initial = self.scan_inner(false, false)?;
        if crate::scip::source_manifest(&initial.snapshot) != expected {
            bail!(
                "content_mismatch: SCIP source manifest does not match captured source and analysis configuration"
            );
        }
        // Validate mappings before changing the active backend identity. A bad
        // import must not displace a valid semantic view of the same sources.
        let (mut batch, references, gaps) =
            crate::scip::analyze(&index, &initial.snapshot, &backend, deadline)?;
        self.state.locked().backend = backend.clone();
        let view = self.scan_inner(false, false)?;
        if crate::scip::source_manifest(&view.snapshot) != expected {
            bail!("content_mismatch: inputs changed during SCIP import");
        }
        let ticket = self
            .state
            .lock()
            .unwrap()
            .coordinator
            .begin_refresh()
            .map_err(anyhow::Error::msg)?;
        batch.snapshot_id = view.snapshot.snapshot_id.clone();
        batch.syntax_epoch = view.snapshot.syntax_epoch;
        batch.input_hash = view.snapshot.input_hash.clone();
        if Instant::now() >= deadline {
            bail!("timeout: SCIP import exceeded query deadline");
        }
        let disk = {
            let state = self.state.locked();
            workspace::capture_config(&state.resolved, &backend)?
        };
        if crate::scip::source_manifest(&Snapshot {
            manifest: disk.manifest,
            ..(*view.snapshot).clone()
        }) != expected
        {
            bail!("content_mismatch: disk inputs changed during SCIP import");
        }
        let _writer = self.writer.locked();
        let mut state = self.state.locked();
        let published = state
            .coordinator
            .complete(&ticket, batch)
            .map_err(|e| anyhow::anyhow!("index_refresh_failed: {e}"))?;
        state.set_extra(&published, references, gaps);
        state.refresh_error = None;
        if state.index_requested == index_goal {
            state.force_refresh = false;
        }
        state.index_completed = state.index_completed.max(index_goal);
        state = self.persist(state)?;
        self.changed.notify_all();
        drop(state);
        let mut result = self.status(&json!({"snapshot_id":published.snapshot.snapshot_id}))?;
        if args["export_manifest"] == true {
            result["source_manifest"] = crate::scip::source_manifest(&published.snapshot);
        }
        Ok(result)
    }

    fn refresh_go(&self) -> Result<()> {
        self.refresh_project_scip(false)
    }

    fn refresh_project_scip(&self, web: bool) -> Result<()> {
        let (root, view, goal) = {
            let state = self.state.locked();
            (
                state.resolved.root.clone(),
                state
                    .coordinator
                    .latest()
                    .context("index_refresh_failed: no project inputs")?,
                state.index_requested,
            )
        };
        let staging = tempfile::tempdir()?;
        let output_path = staging.path().join("index.scip");
        let manifest_path = staging.path().join("inputs.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec(&crate::scip::source_manifest(&view.snapshot))?,
        )?;
        let executable = if web {
            std::env::var_os("SEMWEFT_SCIP_TYPESCRIPT").unwrap_or_else(|| "scip-typescript".into())
        } else {
            std::env::var_os("SEMWEFT_SCIP_GO").unwrap_or_else(|| "scip-go".into())
        };
        let mut command = std::process::Command::new(executable);
        command.current_dir(&root);
        if web {
            command.arg("index");
        } else {
            command.env("GOPROXY", "off").env("GOSUMDB", "off");
            let environment: BTreeMap<String, Option<String>> = serde_json::from_str(
                view.snapshot
                    .manifest
                    .analysis_versions
                    .get("go_build_environment")
                    .context("index_refresh_failed: Go build environment was not captured")?,
            )?;
            for (name, value) in environment {
                if let Some(value) = value {
                    command.env(name, value);
                } else {
                    command.env_remove(name);
                }
            }
        }
        command.arg("--output").arg(&output_path);
        let deadline = Instant::now() + Duration::from_secs(60);
        let Some(output) = crate::process::output_cancellable(&mut command, deadline, || {
            let state = self.state.locked();
            !state.authorized
                || !state.resolved.trusted().unwrap_or(false)
                || state
                    .coordinator
                    .latest()
                    .is_none_or(|v| v.snapshot.snapshot_id != view.snapshot.snapshot_id)
        })?
        else {
            return Ok(());
        };
        if !output.status.success() {
            bail!(
                "index_refresh_failed: project SCIP rebuild failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        {
            let state = self.state.locked();
            if !state.resolved.trusted()?
                || state
                    .coordinator
                    .latest()
                    .is_none_or(|v| v.snapshot.snapshot_id != view.snapshot.snapshot_id)
            {
                return Ok(());
            }
        }
        self.import_scip(
            &json!({"scip":output_path,"scip_manifest":manifest_path,"index_goal":goal}),
            deadline,
        )?;
        Ok(())
    }

    pub fn historical(&self, id: &str, epoch: Option<u64>) -> Result<StoredView> {
        let mut state = self.state.locked();
        let view = state.choose_view(&json!({"snapshot_id":id,"semantic_epoch":epoch}))?;
        state.stored_view(view)
    }

    pub fn publish_revision(&self, manifest: Manifest) -> Result<StoredView> {
        // Revision syntax views use the same monotonic epoch allocator and persistence.
        let _writer = self.writer.locked();
        let mut state = self.state.locked();
        if let Some(version) = state.store.reusable(&manifest) {
            let view = state.load_version(&version)?;
            return self.reuse_revision(state, view, manifest);
        }
        if let Some(view) = state
            .coordinator
            .retained()
            .iter()
            .rev()
            .find(|v| {
                v.epochs.semantic_epoch == v.epochs.syntax_epoch
                    && v.snapshot.manifest.files == manifest.files
                    && v.snapshot.manifest.binary_files == manifest.binary_files
                    && v.snapshot.manifest.analysis_versions.get("analysis_config")
                        == manifest.analysis_versions.get("analysis_config")
                    && v.snapshot.manifest.analysis_versions.get("syntax")
                        == manifest.analysis_versions.get("syntax")
                    && v.snapshot
                        .manifest
                        .analysis_versions
                        .get("go_build_environment")
                        == manifest.analysis_versions.get("go_build_environment")
            })
            .cloned()
        {
            return self.reuse_revision(state, view, manifest);
        }
        if let Some(view) = state
            .coordinator
            .retained()
            .iter()
            .rev()
            .find(|v| v.snapshot.manifest == manifest)
            .cloned()
        {
            return state.stored_view(view);
        }
        drop(state);
        let syntax = self
            .parser
            .lock()
            .unwrap()
            .analyze(&manifest)
            .map_err(anyhow::Error::msg)?;
        let mut state = self.state.locked();
        let old = state.coordinator.latest();
        let view = state
            .coordinator
            .publish_inputs(manifest, syntax)
            .map_err(anyhow::Error::msg)?;
        let stored = StoredView::new(
            view.clone(),
            vec![],
            vec![Diagnostic {
                code: "revision_semantic_unavailable".into(),
                file: None,
                message:
                    "Revision side has L1 candidates; no matching retained L3 index is available"
                        .into(),
            }],
        );
        let stored = stored.with_deriver(state.deriver.clone());
        state.set_extra(&view, vec![], stored.reference_gaps.as_ref().clone());
        state.query_views.insert(stored.clone());
        if let Some(old) = old {
            state
                .coordinator
                .select_latest(&old.snapshot.snapshot_id, old.epochs.semantic_epoch)
                .map_err(anyhow::Error::msg)?;
        }
        state = self.persist(state)?;
        if state
            .coordinator
            .latest()
            .is_some_and(|v| !v.pending_units.is_empty())
        {
            let _ = self.wake.send(WorkerControl::Refresh);
        }
        Ok(stored)
    }

    fn reuse_revision(
        &self,
        mut state: MutexGuard<'_, State>,
        view: Arc<AnalysisView>,
        mut manifest: Manifest,
    ) -> Result<StoredView> {
        let previous_mapping =
            serde_json::to_value(crate::services::configured(&view.snapshot.manifest)?)?;
        let desired_mapping = serde_json::to_value(crate::services::configured(&manifest)?)?;
        if previous_mapping == desired_mapping {
            return state.stored_view(view);
        }
        let source = state.stored_view(view.clone())?;
        let previous = state.coordinator.latest();
        if let Some(backend) = view.snapshot.manifest.analysis_versions.get("semantic") {
            manifest
                .analysis_versions
                .insert("semantic".into(), backend.clone());
        }
        manifest.analysis_versions.insert(
            "semantic_reused_from".into(),
            view.snapshot.snapshot_id.clone(),
        );
        let mut rebound = state
            .coordinator
            .publish_inputs(manifest, view.snapshot.syntax.clone())
            .map_err(anyhow::Error::msg)?;
        if view.epochs.semantic_epoch == view.epochs.syntax_epoch {
            let ticket = state
                .coordinator
                .begin_refresh()
                .map_err(anyhow::Error::msg)?;
            rebound = state
                .coordinator
                .complete(
                    &ticket,
                    SemanticBatch {
                        snapshot_id: rebound.snapshot.snapshot_id.clone(),
                        syntax_epoch: rebound.epochs.syntax_epoch,
                        input_hash: rebound.snapshot.input_hash.clone(),
                        backend: rebound
                            .snapshot
                            .manifest
                            .analysis_versions
                            .get("semantic")
                            .cloned()
                            .unwrap_or_default(),
                        resolutions: view.resolutions.clone(),
                        diagnostics: view.known_gaps.clone(),
                    },
                )
                .map_err(anyhow::Error::msg)?;
        }
        let stored = StoredView::new(
            rebound.clone(),
            source.references.as_ref().clone(),
            source.reference_gaps.as_ref().clone(),
        )
        .with_deriver(state.deriver.clone());
        state.set_extra(
            &rebound,
            stored.references.as_ref().clone(),
            stored.reference_gaps.as_ref().clone(),
        );
        state.query_views.insert(stored.clone());
        if let Some(previous) = previous {
            state
                .coordinator
                .select_latest(
                    &previous.snapshot.snapshot_id,
                    previous.epochs.semantic_epoch,
                )
                .map_err(anyhow::Error::msg)?;
        }
        drop(self.persist(state)?);
        Ok(stored)
    }
}

/// A single bounded generation per file. Keys include all matching declaration
/// identities/kinds, so changes in another file invalidate affected candidates.
#[derive(Default)]
pub struct RelationDeriver {
    files: BTreeMap<String, (String, Arc<Vec<Relation>>, usize)>,
    pub files_rebuilt: usize,
}
impl RelationDeriver {
    pub fn derive(&mut self, view: &AnalysisView) -> Vec<Relation> {
        self.derive_checked(view, || Ok(()))
            .expect("infallible relation derivation")
    }

    fn derive_checked(
        &mut self,
        view: &AnalysisView,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<Vec<Relation>> {
        self.files_rebuilt = 0;
        let syntax = &view.snapshot.syntax;
        let manifest = &view.snapshot.manifest;
        let indexer = manifest
            .analysis_versions
            .get("syntax")
            .cloned()
            .unwrap_or_else(crate::frontend::version);
        let mut by_name: BTreeMap<&str, Vec<&Symbol>> = BTreeMap::new();
        for symbol in &syntax.symbols {
            check()?;
            by_name.entry(&symbol.name).or_default().push(symbol);
        }
        let mut groups: BTreeMap<&str, Vec<&Occurrence>> = BTreeMap::new();
        for occurrence in &syntax.occurrences {
            check()?;
            groups.entry(&occurrence.file).or_default().push(occurrence);
        }
        self.files
            .retain(|file, _| groups.contains_key(file.as_str()));
        let mut result = BTreeMap::new();
        for (file, occurrences) in groups {
            check()?;
            let names: BTreeSet<_> = occurrences.iter().map(|o| o.name.as_str()).collect();
            let targets: BTreeMap<_, Vec<_>> = names
                .into_iter()
                .map(|name| {
                    (
                        name,
                        by_name
                            .get(name)
                            .into_iter()
                            .flatten()
                            .map(|s| (&s.symbol_id, &s.kind))
                            .collect(),
                    )
                })
                .collect();
            let hash = digest(
                serde_json::to_vec(&(
                    &manifest.files[file].file_hash,
                    &indexer,
                    &occurrences,
                    targets,
                ))
                .unwrap(),
            );
            let cached = self
                .files
                .get(file)
                .filter(|(old, _, _)| old == &hash)
                .map(|(_, relations, _)| relations.clone());
            let relations = if let Some(cached) = cached {
                cached
            } else {
                self.files_rebuilt += 1;
                let mut relations = BTreeMap::new();
                for occurrence in occurrences {
                    check()?;
                    let Some(targets) = by_name.get(occurrence.name.as_str()) else {
                        continue;
                    };
                    let caller = occurrence
                        .enclosing_symbol_id
                        .clone()
                        .unwrap_or_else(|| format!("file:{}", occurrence.file));
                    let span = serde_json::to_string(&occurrence.span).unwrap();
                    for symbol in targets {
                        check()?;
                        if occurrence.kind == "call"
                            && !matches!(
                                symbol.kind.as_str(),
                                "function" | "trait_method" | "struct"
                            )
                        {
                            continue;
                        }
                        let id = identity(
                            "rel",
                            &[
                                &occurrence.kind,
                                &caller,
                                &symbol.symbol_id,
                                &occurrence.file,
                                &span,
                            ],
                        );
                        relations.insert(id.clone(), Relation {
                            relation_id:id.clone(), kind:occurrence.kind.clone(),from_symbol:caller.clone(),to_symbol:symbol.symbol_id.clone(),file:occurrence.file.clone(),span:occurrence.span,
                            certainty:Certainty::Candidate,freshness:"current".into(),runtime:"unavailable".into(),origin:None,
                            evidence:vec![Evidence { evidence_id:identity("ev", &[&id,"L1"]), source:"L1".into(),certainty:Certainty::Candidate,indexer:indexer.clone(),derivation:"syntax_occurrence+name_match; lexical scope and shadowing not resolved".into(),file_hash:manifest.files[file].file_hash.clone() }],
                        });
                    }
                }
                let relations = relations.into_values().collect::<Vec<_>>();
                // Account allocated structs and string buffers without serializing
                // every edge a second time solely to weigh the cache. Syntax
                // relations have no origin; evidence buffers are owned per edge.
                let buffers: usize = relations
                    .iter()
                    .map(|r| {
                        r.relation_id.capacity()
                            + r.kind.capacity()
                            + r.from_symbol.capacity()
                            + r.to_symbol.capacity()
                            + r.file.capacity()
                            + r.freshness.capacity()
                            + r.runtime.capacity()
                            + r.evidence.capacity() * std::mem::size_of::<Evidence>()
                            + r.evidence
                                .iter()
                                .map(|e| {
                                    e.evidence_id.capacity()
                                        + e.source.capacity()
                                        + e.indexer.capacity()
                                        + e.derivation.capacity()
                                        + e.file_hash.capacity()
                                })
                                .sum::<usize>()
                    })
                    .sum();
                let weight = (relations.capacity() * std::mem::size_of::<Relation>() + buffers)
                    .saturating_mul(2);
                let relations = Arc::new(relations);
                self.files
                    .insert(file.into(), (hash, relations.clone(), weight));
                relations
            };
            for relation in relations.iter() {
                check()?;
                result.insert(relation.relation_id.clone(), relation.clone());
            }
            while self
                .files
                .values()
                .fold(0usize, |sum, (_, _, weight)| sum.saturating_add(*weight))
                > 64 * 1024 * 1024
            {
                self.files.pop_first();
            }
        }
        Ok(result.into_values().collect())
    }
}
pub fn syntax_relations(view: &AnalysisView) -> Vec<Relation> {
    RelationDeriver::default().derive(view)
}

#[cfg(test)]
fn syntax_relations_reference(view: &AnalysisView) -> Vec<Relation> {
    let mut by_name: BTreeMap<&str, Vec<&Symbol>> = BTreeMap::new();
    for symbol in &view.snapshot.syntax.symbols {
        by_name.entry(&symbol.name).or_default().push(symbol);
    }
    let mut edges = BTreeMap::new();
    for occurrence in &view.snapshot.syntax.occurrences {
        let Some(targets) = by_name.get(occurrence.name.as_str()) else {
            continue;
        };
        for symbol in targets {
            if occurrence.kind == "call"
                && !matches!(symbol.kind.as_str(), "function" | "trait_method" | "struct")
            {
                continue;
            }
            let caller = occurrence
                .enclosing_symbol_id
                .clone()
                .unwrap_or_else(|| format!("file:{}", occurrence.file));
            let id = identity(
                "rel",
                &[
                    &occurrence.kind,
                    &caller,
                    &symbol.symbol_id,
                    &occurrence.file,
                    &serde_json::to_string(&occurrence.span).unwrap(),
                ],
            );
            edges.insert(
                id.clone(),
                Relation {
                    relation_id: id.clone(),
                    kind: occurrence.kind.clone(),
                    from_symbol: caller,
                    to_symbol: symbol.symbol_id.clone(),
                    file: occurrence.file.clone(),
                    span: occurrence.span,
                    certainty: Certainty::Candidate,
                    freshness: "current".into(),
                    runtime: "unavailable".into(),
                    origin: None,
                    evidence: vec![Evidence {
                        evidence_id: identity("ev", &[&id, "L1"]),
                        source: "L1".into(),
                        certainty: Certainty::Candidate,
                        indexer: view
                            .snapshot
                            .manifest
                            .analysis_versions
                            .get("syntax")
                            .cloned()
                            .unwrap_or_else(crate::frontend::version),
                        derivation:
                            "syntax_occurrence+name_match; lexical scope and shadowing not resolved"
                                .into(),
                        file_hash: view.snapshot.manifest.files[&occurrence.file]
                            .file_hash
                            .clone(),
                    }],
                },
            );
        }
    }
    edges.into_values().collect()
}

pub fn relations(stored: &StoredView) -> Vec<Relation> {
    stored.index().relations().to_vec()
}

pub(crate) fn relations_with_deadline(
    stored: &StoredView,
    deadline: Instant,
) -> Result<Vec<Relation>> {
    let check = || crate::query::check_deadline(deadline);
    check()?;
    if let Some(index) = stored.index.get() {
        return index
            .relations()
            .iter()
            .map(|r| {
                check()?;
                Ok(r.clone())
            })
            .collect();
    }
    let derived = stored
        .deriver
        .lock()
        .unwrap()
        .derive_checked(&stored.view, check)?;
    let mut merged = BTreeMap::new();
    for r in derived {
        check()?;
        merged.insert(r.relation_id.clone(), r);
    }
    for relation in stored.view.relations.iter().chain(stored.references.iter()) {
        check()?;
        match merged.get_mut(&relation.relation_id) {
            Some(existing) => {
                existing.evidence.extend(relation.evidence.clone());
                existing.certainty = relation.certainty.clone();
                existing.freshness = relation.freshness.clone();
                existing.origin = relation.origin.clone();
            }
            None => {
                merged.insert(relation.relation_id.clone(), relation.clone());
            }
        }
    }
    Ok(merged.into_values().collect())
}

fn merged_relations(stored: &StoredView) -> Vec<Relation> {
    let mut merged: BTreeMap<_, _> = stored
        .deriver
        .lock()
        .unwrap()
        .derive(&stored.view)
        .into_iter()
        .map(|r| (r.relation_id.clone(), r))
        .collect();
    for relation in stored.view.relations.iter().chain(stored.references.iter()) {
        match merged.get_mut(&relation.relation_id) {
            Some(existing) => {
                existing.evidence.extend(relation.evidence.clone());
                existing.certainty = relation.certainty.clone();
                existing.freshness = relation.freshness.clone();
                existing.origin = relation.origin.clone();
            }
            None => {
                merged.insert(relation.relation_id.clone(), relation.clone());
            }
        }
    }
    merged.into_values().collect()
}

#[cfg(test)]
mod index_tests {
    use super::*;

    #[test]
    fn index_retries_only_transient_input_changes() {
        let mut attempts = 0;
        let result = retry_input_changes(Instant::now() + Duration::from_secs(2), || {
            attempts += 1;
            if attempts < 4 {
                Err(InputsChanged.into())
            } else {
                Ok("stable")
            }
        })
        .unwrap();
        assert_eq!(result, "stable");
        assert_eq!(attempts, 4);
    }

    #[test]
    fn index_does_not_retry_permanent_refresh_errors() {
        let mut attempts = 0;
        let result: Result<()> =
            retry_input_changes(Instant::now() + Duration::from_secs(2), || {
                attempts += 1;
                bail!("index_refresh_failed: invalid fixture");
            });
        assert!(result.unwrap_err().to_string().contains("invalid fixture"));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn expired_index_does_not_start_a_scan() {
        let result: Result<()> = retry_input_changes(Instant::now(), || panic!("expired scan ran"));
        assert!(result.unwrap_err().to_string().starts_with("timeout:"));
    }

    #[test]
    fn continuously_changing_index_inputs_respect_the_deadline() {
        let started = Instant::now();
        let mut attempts = 0;
        let result: Result<()> = retry_input_changes(started + Duration::from_millis(40), || {
            attempts += 1;
            Err(InputsChanged.into())
        });
        assert!(result.unwrap_err().to_string().starts_with("timeout:"));
        assert!(attempts > 0);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn retained_semantics_require_matching_go_build_environment() {
        let temp = tempfile::tempdir().unwrap();
        let resolved = Resolved {
            root: temp.path().into(),
            config_path: None,
            config: config::Config::default(),
            workspace_id: "cache-tests".into(),
            cache: temp.path().join("cache"),
            index_dir: temp.path().join("index"),
            config_dir: temp.path().join("config"),
        };
        let engine = Engine::new(resolved).unwrap();
        let mut held = stored("build-context", 100);
        let view = Arc::make_mut(&mut held.view);
        let snapshot = Arc::make_mut(&mut view.snapshot);
        snapshot
            .manifest
            .analysis_versions
            .insert("go_build_environment".into(), "one".into());
        snapshot.input_hash = snapshot.manifest.input_hash();
        let original = snapshot.manifest.clone();
        {
            let mut state = engine.state.locked();
            state.coordinator = RefreshCoordinator::restore(vec![held.view.clone()]).unwrap();
            state.set_extra(&held.view, vec![], vec![]);
        }
        assert_eq!(
            engine
                .publish_revision(original.clone())
                .unwrap()
                .view
                .snapshot
                .snapshot_id,
            "build-context"
        );
        let mut changed = original;
        changed
            .analysis_versions
            .insert("go_build_environment".into(), "two".into());
        let historical = engine.publish_revision(changed).unwrap();
        assert_ne!(historical.view.snapshot.snapshot_id, "build-context");
        assert_ne!(
            historical.view.epochs.semantic_epoch,
            historical.view.epochs.syntax_epoch
        );
        engine.shutdown();
    }

    #[test]
    fn cache_enforces_byte_budget_and_rechecks_lazy_index_growth() {
        let first = stored("bytes-a", 0);
        let second = stored("bytes-b", 0);
        let budget =
            first.weight.load(Ordering::Relaxed) + second.weight.load(Ordering::Relaxed) - 1;
        let mut cache = QueryViews::default();
        cache.set_limits(8, budget);
        cache.insert(first.clone());
        cache.insert(second.clone());
        assert!(cache.get("bytes-a:0").is_none());
        let held = cache.get("bytes-b:0").unwrap();
        // Model growth from an index constructed outside the state lock.
        held.weight.store(budget + 1, Ordering::Relaxed);
        assert!(cache.get("bytes-b:0").is_some());
        assert!(cache.items.is_empty());
        assert_eq!(held.view.snapshot.snapshot_id, "bytes-b");
        cache.set_limits(0, usize::MAX);
        cache.insert(first.clone());
        assert!(cache.items.is_empty());
        assert_eq!(first.index().view().snapshot.snapshot_id, "bytes-a");
    }

    #[test]
    fn persistence_releases_query_state_and_failed_batches_are_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let resolved = Resolved {
            root: temp.path().into(),
            config_path: None,
            config: config::Config::default(),
            workspace_id: "cache-tests".into(),
            cache: temp.path().join("cache"),
            index_dir: temp.path().join("index"),
            config_dir: temp.path().join("config"),
        };
        let engine = Engine::new(resolved).unwrap();
        let _writer = engine.writer.locked();
        let mut state = engine.state.locked();
        let held = stored("concurrent", 0);
        state.coordinator = RefreshCoordinator::restore(vec![held.view.clone()]).unwrap();
        state.set_extra(&held.view, vec![], vec![]);
        let failed = engine.persist_using(state, |_, _, _| {
            assert!(engine.state.try_lock().is_ok());
            bail!("injected disk error")
        });
        assert!(failed.is_err());
        let state = engine.state.locked();
        assert!(!state.dirty_extra.is_empty());
        let state = engine
            .persist_using(state, |store, latest, updates| {
                // A separate reader can bind and build its index during a slow commit.
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            let mut state = engine.state.locked();
                            assert_eq!(
                                state
                                    .stored_view(held.view.clone())
                                    .unwrap()
                                    .index()
                                    .view()
                                    .snapshot
                                    .snapshot_id,
                                "concurrent"
                            );
                        })
                        .join()
                        .unwrap();
                });
                store.commit(latest, updates).map(|_| ())
            })
            .unwrap();
        assert!(state.dirty_extra.is_empty());
        drop(state);
        drop(_writer);
        engine.shutdown();
        assert_eq!(
            SnapshotStore::open_lazy(&temp.path().join("index"), "cache-tests")
                .unwrap()
                .0
                .entries()
                .count(),
            1
        );
    }

    #[test]
    fn capture_and_analysis_do_not_hold_query_state_and_changed_inputs_do_not_publish() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname=\"lock-test\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        fs::write(temp.path().join("src/lib.rs"), "pub fn first() {}\n").unwrap();
        // Keep the normal computed paths; no trust is granted and all created
        // storage is redirected by a dedicated configuration file.
        let config_path = temp.path().join("semweft.toml");
        fs::write(
            &config_path,
            format!(
                "schema_version=\"0.1\"\n[index]\ndirectory={:?}\n",
                temp.path().join("index").to_string_lossy()
            ),
        )
        .unwrap();
        let resolved = config::resolve(Some(temp.path()), Some(&config_path)).unwrap();
        let engine = Engine::new(resolved).unwrap();
        let first = engine
            .scan_using(false, false, |resolved, backend| {
                assert!(engine.state.try_lock().is_ok());
                workspace::capture_config(resolved, backend)
            })
            .unwrap();
        let mut calls = 0;
        let failed = engine.scan_using(false, false, |resolved, backend| {
            assert!(engine.state.try_lock().is_ok());
            calls += 1;
            let result = workspace::capture_config(resolved, backend)?;
            if calls == 1 {
                fs::write(temp.path().join("src/lib.rs"), "pub fn changed() {}\n")?;
            }
            Ok(result)
        });
        assert!(failed.unwrap_err().to_string().contains("inputs changed"));
        assert_eq!(
            engine
                .state
                .lock()
                .unwrap()
                .coordinator
                .latest()
                .unwrap()
                .snapshot
                .snapshot_id,
            first.snapshot.snapshot_id
        );
        let mut captures = 0;
        let stable = retry_input_changes(Instant::now() + Duration::from_secs(2), || {
            engine.scan_using(false, false, |resolved, backend| {
                let captured = workspace::capture_config(resolved, backend)?;
                captures += 1;
                if captures == 1 {
                    fs::write(temp.path().join("src/lib.rs"), "pub fn stable() {}\n")?;
                }
                Ok(captured)
            })
        })
        .unwrap();
        assert_eq!(captures, 4);
        assert_ne!(stable.snapshot.snapshot_id, first.snapshot.snapshot_id);
        assert_eq!(
            stable.snapshot.manifest.files["src/lib.rs"].content,
            "pub fn stable() {}\n"
        );
        engine.shutdown();
    }

    #[test]
    fn cached_relation_derivation_matches_full_scan_and_invalidates_cross_file_targets() {
        let mut manifest = stored("derive", 0).view.snapshot.manifest.clone();
        manifest.files = BTreeMap::from([
            (
                "a.rs".into(),
                SourceFile::new("pub fn target() {} pub fn caller() { target(); }"),
            ),
            ("b.rs".into(), SourceFile::new("pub fn b() { target(); }")),
        ]);
        let mut parser = Frontends::new().unwrap();
        let mut coordinator = RefreshCoordinator::default();
        let mut derive = RelationDeriver::default();
        for text in [
            "pub fn target() {} pub fn caller() { target(); }",
            "pub fn target() {} pub fn caller() { target(); }",
            "pub struct target;",
            "pub type target = u32;",
            "pub fn renamed() {}",
        ] {
            manifest.files.insert("a.rs".into(), SourceFile::new(text));
            let syntax = parser.analyze(&manifest).unwrap();
            let view = coordinator
                .publish_inputs(manifest.clone(), syntax)
                .unwrap();
            assert_eq!(derive.derive(&view), syntax_relations_reference(&view));
            derive.derive(&view);
            assert_eq!(derive.files_rebuilt, 0);
        }
    }

    fn stored(id: &str, semantic: u64) -> StoredView {
        let manifest = Manifest {
            workspace_id: "cache-tests".into(),
            files: BTreeMap::new(),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "cache fixture".into(),
        };
        StoredView::new(
            Arc::new(AnalysisView {
                snapshot: Arc::new(Snapshot {
                    snapshot_id: id.into(),
                    syntax_epoch: 100,
                    input_hash: manifest.input_hash(),
                    manifest,
                    syntax: SyntaxFacts::default(),
                }),
                epochs: Epochs {
                    syntax_epoch: 100,
                    semantic_epoch: semantic,
                },
                relations: vec![],
                pending_units: vec![],
                known_gaps: vec![],
                resolutions: vec![],
            }),
            vec![],
            vec![],
        )
    }

    #[test]
    fn cache_separates_version_pairs_and_shares_one_index_between_concurrent_readers() {
        let mut cache = QueryViews::default();
        let a = stored("s1", 0);
        cache.insert(a.clone());
        cache.insert(stored("s1", 100));
        cache.insert(stored("s2", 0));
        let hit = cache.get(&view_key(&a.view)).unwrap();
        assert!(Arc::ptr_eq(&hit.index, &a.index));
        assert!(!Arc::ptr_eq(&cache.get("s1:100").unwrap().index, &a.index));
        assert!(!Arc::ptr_eq(&cache.get("s2:0").unwrap().index, &a.index));
        std::thread::scope(|scope| {
            let readers = (0..8)
                .map(|_| scope.spawn(|| hit.index()))
                .collect::<Vec<_>>();
            for reader in readers {
                assert!(std::ptr::eq(reader.join().unwrap(), a.index()));
            }
        });
    }

    #[test]
    fn least_recently_used_eviction_and_invalidation_keep_active_views_readable() {
        let mut cache = QueryViews::default();
        let first = stored("s0", 0);
        let held = stored("s1", 0);
        for i in 0..QUERY_VIEW_CAPACITY {
            cache.insert(match i {
                0 => first.clone(),
                1 => held.clone(),
                _ => stored(&format!("s{i}"), 0),
            });
        }
        cache.get("s0:0").unwrap();
        cache.insert(stored("new", 0));
        assert_eq!(cache.items.len(), QUERY_VIEW_CAPACITY);
        assert!(cache.get("s1:0").is_none());
        assert!(cache.get("s0:0").is_some());
        assert_eq!(held.index().view().snapshot.snapshot_id, "s1");
        cache.remove("s0:0");
        assert!(cache.get("s0:0").is_none());
        assert_eq!(first.index().view().snapshot.snapshot_id, "s0");
        let rebuilt = stored("s0", 0);
        assert!(!Arc::ptr_eq(&rebuilt.index, &first.index));
        cache.insert(rebuilt);
        assert!(cache.get("s0:0").is_some());
    }
}
