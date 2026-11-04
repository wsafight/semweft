use semweft_query::{Engine, QueryService, config};
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn setup() -> (tempfile::TempDir, EngineFixture) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"mapped\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("semweft.toml"),"schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n[[services]]\nname=\"front\"\npaths=[\"src/a.rs\"]\n[[services]]\nname=\"middle\"\npaths=[\"src/b.rs\"]\n[[services]]\nname=\"back\"\npaths=[\"src/c.rs\"]\n").unwrap();
    fs::write(root.join("src/lib.rs"), "mod a; mod b; mod c;\n").unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() { beta(); }\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn beta() { gamma(); }\n").unwrap();
    fs::write(root.join("src/c.rs"), "pub fn gamma() { alpha(); }\n").unwrap();
    let engine = Engine::new(config::resolve(Some(root), None).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    (temp, EngineFixture { engine, service })
}

struct EngineFixture {
    engine: std::sync::Arc<Engine>,
    service: QueryService,
}
impl Drop for EngineFixture {
    fn drop(&mut self) {
        self.engine.shutdown();
    }
}

#[test]
fn mapped_service_cycles_paths_diff_and_fixed_pages_preserve_witnesses() {
    let (temp, fixture) = setup();
    let base = fixture.engine.scan(false).unwrap();
    let mapping = fixture
        .service
        .execute(
            "repo_map",
            json!({"dependencies":"service","cycles":true,"limit":1000}),
            deadline(),
        )
        .unwrap();
    assert_eq!(mapping["cycle_summary"]["services_included"], true);
    assert_eq!(
        mapping["cycles"][0]["nodes"],
        json!(["back", "front", "middle"])
    );
    assert_eq!(mapping["dependencies"].as_array().unwrap().len(), 3);
    for edge in mapping["cycles"][0]["dependencies"].as_array().unwrap() {
        assert!(edge["mapping_digest"].is_string());
        assert_eq!(edge["relation_ids"][0], edge["evidence"][0]["relation_id"]);
        assert_eq!(edge["certainty"], "candidate");
    }
    for (scope, from, to) in [
        ("service", "front", "back"),
        ("module", "src/a.rs", "src/c.rs"),
    ] {
        let path = fixture
            .service
            .execute(
                "find_paths",
                json!({"scope":scope,"from":from,"to":to,"limit":1000}),
                deadline(),
            )
            .unwrap();
        assert_eq!(path["paths"][0]["length"], 2, "{path}");
        assert_eq!(path["paths"][0]["relations"].as_array().unwrap().len(), 2);
        assert!(
            path["path_summary"]["interpretation"]
                .as_str()
                .unwrap()
                .contains("need not share")
        );
        let limited = fixture
            .service
            .execute(
                "find_paths",
                json!({"scope":scope,"from":from,"to":to,"max_depth":1}),
                deadline(),
            )
            .unwrap();
        assert_eq!(limited["path_summary"]["enumeration_complete"], false);
        assert_eq!(
            limited["output"]["continuation"]["suggested"]["scope"],
            scope
        );
        let filtered = fixture
            .service
            .execute(
                "find_paths",
                json!({"scope":scope,"from":from,"to":to,"certainty":["definite"]}),
                deadline(),
            )
            .unwrap();
        assert_eq!(filtered["paths"], json!([]));
    }
    let mut page = fixture
        .service
        .execute(
            "repo_map",
            json!({"dependencies":"service","cycles":true,"limit":1}),
            deadline(),
        )
        .unwrap();
    let pinned = page["snapshot_id"].clone();
    let summary = page["service_mapping"].clone();
    fs::write(temp.path().join("src/c.rs"), "pub fn gamma() {}\n").unwrap();
    fixture.engine.scan(false).unwrap();
    let mut edges = vec![];
    loop {
        assert_eq!(page["snapshot_id"], pinned);
        assert_eq!(page["service_mapping"], summary);
        edges.extend(page["dependencies"].as_array().unwrap().iter().cloned());
        let Some(cursor) = page["output"]["continuation"]["next_cursor"].as_str() else {
            break;
        };
        page = fixture
            .service
            .execute("repo_map", json!({"cursor":cursor}), deadline())
            .unwrap();
    }
    assert_eq!(edges.len(), 3);
    let clean = fixture
        .service
        .execute(
            "repo_map",
            json!({"dependencies":"service","cycles":true}),
            deadline(),
        )
        .unwrap();
    assert_eq!(clean["cycle_summary"]["component_count"], 0);
    let diff = json!({"base":{"kind":"snapshot","snapshot_id":base.snapshot.snapshot_id,"semantic_epoch":0},"head":{"kind":"workspace"}});
    let report = fixture
        .service
        .execute("impact_of", json!({"diff":diff,"limit":1000}), deadline())
        .unwrap();
    assert_eq!(
        report["service_impact_summary"]["affected_services_observed"], 3,
        "{report}"
    );
    assert!(
        report["affected_services"]
            .as_array()
            .unwrap()
            .iter()
            .any(|service| service["name"] == "back"
                && service["changed_files"] == json!(["src/c.rs"]))
    );
    for module in report["affected_modules"].as_array().unwrap() {
        assert!(module["service"].is_string());
    }
}

