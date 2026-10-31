//! Planning, persistence and publication rules not covered by refresh.rs.
use semweft_core::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[allow(clippy::too_many_arguments)]
fn symbol(
    id: &str,
    name: &str,
    path: &str,
    hash: &str,
    qualified_name: &str,
    kind: &str,
    line: u32,
    column: u32,
) -> Symbol {
    let end = column + name.len() as u32;
    Symbol {
        symbol_id: id.into(),
        name: name.into(),
        qualified_name: qualified_name.into(),
        kind: kind.into(),
        file: path.into(),
        file_hash: hash.into(),
        span: Span {
            start: Position { line, column: 1 },
            end: Position { line, column: end },
        },
        name_span: Span {
            start: Position { line, column },
            end: Position { line, column: end },
        },
        signature: format!("fn {name}()"),
    }
}

#[allow(clippy::too_many_arguments)]
fn site(
    id: &str,
    caller: &str,
    path: &str,
    hash: &str,
    line: u32,
    column: u32,
    expression: &str,
    kind: CallKind,
) -> CallSite {
    CallSite {
        callsite_id: id.into(),
        caller_id: Some(caller.into()),
        file: path.into(),
        file_hash: hash.into(),
        span: Span {
            start: Position { line, column },
            end: Position {
                line,
                column: column + expression.len() as u32,
            },
        },
        expression: expression.into(),
        kind,
    }
}

fn make_manifest(files: &[(&str, &str)]) -> Manifest {
    Manifest {
        workspace_id: "ws".into(),
        files: files
            .iter()
            .map(|(path, text)| ((*path).to_string(), SourceFile::new(*text)))
            .collect(),
        binary_files: BTreeMap::new(),
        analysis_versions: BTreeMap::from([("semantic".into(), "fixture@1".into())]),
        scope: "test".into(),
    }
}

fn basic() -> (Manifest, SyntaxFacts) {
    let text = "fn caller() { target(); }\nfn target() {}\n";
    let manifest = make_manifest(&[("src/lib.rs", text)]);
    let hash = manifest.files["src/lib.rs"].file_hash.clone();
    let syntax = SyntaxFacts {
        declarations: BTreeMap::new(),
        imports: BTreeMap::new(),
        symbols: vec![
            symbol(
                "caller",
                "caller",
                "src/lib.rs",
                &hash,
                "src/lib.rs::caller",
                "function",
                1,
                4,
            ),
            symbol(
                "target",
                "target",
                "src/lib.rs",
                &hash,
                "src/lib.rs::target",
                "function",
                2,
                4,
            ),
        ],
        calls: vec![site(
            "call_1",
            "caller",
            "src/lib.rs",
            &hash,
            1,
            15,
            "target",
            CallKind::Direct,
        )],
        diagnostics: vec![],
        skeletons: BTreeMap::from([("src/lib.rs".into(), "lib-skel".into())]),
        occurrences: vec![],
    };
    (manifest, syntax)
}

fn two_files() -> (Manifest, SyntaxFacts) {
    let manifest = make_manifest(&[
        ("src/a.rs", "fn alpha() {}\nfn beta() {}\n"),
        ("src/b.rs", "fn gamma() { beta(); }\n"),
    ]);
    let a = manifest.files["src/a.rs"].file_hash.clone();
    let b = manifest.files["src/b.rs"].file_hash.clone();
    let syntax = SyntaxFacts {
        declarations: BTreeMap::new(),
        imports: BTreeMap::new(),
        symbols: vec![
            symbol(
                "alpha@1",
                "alpha",
                "src/a.rs",
                &a,
                "src/a.rs::alpha",
                "function",
                1,
                4,
            ),
            symbol(
                "beta@1",
                "beta",
                "src/a.rs",
                &a,
                "src/a.rs::beta",
                "function",
                2,
                4,
            ),
            symbol(
                "gamma@1",
                "gamma",
                "src/b.rs",
                &b,
                "src/b.rs::gamma",
                "function",
                1,
                4,
            ),
        ],
        calls: vec![site(
            "c1",
            "gamma@1",
            "src/b.rs",
            &b,
            1,
            14,
            "beta",
            CallKind::Direct,
        )],
        diagnostics: vec![],
        skeletons: BTreeMap::from([
            ("src/a.rs".into(), "a-skel".into()),
            ("src/b.rs".into(), "b-skel".into()),
        ]),
        occurrences: vec![],
    };
    (manifest, syntax)
}

