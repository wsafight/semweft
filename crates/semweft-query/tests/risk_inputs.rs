use protobuf::Message;
use semweft_query::{Engine, QueryService, config, risk_inputs};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(20)
}

fn git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Risk Test")
        .env("GIT_AUTHOR_EMAIL", "risk@example.invalid")
        .env("GIT_COMMITTER_NAME", "Risk Test")
        .env("GIT_COMMITTER_EMAIL", "risk@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn export(engine: &Engine, path: &Path) {
    let status = engine
        .index(
            &json!({"syntax_only":true,"export_manifest":true}),
            deadline(),
        )
        .unwrap();
    fs::write(
        path,
        serde_json::to_vec(&status["source_manifest"]).unwrap(),
    )
    .unwrap();
}

#[test]
fn coverage_and_history_bind_both_versions_score_observations_and_freeze_pages() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"risk\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub mod logic;\npub fn entry() -> i32 { logic::calc() }\n",
    )
    .unwrap();
    let source = "pub fn calc() -> i32 {\n    0\n}\npub fn idle() {}\n";
    fs::write(root.join("src/logic.rs"), source).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "first"]);
    fs::write(root.join("src/logic.rs"), source.replace("    0", "    1")).unwrap();
    git(&root, &["add", "src/logic.rs"]);
    git(&root, &["commit", "-qm", "second"]);
    let resolved = config::resolve(Some(&root), None).unwrap();
    let engine = Engine::new(resolved.clone()).unwrap();
    let base = engine.scan(false).unwrap();
    let inputs = temp.path().join("inputs.json");
    let coverage = temp.path().join("coverage.info");
    export(&engine, &inputs);
    fs::write(&coverage,"TN:first\nSF:./src/logic.rs\nDA:2,1\nend_of_record\nSF:/outside/source.rs\nDA:1,1\nend_of_record\n").unwrap();
    let before =
        risk_inputs::collect(&resolved, Some((&coverage, &inputs)), Some(100), deadline()).unwrap();
    assert_eq!(before.coverage.as_ref().unwrap().omitted_records, 1);
    assert_eq!(before.history.as_ref().unwrap().commits_observed, 2);
    assert!(!before.history.as_ref().unwrap().window_limited);
    assert_eq!(
        before
            .files
            .iter()
            .find(|file| file.path == "src/logic.rs")
            .unwrap()
            .history_changes,
        Some(2)
    );
    assert_eq!(
        before
            .files
            .iter()
            .find(|file| file.path == "src/lib.rs")
            .unwrap()
            .history_changes,
        Some(1)
    );
    let window = risk_inputs::collect(&resolved, None, Some(1), deadline()).unwrap();
    assert!(window.history.as_ref().unwrap().window_limited);
    assert_eq!(
        window
            .files
            .iter()
            .find(|file| file.path == "src/lib.rs")
            .unwrap()
            .history_changes,
        Some(0)
    );
    let preflight_service = QueryService::new(engine.clone());
    let preflight = preflight_service
        .execute(
            "agent_preflight",
            json!({"files":["src/logic.rs"],"risk_inputs":{"head":before},"limit":1000}),
            deadline(),
        )
        .unwrap();
    assert_eq!(preflight["guard"]["coverage"], "partial");
    assert_eq!(preflight["guard"]["hotspots"], "available");
    assert_eq!(
        preflight["guard"]["risk_observations"]["coverage"]["covered_lines"],
        1
    );
    assert_eq!(
        preflight["guard"]["risk_observations"]["hotspots"]["max_window_changes"],
        2
    );
    assert_eq!(
        preflight["guard"]["risk_observations"]["tests_executed"],
        false
    );
    assert!(preflight["symbols"][0]["api_exposure"].is_object());
    fs::write(root.join("src/logic.rs"), source.replace("    0", "    2")).unwrap();
    let stale_preflight = preflight_service
        .execute(
            "agent_preflight",
            json!({"files":["src/logic.rs"],"risk_inputs":{"head":before}}),
            deadline(),
        )
        .unwrap_err();
    assert!(stale_preflight.to_string().starts_with("content_mismatch:"));
    assert!(
        risk_inputs::collect(&resolved, Some((&coverage, &inputs)), None, deadline())
            .unwrap_err()
            .to_string()
            .starts_with("content_mismatch:")
    );
    export(&engine, &inputs);
    fs::write(&coverage, "SF:src/logic.rs\nDA:2,0\nend_of_record\n").unwrap();
    let after =
        risk_inputs::collect(&resolved, Some((&coverage, &inputs)), Some(100), deadline()).unwrap();
    let head = engine.scan(false).unwrap();
    let service = QueryService::new(engine.clone());
    let diff = json!({"base":{"kind":"snapshot","snapshot_id":base.snapshot.snapshot_id,"semantic_epoch":0},"head":{"kind":"snapshot","snapshot_id":head.snapshot.snapshot_id,"semantic_epoch":0}});
    let artifact = json!({"base":before,"head":after});
    let report = service
        .execute(
            "impact_of",
            json!({"diff":diff,"risk_inputs":artifact,"limit":1000}),
            deadline(),
        )
        .unwrap();
    let dimensions = &report["risk_summary"]["dimensions"];
    assert_eq!(report["risk_summary"]["scoring"], risk_inputs::MODEL);
    assert_eq!(dimensions["test_coverage"]["status"], "available");
    assert_eq!(dimensions["test_coverage"]["observed_lines"], 2);
    assert_eq!(dimensions["test_coverage"]["covered_lines"], 1);
    assert_eq!(dimensions["test_coverage"]["executed_coverage"], 50.0);
    assert_eq!(dimensions["test_coverage"]["score"], 8);
    assert_eq!(dimensions["hotspot"]["status"], "available");
    assert_eq!(dimensions["hotspot"]["max_change_frequency"], 1.0);
    assert_eq!(dimensions["hotspot"]["score"], 15);
    assert_eq!(
        report["risk_summary"]["sources"]["head"]["coverage"]["report_digest"],
        artifact["head"]["coverage"]["report_digest"]
    );
    let mut covered = artifact.clone();
    for file in covered["head"]["files"].as_array_mut().unwrap() {
        for line in file
            .get_mut("lines")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            line["hits"] = json!(1);
        }
    }
    let covered_report = service
        .execute(
            "impact_of",
            json!({"diff":diff,"risk_inputs":covered,"limit":1000}),
            deadline(),
        )
        .unwrap();
    assert_eq!(
        covered_report["risk_summary"]["dimensions"]["test_coverage"]["score"],
        0
    );
    assert_eq!(
        report["risk_summary"]["score"].as_u64().unwrap(),
        covered_report["risk_summary"]["score"].as_u64().unwrap() + 8
    );
    let first = service
        .execute(
            "impact_of",
            json!({"diff":diff,"risk_inputs":artifact,"limit":1}),
            deadline(),
        )
        .unwrap();
    let cursor = first["output"]["continuation"]["next_cursor"]
        .as_str()
        .unwrap();
    fs::write(root.join("src/logic.rs"), source.replace("    0", "    3")).unwrap();
    engine.scan(false).unwrap();
    let page = service
        .execute("impact_of", json!({"cursor":cursor}), deadline())
        .unwrap();
    assert_eq!(page["risk_summary"], report["risk_summary"]);
    let stale = service.execute("impact_of",json!({"diff":{"base":diff["base"],"head":{"kind":"workspace"}},"risk_inputs":artifact}),deadline()).unwrap_err();
    assert!(stale.to_string().starts_with("content_mismatch:"));
    let mut wrong = artifact.clone();
    wrong["base"]["history"]["anchor_commit"] = json!("0".repeat(40));
    assert!(
        service
            .execute(
                "impact_of",
                json!({"diff":diff,"risk_inputs":wrong}),
                deadline()
            )
            .unwrap_err()
            .to_string()
            .starts_with("content_mismatch:")
    );
    let mut shallow = artifact.clone();
    shallow["head"]["history"]["shallow"] = json!(true);
    let partial = service
        .execute(
            "impact_of",
            json!({"diff":diff,"risk_inputs":shallow}),
            deadline(),
        )
        .unwrap();
    assert_eq!(
        partial["risk_summary"]["dimensions"]["hotspot"]["status"],
        "partial"
    );
    let configuration_base = engine.scan(false).unwrap();
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    fs::write(root.join("Cargo.toml"), manifest.replace("0.1.0", "0.2.0")).unwrap();
    let configuration_risk = risk_inputs::collect(&resolved, None, Some(100), deadline()).unwrap();
    let configuration = service.execute("impact_of",json!({"diff":{"base":{"kind":"snapshot","snapshot_id":configuration_base.snapshot.snapshot_id,"semantic_epoch":0},"head":{"kind":"workspace"}},"risk_inputs":{"head":configuration_risk}}),deadline()).unwrap();
    assert_eq!(configuration["changes"], json!([]));
    assert_eq!(
        configuration["risk_summary"]["dimensions"]["hotspot"]["score"],
        8
    );
    assert_eq!(
        configuration["risk_summary"]["dimensions"]["hotspot"]["max_frequency_source"]["file"],
        "Cargo.toml"
    );
    engine.shutdown();
}

