use semweft_query::{Engine, QueryService, config};
use serde_json::json;
use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn simple_call_paths_preserve_evidence_bounds_filters_and_frozen_pages() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"paths\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn target() {}\npub fn left() { target(); entry(); }\npub fn right() { target(); }\npub fn entry() { left(); right(); target(); }\n").unwrap();
    let engine = Engine::new(config::resolve(Some(&root), None).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let first = service
        .execute(
            "find_paths",
            json!({"from":"entry","to":"target","limit":1}),
            deadline(),
        )
        .unwrap();
    assert_eq!(first["path_summary"]["path_count"], 3);
    assert_eq!(first["path_summary"]["enumeration_complete"], true);
    assert_eq!(first["output"]["returned_paths"], 1);
    assert_eq!(first["output"]["remaining_paths"], 2);
    let pinned = json!({"from":"entry","to":"target","snapshot_id":first["snapshot_id"],"semantic_epoch":first["epochs"]["semantic"]});
    for (parameter, value, reason) in [
        ("max_depth", 1, "depth_limit"),
        ("max_paths", 1, "path_limit"),
        ("max_nodes", 1, "node_limit"),
    ] {
        let mut args = pinned.clone();
        args[parameter] = json!(value);
        let bounded = service.execute("find_paths", args, deadline()).unwrap();
        assert_eq!(
            bounded["path_summary"]["enumeration_complete"], false,
            "{bounded}"
        );
        assert_eq!(bounded["output"]["truncation_reason"], reason, "{bounded}");
        assert!(bounded["output"]["remaining_paths"].is_null());
        assert!(bounded["output"]["remaining_relations"].is_null());
        assert_eq!(
            bounded["output"]["continuation"]["suggested"]["snapshot_id"],
            first["snapshot_id"]
        );
    }
    for filter in [
        json!({"source":["L3"]}),
        json!({"certainty":["definite"]}),
        json!({"path_prefix":"missing"}),
    ] {
        let mut args = pinned.clone();
        args.as_object_mut()
            .unwrap()
            .extend(filter.as_object().unwrap().clone());
        let empty = service.execute("find_paths", args, deadline()).unwrap();
        assert_eq!(empty["paths"], json!([]));
        assert_eq!(empty["path_summary"]["enumeration_complete"], true);
    }
    let same = service
        .execute(
            "find_paths",
            json!({"from":"target","to":"target"}),
            deadline(),
        )
        .unwrap();
    assert_eq!(same["paths"][0]["length"], 0);
    assert_eq!(same["paths"][0]["relations"], json!([]));
    let absent = service
        .execute(
            "find_paths",
            json!({"from":"target","to":"entry"}),
            deadline(),
        )
        .unwrap();
    assert_eq!(absent["paths"], json!([]));
    assert_eq!(absent["output"]["truncated"], false);
    let cursor = first["output"]["continuation"]["next_cursor"]
        .as_str()
        .unwrap();
    let changed = service
        .execute(
            "find_paths",
            json!({"cursor":cursor,"to":"left"}),
            deadline(),
        )
        .unwrap_err();
    assert_eq!(
        semweft_query::query::error_value(&changed)["error"]["code"],
        "invalid_argument"
    );
    fs::write(root.join("src/lib.rs"), "pub fn other() {}\n").unwrap();
    engine.scan(false).unwrap();
    let mut page = first.clone();
    let mut paths = vec![];
    loop {
        assert_eq!(page["path_summary"], first["path_summary"]);
        assert_eq!(page["snapshot_id"], first["snapshot_id"]);
        assert_eq!(page["epochs"], first["epochs"]);
        let path = &page["paths"][0];
        assert_eq!(path["certainty"], "candidate");
        assert_eq!(
            path["symbol_ids"].as_array().unwrap().len() as u64,
            path["length"].as_u64().unwrap() + 1
        );
        assert_eq!(page["output"]["returned_relations"], path["length"]);
        for relation in path["relations"].as_array().unwrap() {
            assert_eq!(relation["kind"], "call");
            assert_eq!(relation["evidence"][0]["source"], "L1");
            assert_eq!(relation["location"]["file"], "src/lib.rs");
        }
        paths.push(path["path_id"].as_str().unwrap().to_owned());
        let Some(next) = page["output"]["continuation"]["next_cursor"].as_str() else {
            break;
        };
        page = service
            .execute("find_paths", json!({"cursor":next}), deadline())
            .unwrap();
    }
    assert_eq!(
        paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    let replay = service
        .execute("find_paths", json!({"cursor":cursor}), deadline())
        .unwrap();
    assert_eq!(replay["paths"][0]["path_id"], paths[1]);
    engine.shutdown();
}

#[test]
fn verification_pages_bind_both_snapshots_and_keep_full_verdicts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"verify\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    let source = "pub fn stock() -> i32 { 1 }\npub fn order() -> i32 { stock() }\n";
    fs::write(root.join("src/lib.rs"), source).unwrap();
    let engine = Engine::new(config::resolve(Some(&root), None).unwrap()).unwrap();
    let base = engine.scan(false).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        source.replace("fn stock()", "fn inventory()"),
    )
    .unwrap();
    let head = engine.scan(false).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let diff = json!({"base":{"kind":"snapshot","snapshot_id":base.snapshot.snapshot_id,"semantic_epoch":0},
        "head":{"kind":"snapshot","snapshot_id":head.snapshot.snapshot_id,"semantic_epoch":0}});
    let impact = service
        .execute("impact_of", json!({"diff":diff}), deadline())
        .unwrap();
    let first = service
        .execute("verify_change", json!({"diff":diff,"limit":1}), deadline())
        .unwrap();
    assert_eq!(first["snapshots"], impact["snapshots"]);
    assert_eq!(first["changes"], impact["changes"]);
    assert_eq!(first["verification"]["status"], "insufficient_evidence");
    assert_eq!(first["verification"]["violation_count"], 0);
    assert_eq!(first["findings"].as_array().unwrap().len(), 1);
    assert_eq!(first["output"]["truncation_reason"], "page_limit");
    let cursor = first["output"]["continuation"]["next_cursor"]
        .as_str()
        .unwrap();
    let wrong_tool = service
        .execute("impact_of", json!({"cursor":cursor}), deadline())
        .unwrap_err();
    assert_eq!(
        semweft_query::query::error_value(&wrong_tool)["error"]["code"],
        "cursor_expired"
    );
    let wrong_limit = service
        .execute(
            "verify_change",
            json!({"cursor":cursor,"limit":2}),
            deadline(),
        )
        .unwrap_err();
    assert_eq!(
        semweft_query::query::error_value(&wrong_limit)["error"]["code"],
        "invalid_argument"
    );
    fs::write(root.join("src/lib.rs"), "pub fn unrelated() {}\n").unwrap();
    engine.scan(false).unwrap();
    let mut page = first.clone();
    let mut findings = vec![];
    let mut relations = vec![];
    loop {
        assert_eq!(page["verification"], first["verification"]);
        assert_eq!(page["snapshots"], first["snapshots"]);
        assert!(page["output"]["returned_items"].as_u64().unwrap() <= 1);
        findings.extend(page["findings"].as_array().unwrap().iter().cloned());
        relations.extend(page["relations"].as_array().unwrap().iter().cloned());
        let Some(next) = page["output"]["continuation"]["next_cursor"].as_str() else {
            break;
        };
        page = service
            .execute("verify_change", json!({"cursor":next}), deadline())
            .unwrap();
    }
    assert_eq!(
        findings.len() as u64,
        first["verification"]["finding_count"].as_u64().unwrap()
    );
    assert_eq!(relations, *impact["relations"].as_array().unwrap());
    let residual = findings
        .iter()
        .find(|item| item["rule_id"] == "residual_reference_requires_review")
        .unwrap();
    assert_eq!(residual["evidence"]["location"]["file"], "src/lib.rs");
    assert_eq!(residual["evidence"]["location"]["line"], 2);
    assert_eq!(residual["evidence"]["certainty"], "candidate");
    engine.shutdown();
}

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
