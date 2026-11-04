use semweft_query::{Engine, QueryService, config};
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
fn guard_blocks_paths_symbols_and_policy_removal_in_both_diff_sides() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"guard\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    let plain = "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n";
    let policy = format!(
        "{plain}\n[[guard.protected]]\nname=\"protocol\"\npaths=[\"proto/**\"]\nreason=\"manual only\"\n[[guard.protected]]\nname=\"core\"\nsymbols=[\"*::core\"]\nreason=\"core is protected\"\n"
    );
    fs::write(root.join("semweft.toml"), &policy).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn core() {}\npub fn caller() { core(); }\n",
    )
    .unwrap();
    let engine = Engine::new(config::resolve(Some(root), None).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let before = service
        .execute(
            "agent_preflight",
            json!({"files":["src/lib.rs"]}),
            deadline(),
        )
        .unwrap();
    assert_eq!(before["guard"]["decision"], "block");
    assert_eq!(before["guard"]["public_declarations_observed"], 2);
    assert_eq!(before["verification"]["violation_count"], 1);
    let new_file = service
        .execute(
            "agent_preflight",
            json!({"files":["proto/new.proto"]}),
            deadline(),
        )
        .unwrap();
    assert_eq!(new_file["guard"]["decision"], "block");
    let unprotected = service
        .execute(
            "agent_preflight",
            json!({"files":["src/new.rs"]}),
            deadline(),
        )
        .unwrap();
    assert_ne!(unprotected["guard"]["decision"], "block");
    assert!(
        service
            .execute(
                "agent_preflight",
                json!({"files":["../outside"]}),
                deadline()
            )
            .is_err()
    );
    fs::write(root.join("semweft.toml"), plain).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn core() { let changed = 1; }\npub fn caller() { core(); }\n",
    )
    .unwrap();
    let after = service.execute("agent_verify", json!({"diff":{"base":{"kind":"snapshot","snapshot_id":before["snapshot_id"],"semantic_epoch":before["epochs"]["semantic"]},"head":{"kind":"workspace"}}}), deadline()).unwrap();
    assert_eq!(after["guard"]["decision"], "block");
    assert!(
        after["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["rule_id"] == "protected_region"
                && finding["evidence"]["side"] == "base")
    );
    assert_eq!(after["verification"]["status"], "violations_found");
    engine.shutdown();
}

#[test]
fn external_policies_are_frozen_and_protect_deleted_symbols_and_non_source_text() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"external-policy\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn core() {}\n").unwrap();
    fs::write(root.join("sensitive.txt"), "original").unwrap();
    let path = temp.path().join("external.toml");
    let policy = "schema_version=\"0.1\"\n[workspace]\nroot=\"workspace\"\n[index]\ndirectory=\"index\"\n[[guard.protected]]\nname=\"external\"\npaths=[\"sensitive.txt\"]\nsymbols=[\"*::core\"]\nreason=\"outside source tree policy\"\n";
    fs::write(&path, policy).unwrap();
    let engine = Engine::new(config::resolve(Some(&root), Some(&path)).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let before = service
        .execute(
            "agent_preflight",
            json!({"files":["src/lib.rs"]}),
            deadline(),
        )
        .unwrap();
    assert_eq!(before["guard"]["decision"], "block");
    fs::write(root.join("src/lib.rs"), "pub fn replacement() {}\n").unwrap();
    fs::write(root.join("sensitive.txt"), "changed").unwrap();
    let after=service.execute("agent_verify",json!({"diff":{"base":{"kind":"snapshot","snapshot_id":before["snapshot_id"],"semantic_epoch":before["epochs"]["semantic"]},"head":{"kind":"workspace"}}}),deadline()).unwrap();
    assert_eq!(after["guard"]["decision"], "block");
    assert!(
        after["guard"]["changed_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file == "sensitive.txt")
    );
    assert!(
        after["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["rule_id"] == "protected_region"
                && finding["evidence"]["file"] == "src/lib.rs"
                && finding["evidence"]["side"] == "base")
    );
    fs::write(
        &path,
        "schema_version=\"0.1\"\n[workspace]\nroot=\"workspace\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    engine.scan(false).unwrap();
    let pinned=service.execute("agent_preflight",json!({"files":["src/lib.rs"],"snapshot_id":before["snapshot_id"],"semantic_epoch":before["epochs"]["semantic"]}),deadline()).unwrap();
    assert_eq!(pinned["guard"]["decision"], "block");
    engine.shutdown();
}
