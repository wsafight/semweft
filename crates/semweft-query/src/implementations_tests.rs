use super::find;
use crate::{engine::StoredView, query};
use semweft_core::*;
use semweft_lang_rust::RustParser;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const TRAITS: &str = r#"
pub trait Paint {
    fn run(&self);
    fn label(&self) -> u8 { 1 }
    fn probe(&self) {}
}
mod rival { pub trait Paint { fn run(&self); } }
pub struct Item;
impl Paint for Item { fn run(&self) {} }
pub fn exercise(item: &impl Paint) { item.run(); item.run(); }
"#;

fn fixture(files: &[(&str, &str)]) -> StoredView {
    let manifest = Manifest {
        workspace_id: "implementations-semantic-tests".into(),
        files: files
            .iter()
            .map(|(path, text)| (path.to_string(), SourceFile::new(*text)))
            .collect(),
        binary_files: BTreeMap::new(),
        analysis_versions: BTreeMap::new(),
        scope: "test".into(),
    };
    let syntax = RustParser::new().unwrap().analyze(&manifest).unwrap();
    let published = RefreshCoordinator::default()
        .publish_inputs(manifest, syntax)
        .unwrap();
    StoredView::new(published, vec![], vec![])
}

fn with_relations(stored: &StoredView, relations: Vec<Relation>) -> StoredView {
    StoredView::new(stored.view.clone(), relations, vec![])
}

fn symbol<'a>(stored: &'a StoredView, local: &str) -> &'a Symbol {
    stored
        .view
        .snapshot
        .syntax
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == format!("src/lib.rs::{local}"))
        .unwrap_or_else(|| panic!("missing symbol {local}"))
}

