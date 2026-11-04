//! Single-writer, content-addressed publications. Objects become visible only
//! when an incremental journal segment or catalog checkpoint is published; orphan objects are
//! harmless and can be reused after an interrupted commit.
use anyhow::{Context, Result, bail, ensure};
use semweft_core::{
    AnalysisView, CallResolution, Diagnostic, Epochs, Manifest, RefreshCoordinator, Relation,
    Snapshot, SourceFile, SyntaxFacts, digest,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, Weak},
    time::Instant,
};

mod syntax;
use semweft_storage::*;

fn key(view: &AnalysisView) -> ViewKey {
    (
        view.snapshot.snapshot_id.clone(),
        view.epochs.semantic_epoch,
    )
}

fn reuse_hash(manifest: &Manifest) -> String {
    let files: BTreeMap<_, _> = manifest
        .files
        .iter()
        .map(|(path, file)| (path, &file.file_hash))
        .collect();
    digest(
        serde_json::to_vec(&(
            files,
            &manifest.binary_files,
            manifest.analysis_versions.get("analysis_config"),
            manifest.analysis_versions.get("syntax"),
            manifest.analysis_versions.get("go_build_environment"),
        ))
        .unwrap(),
    )
}

fn view_info(view: &AnalysisView) -> ViewInfo {
    ViewInfo {
        syntax_epoch: view.epochs.syntax_epoch,
        input_hash: view.snapshot.input_hash.clone(),
        reuse_hash: reuse_hash(&view.snapshot.manifest),
        revision: view
            .snapshot
            .manifest
            .analysis_versions
            .contains_key("revision"),
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewEntry {
    key: ViewKey,
    object: String,
    #[serde(default)]
    info: Option<ViewInfo>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema_version: String,
    workspace_id: String,
    latest: Option<ViewKey>,
    views: Vec<ViewEntry>,
    #[serde(default)]
    pins: BTreeSet<ViewKey>,
    #[serde(default)]
    syntax_high_water: u64,
    #[serde(default)]
    semantic_high_water: u64,
    #[serde(default)]
    sequence: u64,
}

/// Each transaction is an atomically published segment. Temporary files are
/// never replayed, so process death cannot expose a partial log record.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogDelta {
    sequence: u64,
    workspace_id: String,
    latest: Option<ViewKey>,
    upserts: Vec<ViewEntry>,
    removed: Vec<ViewKey>,
    pins: BTreeSet<ViewKey>,
    syntax_high_water: u64,
    semantic_high_water: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogRecord {
    checksum: String,
    delta: CatalogDelta,
}

#[derive(Deserialize)]
struct LegacyStore {
    workspace_id: String,
    latest: Option<ViewKey>,
    views: Vec<Publication>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestObject {
    workspace_id: String,
    files: BTreeMap<String, String>,
    binary_files: BTreeMap<String, String>,
    analysis_versions: BTreeMap<String, String>,
    scope: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotObject {
    snapshot_id: String,
    syntax_epoch: u64,
    input_hash: String,
    manifest: ManifestObject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    syntax: Option<SyntaxFacts>,
    #[serde(default)]
    syntax_files: BTreeMap<String, String>,
    #[serde(default)]
    syntax_order: Option<syntax::SyntaxOrder>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewObject {
    snapshot: String,
    epochs: Epochs,
    relations: Vec<Relation>,
    pending_units: Vec<String>,
    known_gaps: Vec<Diagnostic>,
    resolutions: Vec<CallResolution>,
    references: Vec<Relation>,
    reference_gaps: Vec<Diagnostic>,
}

/// A daemon must hold its workspace writer lock while opening and committing.
/// History is retained until an explicit retention/GC transaction.
#[derive(Clone)]
pub struct SnapshotStore {
    root: PathBuf,
    catalog: Catalog,
    published: bool,
    uncertain: bool,
    known_views: BTreeMap<ViewKey, Weak<AnalysisView>>,
    snapshots: BTreeMap<String, (Weak<Snapshot>, String)>,
    sources: BTreeSet<String>,
    live_views: Arc<Mutex<Vec<Weak<AnalysisView>>>>,
    io_gate: Arc<RwLock<BTreeMap<ViewKey, String>>>,
    metrics: Arc<Mutex<StorageMetrics>>,
}

struct GcPlan {
    catalog: Catalog,
    marked: BTreeMap<&'static str, BTreeSet<String>>,
    candidates: Vec<PathBuf>,
    stats: GcStats,
}

impl SnapshotStore {
    /// Restores 0.2 objects, or migrates a validated 0.1 monolithic index. The
    /// legacy file remains intact until all objects and the new catalog exist.
    pub fn open(root: &Path, workspace_id: &str) -> Result<(Self, Option<RecoveredStore>)> {
        Self::open_mode(root, workspace_id, true)
    }

    pub fn open_lazy(root: &Path, workspace_id: &str) -> Result<(Self, Option<RecoveredStore>)> {
        Self::open_mode(root, workspace_id, false)
    }

    fn open_mode(
        root: &Path,
        workspace_id: &str,
        eager: bool,
    ) -> Result<(Self, Option<RecoveredStore>)> {
        let started = Instant::now();
        let mut store = Self {
            root: root.into(),
            catalog: Catalog {
                schema_version: "0.2".into(),
                workspace_id: workspace_id.into(),
                latest: None,
                views: vec![],
                pins: BTreeSet::new(),
                syntax_high_water: 0,
                semantic_high_water: 0,
                sequence: 0,
            },
            published: false,
            uncertain: false,
            known_views: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            sources: BTreeSet::new(),
            live_views: Arc::new(Mutex::new(vec![])),
            io_gate: Arc::new(RwLock::new(BTreeMap::new())),
            metrics: Arc::new(Mutex::new(StorageMetrics::default())),
        };
        let path = root.join("views.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((store, None)),
            Err(error) => return Err(error).context("index_store_error: reading catalog"),
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .context("index_store_error: corrupt persistent index")?;
        let recovered = match value["schema_version"].as_str() {
            Some("0.1") => {
                let legacy: LegacyStore = serde_json::from_value(value)
                    .context("index_store_error: corrupt legacy index")?;
                ensure!(
                    legacy.workspace_id == workspace_id,
                    "index_store_error: workspace identity mismatch"
                );
                let recovered = RecoveredStore {
                    latest: legacy.latest,
                    publications: legacy.views,
                };
                validate_recovery(&recovered, workspace_id)?;
                let updates = recovered
                    .publications
                    .iter()
                    .map(|item| PublicationRef {
                        view: &item.view,
                        references: &item.references,
                        reference_gaps: &item.reference_gaps,
                    })
                    .collect::<Vec<_>>();
                store.commit(recovered.latest.clone(), &updates)?;
                if eager {
                    recovered
                } else {
                    let roots = store.root_keys();
                    RecoveredStore {
                        latest: recovered.latest,
                        publications: recovered
                            .publications
                            .into_iter()
                            .filter(|item| roots.contains(&key(&item.view)))
                            .collect(),
                    }
                }
            }
            Some("0.2") => {
                let mut catalog: Catalog =
                    serde_json::from_value(value).context("index_store_error: invalid catalog")?;
                ensure!(
                    catalog.workspace_id == workspace_id,
                    "index_store_error: workspace identity mismatch"
                );
                store.replay(&mut catalog)?;
                let mut seen = BTreeSet::new();
                let mut upgraded = false;
                for entry in &mut catalog.views {
                    ensure!(
                        seen.insert(entry.key.clone()),
                        "index_store_error: duplicate publication"
                    );
                    if entry.info.is_none() {
                        let item = store.read_publication(entry)?;
                        entry.info = Some(view_info(&item.view));
                        upgraded = true;
                    }
                    ensure!(
                        entry.key.1 <= entry.info.as_ref().unwrap().syntax_epoch,
                        "index_store_error: invalid catalog epochs"
                    );
                    store.object_path("views", &entry.object)?;
                }
                ensure!(
                    catalog.latest.as_ref().is_none_or(|key| seen.contains(key)),
                    "index_store_error: latest publication unavailable"
                );
                ensure!(
                    catalog.pins.iter().all(|key| seen.contains(key)),
                    "index_store_error: pinned publication unavailable"
                );
                for entry in &catalog.views {
                    catalog.syntax_high_water = catalog
                        .syntax_high_water
                        .max(entry.info.as_ref().unwrap().syntax_epoch);
                    catalog.semantic_high_water = catalog.semantic_high_water.max(entry.key.1);
                }
                ensure!(
                    catalog.semantic_high_water <= catalog.syntax_high_water,
                    "index_store_error: invalid epoch high water"
                );
                store.catalog = catalog;
                let roots = store.root_keys();
                let entries = store.catalog.views.clone();
                let mut publications = vec![];
                for entry in &entries {
                    if eager || roots.contains(&entry.key) {
                        publications.push(store.read_publication(entry)?);
                    }
                }
                let recovered = RecoveredStore {
                    latest: store.catalog.latest.clone(),
                    publications,
                };
                validate_recovery(&recovered, workspace_id)?;
                if upgraded {
                    atomic_write(&path, &serde_json::to_vec(&store.catalog)?)?;
                }
                *store.io_gate.write().unwrap() = store
                    .catalog
                    .views
                    .iter()
                    .map(|entry| (entry.key.clone(), entry.object.clone()))
                    .collect();
                store.published = true;
                recovered
            }
            _ => bail!("index_store_error: unsupported persistent index version"),
        };
        {
            let mut metrics = store.metrics.lock().unwrap();
            metrics.recovery_ms = started.elapsed().as_secs_f64() * 1000.;
            metrics.recovered_views = recovered.publications.len();
        }
        Ok((store, Some(recovered)))
    }

    fn replay(&self, catalog: &mut Catalog) -> Result<()> {
        let dir = self.root.join("journal");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).context("index_store_error: reading journal"),
        };
        private_dir(&dir)?;
        let mut paths = BTreeMap::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let sequence = name
                .strip_suffix(".json")
                .filter(|n| n.len() == 20)
                .and_then(|n| n.parse::<u64>().ok())
                .context("index_store_error: unexpected journal segment")?;
            if sequence > catalog.sequence {
                ensure!(
                    entry.file_type()?.is_file(),
                    "index_store_error: journal is not a regular file"
                );
                paths.insert(sequence, entry.path());
            }
        }
        for (sequence, path) in paths {
            ensure!(
                catalog.sequence.checked_add(1) == Some(sequence),
                "index_store_error: journal sequence gap"
            );
            let record: LogRecord = serde_json::from_slice(&fs::read(path)?)
                .context("index_store_error: corrupt journal")?;
            let delta = record.delta;
            ensure!(
                digest(serde_json::to_vec(&delta)?) == record.checksum
                    && delta.sequence == sequence
                    && delta.workspace_id == catalog.workspace_id
                    && delta.syntax_high_water >= catalog.syntax_high_water
                    && delta.semantic_high_water >= catalog.semantic_high_water,
                "index_store_error: journal integrity mismatch"
            );
            catalog
                .views
                .retain(|entry| !delta.removed.contains(&entry.key));
            for entry in delta.upserts {
                if let Some(old) = catalog.views.iter_mut().find(|old| old.key == entry.key) {
                    *old = entry;
                } else {
                    catalog.views.push(entry);
                }
            }
            catalog.latest = delta.latest;
            catalog.pins = delta.pins;
            catalog.syntax_high_water = delta.syntax_high_water;
            catalog.semantic_high_water = delta.semantic_high_water;
            catalog.sequence = sequence;
        }
        Ok(())
    }

    // A rename may have succeeded even if a later fsync reports failure. Before
    // another mutation, reload the visible transaction and retry directory sync.
    fn reconcile(&mut self) -> Result<()> {
        if !self.uncertain {
            return Ok(());
        }
        #[cfg(unix)]
        for path in [self.root.clone(), self.root.join("journal")] {
            if path.exists() {
                fs::File::open(path)?
                    .sync_all()
                    .context("index_store_error: syncing uncertain transaction")?;
            }
        }
        let (mut recovered, _) = Self::open_lazy(&self.root, &self.catalog.workspace_id)?;
        recovered.live_views = self.live_views.clone();
        recovered.io_gate = self.io_gate.clone();
        *recovered.io_gate.write().unwrap() = recovered
            .catalog
            .views
            .iter()
            .map(|entry| (entry.key.clone(), entry.object.clone()))
            .collect();
        recovered.metrics = self.metrics.clone();
        *self = recovered;
        Ok(())
    }

    fn publish_catalog(
        &mut self,
        catalog: &mut Catalog,
        checkpoint: bool,
        publish: impl FnOnce(&Path, &[u8]) -> Result<()>,
    ) -> Result<usize> {
        let result = self.publish_catalog_inner(catalog, checkpoint, publish);
        self.uncertain |= result.is_err();
        result
    }

    fn publish_catalog_inner(
        &self,
        catalog: &mut Catalog,
        checkpoint: bool,
        publish: impl FnOnce(&Path, &[u8]) -> Result<()>,
    ) -> Result<usize> {
        catalog.sequence = self
            .catalog
            .sequence
            .checked_add(1)
            .context("index_store_error: log sequence exhausted")?;
        if !self.published || checkpoint || catalog.sequence.is_multiple_of(64) {
            let bytes = serde_json::to_vec(catalog)?;
            publish(&self.root.join("views.json"), &bytes)?;
            // The checkpoint is durable before old log segments are removed.
            // Cleanup failures are harmless and replay ignores covered segments.
            if let Ok(entries) = fs::read_dir(self.root.join("journal")) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name
                        .strip_suffix(".json")
                        .and_then(|n| n.parse::<u64>().ok())
                        .is_some_and(|n| n <= catalog.sequence)
                    {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
            Ok(bytes.len())
        } else {
            let old: BTreeMap<_, _> = self
                .catalog
                .views
                .iter()
                .map(|entry| (&entry.key, entry))
                .collect();
            let new: BTreeSet<_> = catalog.views.iter().map(|entry| &entry.key).collect();
            let delta = CatalogDelta {
                sequence: catalog.sequence,
                workspace_id: catalog.workspace_id.clone(),
                latest: catalog.latest.clone(),
                upserts: catalog
                    .views
                    .iter()
                    .filter(|entry| old.get(&entry.key).is_none_or(|old| *old != *entry))
                    .cloned()
                    .collect(),
                removed: self
                    .catalog
                    .views
                    .iter()
                    .filter(|entry| !new.contains(&entry.key))
                    .map(|entry| entry.key.clone())
                    .collect(),
                pins: catalog.pins.clone(),
                syntax_high_water: catalog.syntax_high_water,
                semantic_high_water: catalog.semantic_high_water,
            };
            let checksum = digest(serde_json::to_vec(&delta)?);
            let bytes = serde_json::to_vec(&LogRecord { checksum, delta })?;
            publish(
                &self
                    .root
                    .join("journal")
                    .join(format!("{:020}.json", catalog.sequence)),
                &bytes,
            )?;
            Ok(bytes.len())
        }
    }

    pub fn metrics(&self) -> StorageMetrics {
        self.metrics.lock().unwrap().clone()
    }
    pub fn latest_key(&self) -> &Option<ViewKey> {
        &self.catalog.latest
    }
    pub fn journal_sequence(&self) -> u64 {
        self.catalog.sequence
    }

    /// Pointer identity skips historical payload comparisons and serialization.
    pub fn is_persisted(&self, view: &Arc<AnalysisView>) -> bool {
        self.known_views
            .get(&key(view))
            .is_some_and(|old| old.ptr_eq(&Arc::downgrade(view)))
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = (&ViewKey, &ViewInfo)> {
        self.catalog.views.iter().map(|entry| {
            (
                &entry.key,
                entry.info.as_ref().expect("catalog metadata initialized"),
            )
        })
    }

    pub fn high_water(&self) -> (u64, u64) {
        (
            self.catalog.syntax_high_water,
            self.catalog.semantic_high_water,
        )
    }

    pub fn pins(&self) -> &BTreeSet<ViewKey> {
        &self.catalog.pins
    }

    pub fn pin(&mut self, version: &ViewKey, pinned: bool) -> Result<()> {
        self.reconcile()?;
        ensure!(
            self.catalog.views.iter().any(|entry| &entry.key == version),
            "snapshot_expired: publication unavailable"
        );
        let mut catalog = self.catalog.clone();
        if pinned {
            catalog.pins.insert(version.clone());
        } else {
            catalog.pins.remove(version);
        }
        if catalog != self.catalog {
            self.publish_catalog(&mut catalog, false, atomic_write)?;
            self.catalog = catalog;
        }
        Ok(())
    }

    /// Single writer mark/sweep. Planning validates roots before any mutation.
    pub fn gc(&mut self, keep_last: usize) -> Result<GcStats> {
        self.gc_with_budget(keep_last, None)
    }
    pub fn gc_with_budget(
        &mut self,
        keep_last: usize,
        max_disk_bytes: Option<u64>,
    ) -> Result<GcStats> {
        let result = self.gc_inner(keep_last, max_disk_bytes);
        let mut metrics = self.metrics.lock().unwrap();
        metrics.gc_runs += 1;
        match &result {
            Ok(stats) => metrics.reclaimed_bytes += stats.reclaimed_bytes,
            Err(_) => metrics.gc_failures += 1,
        }
        result
    }
    pub fn preview_gc(&mut self, keep_last: usize, max_disk_bytes: Option<u64>) -> Result<GcStats> {
        self.reconcile()?;
        let io_gate = self.io_gate.clone();
        let _guard = io_gate.write().unwrap();
        Ok(self.gc_plan(keep_last, max_disk_bytes)?.stats)
    }
    pub fn disk_usage(&self) -> Result<DiskUsage> {
        let _guard = self.io_gate.read().unwrap();
        self.disk_usage_inner()
    }
    fn disk_usage_inner(&self) -> Result<DiskUsage> {
        fn size(path: &Path) -> Result<u64> {
            let entries = match fs::read_dir(path) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(e) => return Err(e.into()),
            };
            let mut bytes = 0;
            for entry in entries {
                let entry = entry?;
                let ty = entry.file_type()?;
                if ty.is_file() {
                    match entry.metadata() {
                        Ok(meta) => bytes += meta.len(),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                } else if ty.is_dir() {
                    bytes += size(&entry.path())?;
                }
            }
            Ok(bytes)
        }
        let mut usage = DiskUsage::default();
        for kind in ["sources", "syntax", "snapshots", "views"] {
            let dir = self.root.join("objects").join(kind);
            let mut objects = ObjectUsage::default();
            if dir.exists() {
                for entry in fs::read_dir(dir)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        objects.objects += 1;
                        objects.bytes += entry.metadata()?.len();
                    }
                }
            }
            usage.objects.insert(kind.into(), objects);
        }
        usage.catalog_bytes = match fs::symlink_metadata(self.root.join("views.json")) {
            Ok(meta) if meta.is_file() => meta.len(),
            Ok(_) => 0,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        usage.journal_bytes = size(&self.root.join("journal"))?;
        usage.total_bytes = size(&self.root)?;
        usage.other_bytes = usage.total_bytes.saturating_sub(
            usage.catalog_bytes
                + usage.journal_bytes
                + usage.objects.values().map(|o| o.bytes).sum::<u64>(),
        );
        Ok(usage)
    }
    fn protected_keys(&self) -> BTreeSet<ViewKey> {
        let mut roots = self.root_keys();
        roots.extend(self.catalog.pins.iter().cloned());
        let mut live = self.live_views.lock().unwrap();
        let mut seen = BTreeSet::new();
        live.retain(|weak| weak.strong_count() > 0 && seen.insert(weak.as_ptr() as usize));
        roots.extend(live.iter().filter_map(Weak::upgrade).map(|view| key(&view)));
        roots
    }
    fn mark_objects(&self, catalog: &Catalog) -> Result<BTreeMap<&'static str, BTreeSet<String>>> {
        let mut marked: BTreeMap<_, _> = ["views", "snapshots", "sources", "syntax"]
            .into_iter()
            .map(|kind| (kind, BTreeSet::new()))
            .collect();
        for entry in &catalog.views {
            if !marked
                .get_mut("views")
                .unwrap()
                .insert(entry.object.clone())
            {
                continue;
            }
            let view: ViewObject = self.read_json("views", &entry.object)?;
            if !marked
                .get_mut("snapshots")
                .unwrap()
                .insert(view.snapshot.clone())
            {
                continue;
            }
            let snapshot: SnapshotObject = self.read_json("snapshots", &view.snapshot)?;
            for hash in snapshot.syntax_files.values() {
                if marked.get_mut("syntax").unwrap().insert(hash.clone()) {
                    let _: SyntaxFacts = self.read_json("syntax", hash)?;
                }
            }
            for hash in snapshot.manifest.files.values() {
                if marked.get_mut("sources").unwrap().insert(hash.clone()) {
                    self.read_object("sources", hash)?;
                }
            }
        }
        Ok(marked)
    }
    fn gc_plan(&self, keep_last: usize, max_disk_bytes: Option<u64>) -> Result<GcPlan> {
        let protected = self.protected_keys();
        let mut epochs: Vec<_> = self.entries().map(|(_, info)| info.syntax_epoch).collect();
        epochs.sort_unstable();
        epochs.dedup();
        let mut retained: BTreeSet<_> = epochs.into_iter().rev().take(keep_last).collect();
        let usage = self.disk_usage_inner()?;
        loop {
            let mut catalog = self.catalog.clone();
            catalog.views.retain(|entry| {
                protected.contains(&entry.key)
                    || retained.contains(&entry.info.as_ref().unwrap().syntax_epoch)
            });
            catalog.sequence = self
                .catalog
                .sequence
                .checked_add(1)
                .context("index_store_error: log sequence exhausted")?;
            let marked = self.mark_objects(&catalog)?;
            let mut candidates = vec![];
            let mut reclaimed = 0;
            for (kind, reachable) in &marked {
                let dir = self.root.join("objects").join(kind);
                if !dir.exists() {
                    continue;
                }
                for entry in fs::read_dir(dir)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !reachable.contains(&name)
                        && self.object_path(kind, &name).is_ok()
                        && entry.file_type()?.is_file()
                    {
                        reclaimed += entry.metadata()?.len();
                        candidates.push(entry.path());
                    }
                }
            }
            // Only recognized journal segments are deleted by checkpointing.
            let mut journal_reclaimed = 0;
            let journal = self.root.join("journal");
            if journal.exists() {
                for entry in fs::read_dir(journal)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if entry.file_type()?.is_file()
                        && name
                            .strip_suffix(".json")
                            .and_then(|n| n.parse::<u64>().ok())
                            .is_some_and(|n| n <= catalog.sequence)
                    {
                        journal_reclaimed += entry.metadata()?.len();
                    }
                }
            }
            let remaining = usage
                .total_bytes
                .saturating_sub(reclaimed + journal_reclaimed + usage.catalog_bytes)
                + serde_json::to_vec(&catalog)?.len() as u64;
            let budget_unmet = max_disk_bytes.is_some_and(|max| remaining > max);
            if budget_unmet && retained.pop_first().is_some() {
                continue;
            }
            let stats = GcStats {
                removed_views: self.catalog.views.len() - catalog.views.len(),
                removed_objects: candidates.len(),
                reclaimed_bytes: reclaimed,
                remaining_bytes: remaining,
                max_disk_bytes,
                budget_unmet,
                protected_views: protected.len(),
            };
            return Ok(GcPlan {
                catalog,
                marked,
                candidates,
                stats,
            });
        }
    }
    fn gc_inner(&mut self, keep_last: usize, max_disk_bytes: Option<u64>) -> Result<GcStats> {
        self.reconcile()?;
        let io_gate = self.io_gate.clone();
        let mut durable_keys = io_gate.write().unwrap();
        let mut plan = self.gc_plan(keep_last, max_disk_bytes)?;
        self.publish_catalog(&mut plan.catalog, true, atomic_write)?;
        self.catalog = plan.catalog;
        *durable_keys = self
            .catalog
            .views
            .iter()
            .map(|entry| (entry.key.clone(), entry.object.clone()))
            .collect();
        self.known_views
            .retain(|key, _| durable_keys.contains_key(key));
        self.snapshots
            .retain(|id, _| durable_keys.keys().any(|key| &key.0 == id));
        self.sources = plan.marked["sources"].clone();
        for path in plan.candidates {
            private_dir(path.parent().unwrap())?;
            fs::remove_file(path)?;
        }
        plan.stats.remaining_bytes = self.disk_usage_inner()?.total_bytes;
        plan.stats.budget_unmet =
            max_disk_bytes.is_some_and(|max| plan.stats.remaining_bytes > max);
        Ok(plan.stats)
    }

    fn root_keys(&self) -> BTreeSet<ViewKey> {
        let latest = self
            .catalog
            .latest
            .clone()
            .or_else(|| self.catalog.views.last().map(|entry| entry.key.clone()));
        let base = self
            .entries()
            .rev()
            .find(|(key, info)| key.1 == info.syntax_epoch && !info.revision)
            .map(|(key, _)| key.clone());
        latest.into_iter().chain(base).collect()
    }

    pub fn find_key(&self, id: &str, epoch: Option<u64>) -> Option<ViewKey> {
        self.entries()
            .rev()
            .find(|(key, _)| key.0 == id && epoch.is_none_or(|epoch| key.1 == epoch))
            .map(|(key, _)| key.clone())
    }

    pub fn reusable(&self, manifest: &Manifest) -> Option<ViewKey> {
        let reuse = reuse_hash(manifest);
        self.entries()
            .rev()
            .find(|(key, info)| key.1 == info.syntax_epoch && info.reuse_hash == reuse)
            .or_else(|| {
                let input = manifest.input_hash();
                self.entries()
                    .rev()
                    .find(|(_, info)| info.input_hash == input)
            })
            .map(|(key, _)| key.clone())
    }

    pub fn load(&mut self, version: &ViewKey) -> Result<Publication> {
        let result = self.load_inner(version);
        let mut metrics = self.metrics.lock().unwrap();
        metrics.historical_loads += 1;
        metrics.load_failures += u64::from(result.is_err());
        result
    }

    fn load_inner(&mut self, version: &ViewKey) -> Result<Publication> {
        self.reconcile()?;
        let entry = self
            .catalog
            .views
            .iter()
            .find(|entry| &entry.key == version)
            .cloned()
            .context("snapshot_expired: publication unavailable")?;
        let publication = self.read_publication(&entry)?;
        validate_recovery(
            &RecoveredStore {
                latest: Some(version.clone()),
                publications: vec![Publication {
                    view: publication.view.clone(),
                    references: vec![],
                    reference_gaps: vec![],
                }],
            },
            &self.catalog.workspace_id,
        )?;
        Ok(publication)
    }

    /// `updates` contains only new views or changed supplemental evidence.
    /// Updates to existing version pairs cannot replace their published facts.
    pub fn commit(
        &mut self,
        latest: Option<ViewKey>,
        updates: &[PublicationRef<'_>],
    ) -> Result<WriteStats> {
        let started = Instant::now();
        let result = self.commit_with(latest, updates, atomic_write);
        let mut metrics = self.metrics.lock().unwrap();
        metrics.commits += 1;
        metrics.last_commit_ms = started.elapsed().as_secs_f64() * 1000.;
        metrics.total_commit_ms += metrics.last_commit_ms;
        match &result {
            Ok(stats) => {
                metrics.object_write_bytes += stats.object_bytes as u64;
                metrics.catalog_write_bytes += stats.catalog_bytes as u64;
                metrics.no_op_commits +=
                    u64::from(stats.object_bytes == 0 && stats.catalog_bytes == 0);
            }
            Err(_) => metrics.commit_failures += 1,
        }
        result
    }

    fn commit_with(
        &mut self,
        latest: Option<ViewKey>,
        updates: &[PublicationRef<'_>],
        publish: impl FnOnce(&Path, &[u8]) -> Result<()>,
    ) -> Result<WriteStats> {
        self.reconcile()?;
        if self.published && updates.is_empty() && latest == self.catalog.latest {
            return Ok(WriteStats::default());
        }
        let mut catalog = self.catalog.clone();
        let mut stats = WriteStats::default();
        let mut seen = BTreeSet::new();
        for item in updates {
            let version = key(item.view);
            ensure!(
                seen.insert(version.clone()),
                "index_store_error: duplicate publication update"
            );
            if let Some(old) = self.known_views.get(&version).and_then(Weak::upgrade) {
                ensure!(
                    Arc::ptr_eq(&old, item.view) || &old == item.view,
                    "index_store_error: immutable publication changed"
                );
            } else if self.catalog.views.iter().any(|entry| entry.key == version) {
                let old = self.load(&version)?;
                ensure!(
                    &old.view == item.view,
                    "index_store_error: immutable publication changed"
                );
            }
            RefreshCoordinator::restore(vec![item.view.clone()]).map_err(|error| {
                anyhow::anyhow!("index_store_error: invalid publication: {error}")
            })?;
            let snapshot = self.write_snapshot(&item.view.snapshot, &mut stats)?;
            let view = item.view;
            let object = ViewObject {
                snapshot,
                epochs: view.epochs.clone(),
                relations: view.relations.clone(),
                pending_units: view.pending_units.clone(),
                known_gaps: view.known_gaps.clone(),
                resolutions: view.resolutions.clone(),
                references: item.references.into(),
                reference_gaps: item.reference_gaps.into(),
            };
            let (object, written) = self.write_object("views", &serde_json::to_vec(&object)?)?;
            stats.views += usize::from(written.is_some());
            stats.object_bytes += written.unwrap_or(0);
            if let Some(entry) = catalog.views.iter_mut().find(|entry| entry.key == version) {
                entry.object = object;
                entry.info = Some(view_info(view));
            } else {
                catalog.views.push(ViewEntry {
                    key: version,
                    object,
                    info: Some(view_info(view)),
                });
            }
        }
        ensure!(
            latest
                .as_ref()
                .is_none_or(|key| catalog.views.iter().any(|entry| &entry.key == key)),
            "index_store_error: latest publication unavailable"
        );
        catalog.latest = latest;
        for entry in &catalog.views {
            catalog.syntax_high_water = catalog
                .syntax_high_water
                .max(entry.info.as_ref().unwrap().syntax_epoch);
            catalog.semantic_high_water = catalog.semantic_high_water.max(entry.key.1);
        }
        if !self.published || catalog != self.catalog {
            stats.catalog_bytes = self.publish_catalog(&mut catalog, false, publish)?;
        }
        // A failed catalog replacement must not mark any publication persisted.
        for item in updates {
            self.live_views
                .lock()
                .unwrap()
                .push(Arc::downgrade(item.view));
            self.known_views
                .insert(key(item.view), Arc::downgrade(item.view));
        }
        self.catalog = catalog;
        *self.io_gate.write().unwrap() = self
            .catalog
            .views
            .iter()
            .map(|entry| (entry.key.clone(), entry.object.clone()))
            .collect();
        self.published = true;
        Ok(stats)
    }

    fn write_snapshot(
        &mut self,
        snapshot: &Arc<Snapshot>,
        stats: &mut WriteStats,
    ) -> Result<String> {
        if let Some((old, hash)) = self
            .snapshots
            .get(&snapshot.snapshot_id)
            .and_then(|(old, hash)| old.upgrade().map(|old| (old, hash)))
        {
            ensure!(
                Arc::ptr_eq(&old, snapshot) || &old == snapshot,
                "index_store_error: immutable snapshot changed"
            );
            return Ok(hash.clone());
        }
        if let Some(version) = self
            .catalog
            .views
            .iter()
            .find(|entry| entry.key.0 == snapshot.snapshot_id)
            .map(|entry| entry.key.clone())
        {
            let old = self.load(&version)?;
            ensure!(
                &old.view.snapshot == snapshot,
                "index_store_error: immutable snapshot changed"
            );
            return Ok(self.snapshots[&snapshot.snapshot_id].1.clone());
        }
        let manifest = &snapshot.manifest;
        ensure!(
            manifest.workspace_id == self.catalog.workspace_id,
            "index_store_error: snapshot workspace mismatch"
        );
        for file in manifest.files.values() {
            ensure!(
                digest(&file.content) == file.file_hash,
                "index_store_error: source content hash mismatch"
            );
            if !self.sources.contains(&file.file_hash) {
                let (_, written) = self.write_object("sources", file.content.as_bytes())?;
                stats.sources += usize::from(written.is_some());
                stats.object_bytes += written.unwrap_or(0);
                self.sources.insert(file.file_hash.clone());
            }
        }
        let (shards, syntax_order) = syntax::split(&snapshot.syntax);
        let mut syntax_files = BTreeMap::new();
        for (file, facts) in shards {
            let (hash, written) = self.write_object("syntax", &serde_json::to_vec(&facts)?)?;
            stats.syntax_files += usize::from(written.is_some());
            stats.object_bytes += written.unwrap_or(0);
            syntax_files.insert(file, hash);
        }
        let object = SnapshotObject {
            snapshot_id: snapshot.snapshot_id.clone(),
            syntax_epoch: snapshot.syntax_epoch,
            input_hash: snapshot.input_hash.clone(),
            manifest: ManifestObject {
                workspace_id: manifest.workspace_id.clone(),
                files: manifest
                    .files
                    .iter()
                    .map(|(path, file)| (path.clone(), file.file_hash.clone()))
                    .collect(),
                binary_files: manifest.binary_files.clone(),
                analysis_versions: manifest.analysis_versions.clone(),
                scope: manifest.scope.clone(),
            },
            syntax: None,
            syntax_files,
            syntax_order: Some(syntax_order),
        };
        let (hash, written) = self.write_object("snapshots", &serde_json::to_vec(&object)?)?;
        stats.snapshots += usize::from(written.is_some());
        stats.object_bytes += written.unwrap_or(0);
        self.snapshots.insert(
            snapshot.snapshot_id.clone(),
            (Arc::downgrade(snapshot), hash.clone()),
        );
        Ok(hash)
    }

    fn read_publication(&mut self, entry: &ViewEntry) -> Result<Publication> {
        let io_gate = self.io_gate.clone();
        let readable = io_gate.read().unwrap();
        ensure!(
            !self.published || readable.contains_key(&entry.key),
            "snapshot_expired: publication collected"
        );
        let hash = readable.get(&entry.key).unwrap_or(&entry.object);
        let object: ViewObject = self.read_json("views", hash)?;
        ensure!(
            object.epochs.semantic_epoch == entry.key.1,
            "index_store_error: publication identity mismatch"
        );
        let snapshot = if let Some((cached, hash)) = self
            .snapshots
            .get(&entry.key.0)
            .and_then(|(cached, hash)| cached.upgrade().map(|cached| (cached, hash)))
        {
            ensure!(
                hash == &object.snapshot,
                "index_store_error: conflicting snapshot objects"
            );
            cached
        } else {
            let snapshot: SnapshotObject = self.read_json("snapshots", &object.snapshot)?;
            ensure!(
                snapshot.snapshot_id == entry.key.0,
                "index_store_error: snapshot identity mismatch"
            );
            let syntax = match (snapshot.syntax, snapshot.syntax_order) {
                (Some(facts), None) if snapshot.syntax_files.is_empty() => facts,
                (None, Some(order)) => syntax::join(
                    snapshot
                        .syntax_files
                        .values()
                        .map(|hash| self.read_json("syntax", hash))
                        .collect::<Result<Vec<SyntaxFacts>>>()?,
                    order,
                )?,
                _ => bail!("index_store_error: invalid syntax representation"),
            };
            let mut files = BTreeMap::new();
            for (path, hash) in snapshot.manifest.files {
                let bytes = self.read_object("sources", &hash)?;
                let content =
                    String::from_utf8(bytes).context("index_store_error: source is not UTF-8")?;
                self.sources.insert(hash.clone());
                files.insert(
                    path,
                    SourceFile {
                        file_hash: hash,
                        content,
                    },
                );
            }
            let snapshot = Arc::new(Snapshot {
                snapshot_id: snapshot.snapshot_id,
                syntax_epoch: snapshot.syntax_epoch,
                input_hash: snapshot.input_hash,
                manifest: Manifest {
                    workspace_id: snapshot.manifest.workspace_id,
                    files,
                    binary_files: snapshot.manifest.binary_files,
                    analysis_versions: snapshot.manifest.analysis_versions,
                    scope: snapshot.manifest.scope,
                },
                syntax,
            });
            self.snapshots.insert(
                snapshot.snapshot_id.clone(),
                (Arc::downgrade(&snapshot), object.snapshot),
            );
            snapshot
        };
        let publication = Publication {
            view: Arc::new(AnalysisView {
                snapshot,
                epochs: object.epochs,
                relations: object.relations,
                pending_units: object.pending_units,
                known_gaps: object.known_gaps,
                resolutions: object.resolutions,
            }),
            references: object.references,
            reference_gaps: object.reference_gaps,
        };
        ensure!(
            entry
                .info
                .as_ref()
                .is_none_or(|info| info == &view_info(&publication.view)),
            "index_store_error: publication metadata mismatch"
        );
        self.live_views
            .lock()
            .unwrap()
            .push(Arc::downgrade(&publication.view));
        self.known_views
            .insert(entry.key.clone(), Arc::downgrade(&publication.view));
        Ok(publication)
    }

    /// Read-only comparison over all physical objects, decoded and checksum validated.
    pub fn codec_benchmark(&self) -> Result<Vec<crate::codec::Measurement>> {
        let _guard = self.io_gate.read().unwrap();
        let mut objects = vec![];
        for kind in ["sources", "syntax", "snapshots", "views"] {
            let dir = self.root.join("objects").join(kind);
            if !dir.exists() {
                continue;
            }
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let hash = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type()?.is_file() && self.object_path(kind, &hash).is_ok() {
                    objects.push(self.read_object(kind, &hash)?);
                }
            }
        }
        crate::codec::compare(&objects)
    }

    /// Atomic per-object migration. Catalog identities and readers remain valid
    /// after every replacement, including interruption halfway through migration.
    pub fn compact_objects(&mut self) -> Result<u64> {
        self.reconcile()?;
        let io_gate = self.io_gate.clone();
        let _guard = io_gate.write().unwrap();
        let mut saved = 0;
        for kind in ["sources", "syntax", "snapshots", "views"] {
            let dir = self.root.join("objects").join(kind);
            if !dir.exists() {
                continue;
            }
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let hash = entry.file_name().to_string_lossy().into_owned();
                if self.object_path(kind, &hash).is_err() {
                    continue;
                }
                let decoded = self.read_object(kind, &hash)?;
                let encoded = crate::codec::preferred(&decoded);
                let old = entry.metadata()?.len();
                if (encoded.len() as u64) < old {
                    atomic_write(&entry.path(), &encoded)?;
                    saved += old - encoded.len() as u64;
                }
            }
        }
        Ok(saved)
    }

    fn object_path(&self, kind: &str, hash: &str) -> Result<PathBuf> {
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "index_store_error: invalid object hash"
        );
        Ok(self.root.join("objects").join(kind).join(hash))
    }

    fn read_object(&self, kind: &str, hash: &str) -> Result<Vec<u8>> {
        let bytes = fs::read(self.object_path(kind, hash)?)
            .with_context(|| format!("index_store_error: reading {kind} object {hash}"))?;
        {
            let mut metrics = self.metrics.lock().unwrap();
            metrics.object_reads += 1;
            metrics.object_read_bytes += bytes.len() as u64;
        }
        let bytes = crate::codec::decode(&bytes)?;
        ensure!(
            digest(&bytes) == hash,
            "index_store_error: corrupt {kind} object {hash}"
        );
        Ok(bytes)
    }

    fn read_json<T: DeserializeOwned>(&self, kind: &str, hash: &str) -> Result<T> {
        serde_json::from_slice(&self.read_object(kind, hash)?)
            .with_context(|| format!("index_store_error: invalid {kind} object {hash}"))
    }

    fn write_object(&self, kind: &str, bytes: &[u8]) -> Result<(String, Option<usize>)> {
        let hash = digest(bytes);
        let path = self.object_path(kind, &hash)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "index_store_error: object is not a regular file"
                );
                self.read_object(kind, &hash)?;
                Ok((hash, None))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                private_dir(&self.root)?;
                private_dir(&self.root.join("objects"))?;
                let encoded = crate::codec::preferred(bytes);
                atomic_write(&path, &encoded)?;
                Ok((hash, Some(encoded.len())))
            }
            Err(error) => Err(error).context("index_store_error: inspecting object"),
        }
    }
}

