//! Storage contracts independent of the content-addressed backend and query indexes.
use anyhow::Result;
use semweft_core::{AnalysisView, Diagnostic, Relation};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

pub type ViewKey = (String, u64);

/// The legacy envelope is also the in-memory recovery representation.
#[derive(Debug, Serialize, Deserialize)]
pub struct Publication {
    pub view: Arc<AnalysisView>,
    pub references: Vec<Relation>,
    pub reference_gaps: Vec<Diagnostic>,
}

/// Borrow new/changed facts without cloning all retained publications.
pub struct PublicationRef<'a> {
    pub view: &'a Arc<AnalysisView>,
    pub references: &'a [Relation],
    pub reference_gaps: &'a [Diagnostic],
}

pub struct RecoveredStore {
    pub latest: Option<ViewKey>,
    pub publications: Vec<Publication>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewInfo {
    pub syntax_epoch: u64,
    pub input_hash: String,
    pub reuse_hash: String,
    pub revision: bool,
}

/// Actual writes, excluding already existing objects. No-op commits write zero bytes.
#[derive(Debug, Default)]
pub struct WriteStats {
    pub sources: usize,
    pub snapshots: usize,
    pub syntax_files: usize,
    pub views: usize,
    pub object_bytes: usize,
    pub catalog_bytes: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct GcStats {
    pub removed_views: usize,
    pub removed_objects: usize,
    pub reclaimed_bytes: u64,
    pub remaining_bytes: u64,
    pub max_disk_bytes: Option<u64>,
    pub budget_unmet: bool,
    pub protected_views: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct ObjectUsage {
    pub objects: usize,
    pub bytes: u64,
}
#[derive(Debug, Default, Serialize)]
pub struct DiskUsage {
    pub objects: std::collections::BTreeMap<String, ObjectUsage>,
    pub catalog_bytes: u64,
    pub journal_bytes: u64,
    pub other_bytes: u64,
    pub total_bytes: u64,
}

/// Cumulative counters for this process, shared with staged writer copies.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StorageMetrics {
    pub recovery_ms: f64,
    pub recovered_views: usize,
    pub historical_loads: u64,
    pub load_failures: u64,
    pub object_reads: u64,
    pub object_read_bytes: u64,
    pub commits: u64,
    pub commit_failures: u64,
    pub no_op_commits: u64,
    pub object_write_bytes: u64,
    pub catalog_write_bytes: u64,
    pub last_commit_ms: f64,
    pub total_commit_ms: f64,
    pub gc_runs: u64,
    pub gc_failures: u64,
    pub reclaimed_bytes: u64,
}

/// Backend contract for immutable publications. The host owns the exclusive
/// workspace writer lock. A successful commit is durable; readers bind a fixed
/// version pair. GC protects latest, semantic roots, pins and active readers.
pub trait PublicationStore: Send {
    fn entries(&self) -> Box<dyn DoubleEndedIterator<Item = (&ViewKey, &ViewInfo)> + '_>;
    fn latest_key(&self) -> &Option<ViewKey>;
    fn high_water(&self) -> (u64, u64);
    fn load(&mut self, version: &ViewKey) -> Result<Publication>;
    fn commit(
        &mut self,
        latest: Option<ViewKey>,
        updates: &[PublicationRef<'_>],
    ) -> Result<WriteStats>;
    fn pin(&mut self, version: &ViewKey, pinned: bool) -> Result<()>;
    fn pins(&self) -> &BTreeSet<ViewKey>;
    fn gc(&mut self, keep_last: usize) -> Result<GcStats>;
    fn preview_gc(&mut self, keep_last: usize, max_disk_bytes: Option<u64>) -> Result<GcStats>;
    fn gc_with_budget(&mut self, keep_last: usize, max_disk_bytes: Option<u64>) -> Result<GcStats>;
    fn disk_usage(&self) -> Result<DiskUsage>;
    fn metrics(&self) -> StorageMetrics;
}