#[test]
fn malformed_coverage_and_forged_artifact_boundaries_fail_explicitly() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"risk\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn entry() {}\n").unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    let resolved = config::resolve(Some(&root), None).unwrap();
    let engine = Engine::new(resolved.clone()).unwrap();
    let inputs = temp.path().join("inputs.json");
    export(&engine, &inputs);
    let coverage = temp.path().join("coverage.info");
    for malformed in [
        "SF:src/lib.rs\nDA:1,1\n",
        "DA:1,1\n",
        "SF:src/lib.rs\nDA:0,1\nend_of_record\n",
        "SF:src/lib.rs\nDA:2,1\nend_of_record\n",
        "SF:src/lib.rs\nDA:1,-1\nend_of_record\n",
    ] {
        fs::write(&coverage, malformed).unwrap();
        assert!(
            risk_inputs::collect(&resolved, Some((&coverage, &inputs)), None, deadline()).is_err(),
            "{malformed}"
        );
    }
    fs::write(&coverage,"TN:one\nSF:src/lib.rs\nDA:1,1\nend_of_record\nTN:two\nSF:src/lib.rs\nDA:1,2\nend_of_record\n").unwrap();
    let artifact =
        risk_inputs::collect(&resolved, Some((&coverage, &inputs)), None, deadline()).unwrap();
    assert_eq!(
        artifact
            .files
            .iter()
            .find(|file| file.path == "src/lib.rs")
            .unwrap()
            .lines
            .as_ref()
            .unwrap()[0]
            .hits,
        3
    );
    fs::write(
        &coverage,
        "SF:src/lib.rs\nDA:1,1,CRo2N6/DzgY35W0C5C+ajw,1\nend_of_record\n",
    )
    .unwrap();
    risk_inputs::collect(&resolved, Some((&coverage, &inputs)), None, deadline()).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn entry() {}\r\n").unwrap();
    export(&engine, &inputs);
    risk_inputs::collect(&resolved, Some((&coverage, &inputs)), None, deadline()).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn another() {}\n").unwrap();
    export(&engine, &inputs);
    assert!(
        risk_inputs::collect(&resolved, Some((&coverage, &inputs)), None, deadline())
            .unwrap_err()
            .to_string()
            .starts_with("content_mismatch:")
    );
    for malformed in ["DA:1,1,,2", "DA:1,1,,0", "DA:1,1,,130", "DA:1,1,,,extra"] {
        fs::write(
            &coverage,
            format!("SF:src/lib.rs\n{malformed}\nend_of_record\n"),
        )
        .unwrap();
        assert!(
            risk_inputs::collect(&resolved, Some((&coverage, &inputs)), None, deadline()).is_err(),
            "{malformed}"
        );
    }
    let value = serde_json::to_value(&artifact).unwrap();
    for bad_path in ["../outside.rs", "/outside.rs", "C:\\outside.rs"] {
        let mut bad = value.clone();
        bad["files"][0]["path"] = json!(bad_path);
        assert!(
            serde_json::from_value::<risk_inputs::Artifact>(bad)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let mut duplicate = value.clone();
    duplicate["files"]
        .as_array_mut()
        .unwrap()
        .push(value["files"][0].clone());
    assert!(
        serde_json::from_value::<risk_inputs::Artifact>(duplicate)
            .unwrap()
            .validate()
            .is_err()
    );
    let mut no_provenance = value;
    no_provenance.as_object_mut().unwrap().remove("coverage");
    assert!(
        serde_json::from_value::<risk_inputs::Artifact>(no_provenance)
            .unwrap()
            .validate()
            .is_err()
    );
    assert!(risk_inputs::collect(&resolved, None, None, deadline()).is_err());
    assert!(
        risk_inputs::collect(&resolved, Some((&coverage, &inputs)), None, Instant::now()).is_err()
    );
    engine.shutdown();
}

#[test]
fn requested_revision_anchors_history_even_when_a_semantic_view_is_reused() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"reuse\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn entry() {}\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "first"]);
    let resolved = config::resolve(Some(&root), None).unwrap();
    let engine = Engine::new(resolved.clone()).unwrap();
    let inputs = temp.path().join("sources.json");
    export(&engine, &inputs);
    let mut index = scip::types::Index::new();
    let mut metadata = scip::types::Metadata::new();
    let mut tool = scip::types::ToolInfo::new();
    tool.name = "empty-fixture-producer".into();
    tool.version = "1".into();
    metadata.tool_info = Some(tool).into();
    index.metadata = Some(metadata).into();
    let scip = temp.path().join("index.scip");
    fs::write(&scip, index.write_to_bytes().unwrap()).unwrap();
    engine
        .index(&json!({"scip":scip,"scip_manifest":inputs}), deadline())
        .unwrap();
    let published = engine.scan(false).unwrap();
    assert_eq!(
        published.epochs.semantic_epoch,
        published.epochs.syntax_epoch
    );
    git(&root, &["commit", "--allow-empty", "-qm", "new-context"]);
    let artifact = risk_inputs::collect(&resolved, None, Some(100), deadline()).unwrap();
    let commit = artifact.history.as_ref().unwrap().anchor_commit.clone();
    let pinned = json!({"kind":"snapshot","snapshot_id":published.snapshot.snapshot_id,"semantic_epoch":published.epochs.semantic_epoch});
    let service = QueryService::new(engine.clone());
    let report = service.execute("impact_of",json!({"diff":{"base":{"kind":"revision","revision":commit},"head":pinned},"risk_inputs":{"base":artifact}}),deadline()).unwrap();
    assert_eq!(
        report["snapshots"]["base"]["snapshot_id"],
        published.snapshot.snapshot_id
    );
    assert_eq!(report["snapshots"]["base"]["commit"], commit);
    assert_ne!(report["snapshots"]["head"]["commit"], commit);
    let mut contextual = pinned.clone();
    contextual["commit_context"] = json!(commit);
    let repeated = service
        .execute(
            "impact_of",
            json!({"diff":{"base":contextual,"head":pinned},"risk_inputs":{"base":artifact}}),
            deadline(),
        )
        .unwrap();
    assert_eq!(repeated["snapshots"], report["snapshots"]);
    assert_eq!(repeated["risk_summary"], report["risk_summary"]);
    assert!(
        service
            .execute(
                "impact_of",
                json!({"diff":{"base":pinned,"head":pinned},"risk_inputs":{"base":artifact}}),
                deadline()
            )
            .unwrap_err()
            .to_string()
            .starts_with("content_mismatch:")
    );
    fs::write(root.join("src/lib.rs"), "pub fn different() {}\n").unwrap();
    git(&root, &["add", "src/lib.rs"]);
    git(&root, &["commit", "-qm", "different-inputs"]);
    contextual["commit_context"] = json!("HEAD");
    assert!(
        service
            .execute(
                "impact_of",
                json!({"diff":{"base":contextual,"head":pinned}}),
                deadline()
            )
            .unwrap_err()
            .to_string()
            .starts_with("content_mismatch:")
    );
    engine.shutdown();
}