impl PublicationStore for SnapshotStore {
    fn entries(&self) -> Box<dyn DoubleEndedIterator<Item = (&ViewKey, &ViewInfo)> + '_> {
        Box::new(SnapshotStore::entries(self))
    }
    fn latest_key(&self) -> &Option<ViewKey> {
        SnapshotStore::latest_key(self)
    }
    fn high_water(&self) -> (u64, u64) {
        SnapshotStore::high_water(self)
    }
    fn load(&mut self, version: &ViewKey) -> Result<Publication> {
        SnapshotStore::load(self, version)
    }
    fn commit(
        &mut self,
        latest: Option<ViewKey>,
        updates: &[PublicationRef<'_>],
    ) -> Result<WriteStats> {
        SnapshotStore::commit(self, latest, updates)
    }
    fn pin(&mut self, version: &ViewKey, pinned: bool) -> Result<()> {
        SnapshotStore::pin(self, version, pinned)
    }
    fn pins(&self) -> &BTreeSet<ViewKey> {
        SnapshotStore::pins(self)
    }
    fn gc(&mut self, keep_last: usize) -> Result<GcStats> {
        SnapshotStore::gc(self, keep_last)
    }
    fn preview_gc(&mut self, keep_last: usize, max_disk_bytes: Option<u64>) -> Result<GcStats> {
        SnapshotStore::preview_gc(self, keep_last, max_disk_bytes)
    }
    fn gc_with_budget(&mut self, keep_last: usize, max_disk_bytes: Option<u64>) -> Result<GcStats> {
        SnapshotStore::gc_with_budget(self, keep_last, max_disk_bytes)
    }
    fn disk_usage(&self) -> Result<DiskUsage> {
        SnapshotStore::disk_usage(self)
    }
    fn metrics(&self) -> StorageMetrics {
        SnapshotStore::metrics(self)
    }
}

