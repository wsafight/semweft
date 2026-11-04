use super::*;
use semweft_core::{Certainty, Position, Span, identity};
use serde_json::{Value, json};

const WORKSPACE: &str = "persistence-test";

fn private_tempdir() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    temp
}

#[cfg(unix)]
#[test]
fn private_directory_creation_is_safe_for_concurrent_callers() {
    use std::sync::Barrier;
    let temp = private_tempdir();
    let barrier = Barrier::new(16);
    std::thread::scope(|scope| {
        let workers = (0..16)
            .map(|_| {
                let barrier = &barrier;
                let root = temp.path();
                scope.spawn(move || {
                    let mut failures = Vec::new();
                    for iteration in 0..64 {
                        barrier.wait();
                        let directory = root.join(format!("round-{iteration}"));
                        if let Err(error) = private_dir(&directory) {
                            failures.push(error.to_string());
                        }
                        barrier.wait();
                    }
                    failures
                })
            })
            .collect::<Vec<_>>();
        let failures = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(failures.is_empty(), "{failures:?}");
    });
}

#[cfg(unix)]
#[test]
fn private_directory_creates_private_ancestors() {
    use std::os::unix::fs::PermissionsExt;
    let temp = private_tempdir();
    let directory = temp.path().join("cache/workspace/objects");
    private_dir(&directory).unwrap();
    for path in directory.ancestors().take(3) {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    private_dir(&directory).unwrap();
}

#[cfg(unix)]
#[test]
fn private_directory_rejects_existing_public_directories_without_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let temp = private_tempdir();
    for mode in [0o755, 0o750, 0o707, 0o770, 0o777] {
        let directory = temp.path().join(format!("mode-{mode:o}"));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(mode)).unwrap();
        let error = private_dir(&directory).unwrap_err();
        assert!(error.to_string().contains("dedicated private storage"));
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
}

#[cfg(unix)]
#[test]
fn private_directory_rejects_live_and_dangling_symlinks() {
    use std::os::unix::fs::symlink;
    let temp = private_tempdir();
    let target = temp.path().join("target");
    private_dir(&target).unwrap();
    for (name, destination) in [("live", target), ("dangling", temp.path().join("missing"))] {
        let link = temp.path().join(name);
        symlink(&destination, &link).unwrap();
        assert!(
            private_dir(&link)
                .unwrap_err()
                .to_string()
                .contains("symbolic link")
        );
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    assert!(!temp.path().join("missing").exists());
}

#[test]
fn private_directory_rejects_existing_files() {
    let temp = private_tempdir();
    let path = temp.path().join("file");
    fs::write(&path, b"preserved").unwrap();
    assert!(private_dir(&path).is_err());
    assert_eq!(fs::read(path).unwrap(), b"preserved");
}

#[test]
fn atomic_writes_share_a_new_private_directory() {
    let temp = private_tempdir();
    let directory = temp.path().join("shared");
    let barrier = std::sync::Barrier::new(16);
    std::thread::scope(|scope| {
        let workers = (0..16)
            .map(|worker| {
                let directory = &directory;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    let path = directory.join(format!("{worker}.json"));
                    atomic_write(&path, format!("{{\"worker\":{worker}}}").as_bytes())?;
                    Ok::<_, anyhow::Error>(fs::read(path)?)
                })
            })
            .collect::<Vec<_>>();
        for (worker, result) in workers.into_iter().enumerate() {
            assert_eq!(
                result.join().unwrap().unwrap(),
                format!("{{\"worker\":{worker}}}").as_bytes()
            );
        }
    });
}

fn publication(epoch: u64, text: &str) -> Publication {
    let common = SourceFile::new("// unchanged source\n".repeat(2048));
    let manifest = Manifest {
        workspace_id: WORKSPACE.into(),
        files: BTreeMap::from([
            ("a.rs".into(), common.clone()),
            ("copy.rs".into(), common),
            ("edit.rs".into(), SourceFile::new(text)),
        ]),
        binary_files: BTreeMap::from([("binary.dat".into(), "binary-hash".into())]),
        analysis_versions: BTreeMap::from([("semantic".into(), "test@1".into())]),
        scope: "fixture".into(),
    };
    Publication {
        view: Arc::new(AnalysisView {
            snapshot: Arc::new(Snapshot {
                snapshot_id: identity("snap", &[&epoch.to_string(), &manifest.input_hash()]),
                syntax_epoch: epoch,
                input_hash: manifest.input_hash(),
                manifest,
                syntax: SyntaxFacts::default(),
            }),
            epochs: Epochs {
                syntax_epoch: epoch,
                semantic_epoch: 0,
            },
            relations: vec![],
            pending_units: vec!["edit.rs".into()],
            known_gaps: vec![gap("pending")],
            resolutions: vec![],
        }),
        references: vec![Relation {
            relation_id: "reference".into(),
            kind: "read".into(),
            from_symbol: "external".into(),
            to_symbol: "target".into(),
            file: "edit.rs".into(),
            span: Span {
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            },
            certainty: Certainty::Candidate,
            freshness: "pending".into(),
            runtime: "unknown".into(),
            evidence: vec![],
            origin: None,
        }],
        reference_gaps: vec![gap("reference-gap")],
    }
}

fn gap(code: &str) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        file: Some("edit.rs".into()),
        message: "中文😀".into(),
    }
}

fn borrowed(item: &Publication) -> PublicationRef<'_> {
    PublicationRef {
        view: &item.view,
        references: &item.references,
        reference_gaps: &item.reference_gaps,
    }
}

fn save(store: &mut SnapshotStore, item: &Publication) -> WriteStats {
    store
        .commit(Some(key(&item.view)), &[borrowed(item)])
        .unwrap()
}