fn resolved_call(symbols: &[Symbol], callsite: &str, target: &str) -> CallResolution {
    let symbol = symbols
        .iter()
        .find(|s| s.symbol_id == target)
        .expect("target exists");
    CallResolution {
        callsite_id: callsite.into(),
        targets: vec![Target {
            symbol_id: target.into(),
            file: symbol.file.clone(),
            span: symbol.name_span,
            concrete_function: true,
            derivation: TargetDerivation::Definition,
        }],
        diagnostics: vec![],
        cfg_inactive: false,
    }
}

fn batch(view: &AnalysisView, resolutions: Vec<CallResolution>) -> SemanticBatch {
    SemanticBatch {
        snapshot_id: view.snapshot.snapshot_id.clone(),
        syntax_epoch: view.epochs.syntax_epoch,
        input_hash: view.snapshot.input_hash.clone(),
        backend: "fixture@1".into(),
        resolutions,
        diagnostics: vec![],
    }
}

fn complete(coordinator: &mut RefreshCoordinator, view: &AnalysisView) -> Arc<AnalysisView> {
    let ticket = coordinator.begin_refresh().unwrap();
    let symbols = &view.snapshot.syntax.symbols;
    let resolutions = view
        .snapshot
        .syntax
        .calls
        .iter()
        .map(|call| {
            // Resolve every call to the last symbol so the refresh is total.
            let target = &symbols[symbols.len() - 1].symbol_id;
            resolved_call(symbols, &call.callsite_id, target)
        })
        .collect();
    coordinator
        .complete(&ticket, batch(view, resolutions))
        .unwrap()
}

fn clone_view(view: &Arc<AnalysisView>) -> AnalysisView {
    (**view).clone()
}

#[test]
fn plans_require_published_inputs() {
    let mut coordinator = RefreshCoordinator::default();
    assert_eq!(coordinator.plan_refresh().unwrap_err(), "not_indexed");
    assert_eq!(coordinator.begin_refresh().unwrap_err(), "not_indexed");
    assert!(coordinator.latest().is_none());
    assert!(coordinator.retained().is_empty());
}

#[test]
fn first_plan_is_full_without_a_semantic_base() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = basic();
    coordinator.publish_inputs(manifest, syntax).unwrap();
    let plan = coordinator.plan_refresh().unwrap();
    assert_eq!(
        plan.scope,
        RefreshScope::Full {
            reason: "no_published_semantic_view".into()
        }
    );
    assert_eq!(plan.base_syntax_epoch, None);
    assert_eq!(plan.query, BTreeSet::from(["call_1".to_string()]));
    assert!(plan.reused.is_empty());
}

#[test]
fn body_edits_reuse_outside_outcomes_and_remap_moved_declarations() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = two_files();
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    let ticket = coordinator.begin_refresh().unwrap();
    let resolutions = vec![resolved_call(
        &pending.snapshot.syntax.symbols,
        "c1",
        "beta@1",
    )];
    coordinator
        .complete(&ticket, batch(&pending, resolutions))
        .unwrap();

    let manifest = make_manifest(&[
        ("src/a.rs", "// moved\nfn alpha() {}\nfn beta() {}\n"),
        ("src/b.rs", "fn gamma() { beta(); }\n"),
    ]);
    let a = manifest.files["src/a.rs"].file_hash.clone();
    let b = manifest.files["src/b.rs"].file_hash.clone();
    // Same skeleton, but both declarations moved down by one line.
    let syntax = SyntaxFacts {
        declarations: BTreeMap::new(),
        imports: BTreeMap::new(),
        symbols: vec![
            symbol(
                "alpha@2",
                "alpha",
                "src/a.rs",
                &a,
                "src/a.rs::alpha",
                "function",
                2,
                4,
            ),
            symbol(
                "beta@2",
                "beta",
                "src/a.rs",
                &a,
                "src/a.rs::beta",
                "function",
                3,
                4,
            ),
            symbol(
                "gamma@1",
                "gamma",
                "src/b.rs",
                &b,
                "src/b.rs::gamma",
                "function",
                1,
                4,
            ),
        ],
        calls: vec![site(
            "c1",
            "gamma@1",
            "src/b.rs",
            &b,
            1,
            14,
            "beta",
            CallKind::Direct,
        )],
        diagnostics: vec![],
        skeletons: BTreeMap::from([
            ("src/a.rs".into(), "a-skel".into()),
            ("src/b.rs".into(), "b-skel".into()),
        ]),
        occurrences: vec![],
    };
    let view = coordinator.publish_inputs(manifest, syntax).unwrap();
    assert_eq!(view.epochs.semantic_epoch, 1, "semantic facts are pending");

    let plan = coordinator.plan_refresh().unwrap();
    assert_eq!(
        plan.scope,
        RefreshScope::BodyLocal {
            changed_files: vec!["src/a.rs".into()]
        }
    );
    assert_eq!(plan.base_syntax_epoch, Some(1));
    assert!(plan.query.is_empty(), "src/b.rs callsite is reusable");
    assert_eq!(plan.reused.len(), 1);
    let reused = &plan.reused[0];
    assert_eq!(reused.callsite_id, "c1");
    // The target declaration moved inside the edited file; its new source
    // coordinates must come from the new snapshot, not the old one.
    assert_eq!(
        reused.targets[0].span,
        view.snapshot.syntax.symbols[1].name_span
    );
    assert_eq!(reused.targets[0].span.start.line, 3);
}

