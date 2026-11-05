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

#[cfg(test)]
mod tests {
    use super::*;
    use semweft_core::{AnalysisView, Epochs, Manifest, Snapshot, SourceFile, SyntaxFacts, digest};
    use std::collections::BTreeMap;

    fn view() -> Arc<AnalysisView> {
        let manifest = Manifest {
            workspace_id: "ws_storage".into(),
            files: BTreeMap::from([("src/lib.rs".into(), SourceFile::new("pub fn entry() {}\n"))]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        };
        let snapshot = Snapshot {
            snapshot_id: "snap_1".into(),
            syntax_epoch: 3,
            input_hash: manifest.input_hash(),
            manifest,
            syntax: SyntaxFacts::default(),
        };
        Arc::new(AnalysisView {
            snapshot: Arc::new(snapshot),
            epochs: Epochs {
                syntax_epoch: 3,
                semantic_epoch: 2,
            },
            relations: vec![],
            pending_units: vec![],
            known_gaps: vec![],
            resolutions: vec![],
        })
    }

    #[test]
    fn view_info_round_trips_and_compares_structurally() {
        let info = ViewInfo {
            syntax_epoch: 7,
            input_hash: digest("input"),
            reuse_hash: digest("reuse"),
            revision: true,
        };
        let encoded = serde_json::to_value(&info).unwrap();
        assert_eq!(encoded["syntax_epoch"], 7);
        assert_eq!(encoded["revision"], true);
        assert!(
            encoded["input_hash"]
                .as_str()
                .unwrap()
                .starts_with(|c: char| c.is_ascii_hexdigit())
        );
        let decoded: ViewInfo = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, info);
        let different = ViewInfo {
            revision: false,
            ..info.clone()
        };
        assert_ne!(decoded, different);
    }

    #[test]
    fn publication_round_trips_with_its_view_and_gaps() {
        let publication = Publication {
            view: view(),
            references: vec![],
            reference_gaps: vec![Diagnostic {
                code: "reference_gap".into(),
                file: Some("src/lib.rs".into()),
                message: "unresolved".into(),
            }],
        };
        let encoded = serde_json::to_value(&publication).unwrap();
        assert_eq!(encoded["view"]["snapshot"]["snapshot_id"], "snap_1");
        assert_eq!(encoded["view"]["epochs"]["semantic_epoch"], 2);
        let decoded: Publication = serde_json::from_value(encoded).unwrap();
        assert_eq!(*decoded.view, *publication.view);
        assert!(decoded.references.is_empty());
        assert_eq!(decoded.reference_gaps, publication.reference_gaps);

        let borrowed = PublicationRef {
            view: &publication.view,
            references: &publication.references,
            reference_gaps: &publication.reference_gaps,
        };
        assert_eq!(borrowed.view.snapshot.snapshot_id, "snap_1");
        assert_eq!(borrowed.reference_gaps[0].code, "reference_gap");
    }

    #[test]
    fn write_and_gc_statistics_start_empty_and_keep_their_json_contract() {
        let writes = WriteStats::default();
        assert_eq!(
            (
                writes.sources,
                writes.snapshots,
                writes.syntax_files,
                writes.views,
                writes.object_bytes,
                writes.catalog_bytes,
            ),
            (0, 0, 0, 0, 0, 0),
            "no-op commits write zero bytes"
        );

        let gc = GcStats::default();
        assert_eq!((gc.removed_views, gc.removed_objects), (0, 0));
        assert!(!gc.budget_unmet);
        assert_eq!(gc.max_disk_bytes, None);
        let encoded = serde_json::to_value(&gc).unwrap();
        for field in [
            "removed_views",
            "removed_objects",
            "reclaimed_bytes",
            "remaining_bytes",
            "max_disk_bytes",
            "budget_unmet",
            "protected_views",
        ] {
            assert!(encoded.get(field).is_some(), "missing {field}");
        }

        let metrics = StorageMetrics::default();
        let encoded = serde_json::to_value(&metrics).unwrap();
        assert_eq!(encoded["commits"], 0);
        assert_eq!(encoded["recovery_ms"], 0.0);
        assert_eq!(encoded["historical_loads"], 0);
    }

    #[test]
    fn disk_usage_reports_objects_catalog_and_totals_independently() {
        let usage = DiskUsage {
            objects: BTreeMap::from([
                (
                    "sources".into(),
                    ObjectUsage {
                        objects: 2,
                        bytes: 100,
                    },
                ),
                (
                    "views".into(),
                    ObjectUsage {
                        objects: 1,
                        bytes: 40,
                    },
                ),
            ]),
            catalog_bytes: 10,
            journal_bytes: 5,
            other_bytes: 1,
            total_bytes: 156,
        };
        let encoded = serde_json::to_value(&usage).unwrap();
        assert_eq!(encoded["objects"]["sources"]["objects"], 2);
        assert_eq!(encoded["objects"]["sources"]["bytes"], 100);
        assert_eq!(encoded["total_bytes"], 156);
        assert_eq!(
            encoded["catalog_bytes"].as_u64().unwrap()
                + encoded["journal_bytes"].as_u64().unwrap()
                + encoded["other_bytes"].as_u64().unwrap(),
            16
        );
    }

    #[test]
    fn view_keys_order_by_name_then_epoch_and_pins_are_sets() {
        let mut keys = vec![
            ("z".to_string(), 1u64),
            ("a".to_string(), 9),
            ("a".to_string(), 2),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                ("a".to_string(), 2),
                ("a".to_string(), 9),
                ("z".to_string(), 1)
            ]
        );
        let pins = BTreeSet::from([("a".to_string(), 2u64)]);
        assert!(pins.contains(&("a".to_string(), 2)));
        assert!(!pins.contains(&("a".to_string(), 9)));
    }
}