fn recovered(root: &Path) -> RecoveredStore {
    SnapshotStore::open(root, WORKSPACE).unwrap().1.unwrap()
}

fn assert_publication(actual: &Publication, expected: &Publication) {
    assert_eq!(actual.view, expected.view);
    assert_eq!(actual.references, expected.references);
    assert_eq!(actual.reference_gaps, expected.reference_gaps);
}

fn catalog(root: &Path) -> Value {
    let (store, _) = SnapshotStore::open_lazy(root, WORKSPACE).unwrap();
    serde_json::to_value(&store.catalog).unwrap()
}

fn legacy_bytes(items: &[Publication], latest: Option<ViewKey>) -> Vec<u8> {
    serde_json::to_vec(
        &json!({"schema_version":"0.1", "workspace_id":WORKSPACE, "latest":latest, "views":items}),
    )
    .unwrap()
}

#[test]
fn new_store_does_not_create_files_until_publication() {
    let temp = private_tempdir();
    let root = temp.path().join("index");
    let (_, restored) = SnapshotStore::open(&root, WORKSPACE).unwrap();
    assert!(restored.is_none());
    assert!(!root.exists());
}

#[test]
fn semantic_reuse_requires_the_same_parser_and_go_build_environment_after_restart() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut item = publication(1, "fn entry() {}\n");
    let view = Arc::make_mut(&mut item.view);
    let snapshot = Arc::make_mut(&mut view.snapshot);
    snapshot.manifest.analysis_versions.extend(BTreeMap::from([
        ("syntax".into(), "parser@1".into()),
        (
            "go_build_environment".into(),
            "{\"GOFLAGS\":\"-tags=one\"}".into(),
        ),
    ]));
    snapshot.input_hash = snapshot.manifest.input_hash();
    view.epochs.semantic_epoch = view.epochs.syntax_epoch;
    view.pending_units.clear();
    save(&mut store, &item);
    drop(store);
    let (store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let manifest = &item.view.snapshot.manifest;
    assert_eq!(store.reusable(manifest), Some(key(&item.view)));
    for (name, changed) in [
        ("syntax", "parser@2"),
        ("go_build_environment", "{\"GOFLAGS\":\"-tags=two\"}"),
    ] {
        let mut different = manifest.clone();
        different
            .analysis_versions
            .insert(name.into(), changed.into());
        assert!(store.reusable(&different).is_none(), "{name}");
        different.analysis_versions.remove(name);
        assert!(store.reusable(&different).is_none(), "missing {name}");
    }
}

#[test]
fn lazy_restore_only_loads_roots_and_preserves_historical_epoch_allocation() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let items = (1..=5)
        .map(|epoch| publication(epoch, &format!("v{epoch}")))
        .collect::<Vec<_>>();
    for item in &items {
        save(&mut store, item);
    }
    // A revision can be newer than the selected latest working-tree view.
    store.commit(Some(key(&items[0].view)), &[]).unwrap();
    drop(store);
    let (mut store, roots) = SnapshotStore::open_lazy(temp.path(), WORKSPACE).unwrap();
    let roots = roots.unwrap();
    assert_eq!(roots.publications.len(), 1);
    assert_publication(&roots.publications[0], &items[0]);
    assert_eq!(store.entries().count(), 5);
    let (syntax, semantic) = store.high_water();
    let mut coordinator = RefreshCoordinator::restore_roots(
        roots.publications.into_iter().map(|p| p.view).collect(),
        syntax,
        semantic,
    )
    .unwrap();
    let manifest = items[4].view.snapshot.manifest.clone();
    let next = coordinator
        .publish_inputs(manifest, SyntaxFacts::default())
        .unwrap();
    assert_eq!(next.epochs.syntax_epoch, 6);
    assert_publication(&store.load(&key(&items[3].view)).unwrap(), &items[3]);
    coordinator.retain_refresh_roots();
    assert_eq!(coordinator.retained().len(), 1);
}

#[test]
fn historical_payloads_are_validated_when_first_loaded() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let old = publication(1, "old");
    save(&mut store, &old);
    let old_hash = store.catalog.views[0].object.clone();
    save(&mut store, &publication(2, "new"));
    fs::write(temp.path().join("objects/views").join(old_hash), "corrupt").unwrap();
    let (mut store, restored) = SnapshotStore::open_lazy(temp.path(), WORKSPACE).unwrap();
    assert_eq!(restored.unwrap().publications.len(), 1);
    assert!(store.load(&key(&old.view)).is_err());
}