fn validate_recovery(recovered: &RecoveredStore, workspace_id: &str) -> Result<()> {
    let mut keys = BTreeSet::new();
    for item in &recovered.publications {
        ensure!(
            keys.insert(key(&item.view)),
            "index_store_error: duplicate publication"
        );
        let manifest = &item.view.snapshot.manifest;
        ensure!(
            manifest.workspace_id == workspace_id,
            "index_store_error: snapshot workspace mismatch"
        );
        for file in manifest.files.values() {
            ensure!(
                digest(&file.content) == file.file_hash,
                "index_store_error: source content hash mismatch"
            );
        }
    }
    let mut coordinator = RefreshCoordinator::restore(
        recovered
            .publications
            .iter()
            .map(|item| item.view.clone())
            .collect(),
    )
    .map_err(|error| anyhow::anyhow!("index_store_error: invalid publication: {error}"))?;
    if let Some((id, epoch)) = &recovered.latest {
        coordinator.select_latest(id, *epoch).map_err(|error| {
            anyhow::anyhow!("index_store_error: invalid latest publication: {error}")
        })?;
    }
    Ok(())
}

pub fn private_dir(path: &Path) -> Result<()> {
    let existed = path.exists();
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("index_store_error: private storage directory cannot be a symbolic link");
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .context("index_store_error: creating private directory")?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "index_store_error: private storage directory cannot be a symbolic link"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "index_store_error: choose a dedicated private storage directory (mode 0700): {}",
            path.display()
        );
        if !existed
            && let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::File::open(parent)?
                .sync_all()
                .context("index_store_error: syncing new directory")?;
        }
    }
    #[cfg(not(unix))]
    let _ = existed;
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("index_store_error: missing parent directory")?;
    private_dir(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)
        .context("index_store_error: writing temporary file")?;
    temp.as_file()
        .sync_all()
        .context("index_store_error: syncing file")?;
    #[cfg(test)]
    tests::write_fault(path, "before_rename")?;
    temp.persist(path)
        .context("index_store_error: publishing file")?;
    #[cfg(test)]
    tests::write_fault(path, "after_rename")?;
    #[cfg(unix)]
    fs::File::open(parent)?
        .sync_all()
        .context("index_store_error: syncing directory")?;
    #[cfg(test)]
    tests::write_fault(path, "after_dir_sync")?;
    Ok(())
}

#[cfg(test)]
mod tests;
