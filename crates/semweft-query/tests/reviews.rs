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
    let status = reviews::status(&resolved, &engine, id).unwrap();
    assert_eq!(status["review_id"], json!(id));
    assert_eq!(status["revision"], json!(0));
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
    let request = reviews::create(&resolved, report.clone(), "Check this change").unwrap();
    let concurrent = reviews::create(&resolved, report.clone(), "Concurrent reviewers").unwrap();
    let outcomes = std::thread::scope(|scope| {
        let workers = ["first", "second"].map(|reviewer| {
            let resolved = &resolved;
            let engine = &engine;
            let id = concurrent["review_id"].clone();
            scope.spawn(move || reviews::decide(resolved, engine, &json!({"review_id":id,"revision":0,"conclusion":"comment","reviewer":reviewer})))
        });
        workers.map(|worker| worker.join().unwrap().is_ok())
    });
    assert_eq!(outcomes.into_iter().filter(|success| *success).count(), 1);
    let request_id = request["review_id"].as_str().unwrap();
    assert_eq!(request["state"], "pending");
    let mut decision = json!({"review_id":request_id,"revision":0,"conclusion":"approve","reviewer":"local reviewer"});
    if !request["items"].as_array().unwrap().is_empty() {
        assert!(reviews::decide(&resolved, &engine, &decision).is_err());
    }
    for (index, item) in request["items"].as_array().unwrap().iter().enumerate() {
        let decided = reviews::decide(&resolved, &engine, &json!({"review_id":request_id,"revision":index,"item_id":item["item_id"],"conclusion":"accept","reviewer":"local reviewer","note":"Evidence checked"})).unwrap();
        assert_eq!(decided["state"], "in_review");
    }
    decision["revision"] = json!(request["items"].as_array().unwrap().len());
    let approved = reviews::decide(&resolved, &engine, &decision).unwrap();
    assert_eq!(approved["state"], "approved");
    assert_eq!(approved["validity"]["approval_applies"], true);
    assert!(reviews::decide(&resolved, &engine, &decision).is_err());
    let reloaded = service
        .execute(
            "get_review_result",
            json!({"review_id":request_id}),
            deadline(),
        )
        .unwrap();
    assert_eq!(reloaded["review"]["events"], approved["events"]);
    let mut annotation = json!({"review_id":request_id,"revision":approved["revision"],"item_id":"source","conclusion":"comment","reviewer":"local reviewer","note":"Check this line", "location":{"side":"head","file":"src/lib.rs","line":1,"file_hash":"wrong"}});
    assert!(reviews::decide(&resolved, &engine, &annotation).is_err());
    annotation["location"]["file_hash"] = json!(semweft_core::digest(
        fs::read(root.join("src/lib.rs")).unwrap()
    ));
    let annotated = reviews::decide(&resolved, &engine, &annotation).unwrap();
    assert_eq!(annotated["state"], "in_review");
    assert_eq!(annotated["validity"]["approval_applies"], false);
    decision["revision"] = annotated["revision"].clone();
    let approved = reviews::decide(&resolved, &engine, &decision).unwrap();
    assert_eq!(approved["state"], "approved");
    fs::write(
        root.join("src/lib.rs"),
        "pub fn entry() { let another = 2; }\n",
    )
    .unwrap();
    let edited = service
        .execute("get_review_result", json!({"review_id":id}), deadline())
        .unwrap();
    assert_eq!(edited["review"]["validity"]["approval_applies"], false);
    let superseded = service
        .execute(
            "get_review_result",
            json!({"review_id":request_id}),
            deadline(),
        )
        .unwrap();
    assert_eq!(superseded["review"]["state"], "superseded");
    decision["revision"] = approved["revision"].clone();
    assert!(reviews::decide(&resolved, &engine, &decision).is_err());
    let path = resolved
        .index_dir
        .join("reviews")
        .join(format!("{request_id}.events.json"));
    let mut events = approved["events"].clone();
    events[0]["note"] = json!("changed after the decision");
    fs::write(path, serde_json::to_vec(&events).unwrap()).unwrap();
    assert!(
        service
            .execute(
                "get_review_result",
                json!({"review_id":request_id}),
                deadline()
            )
            .is_err()
    );
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