#[test]
fn incremental_commits_share_sources_and_snapshots_without_rewriting_history() {
    let temp = private_tempdir();
    let root = temp.path();
    let (mut store, _) = SnapshotStore::open(root, WORKSPACE).unwrap();
    let first = publication(1, "fn first() {}\n");
    let stats = save(&mut store, &first);
    assert_eq!((stats.sources, stats.snapshots, stats.views), (2, 1, 1));
    assert!(store.is_persisted(&first.view));

    let first_objects = ["sources", "snapshots", "views"]
        .into_iter()
        .flat_map(|kind| {
            fs::read_dir(root.join("objects").join(kind))
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    let bytes = fs::read(&path).unwrap();
                    let modified = fs::metadata(&path).unwrap().modified().unwrap();
                    (path, bytes, modified)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let semantic = Publication {
        view: Arc::new(AnalysisView {
            epochs: Epochs {
                syntax_epoch: 1,
                semantic_epoch: 1,
            },
            pending_units: vec![],
            relations: first.references.clone(),
            resolutions: vec![CallResolution {
                callsite_id: "call".into(),
                targets: vec![],
                diagnostics: vec![gap("unresolved")],
                cfg_inactive: true,
            }],
            ..(*first.view).clone()
        }),
        references: first.references.clone(),
        reference_gaps: vec![],
    };
    let stats = save(&mut store, &semantic);
    assert_eq!((stats.sources, stats.snapshots, stats.views), (0, 0, 1));
    let second = publication(2, "fn second() {}\n");
    let stats = save(&mut store, &second);
    assert_eq!((stats.sources, stats.snapshots, stats.views), (1, 1, 1));
    for (path, bytes, modified) in first_objects {
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }
    let no_op = store.commit(Some(key(&second.view)), &[]).unwrap();
    assert_eq!(no_op.object_bytes + no_op.catalog_bytes, 0);
    let same = save(&mut store, &second);
    assert_eq!(same.object_bytes + same.catalog_bytes, 0);

    let (store, restored) = SnapshotStore::open(root, WORKSPACE).unwrap();
    let restored = restored.unwrap();
    assert_eq!(restored.latest, Some(key(&second.view)));
    for (actual, expected) in restored
        .publications
        .iter()
        .zip([&first, &semantic, &second])
    {
        assert_publication(actual, expected);
        assert!(store.is_persisted(&actual.view));
    }
    assert_eq!(restored.publications.len(), 3);
    assert!(Arc::ptr_eq(
        &restored.publications[0].view.snapshot,
        &restored.publications[1].view.snapshot
    ));
}

#[test]
fn supplemental_evidence_replaces_only_its_view_object() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut first = publication(1, "old");
    save(&mut store, &first);
    let old_pointer = catalog(temp.path())["views"][0]["object"].clone();
    first.references[0].freshness = "current".into();
    first.reference_gaps.push(gap("supplemental"));
    let stats = save(&mut store, &first);
    assert_eq!((stats.sources, stats.snapshots, stats.views), (0, 0, 1));
    assert_ne!(catalog(temp.path())["views"][0]["object"], old_pointer);
    let restored = recovered(temp.path());
    assert_eq!(restored.publications.len(), 1);
    assert_publication(&restored.publications[0], &first);
}

#[test]
fn failed_catalog_commit_leaves_previous_publication_and_reuses_orphans_on_retry() {
    for initial in [false, true] {
        let temp = private_tempdir();
        let root = temp.path();
        let (mut store, _) = SnapshotStore::open(root, WORKSPACE).unwrap();
        let first = publication(1, "old");
        if initial {
            save(&mut store, &first);
        }
        let before = fs::read(root.join("views.json")).ok();
        let next = publication(2, "new");
        let result = store.commit_with(Some(key(&next.view)), &[borrowed(&next)], |_, _| {
            bail!("index_store_error: injected catalog replacement failure")
        });
        assert!(result.is_err());
        assert!(!store.is_persisted(&next.view));
        assert_eq!(fs::read(root.join("views.json")).ok(), before);
        let (_, restored) = SnapshotStore::open(root, WORKSPACE).unwrap();
        if initial {
            let restored = restored.unwrap();
            assert_eq!(restored.publications.len(), 1);
            assert_publication(&restored.publications[0], &first);
        } else {
            assert!(restored.is_none());
        }
        let stats = save(&mut store, &next);
        assert_eq!(
            (
                stats.sources,
                stats.snapshots,
                stats.views,
                stats.object_bytes
            ),
            (0, 0, 0, 0)
        );
        assert!(stats.catalog_bytes > 0);
        assert_eq!(recovered(root).latest, Some(key(&next.view)));
    }
}

#[test]
fn source_write_failure_never_publishes_an_incomplete_view() {
    let temp = private_tempdir();
    let root = temp.path();
    let (mut store, _) = SnapshotStore::open(root, WORKSPACE).unwrap();
    let first = publication(1, "old");
    save(&mut store, &first);
    let next = publication(2, "new");
    let blocked = root.join("objects/sources").join(digest("new"));
    fs::create_dir(&blocked).unwrap();
    assert!(
        store
            .commit(Some(key(&next.view)), &[borrowed(&next)])
            .is_err()
    );
    assert_eq!(recovered(root).latest, Some(key(&first.view)));
    fs::remove_dir(blocked).unwrap();
    save(&mut store, &next);
    assert_eq!(recovered(root).latest, Some(key(&next.view)));
}

#[test]
fn legacy_migration_preserves_all_facts_and_a_non_last_latest_pointer() {
    let temp = private_tempdir();
    let items = [publication(1, "old"), publication(2, "new")];
    let latest = Some(key(&items[0].view));
    atomic_write(
        &temp.path().join("views.json"),
        &legacy_bytes(&items, latest.clone()),
    )
    .unwrap();
    let restored = recovered(temp.path());
    assert_eq!(restored.latest, latest);
    assert_eq!(catalog(temp.path())["schema_version"], "0.2");
    assert_eq!(restored.publications.len(), items.len());
    for (actual, expected) in restored.publications.iter().zip(&items) {
        assert_publication(actual, expected);
    }
    let restored = recovered(temp.path());
    assert_eq!(restored.latest, latest);
    assert_publication(&restored.publications[1], &items[1]);
}

#[test]
fn interrupted_legacy_migration_keeps_original_and_can_retry() {
    let temp = private_tempdir();
    let root = temp.path();
    let bytes = legacy_bytes(&[publication(1, "old")], None);
    atomic_write(&root.join("views.json"), &bytes).unwrap();
    private_dir(&root.join("objects")).unwrap();
    fs::write(root.join("objects/snapshots"), "block creation").unwrap();
    assert!(SnapshotStore::open(root, WORKSPACE).is_err());
    assert_eq!(fs::read(root.join("views.json")).unwrap(), bytes);
    fs::remove_file(root.join("objects/snapshots")).unwrap();
    assert_eq!(recovered(root).publications.len(), 1);
    assert_eq!(catalog(root)["schema_version"], "0.2");
}

#[test]
fn corrupt_or_missing_referenced_objects_fail_closed_at_each_level() {
    for kind in ["sources", "snapshots", "views"] {
        for missing in [false, true] {
            let temp = private_tempdir();
            let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
            save(&mut store, &publication(1, "old"));
            let object = fs::read_dir(temp.path().join("objects").join(kind))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            if missing {
                fs::remove_file(object).unwrap();
            } else {
                fs::write(object, "tampered").unwrap();
            }
            let error = SnapshotStore::open(temp.path(), WORKSPACE).err().unwrap();
            assert!(
                format!("{error:#}").contains("index_store_error"),
                "{kind}: {error:#}"
            );
        }
    }
}

#[test]
fn invalid_catalogs_and_legacy_inputs_do_not_get_silently_rebuilt() {
    for case in ["workspace", "version", "latest", "duplicate", "path", "key"] {
        let temp = private_tempdir();
        let root = temp.path();
        let (mut store, _) = SnapshotStore::open(root, WORKSPACE).unwrap();
        save(&mut store, &publication(1, "old"));
        let mut value = catalog(root);
        match case {
            "workspace" => value["workspace_id"] = json!("another-workspace"),
            "version" => value["schema_version"] = json!("unknown"),
            "latest" => value["latest"] = json!(["missing", 0]),
            "duplicate" => {
                let entry = value["views"][0].clone();
                value["views"].as_array_mut().unwrap().push(entry);
            }
            "path" => value["views"][0]["object"] = json!("../../outside"),
            "key" => value["views"][0]["key"][0] = json!("wrong-id"),
            _ => unreachable!(),
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        atomic_write(&root.join("views.json"), &bytes).unwrap();
        assert!(SnapshotStore::open(root, WORKSPACE).is_err(), "{case}");
        assert_eq!(fs::read(root.join("views.json")).unwrap(), bytes);
    }
    let temp = private_tempdir();
    let mut bad = publication(1, "old");
    Arc::make_mut(&mut Arc::make_mut(&mut bad.view).snapshot)
        .manifest
        .files
        .get_mut("edit.rs")
        .unwrap()
        .file_hash = digest("different");
    let bytes = legacy_bytes(&[bad], None);
    atomic_write(&temp.path().join("views.json"), &bytes).unwrap();
    assert!(SnapshotStore::open(temp.path(), WORKSPACE).is_err());
    assert_eq!(fs::read(temp.path().join("views.json")).unwrap(), bytes);
}

#[test]
fn immutable_facts_cannot_be_replaced_under_an_existing_version() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut item = publication(1, "old");
    save(&mut store, &item);
    Arc::make_mut(&mut item.view).known_gaps.push(gap("forged"));
    assert!(
        store
            .commit(Some(key(&item.view)), &[borrowed(&item)])
            .is_err()
    );
    assert_eq!(
        recovered(temp.path()).publications[0].view.known_gaps.len(),
        1
    );
}

#[test]
fn empty_source_is_counted_once_and_round_trips() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let item = publication(1, "");
    assert_eq!(save(&mut store, &item).sources, 2);
    assert_publication(&recovered(temp.path()).publications[0], &item);
}