fn execute(stored: &StoredView, local: &str, mut args: Value) -> Value {
    args["symbol"] = json!(symbol(stored, local).symbol_id);
    let mut result = query::envelope(stored, json!({}));
    find(
        &mut result,
        stored,
        &args,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    result
}

fn evidence(
    stored: &StoredView,
    file: &str,
    id: &str,
    derivation: &str,
    certainty: Certainty,
) -> Evidence {
    Evidence {
        evidence_id: id.into(),
        source: "L3".into(),
        certainty,
        indexer: if derivation.starts_with("SCIP") {
            "scip-test"
        } else {
            "rust-analyzer-test"
        }
        .into(),
        derivation: derivation.into(),
        file_hash: stored.view.snapshot.manifest.files[file]
            .file_hash
            .as_str()
            .into(),
    }
}

fn header(
    stored: &StoredView,
    block: &ImplBlock,
    target: &Symbol,
    certainty: Certainty,
) -> Relation {
    let derivation = if certainty == Certainty::Definite {
        "SCIP occurrence aligned to captured syntax"
    } else {
        "lsp_reference+syntax_occurrence; binding does not prove runtime dispatch or access behavior"
    };
    let relation_id = format!("header-{}-{}", block.impl_id, target.symbol_id);
    Relation {
        relation_id: relation_id.clone(),
        kind: "type".into(),
        from_symbol: format!("file:{}", block.file),
        to_symbol: target.symbol_id.clone(),
        file: block.file.clone(),
        span: block.trait_span,
        certainty: certainty.clone(),
        freshness: "current".into(),
        runtime: "unavailable".into(),
        evidence: vec![evidence(
            stored,
            &block.file,
            &relation_id,
            derivation,
            certainty,
        )],
        origin: None,
    }
}

fn call_relation(
    stored: &StoredView,
    call: &CallSite,
    target: &Symbol,
    derivation: &str,
    source: &str,
) -> Relation {
    let relation_id = format!("call-{}-{}", call.callsite_id, target.symbol_id);
    let mut binding = evidence(
        stored,
        &call.file,
        &relation_id,
        derivation,
        Certainty::Candidate,
    );
    binding.source = source.into();
    Relation {
        relation_id,
        kind: "call".into(),
        from_symbol: call
            .caller_id
            .clone()
            .unwrap_or_else(|| format!("file:{}", call.file)),
        to_symbol: target.symbol_id.clone(),
        file: call.file.clone(),
        span: call.span,
        certainty: Certainty::Candidate,
        freshness: "current".into(),
        runtime: "unavailable".into(),
        evidence: vec![binding],
        origin: None,
    }
}

fn has_gap(result: &Value, kind: &str) -> bool {
    result["completeness"]["known_gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|gap| gap["kind"] == kind)
}

#[test]
fn definite_competing_trait_excludes_with_filtered_explanation() {
    let plain = fixture(&[("src/lib.rs", TRAITS)]);
    let block = &plain.view.snapshot.syntax.impls[0];
    let rival = symbol(&plain, "rival::Paint");
    let stored = with_relations(
        &plain,
        vec![header(&plain, block, rival, Certainty::Definite)],
    );
    let result = execute(&stored, "Paint", json!({}));
    assert_eq!(result["relations"], json!([]));
    assert_eq!(result["implementation_counts"]["total"], 0);
    assert_eq!(result["implementation_counts"]["excluded"], 1);
    let excluded = &result["excluded_candidates"][0];
    assert_eq!(excluded["bound_to"], rival.symbol_id);
    assert_eq!(excluded["certainty"], "definite");
    assert!(
        excluded["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["source"] == "L3")
    );
    for filters in [
        json!({"source":["L1"]}),
        json!({"certainty":["candidate"]}),
        json!({"freshness":["pending"]}),
        json!({"path_prefix":"tests/"}),
    ] {
        let filtered = execute(&stored, "Paint", filters.clone());
        assert_eq!(filtered["excluded_candidates"], json!([]), "{filters}");
        assert_eq!(
            filtered["implementation_counts"]["excluded"], 0,
            "{filters}"
        );
    }
    let semantic = execute(
        &stored,
        "Paint",
        json!({"source":["L3"],"certainty":["definite"],"freshness":["current"]}),
    );
    assert_eq!(semantic["implementation_counts"]["excluded"], 1);
}

#[test]
fn candidate_competitor_keeps_name_candidate_and_target_lsp_stays_candidate() {
    let plain = fixture(&[("src/lib.rs", TRAITS)]);
    let block = &plain.view.snapshot.syntax.impls[0];
    let competing = header(
        &plain,
        block,
        symbol(&plain, "rival::Paint"),
        Certainty::Candidate,
    );
    let stored = with_relations(&plain, vec![competing.clone()]);
    let result = execute(&stored, "Paint", json!({}));
    assert_eq!(result["implementation_counts"]["total"], 1);
    assert_eq!(result["implementation_counts"]["excluded"], 0);
    assert_eq!(result["relations"][0]["certainty"], "candidate");
    assert_eq!(result["implementation_counts"]["with_semantic_evidence"], 0);
    assert!(has_gap(&result, "implementation_semantic_evidence_missing"));
    assert_eq!(
        execute(&stored, "Paint", json!({"source":["L3"]}))["relations"],
        json!([])
    );
    let target = header(&plain, block, symbol(&plain, "Paint"), Certainty::Candidate);
    let both = with_relations(&plain, vec![competing, target]);
    let semantic = execute(
        &both,
        "Paint",
        json!({"source":["L3"],"certainty":["candidate"]}),
    );
    assert_eq!(semantic["implementation_counts"]["total"], 1);
    assert_eq!(semantic["relations"][0]["certainty"], "candidate");
    assert_eq!(
        execute(
            &both,
            "Paint",
            json!({"source":["L3"],"certainty":["definite"]})
        )["relations"],
        json!([])
    );
}

#[test]
fn conflicting_definite_headers_remain_candidate_after_source_and_certainty_filters() {
    let plain = fixture(&[("src/lib.rs", TRAITS)]);
    let block = &plain.view.snapshot.syntax.impls[0];
    let stored = with_relations(
        &plain,
        vec![
            header(&plain, block, symbol(&plain, "Paint"), Certainty::Definite),
            header(
                &plain,
                block,
                symbol(&plain, "rival::Paint"),
                Certainty::Definite,
            ),
        ],
    );
    for filters in [
        json!({}),
        json!({"source":["L3"]}),
        json!({"source":["L1","L3"],"certainty":["candidate"]}),
        json!({"source":["L3"],"certainty":["candidate"]}),
    ] {
        let result = execute(&stored, "Paint", filters.clone());
        assert_eq!(result["implementation_counts"]["total"], 1, "{filters}");
        assert_eq!(result["implementation_counts"]["excluded"], 0, "{filters}");
        assert_eq!(
            result["relations"][0]["certainty"], "candidate",
            "{filters}"
        );
        assert!(
            has_gap(&result, "impl_header_binding_ambiguous"),
            "{filters}"
        );
        assert!(
            result["relations"][0]["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["source"] == "L3" && item["certainty"] == "definite")
        );
    }
    for filters in [
        json!({"certainty":["definite"]}),
        json!({"source":["L3"],"certainty":["definite"]}),
    ] {
        assert_eq!(execute(&stored, "Paint", filters)["relations"], json!([]));
    }
}

#[test]
fn semantic_alias_has_no_fabricated_l1_name_binding() {
    let plain = fixture(&[(
        "src/lib.rs",
        "pub trait Paint { fn run(&self); }\nuse Paint as Alias;\nstruct Item;\nimpl Alias for Item { fn run(&self) {} }\n",
    )]);
    let block = &plain.view.snapshot.syntax.impls[0];
    assert_eq!(block.trait_path, "Alias");
    let stored = with_relations(
        &plain,
        vec![header(
            &plain,
            block,
            symbol(&plain, "Paint"),
            Certainty::Definite,
        )],
    );
    for target in ["Paint", "Paint::run"] {
        let result = execute(&stored, target, json!({}));
        assert_eq!(result["implementation_counts"]["total"], 1);
        assert_eq!(result["relations"][0]["certainty"], "definite");
        assert!(
            result["relations"][0]["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["source"] == "L3")
        );
        assert_eq!(
            execute(&stored, target, json!({"source":["L1"]}))["relations"],
            json!([])
        );
        assert_eq!(
            execute(
                &stored,
                target,
                json!({"source":["L3"],"certainty":["definite"]})
            )["implementation_counts"]["total"],
            1
        );
        assert_eq!(execute(&plain, target, json!({}))["relations"], json!([]));
    }
}

#[test]
fn header_binding_rejects_wrong_kind_hash_origin_span_and_stale_coordinates() {
    let plain = fixture(&[("src/lib.rs", TRAITS)]);
    let block = &plain.view.snapshot.syntax.impls[0];
    for defect in ["kind", "hash", "origin", "span", "stale", "source"] {
        for target in ["Paint", "rival::Paint"] {
            let mut binding = header(&plain, block, symbol(&plain, target), Certainty::Definite);
            match defect {
                "kind" => binding.kind = "call".into(),
                "hash" => binding.evidence[0].file_hash = "different-file-hash".into(),
                "origin" => {
                    binding.origin = Some(RelationOrigin {
                        snapshot_id: "old-snapshot".into(),
                        epochs: plain.view.epochs.clone(),
                        file_hash: block.file_hash.clone(),
                        reason: "historical coordinates".into(),
                    })
                }
                "span" => binding.span = block.span,
                "stale" => binding.freshness = "stale".into(),
                "source" => binding.evidence[0].source = "L2".into(),
                _ => unreachable!(),
            }
            let stored = with_relations(&plain, vec![binding]);
            let result = execute(&stored, "Paint", json!({}));
            assert_eq!(
                result["implementation_counts"]["total"], 1,
                "{defect}: {target}"
            );
            assert_eq!(
                result["implementation_counts"]["excluded"], 0,
                "{defect}: {target}"
            );
            assert_eq!(
                result["implementation_counts"]["with_semantic_evidence"], 0,
                "{defect}: {target}"
            );
            assert_eq!(
                result["relations"][0]["certainty"], "candidate",
                "{defect}: {target}"
            );
            assert_eq!(
                execute(&stored, "Paint", json!({"source":["L3"]}))["relations"],
                json!([]),
                "{defect}: {target}"
            );
        }
    }
}

#[test]
fn dispatch_requires_semantic_trait_method_at_the_same_callsite() {
    let plain = fixture(&[("src/lib.rs", TRAITS)]);
    let method = plain
        .index()
        .symbol(&plain.view.snapshot.syntax.impls[0].methods[0])
        .unwrap();
    let calls: Vec<_> = plain
        .view
        .snapshot
        .syntax
        .calls
        .iter()
        .filter(|call| call.expression.contains("run"))
        .collect();
    assert_eq!(calls.len(), 2);
    let implementation = call_relation(
        &plain,
        calls[0],
        method,
        "lsp_implementation+syntax_callsite",
        "L3",
    );
    let trait_method = symbol(&plain, "Paint::run");
    let cases = [
        vec![implementation.clone()],
        vec![
            implementation.clone(),
            call_relation(
                &plain,
                calls[0],
                trait_method,
                "syntax_callsite+name_match",
                "L1",
            ),
        ],
        vec![
            implementation.clone(),
            call_relation(
                &plain,
                calls[1],
                trait_method,
                "lsp_definition+syntax_callsite",
                "L3",
            ),
        ],
    ];
    for relations in cases {
        let stored = with_relations(&plain, relations);
        let result = execute(&stored, "Paint::run", json!({}));
        assert_eq!(result["implementation_counts"]["total"], 1);
        assert_eq!(result["implementation_counts"]["with_semantic_evidence"], 0);
        assert!(
            result["relations"][0]["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["source"] == "L1")
        );
        assert_eq!(
            execute(&stored, "Paint::run", json!({"source":["L3"]}))["relations"],
            json!([])
        );
    }
}

#[test]
fn semantic_dispatch_keeps_only_implementation_evidence_and_candidate_certainty() {
    let plain = fixture(&[("src/lib.rs", TRAITS)]);
    let method = plain
        .index()
        .symbol(&plain.view.snapshot.syntax.impls[0].methods[0])
        .unwrap();
    let call = plain
        .view
        .snapshot
        .syntax
        .calls
        .iter()
        .find(|call| call.expression.contains("run"))
        .unwrap();
    let mut implementation = call_relation(
        &plain,
        call,
        method,
        "lsp_implementation+syntax_callsite",
        "L3",
    );
    let mut unrelated = evidence(
        &plain,
        &call.file,
        "unrelated-l2",
        "unrelated_definite_l2",
        Certainty::Definite,
    );
    unrelated.source = "L2".into();
    implementation.evidence.push(unrelated);
    implementation.evidence.push(evidence(
        &plain,
        &call.file,
        "unrelated-l3",
        "unrelated_definite_l3",
        Certainty::Definite,
    ));
    implementation.certainty = Certainty::Definite;
    let trait_binding = call_relation(
        &plain,
        call,
        symbol(&plain, "Paint::run"),
        "lsp_definition+syntax_callsite",
        "L3",
    );
    let stored = with_relations(&plain, vec![implementation.clone(), trait_binding.clone()]);
    let result = execute(
        &stored,
        "Paint::run",
        json!({"source":["L3"],"certainty":["candidate"]}),
    );
    assert_eq!(result["implementation_counts"]["total"], 1);
    assert_eq!(result["implementation_counts"]["with_semantic_evidence"], 1);
    let relation = &result["relations"][0];
    assert_eq!(relation["certainty"], "candidate");
    let evidence = relation["evidence"].as_array().unwrap();
    assert!(evidence.iter().any(|item| {
        item["derivation"]
            .as_str()
            .unwrap()
            .starts_with("impl_dispatch:")
    }));
    assert!(evidence.iter().all(|item| {
        item["source"] == "L3"
            && item["certainty"] == "candidate"
            && !item["derivation"]
                .as_str()
                .unwrap()
                .contains("unrelated_definite")
    }));
    assert_eq!(
        execute(
            &stored,
            "Paint::run",
            json!({"source":["L3"],"certainty":["definite"]})
        )["relations"],
        json!([])
    );
    assert_eq!(
        execute(&stored, "Paint::run", json!({"source":["L2"]}))["relations"],
        json!([])
    );
    let mut pending_binding = trait_binding.clone();
    pending_binding.freshness = "pending".into();
    let pending = with_relations(&plain, vec![implementation.clone(), pending_binding]);
    assert_eq!(
        (
            execute(&pending, "Paint::run", json!({"source":["L3"]}))["relations"][0]["freshness"]
                .clone(),
            execute(&pending, "Paint::run", json!({"source":["L1"]}))["relations"][0]["freshness"]
                .clone(),
        ),
        (json!("pending"), json!("current"))
    );
    let mut other_indexer_binding = trait_binding;
    other_indexer_binding.evidence[0].indexer = "different-semantic-backend".into();
    let other_indexer = with_relations(&plain, vec![implementation, other_indexer_binding]);
    assert_eq!(
        execute(&other_indexer, "Paint::run", json!({"source":["L3"]}))["relations"],
        json!([])
    );
}

#[test]
fn defaults_obey_filters_and_test_method_does_not_reclassify_production_impl() {
    let plain = fixture(&[
        (
            "src/lib.rs",
            "pub trait Paint { fn label(&self) -> u8 { 1 } fn probe(&self) {} }\nstruct Item;\nimpl Paint for Item { #[cfg(test)] fn probe(&self) {} }\n",
        ),
        (
            "src/other.rs",
            "struct Other; impl crate::Paint for Other {}\n",
        ),
        ("tests/smoke.rs", "struct Fake; impl Paint for Fake {}\n"),
    ]);
    let block = plain
        .view
        .snapshot
        .syntax
        .impls
        .iter()
        .find(|block| block.file == "src/lib.rs")
        .unwrap();
    let stored = with_relations(
        &plain,
        vec![header(
            &plain,
            block,
            symbol(&plain, "Paint"),
            Certainty::Definite,
        )],
    );
    let traits = execute(&stored, "Paint", json!({}));
    assert_eq!(traits["implementation_counts"]["total"], 3);
    assert_eq!(traits["implementation_counts"]["production"], 2);
    assert_eq!(traits["implementation_counts"]["test"], 1);
    let production = traits["relations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|relation| relation["implementation"]["self_type"] == "Item")
        .unwrap();
    assert_eq!(production["scope"], "production");
    let methods = execute(&stored, "Paint::probe", json!({}));
    assert_eq!(methods["implementation_counts"]["total"], 1);
    assert_eq!(methods["implementation_counts"]["production"], 0);
    assert_eq!(methods["implementation_counts"]["test"], 1);
    assert_eq!(methods["relations"][0]["scope"], "test");
    let all = execute(&stored, "Paint::label", json!({}));
    assert_eq!(all["relations"], json!([]));
    assert_eq!(all["implementation_counts"]["total"], 0);
    assert_eq!(all["implementation_counts"]["inherited_defaults"], 3);
    assert!(
        all["inherited_defaults"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["evidence"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
                && item["certainty"].is_string())
    );
    for (filters, expected) in [
        (json!({"source":["L3"]}), 1),
        (json!({"source":["L3"],"certainty":["definite"]}), 1),
        (json!({"source":["L1"],"certainty":["definite"]}), 0),
        (json!({"source":["L1"],"certainty":["candidate"]}), 3),
        (json!({"path_prefix":"tests/"}), 1),
        (json!({"path_prefix":"src/other.rs"}), 1),
        (json!({"freshness":["pending"]}), 0),
    ] {
        let filtered = execute(&stored, "Paint::label", filters.clone());
        assert_eq!(
            filtered["implementation_counts"]["inherited_defaults"], expected,
            "{filters}"
        );
        assert_eq!(
            filtered["inherited_defaults"].as_array().unwrap().len(),
            expected as usize,
            "{filters}"
        );
        assert_eq!(filtered["implementation_counts"]["total"], 0, "{filters}");
        assert_eq!(filtered["relations"], json!([]), "{filters}");
    }
    let semantic = execute(&stored, "Paint::label", json!({"source":["L3"]}));
    assert_eq!(semantic["inherited_defaults"][0]["scope"], "production");
    assert_eq!(semantic["inherited_defaults"][0]["certainty"], "definite");
    assert!(
        semantic["inherited_defaults"][0]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["source"] == "L3")
    );
}
