//! Isolated storage/refresh measurements. Capture, language analysis and process
//! spawning are injected by the host; this library never invokes repository tools.
use anyhow::{Context, Result, ensure};
use semweft_core::{AnalysisView, Manifest, RefreshCoordinator, Relation, SourceFile, SyntaxFacts};
use semweft_store::{Direction, IndexedView, PublicationRef, SnapshotStore};
use serde_json::{Value, json};
use std::{
    hint::black_box,
    path::{Path, PathBuf},
    time::Instant,
};

mod suite;
pub use suite::{corpus_id, load_input, run_suite, save_input};

pub fn peak_rss() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // getrusage initializes the output on success.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        let usage = unsafe { usage.assume_init() };
        let bytes = u64::try_from(usage.ru_maxrss).ok()?;
        Some(bytes * if cfg!(target_os = "macos") { 1 } else { 1024 })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

pub fn recover(index: &Path, workspace_id: &str) -> Result<Value> {
    let start = Instant::now();
    let (store, recovered) = SnapshotStore::open_lazy(index, workspace_id)?;
    let metadata: &dyn semweft_store::PublicationStore = &store;
    let recovered = recovered.context("index_store_error: benchmark index missing")?;
    Ok(
        json!({"restore_ms":start.elapsed().as_secs_f64()*1000., "peak_rss_bytes":peak_rss(), "loaded_views":recovered.publications.len(),"retained_views":metadata.entries().count()}),
    )
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct StorageInput {
    pub workspace: PathBuf,
    pub capture_ms: f64,
    pub manifest: Manifest,
    pub edited_file: String,
    pub revisions: u32,
}

pub fn run_storage(
    input: StorageInput,
    mut analyze: impl FnMut(&Manifest) -> Result<SyntaxFacts>,
    mut relations: impl FnMut(&AnalysisView) -> Vec<Relation>,
    recover_in_child: impl Fn(&Path, &str) -> Result<Value>,
) -> Result<Value> {
    ensure!(
        input.revisions > 0 && input.revisions <= 1000,
        "invalid_argument: revisions must be 1..=1000"
    );
    let corpus = corpus_id(&input)?;
    let StorageInput {
        workspace,
        capture_ms,
        mut manifest,
        edited_file: path,
        revisions,
    } = input;
    ensure!(
        manifest.files.contains_key(&path),
        "invalid_argument: edited file is not captured"
    );
    manifest
        .analysis_versions
        .insert("trust".into(), "false".into());
    let original = manifest.files[&path].content.clone();
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("index");
    let (mut store, _) = SnapshotStore::open(&index, &manifest.workspace_id)?;
    let mut coordinator = RefreshCoordinator::default();
    let mut samples = vec![];
    for revision in 1..=revisions {
        manifest.files.insert(
            path.clone(),
            SourceFile::new(format!(
                "{original}\n// semweft benchmark revision {revision}\n"
            )),
        );
        let start = Instant::now();
        let syntax = analyze(&manifest)?;
        let view = coordinator
            .publish_inputs(manifest.clone(), syntax)
            .map_err(anyhow::Error::msg)?;
        let parse_publish_ms = start.elapsed().as_secs_f64() * 1000.;
        let start = Instant::now();
        let stats = store.commit(
            Some((
                view.snapshot.snapshot_id.clone(),
                view.epochs.semantic_epoch,
            )),
            &[PublicationRef {
                view: &view,
                references: &[],
                reference_gaps: &[],
            }],
        )?;
        let persist_ms = start.elapsed().as_secs_f64() * 1000.;
        coordinator.retain_refresh_roots();
        if revision == 1 || revision == revisions || revision.is_multiple_of(10) {
            let cold = Instant::now();
            let query = IndexedView::new(view.clone(), relations(&view));
            let target = query
                .relations()
                .first()
                .map(|r| r.to_symbol.as_str())
                .or_else(|| {
                    view.snapshot
                        .syntax
                        .symbols
                        .first()
                        .map(|s| s.symbol_id.as_str())
                })
                .unwrap_or("");
            let count = query.adjacent(target, Direction::Incoming).count();
            let cold_query_ms = cold.elapsed().as_secs_f64() * 1000.;
            let hot = Instant::now();
            for _ in 0..1000 {
                assert_eq!(
                    black_box(query.adjacent(target, Direction::Incoming).count()),
                    count
                );
            }
            let hot_1000_ms = hot.elapsed().as_secs_f64() * 1000.;
            let recovery = recover_in_child(&index, &manifest.workspace_id)?;
            samples.push(json!({"retained_views":revision,"parse_publish_ms":parse_publish_ms,"persist_ms":persist_ms,"objects_written":stats.sources+stats.syntax_files+stats.snapshots+stats.views,"object_bytes_written":stats.object_bytes,"full_snapshot_payload_bytes":semweft_store::payload_bytes(&view.snapshot),"catalog_bytes_written":stats.catalog_bytes,"disk_bytes":suite::disk_bytes(&index)?,"cold_index_and_query_ms":cold_query_ms,"hot_1000_queries_ms":hot_1000_ms,"query_checksum":count,"publisher_peak_rss_bytes":peak_rss(),"recovery":recovery}));
        }
    }
    Ok(
        json!({"schema_version":"0.1","benchmark":"storage","corpus_id":corpus,"workspace":workspace,"files":manifest.files.len(),"source_bytes":manifest.files.values().map(|f|f.content.len()).sum::<usize>(),"edited_file":path,"capture_ms":capture_ms,"revisions":revisions,"scope":"L1 real inputs; in-memory comment edits; temporary index; warm OS cache recovery in fresh child; no builds or L3; RSS is process peak where supported", "codecs":store.codec_benchmark()?, "samples":samples}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use semweft_core::Manifest;
    use std::collections::BTreeMap;

    fn manifest() -> Manifest {
        Manifest {
            workspace_id: "ws_bench".into(),
            files: BTreeMap::from([("src/lib.rs".into(), SourceFile::new("pub fn entry() {}\n"))]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        }
    }

    fn input(revisions: u32) -> StorageInput {
        StorageInput {
            workspace: PathBuf::from("/tmp/semweft-bench-workspace"),
            capture_ms: 2.5,
            manifest: manifest(),
            edited_file: "src/lib.rs".into(),
            revisions,
        }
    }

    #[test]
    fn run_storage_samples_the_first_last_and_decade_revisions() {
        let mut analyzed = vec![];
        let recovered = std::cell::Cell::new(0);
        let result = run_storage(
            input(12),
            |manifest| {
                analyzed.push(manifest.clone());
                Ok(SyntaxFacts::default())
            },
            |view| {
                assert!(view.relations.is_empty());
                vec![]
            },
            |index, workspace| {
                recovered.set(recovered.get() + 1);
                assert!(index.exists(), "the benchmark index is persisted");
                assert_eq!(workspace, "ws_bench");
                Ok(json!({"restore_ms": 0.25, "retained_views": 1}))
            },
        )
        .unwrap();
        assert_eq!(analyzed.len(), 12);
        assert_eq!(analyzed[0].analysis_versions["trust"], "false");
        assert_eq!(
            analyzed[11].files["src/lib.rs"].content,
            "pub fn entry() {}\n\n// semweft benchmark revision 12\n"
        );
        assert_eq!(recovered.get(), 3);
        assert_eq!(result["schema_version"], "0.1");
        assert_eq!(result["benchmark"], "storage");
        assert_eq!(result["corpus_id"], corpus_id(&input(12)).unwrap());
        assert_eq!(result["revisions"], 12);
        assert_eq!(result["files"], 1);
        assert!(result["source_bytes"].as_u64().unwrap() > 0);
        let samples = result["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 3);
        for (sample, revision) in samples.iter().zip([1, 10, 12]) {
            assert_eq!(sample["retained_views"], revision);
            assert_eq!(sample["query_checksum"], 0);
            assert_eq!(sample["recovery"]["restore_ms"], 0.25);
            assert!(sample["parse_publish_ms"].as_f64().is_some());
            assert!(sample["persist_ms"].as_f64().is_some());
            assert!(sample["disk_bytes"].as_u64().is_some());
        }
        assert!(result["codecs"].is_array());
    }

    #[test]
    fn run_storage_rejects_invalid_revision_counts_and_unknown_files() {
        let error = run_storage(
            input(0),
            |_| Ok(SyntaxFacts::default()),
            |_| vec![],
            |_, _| unreachable!("nothing is stored"),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("revisions must be 1..=1000"),
            "{error:#}"
        );

        let error = run_storage(
            input(1001),
            |_| Ok(SyntaxFacts::default()),
            |_| vec![],
            |_, _| unreachable!("nothing is stored"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("revisions must be 1..=1000"));

        let mut missing = input(2);
        missing.edited_file = "src/missing.rs".into();
        let error = run_storage(
            missing,
            |_| Ok(SyntaxFacts::default()),
            |_| vec![],
            |_, _| unreachable!("nothing is stored"),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("edited file is not captured"),
            "{error:#}"
        );
    }

    #[test]
    fn recover_rejects_a_missing_index() {
        let temp = tempfile::tempdir().unwrap();
        let error = recover(&temp.path().join("absent"), "ws_bench").unwrap_err();
        assert!(
            format!("{error:#}").contains("benchmark index missing"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn peak_rss_reports_a_positive_process_peak() {
        assert!(peak_rss().is_some_and(|bytes| bytes > 0));
    }

    #[cfg(not(unix))]
    #[test]
    fn peak_rss_is_unavailable_off_unix() {
        assert_eq!(peak_rss(), None);
    }
}
