#![cfg(feature = "web-experimental")]
use semweft_query::{Engine, QueryService, config};
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
fn jsonc_inheritance_aliases_and_literal_dynamic_modules_are_candidate_dependencies() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("package.json"), "{\"name\":\"web-modules\"}").unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(
        root.join("tsconfig.base.json"),
        "{/*jsonc*/\"compilerOptions\":{\"baseUrl\":\"./src\",\"paths\":{\"@/*\":[\"*\"],},},}",
    )
    .unwrap();
    fs::write(
        root.join("tsconfig.json"),
        "{\"extends\":\"./tsconfig.base.json\"}",
    )
    .unwrap();
    fs::write(root.join("src/main.ts"),"import { value } from '@/logic';\nconst lazy = import('./lazy.js');\nconst legacy = require('./legacy');\nconst unknown = import(variable);\n").unwrap();
    for path in ["logic.ts", "lazy.ts", "legacy.ts"] {
        fs::write(
            root.join("src").join(path),
            "export function value() { return 1; }\n",
        )
        .unwrap();
    }
    let engine = Engine::new(config::resolve(Some(root), None).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let result = service
        .execute(
            "repo_map",
            json!({"dependencies":"module","limit":1000}),
            deadline(),
        )
        .unwrap();
    for specifier in ["@/logic", "./lazy.js", "./legacy"] {
        let dependency = result["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["specifier"] == specifier)
            .unwrap();
        assert_eq!(dependency["certainty"], "candidate");
    }
    assert!(
        result["completeness"]["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["kind"] == "dynamic_module_specifier")
    );
    let snapshot = result["snapshot_id"].clone();
    fs::write(
        root.join("tsconfig.base.json"),
        "{\"compilerOptions\":{\"baseUrl\":\"./src\",\"paths\":{\"@/*\":[\"absent/*\"]}}}",
    )
    .unwrap();
    let changed = service
        .execute(
            "repo_map",
            json!({"dependencies":"module","limit":1000}),
            deadline(),
        )
        .unwrap();
    assert_ne!(snapshot, changed["snapshot_id"]);
    assert!(
        !changed["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["specifier"] == "@/logic")
    );
    let pinned = service.execute("repo_map",json!({"dependencies":"module","snapshot_id":snapshot,"semantic_epoch":result["epochs"]["semantic"],"limit":1000}),deadline()).unwrap();
    assert_eq!(pinned["dependencies"], result["dependencies"]);
    engine.shutdown();
}
