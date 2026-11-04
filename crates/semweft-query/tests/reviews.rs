use semweft_query::{Engine, QueryService, config, reviews};
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
fn review_binds_diff_snapshots_rules_and_invalidates_after_edits() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"review\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn entry() {}\n").unwrap();
    let resolved = config::resolve(Some(root), None).unwrap();
    let engine = Engine::new(resolved.clone()).unwrap();
    let base = engine.scan(false).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn entry() { let updated = 1; }\n",
    )
    .unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let report = service.execute("agent_verify",json!({"diff":{"base":{"kind":"snapshot","snapshot_id":base.snapshot.snapshot_id,"semantic_epoch":0},"head":{"kind":"workspace"}},"limit":1000}),deadline()).unwrap();
    assert!(report["diff_identity"]["digest"].is_string());
    let record = reviews::save(
        &resolved,
        report.clone(),
        "approve",
        "manual review, not behavioral proof",
    )
    .unwrap();
    let id = record["review_id"].as_str().unwrap();
    let current = service
        .execute("get_review_result", json!({"review_id":id}), deadline())
        .unwrap();
    assert_eq!(current["review"]["validity"]["approval_applies"], true);
    let mut legacy_report = report.clone();
    legacy_report["risk_summary"]["scoring"] = json!("heuristic_v1");
    let legacy = reviews::save(&resolved, legacy_report, "approve", "old model").unwrap();
    let legacy = service
        .execute(
            "get_review_result",
            json!({"review_id":legacy["review_id"]}),
            deadline(),
        )
        .unwrap();
    assert_eq!(legacy["review"]["validity"]["approval_applies"], false);
    assert_eq!(legacy["review"]["validity"]["rules_match"], false);
    assert_eq!(
        current["review"]["diff_digest"],
        report["diff_identity"]["digest"]
    );
    fs::write(
        root.join("src/lib.rs"),
        "pub fn entry() { let another = 2; }\n",
    )
    .unwrap();
    let edited = service
        .execute("get_review_result", json!({"review_id":id}), deadline())
        .unwrap();
    assert_eq!(edited["review"]["validity"]["approval_applies"], false);
    let pinned = service.execute("get_review_result",json!({"review_id":id,"snapshot_id":report["snapshot_id"],"semantic_epoch":report["epochs"]["semantic"]}),deadline()).unwrap();
    assert_eq!(pinned["review"]["validity"]["approval_applies"], true);
    let mut corrupt = record.clone();
    corrupt["note"] = json!("tampered");
    fs::write(
        resolved
            .index_dir
            .join("reviews")
            .join(format!("{id}.json")),
        serde_json::to_vec(&corrupt).unwrap(),
    )
    .unwrap();
    assert!(
        service
            .execute("get_review_result", json!({"review_id":id}), deadline())
            .is_err()
    );
    assert!(reviews::save(&resolved, report, "certify", "").is_err());
    assert!(
        service
            .execute(
                "get_review_result",
                json!({"review_id":"../escape"}),
                deadline()
            )
            .is_err()
    );
    engine.shutdown();
}
