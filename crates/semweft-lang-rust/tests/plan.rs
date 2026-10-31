//! Refresh planning over real syntax facts, with a scripted semantic backend.
use semweft_core::*;
use semweft_lang_rust::RustParser;
use std::collections::{BTreeMap, BTreeSet};

const LIB: &str =
    "pub mod other;\npub fn target() -> u32 {\n    1\n}\npub fn caller() -> u32 { target() }\n";
const OTHER: &str = "pub fn remote() -> u32 { crate::target() }\n";

fn manifest(lib: &str, other: &str, cargo: &str) -> Manifest {
    Manifest {
        workspace_id: "ws".into(),
        files: BTreeMap::from([
            ("Cargo.toml".into(), SourceFile::new(cargo)),
            ("src/lib.rs".into(), SourceFile::new(lib)),
            ("src/other.rs".into(), SourceFile::new(other)),
        ]),
        binary_files: BTreeMap::new(),
        analysis_versions: BTreeMap::new(),
        scope: "test".into(),
    }
}

/// Resolves every call by its final path segment, like an ideal backend.
fn resolve(view: &AnalysisView, only: &BTreeSet<String>) -> Vec<CallResolution> {
    let syntax = &view.snapshot.syntax;
    syntax
        .calls
        .iter()
        .filter(|c| only.contains(&c.callsite_id))
        .map(|call| {
            let name = call.expression.rsplit("::").next().unwrap();
            CallResolution {
                callsite_id: call.callsite_id.clone(),
                targets: syntax
                    .symbols
                    .iter()
                    .filter(|s| s.name == name)
                    .map(|s| Target {
                        symbol_id: s.symbol_id.clone(),
                        file: s.file.clone(),
                        span: s.name_span,
                        concrete_function: true,
                        derivation: TargetDerivation::Definition,
                    })
                    .collect(),
                diagnostics: vec![],
                cfg_inactive: false,
            }
        })
        .collect()
}

struct Harness {
    parser: RustParser,
    coordinator: RefreshCoordinator,
}

impl Harness {
    fn new() -> Self {
        Self {
            parser: RustParser::new().unwrap(),
            coordinator: RefreshCoordinator::default(),
        }
    }

    fn edit(&mut self, manifest: Manifest) -> RefreshPlan {
        let syntax = self.parser.analyze(&manifest).unwrap();
        self.coordinator.publish_inputs(manifest, syntax).unwrap();
        self.coordinator.plan_refresh().unwrap()
    }

    fn complete(&mut self, plan: RefreshPlan) -> std::sync::Arc<AnalysisView> {
        let ticket = self.coordinator.begin_refresh().unwrap();
        let view = self.coordinator.latest().unwrap();
        let mut resolutions = resolve(&view, &plan.query);
        resolutions.extend(plan.reused);
        let batch = SemanticBatch {
            snapshot_id: view.snapshot.snapshot_id.clone(),
            syntax_epoch: view.epochs.syntax_epoch,
            input_hash: view.snapshot.input_hash.clone(),
            backend: "scripted".into(),
            resolutions,
            diagnostics: vec![],
        };
        self.coordinator.complete(&ticket, batch).unwrap()
    }
}

/// Cold analysis of the same manifest for comparison.
fn cold(manifest: Manifest) -> std::sync::Arc<AnalysisView> {
    let mut harness = Harness::new();
    let plan = harness.edit(manifest);
    harness.complete(plan)
}

fn relation_ids(view: &AnalysisView) -> Vec<(String, String)> {
    view.relations
        .iter()
        .map(|r| (r.relation_id.clone(), r.to_symbol.clone()))
        .collect()
}

#[test]
fn body_edits_requery_only_changed_files_and_remap_moved_targets() {
    let mut harness = Harness::new();
    let plan = harness.edit(manifest(LIB, OTHER, "a"));
    assert!(matches!(plan.scope, RefreshScope::Full { .. }));
    harness.complete(plan);

    // The body grows by two lines, so `caller` and its callsite move.
    let lib = LIB.replace("    1\n", "    let x = 1;\n    let y = x;\n    y\n");
    let plan = harness.edit(manifest(&lib, OTHER, "a"));
    assert_eq!(
        plan.scope,
        RefreshScope::BodyLocal {
            changed_files: vec!["src/lib.rs".into()]
        }
    );
    assert_eq!(plan.query.len(), 1, "only the call inside src/lib.rs");
    assert_eq!(plan.reused.len(), 1, "src/other.rs keeps its outcome");
    let current = harness.complete(plan);
    assert_eq!(
        relation_ids(&current),
        relation_ids(&cold(manifest(&lib, OTHER, "a")))
    );
}

#[test]
fn structural_and_build_changes_fall_back_to_a_full_refresh() {
    let mut harness = Harness::new();
    let plan = harness.edit(manifest(LIB, OTHER, "a"));
    harness.complete(plan);
    let reason = |plan: RefreshPlan| match plan.scope {
        RefreshScope::Full { reason } => reason,
        other => panic!("expected full refresh, got {other:?}"),
    };
    let signature = LIB.replace("target() -> u32", "target(x: u8) -> u32");
    assert_eq!(
        reason(harness.edit(manifest(&signature, OTHER, "a"))),
        "item_skeleton_changed: src/lib.rs"
    );
    assert_eq!(
        reason(harness.edit(manifest(LIB, OTHER, "b"))),
        "build_or_configuration_input_changed"
    );
}

#[test]
fn superseded_edits_stay_in_the_next_plan() {
    let mut harness = Harness::new();
    let plan = harness.edit(manifest(LIB, OTHER, "a"));
    harness.complete(plan);
    let lib = LIB.replace("    1\n", "    2\n");
    // This refresh is never completed; the next plan must still cover it.
    harness.edit(manifest(&lib, OTHER, "a"));
    let other = OTHER.replace("crate::target()", "crate::target() + 1");
    let plan = harness.edit(manifest(&lib, &other, "a"));
    assert_eq!(plan.base_syntax_epoch, Some(1));
    assert_eq!(
        plan.scope,
        RefreshScope::BodyLocal {
            changed_files: vec!["src/lib.rs".into(), "src/other.rs".into()]
        }
    );
    let current = harness.complete(plan);
    assert_eq!(current.epochs.semantic_epoch, 3);
    assert_eq!(
        relation_ids(&current),
        relation_ids(&cold(manifest(&lib, &other, "a")))
    );
}