#[test]
fn write_volume_grows_with_new_facts_instead_of_repeated_historical_sources() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut items = vec![];
    let mut legacy_total = 0;
    let mut actual_total = 0;
    for epoch in 1..=20 {
        items.push(publication(epoch, &format!("// edit {epoch}\n")));
        let item = items.last().unwrap();
        let stats = save(&mut store, item);
        if epoch > 1 {
            assert_eq!((stats.sources, stats.snapshots, stats.views), (1, 1, 1));
            assert!(stats.object_bytes < 4096, "{stats:?}");
        }
        actual_total += stats.object_bytes + stats.catalog_bytes;
        legacy_total += legacy_bytes(&items, Some(key(&item.view))).len();
    }
    assert!(
        actual_total * 20 < legacy_total,
        "actual {actual_total}, legacy {legacy_total}"
    );
    assert_eq!(recovered(temp.path()).publications.len(), 20);
    println!(
        "20 synthetic revisions: monolithic rewrites {legacy_total} bytes; deduplicated objects + catalog writes {actual_total} bytes"
    );
}

#[test]
fn gc_protects_pins_live_readers_roots_and_epoch_high_water() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut keys = vec![];
    for epoch in 1..=6 {
        let item = publication(epoch, &format!("v{epoch}"));
        keys.push(key(&item.view));
        save(&mut store, &item);
    }
    store.pin(&keys[0], true).unwrap();
    let held = store.load(&keys[1]).unwrap();
    store.commit(Some(keys[2].clone()), &[]).unwrap();
    let stats = store.gc(0).unwrap();
    assert_eq!(stats.removed_views, 3);
    assert!(stats.reclaimed_bytes > 0);
    assert!(store.load(&keys[0]).is_ok());
    assert!(store.load(&keys[1]).is_ok());
    assert!(store.load(&keys[5]).is_err());
    assert_eq!(store.high_water(), (6, 0));
    drop(held);
    drop(store);
    let (mut store, roots) = SnapshotStore::open_lazy(temp.path(), WORKSPACE).unwrap();
    assert_eq!(store.high_water(), (6, 0));
    assert!(store.pins().contains(&keys[0]));
    store.pin(&keys[0], false).unwrap();
    store.gc(0).unwrap();
    assert!(store.load(&keys[0]).is_err());
    assert!(store.load(&keys[1]).is_err());
    assert_eq!(roots.unwrap().latest, Some(keys[2].clone()));
    assert_eq!(recovered(temp.path()).publications.len(), 1);
}