#[test]
fn ambiguous_declarations_prevent_target_reuse() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = two_files();
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    let ticket = coordinator.begin_refresh().unwrap();
    coordinator
        .complete(
            &ticket,
            batch(
                &pending,
                vec![resolved_call(
                    &pending.snapshot.syntax.symbols,
                    "c1",
                    "beta@1",
                )],
            ),
        )
        .unwrap();

    let manifest = make_manifest(&[
        (
            "src/a.rs",
            "// moved\nfn alpha() {}\nfn beta() {}\nfn beta() {}\n",
        ),
        ("src/b.rs", "fn gamma() { beta(); }\n"),
    ]);
    let a = manifest.files["src/a.rs"].file_hash.clone();
    let b = manifest.files["src/b.rs"].file_hash.clone();
    // Two declarations share file, qualified name and kind: the moved target
    // cannot be identified unambiguously, so the callsite must be re-queried.
    let syntax = SyntaxFacts {
        declarations: BTreeMap::new(),
        imports: BTreeMap::new(),
        symbols: vec![
            symbol(
                "alpha@2",
                "alpha",
                "src/a.rs",
                &a,
                "src/a.rs::alpha",
                "function",
                2,
                4,
            ),
            symbol(
                "beta@2",
                "beta",
                "src/a.rs",
                &a,
                "src/a.rs::beta",
                "function",
                3,
                4,
            ),
            symbol(
                "beta@3",
                "beta",
                "src/a.rs",
                &a,
                "src/a.rs::beta",
                "function",
                4,
                4,
            ),
            symbol(
                "gamma@1",
                "gamma",
                "src/b.rs",
                &b,
                "src/b.rs::gamma",
                "function",
                1,
                4,
            ),
        ],
        calls: vec![site(
            "c1",
            "gamma@1",
            "src/b.rs",
            &b,
            1,
            14,
            "beta",
            CallKind::Direct,
        )],
        diagnostics: vec![],
        skeletons: BTreeMap::from([
            ("src/a.rs".into(), "a-skel".into()),
            ("src/b.rs".into(), "b-skel".into()),
        ]),
        occurrences: vec![],
    };
    coordinator.publish_inputs(manifest, syntax).unwrap();
    let plan = coordinator.plan_refresh().unwrap();
    assert_eq!(plan.query, BTreeSet::from(["c1".to_string()]));
    assert!(plan.reused.is_empty());
}

#[test]
fn analysis_build_and_file_set_changes_force_full_refreshes() {
    let reason = |coordinator: &mut RefreshCoordinator, manifest, syntax| {
        coordinator.publish_inputs(manifest, syntax).unwrap();
        match coordinator.plan_refresh().unwrap().scope {
            RefreshScope::Full { reason } => reason,
            other => panic!("expected full refresh, got {other:?}"),
        }
    };
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = basic();
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    complete(&mut coordinator, &pending);

    let (mut changed, syntax) = basic();
    changed
        .analysis_versions
        .insert("engine".into(), "fixture@2".into());
    assert_eq!(
        reason(&mut coordinator, changed, syntax),
        "analysis_versions_changed"
    );

    let (mut changed, syntax) = basic();
    changed.files.insert(
        "Cargo.toml".into(),
        SourceFile::new("[package]\nname=\"x\"\n"),
    );
    assert_eq!(
        reason(&mut coordinator, changed, syntax),
        "build_or_configuration_input_changed"
    );

    let (mut changed, mut syntax) = basic();
    changed
        .files
        .insert("src/extra.rs".into(), SourceFile::new("\n"));
    syntax
        .skeletons
        .insert("src/extra.rs".into(), "extra-skel".into());
    assert_eq!(
        reason(&mut coordinator, changed, syntax),
        "rust_file_set_changed"
    );
}

