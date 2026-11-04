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
            impls: vec![],
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

#[test]
fn every_web_extension_stays_pending_until_its_own_semantic_publication() {
    for extension in ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"] {
        let path = format!("src/entry.{extension}");
        let mut coordinator = RefreshCoordinator::default();
        let mut manifest = Manifest {
            workspace_id: "web-pending".into(),
            files: BTreeMap::from([(path.clone(), SourceFile::new("export const value = 1;"))]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::from([("semantic".into(), "fixture@1".into())]),
            scope: "fixture".into(),
        };
        for value in [1, 2] {
            manifest.files.insert(
                path.clone(),
                SourceFile::new(format!("export const value = {value};")),
            );
            let pending = coordinator
                .publish_inputs(manifest.clone(), SyntaxFacts::default())
                .unwrap();
            assert_eq!(pending.pending_units, vec![path.clone()], "{extension}");
            assert_ne!(pending.epochs.syntax_epoch, pending.epochs.semantic_epoch);
            let ticket = coordinator.begin_refresh().unwrap();
            let current = coordinator
                .complete(
                    &ticket,
                    SemanticBatch {
                        snapshot_id: pending.snapshot.snapshot_id.clone(),
                        syntax_epoch: pending.epochs.syntax_epoch,
                        input_hash: pending.snapshot.input_hash.clone(),
                        backend: "fixture@1".into(),
                        resolutions: vec![],
                        diagnostics: vec![],
                    },
                )
                .unwrap();
            assert!(current.pending_units.is_empty());
            assert_eq!(current.epochs.syntax_epoch, current.epochs.semantic_epoch);
        }
    }
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
fn scip_go_static_dispatch_requires_concrete_current_go_bindings() {
    for (extension, kind, concrete, inactive, expected) in [
        ("go", "function", true, false, Certainty::Definite),
        ("go", "trait_method", false, false, Certainty::Candidate),
        ("go", "function", true, true, Certainty::Candidate),
        ("ts", "function", true, false, Certainty::Candidate),
        ("rs", "function", true, false, Certainty::Candidate),
    ] {
        let (mut manifest, mut syntax) = input("static");
        let path = format!("src/lib.{extension}");
        let file = manifest.files.remove("src/lib.rs").unwrap();
        manifest.files.insert(path.clone(), file);
        for symbol in &mut syntax.symbols {
            symbol.file = path.clone();
        }
        syntax.symbols[1].kind = kind.into();
        syntax.calls[0].file = path.clone();
        syntax.calls[0].kind = CallKind::Method;
        let mut coordinator = RefreshCoordinator::default();
        let pending = coordinator.publish_inputs(manifest, syntax).unwrap();
        let ticket = coordinator.begin_refresh().unwrap();
        let mut batch = batch(&pending);
        batch.resolutions[0].cfg_inactive = inactive;
        let target = &mut batch.resolutions[0].targets[0];
        target.file = path;
        target.concrete_function = concrete;
        target.derivation = TargetDerivation::ScipGoStaticBinding;
        let current = coordinator.complete(&ticket, batch).unwrap();
        assert_eq!(current.relations[0].certainty, expected);
        assert_eq!(
            current.relations[0].evidence[0].derivation,
            "scip_occurrence+go_static_dispatch+syntax_callsite"
        );
    }
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

fn impl_input() -> (Manifest, SyntaxFacts) {
    let text = "trait T { fn run(&self); }\nstruct S;\nimpl T for S {\n    fn run(&self) {}\n}\n";
    let file = SourceFile::new(text);
    let method_span = Span {
        start: Position { line: 4, column: 5 },
        end: Position {
            line: 4,
            column: 21,
        },
    };
    let syntax = SyntaxFacts {
        symbols: vec![Symbol {
            symbol_id: "method".into(),
            name: "run".into(),
            qualified_name: "src/lib.rs::<T for S>::run".into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            file_hash: file.file_hash.clone(),
            span: method_span,
            name_span: Span {
                start: Position { line: 4, column: 8 },
                end: Position {
                    line: 4,
                    column: 11,
                },
            },
            signature: "fn run(&self)".into(),
        }],
        impls: vec![ImplBlock {
            impl_id: "impl".into(),
            file: "src/lib.rs".into(),
            file_hash: file.file_hash.clone(),
            span: Span {
                start: Position { line: 3, column: 1 },
                end: Position { line: 5, column: 2 },
            },
            trait_path: "T".into(),
            trait_span: Span {
                start: Position { line: 3, column: 6 },
                end: Position { line: 3, column: 7 },
            },
            self_type: "S".into(),
            generics: vec![],
            negative: false,
            unsafe_impl: false,
            test_only: false,
            methods: vec!["method".into()],
        }],
        ..Default::default()
    };
    let manifest = Manifest {
        workspace_id: "workspace".into(),
        files: BTreeMap::from([
            ("src/lib.rs".into(), file.clone()),
            ("src/other.rs".into(), file),
        ]),
        binary_files: BTreeMap::new(),
        analysis_versions: BTreeMap::new(),
        scope: "fixture".into(),
    };
    (manifest, syntax)
}

fn assert_impl_rejected(mutate: impl FnOnce(&mut SyntaxFacts), expected: &str) {
    let (manifest, mut syntax) = impl_input();
    let mut coordinator = RefreshCoordinator::default();
    let valid = coordinator
        .publish_inputs(manifest.clone(), syntax.clone())
        .unwrap();
    mutate(&mut syntax);
    let mut fresh = RefreshCoordinator::default();
    assert_eq!(
        fresh.publish_inputs(manifest, syntax.clone()).unwrap_err(),
        expected
    );
    assert!(fresh.latest().is_none());
    let mut persisted = (*valid).clone();
    Arc::make_mut(&mut persisted.snapshot).syntax = syntax;
    assert_eq!(
        RefreshCoordinator::restore(vec![Arc::new(persisted)])
            .err()
            .as_deref(),
        Some(expected)
    );
}

#[test]
fn impl_facts_publish_and_restore_with_methods_and_marker_blocks() {
    let (manifest, mut syntax) = impl_input();
    let mut marker = syntax.impls[0].clone();
    marker.impl_id = "marker".into();
    marker.methods.clear();
    marker.negative = true;
    marker.unsafe_impl = true;
    syntax.impls.push(marker);
    let mut coordinator = RefreshCoordinator::default();
    let published = coordinator
        .publish_inputs(manifest, syntax.clone())
        .unwrap();
    let restored = RefreshCoordinator::restore(vec![published.clone()]).unwrap();
    assert_eq!(restored.latest().unwrap().snapshot.syntax, syntax);
    assert_eq!(restored.latest().unwrap(), published);
}

#[test]
fn impl_facts_validate_ids_file_hashes_and_spans_before_publication_and_restore() {
    assert_impl_rejected(
        |syntax| syntax.impls.push(syntax.impls[0].clone()),
        "duplicate_impl",
    );
    assert_impl_rejected(
        |syntax| syntax.impls[0].file = "missing.rs".into(),
        "file_not_found",
    );
    assert_impl_rejected(
        |syntax| syntax.impls[0].file_hash = "stale".into(),
        "content_mismatch",
    );
    assert_impl_rejected(
        |syntax| syntax.impls[0].span.end = syntax.impls[0].span.start,
        "invalid_span",
    );
    assert_impl_rejected(
        |syntax| syntax.impls[0].span.end.column = 999,
        "column out of bounds or inside a UTF-8 character",
    );
    assert_impl_rejected(
        |syntax| syntax.impls[0].trait_span.end.column = 1,
        "invalid_span",
    );
    assert_impl_rejected(
        |syntax| syntax.impls[0].trait_span.start.column = 999,
        "invalid_span",
    );
    assert_impl_rejected(
        |syntax| syntax.impls[0].trait_span.start.line = 1,
        "invalid_span",
    );
}

#[test]
fn impl_facts_validate_method_references_before_publication_and_restore() {
    assert_impl_rejected(
        |syntax| syntax.impls[0].methods.push("unknown".into()),
        "unknown_impl_method",
    );
    assert_impl_rejected(
        |syntax| syntax.impls[0].methods.push("method".into()),
        "duplicate_impl_method",
    );
    assert_impl_rejected(
        |syntax| {
            let mut other = syntax.impls[0].clone();
            other.impl_id = "other".into();
            syntax.impls.push(other);
        },
        "duplicate_impl_method",
    );
    assert_impl_rejected(
        |syntax| syntax.symbols[0].kind = "trait_method".into(),
        "invalid_impl_method",
    );
    assert_impl_rejected(
        |syntax| syntax.symbols[0].file = "src/other.rs".into(),
        "invalid_impl_method",
    );
    assert_impl_rejected(
        |syntax| syntax.symbols[0].span.start.line = 1,
        "invalid_impl_method",
    );
    assert_impl_rejected(
        |syntax| syntax.symbols[0].name_span.start.line = 1,
        "invalid_impl_method",
    );
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