#[test]
fn gc_validates_reachable_objects_before_deleting_anything() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    save(&mut store, &publication(1, "old"));
    save(&mut store, &publication(2, "new"));
    let hash = store.catalog.views[1].object.clone();
    fs::write(store.object_path("views", &hash).unwrap(), "broken").unwrap();
    let before = fs::read(temp.path().join("views.json")).unwrap();
    assert!(store.gc(0).is_err());
    assert_eq!(fs::read(temp.path().join("views.json")).unwrap(), before);
    assert_eq!(store.entries().count(), 2);
}

#[test]
fn journal_is_incremental_replayed_and_checkpointed() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut last = None;
    for epoch in 1..=70 {
        let item = publication(epoch, &format!("v{epoch}"));
        let stats = save(&mut store, &item);
        if epoch == 63 {
            assert!(stats.catalog_bytes < 1500);
        }
        last = Some(key(&item.view));
    }
    let checkpoint: Catalog =
        serde_json::from_slice(&fs::read(temp.path().join("views.json")).unwrap()).unwrap();
    assert_eq!(checkpoint.sequence, 64);
    assert_eq!(
        fs::read_dir(temp.path().join("journal")).unwrap().count(),
        6
    );
    let (restored, roots) = SnapshotStore::open_lazy(temp.path(), WORKSPACE).unwrap();
    assert_eq!(restored.catalog.sequence, 70);
    assert_eq!(restored.entries().count(), 70);
    assert_eq!(roots.unwrap().latest, last);
    // A torn temporary write is invisible; corrupt committed segments fail closed.
    fs::write(temp.path().join("journal/.tmp-interrupted"), "partial").unwrap();
    assert!(SnapshotStore::open_lazy(temp.path(), WORKSPACE).is_ok());
    fs::write(
        temp.path().join("journal/00000000000000000070.json"),
        "partial",
    )
    .unwrap();
    assert!(SnapshotStore::open_lazy(temp.path(), WORKSPACE).is_err());
}

fn syntax_publication(epoch: u64) -> Publication {
    let mut item = publication(epoch, &format!("edit {epoch}"));
    let snapshot = Arc::make_mut(&mut Arc::make_mut(&mut item.view).snapshot);
    for index in 0..30 {
        // Interleave files to exercise preservation of arbitrary frontend order.
        for file in ["copy.rs", "a.rs", "edit.rs"] {
            let id = format!("{file}:{index}");
            let span = Span {
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            };
            snapshot.syntax.symbols.push(semweft_core::Symbol {
                symbol_id: id.clone(),
                name: id.clone(),
                qualified_name: id.clone(),
                kind: "function".into(),
                file: file.into(),
                file_hash: snapshot.manifest.files[file].file_hash.clone(),
                span,
                name_span: span,
                signature: "signature".repeat(30),
            });
            snapshot.syntax.impls.push(semweft_core::ImplBlock {
                impl_id: format!("impl:{file}:{index}"),
                file: file.into(),
                file_hash: snapshot.manifest.files[file].file_hash.clone(),
                span,
                trait_path: "module::Trait".into(),
                trait_span: span,
                self_type: "Container<T>".into(),
                generics: vec!["T".into()],
                negative: index % 2 == 0,
                unsafe_impl: index % 3 == 0,
                test_only: file == "copy.rs",
                methods: vec![id.clone()],
            });
            snapshot.syntax.declarations.insert(
                id.clone(),
                semweft_core::DeclarationInfo {
                    docs: "docs".repeat(50),
                    ..Default::default()
                },
            );
            snapshot.syntax.calls.push(semweft_core::CallSite {
                callsite_id: id.clone(),
                caller_id: Some(id.clone()),
                file: file.into(),
                file_hash: snapshot.manifest.files[file].file_hash.clone(),
                span,
                expression: "test()".into(),
                kind: semweft_core::CallKind::Direct,
            });
            snapshot.syntax.occurrences.push(semweft_core::Occurrence {
                name: id,
                file: file.into(),
                span,
                kind: "call".into(),
                enclosing_symbol_id: None,
            });
            snapshot
                .syntax
                .imports
                .insert(file.into(), vec!["test".into()]);
            snapshot
                .syntax
                .skeletons
                .insert(file.into(), "skeleton".into());
            snapshot.syntax.diagnostics.push(Diagnostic {
                code: "file".into(),
                file: Some(file.into()),
                message: "diagnostic".into(),
            });
        }
    }
    snapshot.syntax.diagnostics.push(Diagnostic {
        code: "global".into(),
        file: None,
        message: "global diagnostic".into(),
    });
    snapshot.syntax.impls.sort_by(|left, right| {
        (&left.file, left.span.start, &left.impl_id).cmp(&(
            &right.file,
            right.span.start,
            &right.impl_id,
        ))
    });
    item
}

