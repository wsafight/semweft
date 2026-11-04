use semweft_query::{Engine, QueryService, config};
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
fn module_cycles_return_internal_edge_evidence_and_pinned_summary() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"cycles\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "mod a; mod b;\n").unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() { beta(); }\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn beta() { alpha(); }\n").unwrap();
    let engine = Engine::new(config::resolve(Some(root), None).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let result = service
        .execute(
            "repo_map",
            json!({"cycles":true,"dependencies":"module","limit":1000}),
            deadline(),
        )
        .unwrap();
    assert_eq!(result["cycle_summary"]["component_count"], 1);
    assert_eq!(
        result["cycles"][0]["nodes"],
        json!(["src/a.rs", "src/b.rs"])
    );
    assert_eq!(
        result["cycles"][0]["dependencies"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(result["cycles"][0]["certainty"], "candidate");
    let mut page = service
        .execute(
            "repo_map",
            json!({"cycles":true,"dependencies":"module","limit":1}),
            deadline(),
        )
        .unwrap();
    let mut cycles = vec![];
    loop {
        assert_eq!(page["cycle_summary"], result["cycle_summary"]);
        cycles.extend(page["cycles"].as_array().unwrap().iter().cloned());
        let Some(cursor) = page["output"]["continuation"]["next_cursor"].as_str() else {
            break;
        };
        page = service
            .execute("repo_map", json!({"cursor":cursor}), deadline())
            .unwrap();
    }
    assert_eq!(cycles, *result["cycles"].as_array().unwrap());
    fs::write(root.join("src/b.rs"), "pub fn beta() {}\n").unwrap();
    let clean = service
        .execute(
            "repo_map",
            json!({"cycles":true,"dependencies":"module"}),
            deadline(),
        )
        .unwrap();
    assert_eq!(clean["cycle_summary"]["component_count"], 0);
    assert!(
        service
            .execute(
                "repo_map",
                json!({"cycles":true,"dependencies":"none"}),
                deadline()
            )
            .is_err()
    );
    engine.shutdown();
}