#[test]
fn skeleton_changes_and_missing_skeletons_force_full_refreshes() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = basic();
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    complete(&mut coordinator, &pending);

    let (mut changed, mut syntax) = basic();
    changed.files.get_mut("src/lib.rs").unwrap().content =
        "fn caller() { target(); }\nfn target() {}\nfn other() {}\n".into();
    changed.files.get_mut("src/lib.rs").unwrap().file_hash =
        digest(changed.files["src/lib.rs"].content.as_bytes());
    for symbol in &mut syntax.symbols {
        symbol.file_hash = changed.files["src/lib.rs"].file_hash.clone();
    }
    for call in &mut syntax.calls {
        call.file_hash = changed.files["src/lib.rs"].file_hash.clone();
    }
    syntax
        .skeletons
        .insert("src/lib.rs".into(), "new-skel".into());
    coordinator.publish_inputs(changed, syntax).unwrap();
    assert_eq!(
        coordinator.plan_refresh().unwrap().scope,
        RefreshScope::Full {
            reason: "item_skeleton_changed: src/lib.rs".into()
        }
    );

    let (mut changed, mut syntax) = basic();
    changed.files.get_mut("src/lib.rs").unwrap().content =
        "fn caller() { target(); }\nfn target() { 0; }\n".into();
    changed.files.get_mut("src/lib.rs").unwrap().file_hash =
        digest(changed.files["src/lib.rs"].content.as_bytes());
    for symbol in &mut syntax.symbols {
        symbol.file_hash = changed.files["src/lib.rs"].file_hash.clone();
    }
    for call in &mut syntax.calls {
        call.file_hash = changed.files["src/lib.rs"].file_hash.clone();
    }
    syntax.skeletons.clear();
    coordinator.publish_inputs(changed, syntax).unwrap();
    assert_eq!(
        coordinator.plan_refresh().unwrap().scope,
        RefreshScope::Full {
            reason: "skeleton_unavailable: src/lib.rs".into()
        }
    );
}

#[test]
fn restore_accepts_published_views_and_rejects_corrupt_envelopes() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = basic();
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    let done = complete(&mut coordinator, &pending);
    let restored = RefreshCoordinator::restore(coordinator.retained().to_vec()).unwrap();
    assert_eq!(restored.latest().unwrap(), done);
    assert_eq!(restored.latest_current().unwrap(), done);
    assert_eq!(restored.view(&done.snapshot.snapshot_id, 1).unwrap(), done);

    let mut bad_hash = clone_view(&done);
    bad_hash.snapshot = Arc::new(Snapshot {
        input_hash: "forged".into(),
        ..(*done.snapshot).clone()
    });
    assert_eq!(
        RefreshCoordinator::restore(vec![Arc::new(bad_hash)])
            .err()
            .as_deref(),
        Some("index_store_error: invalid persisted version")
    );

    let mut bad_epoch = clone_view(&done);
    bad_epoch.epochs.syntax_epoch += 1;
    assert_eq!(
        RefreshCoordinator::restore(vec![Arc::new(bad_epoch)])
            .err()
            .as_deref(),
        Some("index_store_error: invalid persisted version")
    );

    let mut syntax = pending.snapshot.syntax.clone();
    syntax.symbols[0].name_span.end.column = 9999;
    let mut bad_syntax = clone_view(&pending);
    bad_syntax.snapshot = Arc::new(Snapshot {
        syntax,
        ..(*pending.snapshot).clone()
    });
    assert!(RefreshCoordinator::restore(vec![Arc::new(bad_syntax)]).is_err());
}

#[test]
fn selecting_unknown_versions_reports_snapshot_expiry() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = basic();
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    let done = complete(&mut coordinator, &pending);
    assert_eq!(
        coordinator
            .select_latest(&done.snapshot.snapshot_id, 99)
            .unwrap_err(),
        "snapshot_expired"
    );
    assert_eq!(
        coordinator.select_latest("snap_missing", 1).unwrap_err(),
        "snapshot_expired"
    );
    coordinator
        .select_latest(&pending.snapshot.snapshot_id, 0)
        .unwrap();
    assert_eq!(coordinator.latest().unwrap(), pending);
    assert!(coordinator.view("snap_missing", 0).is_none());
}