#[test]
fn impl_file_shards_round_trip_marker_blocks_and_canonical_order() {
    let marker = |file: &str, id: &str, line| semweft_core::ImplBlock {
        impl_id: id.into(),
        file: file.into(),
        file_hash: format!("hash:{file}"),
        span: Span {
            start: Position { line, column: 1 },
            end: Position { line, column: 20 },
        },
        trait_path: "module::Marker".into(),
        trait_span: Span {
            start: Position { line, column: 6 },
            end: Position { line, column: 12 },
        },
        self_type: "S".into(),
        generics: vec![],
        negative: file == "b.rs",
        unsafe_impl: file == "a.rs",
        test_only: true,
        methods: vec![],
    };
    let facts = SyntaxFacts {
        impls: vec![
            marker("b.rs", "b", 1),
            marker("a.rs", "later", 2),
            marker("a.rs", "second", 1),
            marker("a.rs", "first", 1),
        ],
        ..Default::default()
    };
    let (shards, order) = syntax::split(&facts);
    assert_eq!(shards.len(), 2);
    assert_eq!(shards["a.rs"].impls.len(), 3);
    assert_eq!(shards["b.rs"].impls.len(), 1);
    let shards = shards
        .into_iter()
        .map(|(file, shard)| {
            assert!(shard.symbols.is_empty());
            assert!(shard.impls.iter().all(|block| block.file == file));
            serde_json::from_slice::<SyntaxFacts>(&serde_json::to_vec(&shard).unwrap()).unwrap()
        })
        .collect();
    let restored = syntax::join(shards, order).unwrap();
    assert_eq!(
        restored.impls,
        [
            marker("a.rs", "first", 1),
            marker("a.rs", "second", 1),
            marker("a.rs", "later", 2),
            marker("b.rs", "b", 1),
        ]
    );
}

#[test]
fn marker_impl_without_symbols_survives_storage_restart() {
    let temp = private_tempdir();
    let text = "impl Marker for S {}";
    let mut item = publication(1, text);
    let snapshot = Arc::make_mut(&mut Arc::make_mut(&mut item.view).snapshot);
    snapshot.syntax.impls.push(semweft_core::ImplBlock {
        impl_id: "marker".into(),
        file: "edit.rs".into(),
        file_hash: snapshot.manifest.files["edit.rs"].file_hash.clone(),
        span: Span {
            start: Position { line: 1, column: 1 },
            end: Position {
                line: 1,
                column: text.len() as u32 + 1,
            },
        },
        trait_path: "Marker".into(),
        trait_span: Span {
            start: Position { line: 1, column: 6 },
            end: Position {
                line: 1,
                column: 12,
            },
        },
        self_type: "S".into(),
        generics: vec![],
        negative: false,
        unsafe_impl: false,
        test_only: false,
        methods: vec![],
    });
    assert!(snapshot.syntax.symbols.is_empty());
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    assert_eq!(save(&mut store, &item).syntax_files, 1);
    drop(store);
    assert_publication(&recovered(temp.path()).publications[0], &item);
}

#[test]
fn file_syntax_reuses_unchanged_facts_and_preserves_order_and_all_fields() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let first = syntax_publication(1);
    assert_eq!(save(&mut store, &first).syntax_files, 4);
    let first_objects = fs::read_dir(temp.path().join("objects/syntax"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = fs::read(&path).unwrap();
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            (path, bytes, modified)
        })
        .collect::<Vec<_>>();
    let next = syntax_publication(2);
    let stats = save(&mut store, &next);
    assert_eq!(stats.syntax_files, 1);
    assert!(stats.object_bytes < serde_json::to_vec(&next).unwrap().len() / 2);
    for (path, bytes, modified) in first_objects {
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }
    drop(store);
    for (actual, expected) in recovered(temp.path())
        .publications
        .iter()
        .zip([&first, &next])
    {
        assert_publication(actual, expected);
    }
    drop(first);
    drop(next);
    let (mut store, roots) = SnapshotStore::open_lazy(temp.path(), WORKSPACE).unwrap();
    drop(roots);
    assert_eq!(store.gc(1).unwrap().removed_views, 1);
    assert_eq!(
        fs::read_dir(temp.path().join("objects/syntax"))
            .unwrap()
            .count(),
        4
    );
    assert_eq!(recovered(temp.path()).publications.len(), 1);
}

#[test]
fn legacy_inline_syntax_snapshot_remains_readable() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let item = syntax_publication(1);
    save(&mut store, &item);
    let mut view: ViewObject = store
        .read_json("views", &store.catalog.views[0].object)
        .unwrap();
    let mut snapshot: serde_json::Value = store.read_json("snapshots", &view.snapshot).unwrap();
    snapshot["syntax"] = serde_json::to_value(&item.view.snapshot.syntax).unwrap();
    snapshot.as_object_mut().unwrap().remove("syntax_files");
    snapshot.as_object_mut().unwrap().remove("syntax_order");
    view.snapshot = store
        .write_object("snapshots", &serde_json::to_vec(&snapshot).unwrap())
        .unwrap()
        .0;
    store.catalog.views[0].object = store
        .write_object("views", &serde_json::to_vec(&view).unwrap())
        .unwrap()
        .0;
    atomic_write(
        &temp.path().join("views.json"),
        &serde_json::to_vec(&store.catalog).unwrap(),
    )
    .unwrap();
    assert_publication(&recovered(temp.path()).publications[0], &item);
}

#[test]
fn legacy_syntax_shards_without_impls_remain_readable() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut item = syntax_publication(1);
    Arc::make_mut(&mut Arc::make_mut(&mut item.view).snapshot)
        .syntax
        .impls
        .clear();
    save(&mut store, &item);
    let mut view: ViewObject = store
        .read_json("views", &store.catalog.views[0].object)
        .unwrap();
    let mut snapshot: SnapshotObject = store.read_json("snapshots", &view.snapshot).unwrap();
    for hash in snapshot.syntax_files.values_mut() {
        let mut shard: Value = store.read_json("syntax", hash).unwrap();
        shard.as_object_mut().unwrap().remove("impls");
        *hash = store
            .write_object("syntax", &serde_json::to_vec(&shard).unwrap())
            .unwrap()
            .0;
    }
    view.snapshot = store
        .write_object("snapshots", &serde_json::to_vec(&snapshot).unwrap())
        .unwrap()
        .0;
    store.catalog.views[0].object = store
        .write_object("views", &serde_json::to_vec(&view).unwrap())
        .unwrap()
        .0;
    atomic_write(
        &temp.path().join("views.json"),
        &serde_json::to_vec(&store.catalog).unwrap(),
    )
    .unwrap();
    drop(store);
    assert_publication(&recovered(temp.path()).publications[0], &item);
}