#[test]
fn mapping_validation_rejects_ambiguous_ownership_and_unknown_endpoints() {
    let (temp, fixture) = setup();
    assert!(
        fixture
            .service
            .execute(
                "find_paths",
                json!({"scope":"service","from":"missing","to":"back"}),
                deadline()
            )
            .is_err()
    );
    assert!(
        fixture
            .service
            .execute(
                "find_paths",
                json!({"scope":"module","from":"../outside","to":"src/c.rs"}),
                deadline()
            )
            .is_err()
    );
    let file = temp.path().join("semweft.toml");
    let config = fs::read_to_string(&file).unwrap();
    fs::write(
        &file,
        format!("{config}\n[[services]]\nname=\"overlap\"\npaths=[\"src/**\"]\n"),
    )
    .unwrap();
    let error = fixture
        .service
        .execute("repo_map", json!({"dependencies":"service"}), deadline())
        .unwrap_err();
    assert!(error.to_string().contains("multiple services"), "{error}");
    let invalid: Vec<semweft_query::services::Service> =
        serde_json::from_value(json!([{"name":"bad","paths":["../outside"]}])).unwrap();
    assert!(semweft_query::services::validate(&invalid).is_err());
    let duplicate: Vec<semweft_query::services::Service> = serde_json::from_value(
        json!([{"name":"same","paths":["src/a.rs"]},{"name":"same","paths":["src/b.rs"]}]),
    )
    .unwrap();
    assert!(semweft_query::services::validate(&duplicate).is_err());
}

#[test]
fn revision_rebinds_external_service_context_while_reusing_semantic_results() {
    use protobuf::Message;
    use std::process::Command;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"contexts\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn entry() {}\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &["commit", "-qm", "base"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let config_path = temp.path().join("external.toml");
    fs::write(&config_path,"schema_version=\"0.1\"\n[workspace]\nroot=\"repo\"\n[index]\ndirectory=\"index\"\n[[services]]\nname=\"external\"\npaths=[\"src/**\"]\n").unwrap();
    let resolved = config::resolve(Some(&root), Some(&config_path)).unwrap();
    let engine = Engine::new(resolved.clone()).unwrap();
    let inputs = engine
        .index(
            &json!({"syntax_only":true,"export_manifest":true}),
            deadline(),
        )
        .unwrap();
    let manifest = temp.path().join("sources.json");
    fs::write(
        &manifest,
        serde_json::to_vec(&inputs["source_manifest"]).unwrap(),
    )
    .unwrap();
    let mut index = scip::types::Index::new();
    let mut metadata = scip::types::Metadata::new();
    let mut producer = scip::types::ToolInfo::new();
    producer.name = "service-context-fixture".into();
    producer.version = "1".into();
    metadata.tool_info = Some(producer).into();
    index.metadata = Some(metadata).into();
    let path = temp.path().join("index.scip");
    fs::write(&path, index.write_to_bytes().unwrap()).unwrap();
    engine
        .index(&json!({"scip":path,"scip_manifest":manifest}), deadline())
        .unwrap();
    let published = engine.scan(false).unwrap();
    assert_eq!(
        published.epochs.semantic_epoch,
        published.epochs.syntax_epoch
    );
    let service = QueryService::new(engine.clone());
    let pinned = json!({"kind":"snapshot","snapshot_id":published.snapshot.snapshot_id,"semantic_epoch":published.epochs.semantic_epoch});
    let report = service
        .execute(
            "impact_of",
            json!({"diff":{"base":{"kind":"revision","revision":"HEAD"},"head":pinned}}),
            deadline(),
        )
        .unwrap();
    assert_ne!(
        report["snapshots"]["base"]["snapshot_id"],
        published.snapshot.snapshot_id
    );
    assert_eq!(
        report["snapshots"]["base"]["semantic_reused_from"],
        published.snapshot.snapshot_id
    );
    let rebound = engine
        .historical(
            report["snapshots"]["base"]["snapshot_id"].as_str().unwrap(),
            report["snapshots"]["base"]["epochs"]["semantic"].as_u64(),
        )
        .unwrap();
    assert_eq!(
        rebound.view.epochs.semantic_epoch,
        rebound.view.epochs.syntax_epoch
    );
    assert_eq!(rebound.view.resolutions, published.resolutions);
    assert_eq!(
        report["service_impact_summary"]["base"]["services"],
        json!([])
    );
    assert_eq!(
        report["service_impact_summary"]["head"]["services"][0]["name"],
        "external"
    );
    assert_eq!(
        engine.scan(false).unwrap().snapshot.snapshot_id,
        published.snapshot.snapshot_id
    );
    engine.shutdown();
}
