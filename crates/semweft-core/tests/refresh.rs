use semweft_core::*;
use std::{collections::BTreeMap, sync::Arc};

fn input(version: &str) -> (Manifest, SyntaxFacts) {
    let text = format!("fn caller() {{ target(); }} // {version}\nfn target() {{}}\n");
    let file = SourceFile::new(text.clone());
    let pos = |line, column| Position { line, column };
    let caller = Symbol {
        symbol_id: "caller".into(),
        name: "caller".into(),
        qualified_name: "lib::caller".into(),
        kind: "function".into(),
        file: "src/lib.rs".into(),
        file_hash: file.file_hash.clone(),
        span: Span {
            start: pos(1, 1),
            end: pos(1, text.lines().next().unwrap().len() as u32 + 1),
        },
        name_span: Span {
            start: pos(1, 4),
            end: pos(1, 10),
        },
        signature: "fn caller()".into(),
    };
    let target = Symbol {
        symbol_id: "target".into(),
        name: "target".into(),
        qualified_name: "lib::target".into(),
        kind: "function".into(),
        file: "src/lib.rs".into(),
        file_hash: file.file_hash.clone(),
        span: Span {
            start: pos(2, 1),
            end: pos(2, 15),
        },
        name_span: Span {
            start: pos(2, 4),
            end: pos(2, 10),
        },
        signature: "fn target()".into(),
    };
    let call = CallSite {
        callsite_id: "call_1".into(),
        caller_id: Some("caller".into()),
        file: "src/lib.rs".into(),
        file_hash: file.file_hash.clone(),
        span: Span {
            start: pos(1, 15),
            end: pos(1, 21),
        },
        expression: "target".into(),
        kind: CallKind::Direct,
    };
    (
        Manifest {
            workspace_id: "workspace".into(),
            files: BTreeMap::from([("src/lib.rs".into(), file)]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::from([("semantic".into(), "fixture@1".into())]),
            scope: "fixture".into(),
        },
        SyntaxFacts {
            declarations: BTreeMap::new(),
            imports: BTreeMap::new(),
            symbols: vec![caller, target],
            calls: vec![call],
            diagnostics: vec![],
            skeletons: BTreeMap::new(),
            occurrences: vec![],
        },
    )
}

fn publish(coordinator: &mut RefreshCoordinator, version: &str) -> Arc<AnalysisView> {
    let (manifest, syntax) = input(version);
    coordinator.publish_inputs(manifest, syntax).unwrap()
}

fn batch(view: &AnalysisView) -> SemanticBatch {
    SemanticBatch {
        snapshot_id: view.snapshot.snapshot_id.clone(),
        syntax_epoch: view.epochs.syntax_epoch,
        input_hash: view.snapshot.input_hash.clone(),
        backend: "fixture@1".into(),
        resolutions: vec![CallResolution {
            callsite_id: "call_1".into(),
            targets: vec![Target {
                symbol_id: "target".into(),
                file: "src/lib.rs".into(),
                span: view.snapshot.syntax.symbols[1].name_span,
                concrete_function: true,
                derivation: TargetDerivation::Definition,
            }],
            diagnostics: vec![],
            cfg_inactive: false,
        }],
        diagnostics: vec![],
    }
}

#[test]
fn identical_inputs_reuse_snapshot_and_reverted_inputs_get_a_new_epoch() {
    let mut coordinator = RefreshCoordinator::default();
    let first = publish(&mut coordinator, "a");
    assert_eq!(
        first.epochs,
        Epochs {
            syntax_epoch: 1,
            semantic_epoch: 0
        }
    );
    assert!(Arc::ptr_eq(&first, &publish(&mut coordinator, "a")));
    let second = publish(&mut coordinator, "b");
    let third = publish(&mut coordinator, "a");
    assert_eq!(second.epochs.syntax_epoch, 2);
    assert_eq!(third.epochs.syntax_epoch, 3);
    assert_ne!(first.snapshot.snapshot_id, third.snapshot.snapshot_id);
}

#[test]
fn semantic_publication_preserves_pinned_pending_views() {
    let mut coordinator = RefreshCoordinator::default();
    let pending = publish(&mut coordinator, "a");
    let ticket = coordinator.begin_refresh().unwrap();
    let current = coordinator.complete(&ticket, batch(&pending)).unwrap();
    assert_eq!(current.snapshot.snapshot_id, pending.snapshot.snapshot_id);
    assert_eq!(current.epochs.semantic_epoch, 1);
    assert_eq!(current.relations.len(), 1);
    assert_eq!(current.completeness_status(), "within_scope");
    assert_eq!(pending.epochs.semantic_epoch, 0);
    assert!(!pending.pending_units.is_empty());
    assert!(pending.relations.is_empty());
    assert!(Arc::ptr_eq(
        &pending,
        &coordinator.view(&pending.snapshot.snapshot_id, 0).unwrap()
    ));
    let edited = publish(&mut coordinator, "b");
    assert_eq!(
        edited.epochs,
        Epochs {
            syntax_epoch: 2,
            semantic_epoch: 1
        }
    );
    assert!(
        edited.relations.is_empty(),
        "old source coordinates must not leak into the new snapshot"
    );
}

#[test]
fn continuous_edits_reject_old_results_and_keep_the_union_pending() {
    let mut coordinator = RefreshCoordinator::default();
    let initial = publish(&mut coordinator, "a");
    let first_ticket = coordinator.begin_refresh().unwrap();
    let second = publish(&mut coordinator, "b");
    let second_ticket = coordinator.begin_refresh().unwrap();
    assert_eq!(
        coordinator
            .complete(&first_ticket, batch(&initial))
            .unwrap_err(),
        "superseded"
    );
    assert_eq!(coordinator.latest().unwrap(), second);
    assert_eq!(
        coordinator
            .complete(&second_ticket, batch(&second))
            .unwrap()
            .epochs
            .semantic_epoch,
        2
    );
}

#[test]
fn cancelled_refresh_cannot_publish_even_with_matching_inputs() {
    let mut coordinator = RefreshCoordinator::default();
    let pending = publish(&mut coordinator, "a");
    let ticket = coordinator.begin_refresh().unwrap();
    coordinator.cancel_refresh();
    assert_eq!(
        coordinator.complete(&ticket, batch(&pending)).unwrap_err(),
        "superseded"
    );
    assert_eq!(coordinator.latest().unwrap(), pending);
}

#[test]
fn mismatched_or_incomplete_batches_never_publish_partial_semantics() {
    let mut coordinator = RefreshCoordinator::default();
    let pending = publish(&mut coordinator, "a");
    let ticket = coordinator.begin_refresh().unwrap();
    let mut bad = batch(&pending);
    bad.input_hash = "wrong".into();
    assert_eq!(
        coordinator.complete(&ticket, bad).unwrap_err(),
        "content_mismatch"
    );
    let mut bad = batch(&pending);
    bad.resolutions.clear();
    assert_eq!(
        coordinator.complete(&ticket, bad).unwrap_err(),
        "incomplete_refresh"
    );
    let mut bad = batch(&pending);
    bad.resolutions.push(bad.resolutions[0].clone());
    assert_eq!(
        coordinator.complete(&ticket, bad).unwrap_err(),
        "duplicate_callsite"
    );
    assert!(Arc::ptr_eq(&pending, &coordinator.latest().unwrap()));
}

#[test]
fn unresolved_outcomes_finish_the_refresh_but_preserve_analysis_gaps() {
    let mut coordinator = RefreshCoordinator::default();
    let pending = publish(&mut coordinator, "a");
    let ticket = coordinator.begin_refresh().unwrap();
    let mut result = batch(&pending);
    result.resolutions[0].targets.clear();
    let current = coordinator.complete(&ticket, result).unwrap();
    assert!(current.pending_units.is_empty());
    assert_eq!(current.completeness_status(), "partial");
    assert_eq!(current.known_gaps[0].code, "unresolved_call");
}

#[test]
fn rebuilds_cannot_overwrite_a_previously_published_version_pair() {
    let mut coordinator = RefreshCoordinator::default();
    let pending = publish(&mut coordinator, "a");
    let ticket = coordinator.begin_refresh().unwrap();
    let current = coordinator.complete(&ticket, batch(&pending)).unwrap();
    let ticket = coordinator.begin_refresh().unwrap();
    assert!(Arc::ptr_eq(
        &current,
        &coordinator.complete(&ticket, batch(&pending)).unwrap()
    ));
    let mut inconsistent = batch(&pending);
    inconsistent.resolutions[0].targets[0].concrete_function = false;
    assert_eq!(
        coordinator.complete(&ticket, inconsistent).unwrap_err(),
        "inconsistent_semantic_rebuild"
    );
    assert_eq!(coordinator.latest().unwrap(), current);
}

#[test]
fn method_bindings_and_multiple_targets_remain_candidates() {
    let mut coordinator = RefreshCoordinator::default();
    let (manifest, mut syntax) = input("a");
    syntax.calls[0].kind = CallKind::Method;
    let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
    let ticket = coordinator.begin_refresh().unwrap();
    let current = coordinator.complete(&ticket, batch(&pending)).unwrap();
    assert_eq!(current.relations[0].certainty, Certainty::Candidate);
    assert_eq!(current.relations[0].evidence[0].source, "L3");
    assert_eq!(current.relations[0].runtime, "unavailable");

    let mut coordinator = RefreshCoordinator::default();
    let pending = publish(&mut coordinator, "b");
    let ticket = coordinator.begin_refresh().unwrap();
    let mut result = batch(&pending);
    let mut other = result.resolutions[0].targets[0].clone();
    other.symbol_id = "caller".into();
    other.span = pending.snapshot.syntax.symbols[0].name_span;
    result.resolutions[0].targets.push(other);
    assert!(
        coordinator
            .complete(&ticket, result)
            .unwrap()
            .relations
            .iter()
            .all(|r| r.certainty == Certainty::Candidate)
    );
}

#[test]
fn implementation_targets_are_candidates_with_their_own_derivation() {
    let mut coordinator = RefreshCoordinator::default();
    let pending = publish(&mut coordinator, "a");
    let ticket = coordinator.begin_refresh().unwrap();
    let mut result = batch(&pending);
    result.resolutions[0].targets[0].derivation = TargetDerivation::Implementation;
    let current = coordinator.complete(&ticket, result).unwrap();
    assert_eq!(current.relations[0].certainty, Certainty::Candidate);
    assert_eq!(
        current.relations[0].evidence[0].derivation,
        "lsp_implementation+syntax_callsite"
    );
}

#[test]
fn bindings_in_cfg_inactive_code_remain_candidates() {
    let mut coordinator = RefreshCoordinator::default();
    let pending = publish(&mut coordinator, "a");
    let ticket = coordinator.begin_refresh().unwrap();
    let mut result = batch(&pending);
    result.resolutions[0].cfg_inactive = true;
    let current = coordinator.complete(&ticket, result).unwrap();
    assert_eq!(current.relations[0].certainty, Certainty::Candidate);
}

#[test]
fn binary_and_analysis_input_changes_create_new_snapshots() {
    let mut coordinator = RefreshCoordinator::default();
    let initial = publish(&mut coordinator, "a");
    let (mut manifest, syntax) = input("a");
    manifest
        .binary_files
        .insert("resource.bin".into(), digest([255]));
    let resource = coordinator
        .publish_inputs(manifest.clone(), syntax.clone())
        .unwrap();
    manifest
        .analysis_versions
        .insert("semantic".into(), "fixture@2".into());
    let backend = coordinator.publish_inputs(manifest, syntax).unwrap();
    assert_ne!(initial.snapshot.snapshot_id, resource.snapshot.snapshot_id);
    assert_ne!(resource.snapshot.snapshot_id, backend.snapshot.snapshot_id);
}

#[test]
fn invalid_file_hashes_and_source_spans_are_rejected_before_publication() {
    let mut coordinator = RefreshCoordinator::default();
    let (mut manifest, syntax) = input("a");
    manifest.files.get_mut("src/lib.rs").unwrap().file_hash = "forged".into();
    assert!(coordinator.publish_inputs(manifest, syntax).is_err());
    let (manifest, mut syntax) = input("a");
    syntax.calls[0].span.end.column = 999;
    assert!(coordinator.publish_inputs(manifest, syntax).is_err());
    assert!(coordinator.latest().is_none());
}

#[test]
fn stale_target_locations_and_backend_versions_cannot_publish() {
    let mut coordinator = RefreshCoordinator::default();
    let pending = publish(&mut coordinator, "a");
    let ticket = coordinator.begin_refresh().unwrap();
    let mut stale = batch(&pending);
    stale.resolutions[0].targets[0].span.start.column = 1;
    assert_eq!(
        coordinator.complete(&ticket, stale).unwrap_err(),
        "content_mismatch"
    );
    let mut wrong_backend = batch(&pending);
    wrong_backend.backend = "fixture@2".into();
    assert_eq!(
        coordinator.complete(&ticket, wrong_backend).unwrap_err(),
        "content_mismatch"
    );
    assert_eq!(coordinator.latest().unwrap(), pending);
}
