use semweft_query::{Engine, QueryService, config};
use serde_json::json;
use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn cargo_membership_is_independent_of_source_ignore_rules() {
    for members in ["\"crates/*\"", "\"crates/visible\",\"crates/hidden\""] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg(&root)
                .output()
                .unwrap()
                .status
                .success()
        );
        fs::write(
            root.join("Cargo.toml"),
            format!("[workspace]\nmembers=[{members}]\n"),
        )
        .unwrap();
        fs::write(root.join(".gitignore"), "crates/hidden/\n").unwrap();
        for name in ["visible", "hidden"] {
            let package = root.join("crates").join(name);
            fs::create_dir_all(package.join("src")).unwrap();
            fs::write(
                package.join("Cargo.toml"),
                format!("[package]\nname=\"{name}\"\nversion=\"0.1.0\"\n"),
            )
            .unwrap();
            fs::write(
                package.join("src/lib.rs"),
                format!("pub fn {name}_marker() {{}}\n"),
            )
            .unwrap();
        }
        assert_eq!(
            config::members(&root).unwrap(),
            ["crates/hidden", "crates/visible"]
        );
        // Starting from an ignored member must still resolve the Cargo workspace.
        assert_eq!(
            config::resolve(Some(&root.join("crates/hidden")), None)
                .unwrap()
                .root,
            root
        );
        for respect in [true, false] {
            fs::write(root.join("semweft.toml"), format!("schema_version=\"0.1\"\n[workspace]\nrespect_gitignore={respect}\n[index]\ndirectory=\"index\"\n")).unwrap();
            let resolved = config::resolve(Some(&root), None).unwrap();
            let captured = semweft_query::workspace::capture_config(&resolved, "test").unwrap();
            assert_eq!(
                captured
                    .manifest
                    .files
                    .contains_key("crates/hidden/src/lib.rs"),
                !respect
            );
            assert_eq!(
                captured
                    .manifest
                    .files
                    .contains_key("crates/hidden/Cargo.toml"),
                !respect
            );
            assert!(
                captured
                    .manifest
                    .files
                    .contains_key("crates/visible/src/lib.rs")
            );
        }
    }
}

#[test]
fn fixed_snapshot_search_and_map_obey_computation_budgets() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"budget\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    let source = (0..3000)
        .map(|i| format!("pub fn item_{i:04}() {{}}\n"))
        .collect::<String>();
    fs::write(root.join("src/lib.rs"), source).unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    let engine = Engine::new(config::resolve(Some(&root), None).unwrap()).unwrap();
    let view = engine.scan(false).unwrap();
    let service = QueryService::new(engine.clone());
    // Warm binding separately, so these deadlines exercise computation rather
    // than the initial immutable-view load.
    service
        .execute(
            "search_symbols",
            json!({"snapshot_id":view.snapshot.snapshot_id,"query":"item_0000","match":"exact"}),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
    for tool in ["search_symbols", "repo_map"] {
        let mut args = json!({"snapshot_id":view.snapshot.snapshot_id,"timeout_ms":1});
        if tool == "search_symbols" {
            args["query"] = json!("item_");
        }
        let started = Instant::now();
        let result = service.execute(tool, args.clone(), started + Duration::from_millis(1));
        // Allow scheduling noise; the old full computation took over a second.
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{tool} ignored its deadline"
        );
        match result {
            Ok(value) => {
                assert_eq!(value["output"]["truncation_reason"], "timeout");
                assert_eq!(value["evidence_summary"]["basis"], "partial");
                assert_eq!(value["evidence_summary"]["exact"], false);
                assert!(value["output"]["remaining_items"].is_null());
                assert!(value["output"]["returned_items"].as_u64().unwrap() > 0);
                assert_eq!(value["output"]["continuation"]["kind"], "requery");
                assert_eq!(
                    value["output"]["continuation"]["suggested"]["snapshot_id"],
                    view.snapshot.snapshot_id
                );
            }
            Err(error) => assert_eq!(
                semweft_query::query::error_value(&error)["error"]["code"],
                "timeout"
            ),
        }
        assert_eq!(
            semweft_query::query::error_value(
                &service.execute(tool, args, Instant::now()).unwrap_err()
            )["error"]["code"],
            "timeout"
        );
        let (stored, fresh) = engine
            .bind(
                &json!({"snapshot_id":view.snapshot.snapshot_id}),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
        let mut direct_args = json!({"query":"item_"});
        if tool == "repo_map" {
            direct_args = json!({});
        }
        let error = service
            .compute(tool, &direct_args, &stored, fresh, Instant::now())
            .unwrap_err();
        assert_eq!(
            semweft_query::query::error_value(&error)["error"]["code"],
            "timeout"
        );
    }
    let complete = service
        .execute(
            "search_symbols",
            json!({"snapshot_id":view.snapshot.snapshot_id,"query":"item_","limit":1}),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(complete["symbols"][0]["name"], "item_0000");
    assert_eq!(complete["evidence_summary"]["basis"], "complete");
    assert_eq!(complete["output"]["remaining_items"], 2999);
    engine.shutdown();
}

#[test]
fn library_queries_refresh_and_recover_without_cli_or_transport() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"example\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "fn apple() {}\nfn apricot() {}\n").unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    let resolved = config::resolve(Some(&root), None).unwrap();
    let deadline = || Instant::now() + Duration::from_secs(10);
    let engine = Engine::new(resolved.clone()).unwrap();
    let service = QueryService::new(engine.clone());
    let first = service
        .execute(
            "search_symbols",
            json!({"query":"ap","match":"prefix","limit":1}),
            deadline(),
        )
        .unwrap();
    let cursor = first["output"]["continuation"]["next_cursor"]
        .as_str()
        .unwrap();
    fs::write(root.join("src/lib.rs"), "fn avocado() {}\n").unwrap();
    let current = service
        .execute("search_symbols", json!({"query":"av"}), deadline())
        .unwrap();
    assert_ne!(current["snapshot_id"], first["snapshot_id"]);
    let next = service
        .execute("search_symbols", json!({"cursor":cursor}), deadline())
        .unwrap();
    assert_eq!(next["snapshot_id"], first["snapshot_id"]);
    assert_eq!(next["symbols"][0]["name"], "apricot");
    engine.shutdown();
    drop(service);
    drop(engine);
    let engine = Engine::new(resolved).unwrap();
    let service = QueryService::new(engine.clone());
    let recovered = service
        .execute(
            "search_symbols",
            json!({"query":"ap", "snapshot_id":first["snapshot_id"]}),
            deadline(),
        )
        .unwrap();
    assert_eq!(recovered["symbols"].as_array().unwrap().len(), 2);
    engine.shutdown();
}
