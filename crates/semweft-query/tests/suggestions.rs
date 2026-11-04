use semweft_query::{Engine, QueryService, config, query};
use serde_json::{Value, json};
use std::{
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

const SOURCE: &str = r#"pub fn target() {}
pub fn helper() { target(); }
pub fn unrelated() {}
#[cfg(test)]
mod tests {
    fn support() { helper(); }
    #[test]
    fn direct() { target(); }
    #[test]
    fn indirect() { support(); }
    #[test]
    fn reference() { let callback = target; callback(); }
    #[test]
    #[ignore = "requires a service"]
    fn ignored_case() { target(); }
    #[tokio::test(flavor = "current_thread")]
    async fn async_tokio() { target(); }
    #[async_std::test]
    async fn async_std_case() { target(); }
    #[test]
    fn other() { unrelated(); }
}
"#;

fn setup(source: &str) -> (tempfile::TempDir, Arc<Engine>, QueryService) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"suggestions\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), source).unwrap();
    let engine = Engine::new(config::resolve(Some(&root), None).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    (temp, engine, service)
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn candidate<'a>(result: &'a Value, name: &str) -> &'a Value {
    result["test_candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["test"]["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}: {result}"))
}

#[test]
fn tests_follow_call_and_reference_paths_and_require_explicit_entry_attributes() {
    let (_temp, engine, service) = setup(SOURCE);
    let result = service
        .execute(
            "suggest_tests",
            json!({"symbol":"target","depth":5,"limit":1000}),
            deadline(),
        )
        .unwrap();
    assert_eq!(result["test_summary"]["candidate_count"], 6, "{result}");
    assert_eq!(result["test_summary"]["head_nonignored_candidates"], 5);
    assert_eq!(result["summary"]["tests_to_run"], 5);
    assert_eq!(result["test_summary"]["traversal_complete"], true);
    assert_eq!(result["test_summary"]["coverage"], "unavailable");
    assert_eq!(result["completeness"]["status"], "partial");
    assert_eq!(candidate(&result, "ignored_case")["ignored"], true);
    assert_eq!(
        candidate(&result, "indirect")["witnesses"][0]["path"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        candidate(&result, "reference")["witnesses"][0]["reason"],
        "reference_path"
    );
    for test in result["test_candidates"].as_array().unwrap() {
        assert_eq!(test["certainty"], "candidate");
        assert_eq!(test["executed"], false);
        assert_eq!(test["test_snapshot"]["snapshot_id"], result["snapshot_id"]);
        for witness in test["witnesses"].as_array().unwrap() {
            let path = witness["path"].as_array().unwrap();
            assert_eq!(path[0]["from"], test["test"]["symbol_id"]);
            assert_eq!(path.last().unwrap()["to"], result["target"]["symbol_id"]);
            for pair in path.windows(2) {
                assert_eq!(pair[0]["to"], pair[1]["from"]);
            }
            for edge in path {
                assert_eq!(edge["evidence"][0]["source"], "L1");
                assert!(edge["location"]["line"].as_u64().is_some());
            }
        }
    }
    let selected = service
        .execute("suggest_tests", json!({"symbol":"direct"}), deadline())
        .unwrap();
    assert_eq!(selected["test_summary"]["candidate_count"], 1);
    assert_eq!(
        selected["test_candidates"][0]["witnesses"][0]["reason"],
        "selected_test"
    );
    assert_eq!(
        selected["test_candidates"][0]["witnesses"][0]["path"],
        json!([])
    );
    let none = service
        .execute(
            "suggest_tests",
            json!({"symbol":"target","source":["L3"]}),
            deadline(),
        )
        .unwrap();
    assert_eq!(none["test_summary"]["candidate_count"], 0);
    assert!(
        none["completeness"]["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["kind"] == "test_selection_without_coverage")
    );
    let definite = service
        .execute(
            "suggest_tests",
            json!({"symbol":"target","certainty":["definite"]}),
            deadline(),
        )
        .unwrap();
    assert_eq!(definite["test_summary"]["candidate_count"], 0);
    engine.shutdown();
}

#[test]
fn bounded_suggestions_offer_fixed_snapshot_requeries_and_frozen_pages() {
    let (temp, engine, service) = setup(SOURCE);
    let first = service
        .execute(
            "suggest_tests",
            json!({"symbol":"target","depth":1,"limit":1}),
            deadline(),
        )
        .unwrap();
    assert_eq!(first["test_summary"]["traversal_complete"], false);
    assert_eq!(first["output"]["truncation_reason"], "depth_limit");
    let mut suggested = first["output"]["continuation"]["suggested"].clone();
    assert_eq!(suggested["snapshot_id"], first["snapshot_id"]);
    suggested["depth"] = json!(5);
    suggested["limit"] = json!(1000);
    let cursor = first["output"]["continuation"]["next_cursor"]
        .as_str()
        .unwrap();
    let error = service
        .execute(
            "suggest_tests",
            json!({"cursor":cursor,"depth":2}),
            deadline(),
        )
        .unwrap_err();
    assert_eq!(
        query::error_value(&error)["error"]["code"],
        "invalid_argument"
    );
    fs::write(temp.path().join("src/lib.rs"), "pub fn replaced() {}\n").unwrap();
    engine.scan(false).unwrap();
    let expanded = service
        .execute("suggest_tests", suggested, deadline())
        .unwrap();
    assert_eq!(expanded["test_summary"]["candidate_count"], 6);
    assert_eq!(expanded["snapshot_id"], first["snapshot_id"]);
    let mut page = first.clone();
    let mut tests = vec![];
    loop {
        assert_eq!(page["test_summary"], first["test_summary"]);
        assert_eq!(page["snapshot_id"], first["snapshot_id"]);
        tests.extend(page["test_candidates"].as_array().unwrap().iter().cloned());
        let Some(cursor) = page["output"]["continuation"]["next_cursor"].as_str() else {
            break;
        };
        page = service
            .execute("suggest_tests", json!({"cursor":cursor}), deadline())
            .unwrap();
    }
    assert_eq!(
        tests.len() as u64,
        first["test_summary"]["candidate_count"].as_u64().unwrap()
    );
    let bounded = service
        .execute(
            "suggest_tests",
            json!({"symbol":"target","snapshot_id":first["snapshot_id"],"max_nodes":1}),
            deadline(),
        )
        .unwrap();
    assert_eq!(bounded["output"]["truncation_reason"], "node_limit");
    assert_eq!(bounded["test_summary"]["traversal_complete"], false);
    engine.shutdown();
}

#[test]
fn diff_maps_old_witnesses_to_current_tests_and_keeps_deleted_tests_separate() {
    let source = "pub fn target() {}\n#[test]\nfn retained() { target(); }\n#[test]\nfn removed() { target(); }\n";
    let (temp, engine, service) = setup(source);
    let base = engine.scan(false).unwrap();
    fs::write(
        temp.path().join("src/lib.rs"),
        "#[test]\nfn retained() { target(); }\n",
    )
    .unwrap();
    let head = engine.scan(false).unwrap();
    let diff = json!({"base":{"kind":"snapshot","snapshot_id":base.snapshot.snapshot_id,"semantic_epoch":0},
        "head":{"kind":"snapshot","snapshot_id":head.snapshot.snapshot_id,"semantic_epoch":0}});
    let result = service
        .execute(
            "suggest_tests",
            json!({"diff":diff,"limit":1000}),
            deadline(),
        )
        .unwrap();
    assert_eq!(result["test_summary"]["head_candidates"], 1, "{result}");
    assert_eq!(result["test_summary"]["base_only_candidates"], 1);
    let retained = candidate(&result, "retained");
    assert_eq!(retained["availability"], "head");
    assert_eq!(
        retained["test_snapshot"]["snapshot_id"],
        head.snapshot.snapshot_id
    );
    assert!(
        retained["witnesses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|witness| witness["side"] == "base"
                && !witness["path"].as_array().unwrap().is_empty())
    );
    assert_eq!(candidate(&result, "removed")["availability"], "base_only");
    let first = service
        .execute(
            "suggest_tests",
            json!({"diff":diff,"limit":1,"max_nodes":1}),
            deadline(),
        )
        .unwrap();
    let suggested = &first["output"]["continuation"]["suggested"];
    assert_eq!(
        suggested["diff"]["base"]["snapshot_id"],
        base.snapshot.snapshot_id
    );
    assert_eq!(
        suggested["diff"]["head"]["snapshot_id"],
        head.snapshot.snapshot_id
    );
    query::validate("suggest_tests", suggested).unwrap();
    fs::write(temp.path().join("src/lib.rs"), "pub fn replaced() {}\n").unwrap();
    engine.scan(false).unwrap();
    let next = service
        .execute(
            "suggest_tests",
            json!({"cursor":first["output"]["continuation"]["next_cursor"]}),
            deadline(),
        )
        .unwrap();
    assert_eq!(next["test_summary"], first["test_summary"]);
    assert_eq!(next["snapshots"], first["snapshots"]);
    engine.shutdown();
}