// Faults are enabled only in this test binary and only in the isolated child.
pub(super) fn write_fault(path: &Path, stage: &str) -> Result<()> {
    let Ok(target) = std::env::var("SEMWEFT_TEST_FAULT_TARGET") else {
        return Ok(());
    };
    let kind = if path.file_name().is_some_and(|name| name == "views.json") {
        "catalog"
    } else {
        path.parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("")
    };
    if target != format!("{kind}:{stage}") {
        return Ok(());
    }
    if std::env::var("SEMWEFT_TEST_FAULT_ACTION").as_deref() == Ok("exit") {
        // exit skips Rust destructors, including the temporary file's cleanup.
        std::process::exit(91);
    }
    bail!("index_store_error: injected disk full")
}

#[test]
fn interrupted_writer_child() {
    let Ok(root) = std::env::var("SEMWEFT_TEST_CHILD_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let mode = std::env::var("SEMWEFT_TEST_CHILD_MODE").unwrap();
    let result = (|| -> Result<()> {
        let (mut store, roots) = SnapshotStore::open_lazy(root, WORKSPACE)?;
        drop(roots);
        if mode == "migration" {
            return Ok(());
        }
        if mode == "gc" {
            store.gc(0)?;
        } else if mode == "compact" {
            store.compact_objects()?;
        } else {
            let next = publication(2, "new");
            store.commit(Some(key(&next.view)), &[borrowed(&next)])?;
        }
        Ok(())
    })();
    assert!(result.is_err(), "fault must have fired");
    std::process::exit(92);
}

#[test]
fn abrupt_process_exit_and_disk_failures_preserve_recoverable_transactions() {
    for mode in ["refresh", "checkpoint", "migration", "gc", "compact"] {
        for stage in ["before_rename", "after_rename", "after_dir_sync"] {
            for action in ["exit", "error"] {
                let temp = private_tempdir();
                let old = publication(1, "old");
                if mode == "migration" {
                    atomic_write(&temp.path().join("views.json"), &legacy_bytes(&[old], None))
                        .unwrap();
                } else {
                    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
                    save(&mut store, &old);
                    if mode == "checkpoint" {
                        store.catalog.sequence = 63;
                        atomic_write(
                            &temp.path().join("views.json"),
                            &serde_json::to_vec(&store.catalog).unwrap(),
                        )
                        .unwrap();
                    }
                    if mode == "gc" {
                        save(&mut store, &publication(2, "new"));
                    }
                    if mode == "compact" {
                        for entry in fs::read_dir(temp.path().join("objects/sources")).unwrap() {
                            let entry = entry.unwrap();
                            let decoded = store
                                .read_object("sources", &entry.file_name().to_string_lossy())
                                .unwrap();
                            fs::write(entry.path(), decoded).unwrap();
                        }
                    }
                }
                let kind = if mode == "refresh" {
                    "journal"
                } else if mode == "compact" {
                    "sources"
                } else {
                    "catalog"
                };
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "persistence::tests::interrupted_writer_child",
                        "--nocapture",
                    ])
                    .env("SEMWEFT_TEST_CHILD_ROOT", temp.path())
                    .env("SEMWEFT_TEST_CHILD_MODE", mode)
                    .env("SEMWEFT_TEST_FAULT_TARGET", format!("{kind}:{stage}"))
                    .env("SEMWEFT_TEST_FAULT_ACTION", action)
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(if action == "exit" { 91 } else { 92 }),
                    "{mode} {stage} {action}: {}",
                    String::from_utf8_lossy(&output.stdout)
                );
                let recovered = recovered(temp.path());
                let expected = if mode == "migration"
                    || mode == "compact"
                    || (mode != "gc" && stage == "before_rename")
                {
                    1
                } else {
                    2
                };
                assert_eq!(
                    recovered
                        .publications
                        .last()
                        .unwrap()
                        .view
                        .epochs
                        .syntax_epoch,
                    expected,
                    "{mode} {stage} {action}"
                );
                if mode == "gc" {
                    assert_eq!(
                        recovered.publications.len(),
                        if stage == "before_rename" { 2 } else { 1 }
                    );
                }
                if mode == "compact" {
                    let (mut store, _) = SnapshotStore::open_lazy(temp.path(), WORKSPACE).unwrap();
                    store.compact_objects().unwrap();
                    assert_publication(&recovered.publications[0], &publication(1, "old"));
                }
            }
        }
    }
}

#[test]
fn retry_reconciles_a_transaction_visible_before_a_late_sync_error() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let first = publication(1, "old");
    save(&mut store, &first);
    let next = publication(2, "new");
    assert!(
        store
            .commit_with(Some(key(&next.view)), &[borrowed(&next)], |path, bytes| {
                atomic_write(path, bytes)?;
                bail!("index_store_error: injected late sync error")
            })
            .is_err()
    );
    // A following pin transaction must preserve the publication already on disk.
    store.pin(&key(&first.view), true).unwrap();
    assert_eq!(store.entries().count(), 2);
    assert_eq!(store.latest_key(), &Some(key(&next.view)));
    let mut catalog = store.catalog.clone();
    catalog.pins.insert(key(&next.view));
    assert!(
        store
            .publish_catalog(&mut catalog, false, |path, bytes| {
                atomic_write(path, bytes)?;
                bail!("late sync error")
            })
            .is_err()
    );
    store.pin(&key(&next.view), false).unwrap();
    let restored = SnapshotStore::open_lazy(temp.path(), WORKSPACE).unwrap().0;
    assert_eq!(restored.entries().count(), 2);
    assert!(restored.pins().contains(&key(&first.view)));
    assert!(!restored.pins().contains(&key(&next.view)));
}

