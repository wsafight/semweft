use semweft_query::{Engine, QueryService, architecture, config};
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
fn layer_rules_baseline_new_existing_resolved_and_fixed_policy() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"architecture\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    let config_text = "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n[[architecture.layers]]\nname=\"domain\"\npaths=[\"src/domain.rs\"]\n[[architecture.layers]]\nname=\"infra\"\npaths=[\"src/infra.rs\"]\nallow=[\"domain\"]\n[[architecture.layers]]\nname=\"api\"\npaths=[\"src/api.rs\"]\nallow=[\"domain\"]\n";
    fs::write(root.join("semweft.toml"), config_text).unwrap();
    fs::write(root.join("src/lib.rs"), "mod domain; mod infra; mod api;\n").unwrap();
    fs::write(
        root.join("src/domain.rs"),
        "pub fn domain_entry() { database(); }\n",
    )
    .unwrap();
    fs::write(root.join("src/infra.rs"), "pub fn database() {}\n").unwrap();
    fs::write(
        root.join("src/api.rs"),
        "pub fn api_entry() { domain_entry(); }\n",
    )
    .unwrap();
    let engine = Engine::new(config::resolve(Some(root), None).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let initial = service
        .execute("arch_check", json!({}), deadline())
        .unwrap();
    assert_eq!(initial["architecture"]["new_count"], 1);
    assert_eq!(initial["findings"][0]["status"], "insufficient_evidence");
    assert_eq!(
        initial["findings"][0]["evidence"]["dependency_path"],
        json!(["src/domain.rs", "src/infra.rs"])
    );
    assert!(
        !initial["findings"][0]["evidence"]["relations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let baseline = serde_json::to_value(architecture::baseline(&initial).unwrap()).unwrap();
    let existing = service
        .execute("arch_check", json!({"baseline":baseline}), deadline())
        .unwrap();
    assert_eq!(existing["architecture"]["existing_count"], 1);
    assert_eq!(existing["architecture"]["new_count"], 0);
    fs::write(
        root.join("src/api.rs"),
        "pub fn api_entry() { database(); }\n",
    )
    .unwrap();
    let changed = service
        .execute("arch_check", json!({"baseline":baseline}), deadline())
        .unwrap();
    assert_eq!(changed["architecture"]["existing_count"], 1);
    assert_eq!(changed["architecture"]["new_count"], 1);
    let diff = json!({"base":{"kind":"snapshot","snapshot_id":initial["snapshot_id"],"semantic_epoch":initial["epochs"]["semantic"]},"head":{"kind":"snapshot","snapshot_id":changed["snapshot_id"],"semantic_epoch":changed["epochs"]["semantic"]}});
    let comparison = service
        .execute("arch_check", json!({"diff":diff}), deadline())
        .unwrap();
    assert_eq!(comparison["architecture"]["new_count"], 1);
    assert_eq!(comparison["architecture"]["comparison"], "diff_base");
    fs::write(root.join("src/domain.rs"), "pub fn domain_entry() {}\n").unwrap();
    let resolved = service
        .execute("arch_check", json!({"baseline":baseline}), deadline())
        .unwrap();
    assert_eq!(
        resolved["architecture"]["resolved_fingerprints"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let pinned = service.execute("arch_check", json!({"snapshot_id":initial["snapshot_id"],"semantic_epoch":initial["epochs"]["semantic"]}), deadline()).unwrap();
    assert_eq!(pinned["architecture"], initial["architecture"]);
    let mut bad_baseline = baseline.clone();
    bad_baseline["rules_digest"] = json!("different");
    assert!(
        service
            .execute("arch_check", json!({"baseline":bad_baseline}), deadline())
            .is_err()
    );
    let bad_policy =
        json!({"layers":[{"name":"duplicate","paths":["**"]},{"name":"duplicate","paths":["**"]}]});
    assert!(
        service
            .execute("arch_check", json!({"policy":bad_policy}), deadline())
            .is_err()
    );
    engine.shutdown();
}