#[test]
fn latest_current_ignores_revision_views_and_stale_epochs() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = basic();
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    let done = complete(&mut coordinator, &pending);

    let (mut revision, syntax) = basic();
    revision
        .analysis_versions
        .insert("revision".into(), "deadbeef".into());
    let revision = coordinator.publish_inputs(revision, syntax).unwrap();
    assert_eq!(coordinator.latest().unwrap(), revision);
    assert_eq!(coordinator.latest_current().unwrap(), done);

    let mut empty = RefreshCoordinator::default();
    let (manifest, syntax) = basic();
    let mut only_pending = manifest.clone();
    only_pending
        .analysis_versions
        .insert("revision".into(), "deadbeef".into());
    empty.publish_inputs(only_pending, syntax).unwrap();
    assert!(empty.latest_current().is_none());
}

#[test]
fn complete_rejects_unknown_callsites_targets_and_invalid_assertions() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = basic();
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    let ticket = coordinator.begin_refresh().unwrap();

    let unknown_callsite = CallResolution {
        callsite_id: "missing".into(),
        targets: vec![],
        diagnostics: vec![],
        cfg_inactive: false,
    };
    assert_eq!(
        coordinator
            .complete(&ticket, batch(&pending, vec![unknown_callsite]))
            .unwrap_err(),
        "unknown_callsite"
    );

    let mut resolution = resolved_call(&pending.snapshot.syntax.symbols, "call_1", "target");
    resolution.targets[0].symbol_id = "ghost".into();
    assert_eq!(
        coordinator
            .complete(&ticket, batch(&pending, vec![resolution]))
            .unwrap_err(),
        "unknown_target"
    );

    let mut coordinator = RefreshCoordinator::default();
    let (manifest, mut syntax) = basic();
    syntax.symbols.push(symbol(
        "widget",
        "widget",
        "src/lib.rs",
        &manifest.files["src/lib.rs"].file_hash,
        "src/lib.rs::widget",
        "struct",
        2,
        1,
    ));
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    let ticket = coordinator.begin_refresh().unwrap();
    let mut assertion = resolved_call(&pending.snapshot.syntax.symbols, "call_1", "widget");
    assertion.targets[0].concrete_function = true;
    assert_eq!(
        coordinator
            .complete(&ticket, batch(&pending, vec![assertion]))
            .unwrap_err(),
        "invalid_target_assertion"
    );
}

#[test]
fn publish_rejects_duplicate_symbols_unknown_callers_and_invalid_spans() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, mut syntax) = basic();
    let duplicate = syntax.symbols[0].clone();
    syntax.symbols.push(duplicate);
    assert_eq!(
        coordinator.publish_inputs(manifest, syntax).unwrap_err(),
        "duplicate_symbol"
    );

    let (manifest, mut syntax) = basic();
    syntax.calls[0].caller_id = Some("ghost".into());
    assert_eq!(
        coordinator.publish_inputs(manifest, syntax).unwrap_err(),
        "unknown_caller"
    );

    let (manifest, mut syntax) = basic();
    syntax.symbols[1].span.start.line = 9;
    assert!(coordinator.publish_inputs(manifest, syntax).is_err());
    assert!(coordinator.latest().is_none());

    let (mut manifest, syntax) = basic();
    manifest.binary_files.insert("blob".into(), digest([0, 1]));
    assert!(coordinator.publish_inputs(manifest, syntax).is_ok());
}

#[test]
fn identical_inputs_with_different_syntax_are_rejected() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, syntax) = basic();
    coordinator.publish_inputs(manifest, syntax).unwrap();
    let (manifest, mut syntax) = basic();
    syntax.symbols[0].signature = "changed".into();
    assert_eq!(
        coordinator.publish_inputs(manifest, syntax).unwrap_err(),
        "inconsistent_syntax_rebuild"
    );
}

#[test]
fn refresh_scope_serializes_with_its_mode_tag() {
    let full = RefreshScope::Full {
        reason: "test".into(),
    };
    let body = RefreshScope::BodyLocal {
        changed_files: vec!["src/lib.rs".into()],
    };
    assert_eq!(
        serde_json::to_value(&full).unwrap(),
        serde_json::json!({"mode":"full","reason":"test"})
    );
    assert_eq!(
        serde_json::to_value(&body).unwrap(),
        serde_json::json!({"mode":"body_local","changed_files":["src/lib.rs"]})
    );
}