#[test]
fn staged_gc_observes_concurrent_read_leases_and_stale_readers_report_expiration() {
    let temp = private_tempdir();
    let (mut writer, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut keys = vec![];
    for epoch in 1..=3 {
        let item = publication(epoch, &format!("v{epoch}"));
        keys.push(key(&item.view));
        save(&mut writer, &item);
    }
    let mut reader = writer.clone();
    let held = reader.load(&keys[0]).unwrap();
    assert_eq!(writer.gc(0).unwrap().removed_views, 1);
    assert!(reader.load(&keys[0]).is_ok());
    drop(held);
    assert_eq!(writer.gc(0).unwrap().removed_views, 1);
    let error = reader.load(&keys[0]).unwrap_err();
    assert!(
        error.to_string().starts_with("snapshot_expired:"),
        "{error:#}"
    );
    assert!(reader.load(&keys[2]).is_ok());
}

#[test]
fn stale_reader_uses_current_supplemental_object_after_gc() {
    let temp = private_tempdir();
    let (mut writer, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut item = publication(1, "first");
    save(&mut writer, &item);
    let mut reader = writer.clone();
    item.reference_gaps.push(gap("new supplemental evidence"));
    save(&mut writer, &item);
    assert!(writer.gc(0).unwrap().removed_objects > 0);
    assert_publication(&reader.load(&key(&item.view)).unwrap(), &item);
}

#[test]
fn mixed_legacy_and_encoded_objects_migrate_without_changing_publications() {
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let item = publication(1, "// repeated source\n".repeat(2000).as_str());
    save(&mut store, &item);
    let checkpoint = fs::read(temp.path().join("views.json")).unwrap();
    for kind in ["sources", "syntax", "snapshots", "views"] {
        if !temp.path().join("objects").join(kind).exists() {
            continue;
        }
        for (i, entry) in fs::read_dir(temp.path().join("objects").join(kind))
            .unwrap()
            .enumerate()
        {
            let entry = entry.unwrap();
            let decoded = store
                .read_object(kind, &entry.file_name().to_string_lossy())
                .unwrap();
            let codec = if i % 2 == 0 {
                crate::codec::Codec::Raw
            } else {
                crate::codec::Codec::Deflate
            };
            fs::write(entry.path(), crate::codec::encode(&decoded, codec)).unwrap();
        }
    }
    assert_publication(&recovered(temp.path()).publications[0], &item);
    assert!(store.compact_objects().unwrap() > 0);
    assert_eq!(store.compact_objects().unwrap(), 0);
    assert_eq!(
        fs::read(temp.path().join("views.json")).unwrap(),
        checkpoint
    );
    assert_publication(&recovered(temp.path()).publications[0], &item);
    assert_eq!(store.codec_benchmark().unwrap().len(), 3);
}

#[test]
fn gc_preview_is_read_only_and_disk_budget_preserves_pins_and_live_readers() {
    fn inventory(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
        let mut result = BTreeMap::new();
        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                result.extend(inventory(&entry.path()));
            } else {
                result.insert(
                    entry.path(),
                    (
                        fs::read(entry.path()).unwrap(),
                        entry.metadata().unwrap().modified().unwrap(),
                    ),
                );
            }
        }
        result
    }
    let temp = private_tempdir();
    let (mut store, _) = SnapshotStore::open(temp.path(), WORKSPACE).unwrap();
    let mut versions = vec![];
    for epoch in 1..=6 {
        let item = publication(epoch, &format!("fn revision_{epoch}() {{}}"));
        save(&mut store, &item);
        versions.push(key(&item.view));
    }
    store.pin(&versions[0], true).unwrap();
    let reader = store.load(&versions[1]).unwrap();
    store
        .write_object("sources", b"unreachable orphan")
        .unwrap();
    let before = inventory(temp.path());
    let sequence = store.journal_sequence();
    let usage = store.disk_usage().unwrap();
    assert_eq!(
        usage.total_bytes,
        before
            .values()
            .map(|(bytes, _)| bytes.len() as u64)
            .sum::<u64>()
    );
    let minimum = store.preview_gc(usize::MAX, Some(0)).unwrap();
    assert!(minimum.budget_unmet);
    assert_eq!(minimum.protected_views, 3);
    assert_eq!(minimum.removed_views, 3);
    assert_eq!(store.journal_sequence(), sequence);
    assert_eq!(inventory(temp.path()), before);
    let preview = store
        .preview_gc(usize::MAX, Some(minimum.remaining_bytes + 32))
        .unwrap();
    assert!(!preview.budget_unmet);
    let actual = store
        .gc_with_budget(usize::MAX, Some(minimum.remaining_bytes + 32))
        .unwrap();
    assert_eq!(actual.removed_views, preview.removed_views);
    assert_eq!(actual.removed_objects, preview.removed_objects);
    assert_eq!(actual.reclaimed_bytes, preview.reclaimed_bytes);
    assert_eq!(actual.remaining_bytes, preview.remaining_bytes);
    assert!(!actual.budget_unmet);
    assert_publication(&store.load(&versions[1]).unwrap(), &reader);
    assert!(store.find_key(&versions[0].0, None).is_some());
    assert!(store.find_key(&versions[5].0, None).is_some());
    assert!(store.find_key(&versions[2].0, None).is_none());
    let impossible = store.gc_with_budget(0, Some(0)).unwrap();
    assert!(impossible.budget_unmet);
    assert!(impossible.remaining_bytes > 0);
}
