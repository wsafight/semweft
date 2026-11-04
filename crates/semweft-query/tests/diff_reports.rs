use semweft_query::{Engine, QueryService, config, query};
use serde_json::{Value, json};
use std::{
    fs,
    time::{Duration, Instant},
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

#[test]
fn diff_reports_preserve_module_evidence_risk_signals_and_fixed_pages() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir(root.join("src")).unwrap();
    let manifest = "[package]\nname=\"report\"\nversion=\"0.1.0\"\n";
    fs::write(root.join("Cargo.toml"), manifest).unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub mod domain;\npub mod service;\npub mod untouched;\n",
    )
    .unwrap();
    fs::write(root.join("src/domain.rs"), "pub fn stock() -> i32 { 1 }\n").unwrap();
    fs::write(
        root.join("src/service.rs"),
        "pub fn order() -> i32 { stock() }\n",
    )
    .unwrap();
    fs::write(root.join("src/untouched.rs"), "pub fn other() {}\n").unwrap();
    let engine = Engine::new(config::resolve(Some(&root), None).unwrap()).unwrap();
    let base = engine.scan(false).unwrap();
    fs::write(
        root.join("src/domain.rs"),
        "pub fn inventory() -> i32 { 1 }\nfn backup() { inventory(); }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/service.rs"),
        "pub fn order() -> i32 { stock() }\npub fn audit() { inventory(); }\n",
    )
    .unwrap();
    fs::write(root.join("Cargo.toml"), manifest.replace("0.1.0", "0.2.0")).unwrap();
    let head = engine.scan(false).unwrap();
    let service = QueryService::new(engine.clone());
    let diff = json!({"base":{"kind":"snapshot","snapshot_id":base.snapshot.snapshot_id,"semantic_epoch":0},
        "head":{"kind":"snapshot","snapshot_id":head.snapshot.snapshot_id,"semantic_epoch":0}});
    let result = service
        .execute("impact_of", json!({"diff":diff,"limit":1000}), deadline())
        .unwrap();
    assert_eq!(
        result["diff_summary"]["change_kinds"]["renamed"], 1,
        "{result}"
    );
    assert_eq!(result["diff_summary"]["configuration_changes"], 1);
    assert_eq!(result["diff_summary"]["residual_references"], 1);
    assert_eq!(result["diff_summary"]["module_aggregation_complete"], true);
    assert_eq!(result["diff_summary"]["affected_modules_observed"], 4);
    let modules = result["affected_modules"].as_array().unwrap();
    assert_eq!(modules.len(), 4);
    for module in modules {
        let side = module["side"].as_str().unwrap();
        assert_eq!(
            module["snapshot_id"],
            result["snapshots"][side]["snapshot_id"]
        );
        assert_eq!(module["crate"], "report");
        assert!(!module["affected_symbol_ids"].as_array().unwrap().is_empty());
        assert_ne!(module["path"], "src/untouched.rs");
    }
    let residual_module = modules
        .iter()
        .find(|module| module["path"] == "src/service.rs" && module["side"] == "head")
        .unwrap();
    assert_eq!(residual_module["residual_reference_count"], 1);
    assert_eq!(result["residual_references"][0]["side"], "head");
    assert_eq!(
        result["residual_references"][0]["location"]["coordinate_snapshot_id"],
        head.snapshot.snapshot_id
    );
    assert!(result["relation_changes"]["added_count"].as_u64().unwrap() > 0);
    assert!(
        result["relation_changes"]["removed_count"]
            .as_u64()
            .unwrap()
            > 0
    );
    for (key, side) in [("added", "head"), ("removed", "base")] {
        for relation in result["relation_changes"][key].as_array().unwrap() {
            assert_eq!(relation["side"], side);
        }
    }
    assert!(result["risk_summary"]["score"].as_u64().is_some());
    assert_eq!(result["summary"]["risk"], result["risk_summary"]["score"]);
    assert_eq!(result["risk_summary"]["scoring"], "heuristic_v2");
    assert!(result["risk_summary"]["score"].as_u64().unwrap() <= 100);
    assert_eq!(
        result["risk_summary"]["score_basis"],
        "review_priority_not_probability"
    );
    let dimensions = &result["risk_summary"]["dimensions"];
    assert!(dimensions["impact_scope"]["score"].as_u64().unwrap() > 0);
    assert!(dimensions["public_api"]["score"].as_u64().unwrap() > 0);
    assert_eq!(
        result["risk_summary"]["score"].as_u64().unwrap(),
        dimensions["impact_scope"]["score"].as_u64().unwrap()
            + dimensions["public_api"]["score"].as_u64().unwrap()
            + dimensions["call_impact"]["score"].as_u64().unwrap()
            + dimensions["test_coverage"]["score"].as_u64().unwrap()
            + dimensions["hotspot"]["score"].as_u64().unwrap()
            + dimensions["evidence_gaps"]["score"].as_u64().unwrap()
    );
    assert_eq!(
        result["risk_summary"]["dimensions"]["test_coverage"]["status"],
        "unavailable"
    );
    assert_eq!(
        result["risk_summary"]["dimensions"]["hotspot"]["status"],
        "unavailable"
    );
    assert_eq!(
        result["risk_summary"]["dimensions"]["test_coverage"]["executed_coverage"],
        Value::Null
    );
    for kind in [
        "public_declaration_changes",
        "residual_references",
        "configuration_changes",
        "indexed_impact_relations",
        "analysis_gaps",
    ] {
        assert!(
            result["risk_summary"]["signals"]
                .as_array()
                .unwrap()
                .iter()
                .any(|signal| signal["kind"] == kind),
            "{kind}"
        );
    }
    for tool in ["impact_of", "verify_change", "suggest_tests"] {
        let first = service
            .execute(tool, json!({"diff":diff,"limit":1}), deadline())
            .unwrap();
        fs::write(root.join("src/domain.rs"), "pub fn unrelated() {}\n").unwrap();
        engine.scan(false).unwrap();
        let mut page = first.clone();
        let mut paged_modules = vec![];
        loop {
            assert_eq!(page["snapshots"], result["snapshots"]);
            assert_eq!(page["diff_summary"], result["diff_summary"]);
            assert_eq!(page["risk_summary"], result["risk_summary"]);
            assert_eq!(page["output"]["returned_items"], 1);
            paged_modules.extend(page["affected_modules"].as_array().unwrap().iter().cloned());
            let Some(cursor) = page["output"]["continuation"]["next_cursor"].as_str() else {
                break;
            };
            page = service
                .execute(tool, json!({"cursor":cursor}), deadline())
                .unwrap();
        }
        assert_eq!(paged_modules, *modules, "{tool}");
    }
    let bounded = service
        .execute(
            "impact_of",
            json!({"diff":diff,"max_nodes":1,"source":["L1"]}),
            deadline(),
        )
        .unwrap();
    assert_eq!(bounded["output"]["truncation_reason"], "node_limit");
    assert_eq!(bounded["diff_summary"]["analysis_basis"], "partial");
    assert!(
        bounded["risk_summary"]["signals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|signal| signal["kind"] == "truncated_analysis")
    );
    assert!(
        bounded["risk_summary"]["dimensions"]["evidence_gaps"]["score"]
            .as_u64()
            .unwrap()
            >= 10
    );
    let suggested = &bounded["output"]["continuation"]["suggested"];
    query::validate("impact_of", suggested).unwrap();
    assert_eq!(suggested["diff"], diff);
    assert_eq!(suggested["source"], json!(["L1"]));
    let empty = service
        .execute(
            "impact_of",
            json!({"diff":{"base":diff["head"],"head":diff["head"]}}),
            deadline(),
        )
        .unwrap();
    assert_eq!(empty["diff_summary"]["changed_symbols"], 0);
    assert_eq!(empty["affected_modules"], json!([]));
    assert_eq!(empty["risk_summary"]["score"], Value::Null);
    engine.shutdown();
}
