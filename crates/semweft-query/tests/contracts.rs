use semweft_query::{Engine, QueryService, config};
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
fn protobuf_changes_bind_wire_rules_generated_candidates_and_fixed_diff_pages() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::create_dir(root.join("proto")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"contracts\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("semweft.toml"),"schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n[[contracts.mappings]]\nidl=\"proto/stock.proto\"\ncontract=\"demo.Stock.count\"\nsymbol=\"count\"\ngenerator=\"prost@0.14.4\"\n").unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct Stock { pub count: i32 }\npub fn read(stock: Stock) -> i32 { stock.count }\n",
    )
    .unwrap();
    let proto = "syntax = \"proto3\";\npackage demo;\nmessage Stock { int32 count = 1; }\nservice Inventory { rpc Get(Stock) returns (Stock); }\n";
    fs::write(root.join("proto/stock.proto"), proto).unwrap();
    let engine = Engine::new(config::resolve(Some(root), None).unwrap()).unwrap();
    let base = engine.scan(false).unwrap();
    fs::write(
        root.join("proto/stock.proto"),
        proto
            .replace("count = 1", "count = 3")
            .replace("rpc Get(Stock) returns (Stock);", ""),
    )
    .unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let diff = json!({"base":{"kind":"snapshot","snapshot_id":base.snapshot.snapshot_id,"semantic_epoch":0},"head":{"kind":"workspace"}});
    let result = service
        .execute(
            "contract_impact",
            json!({"diff":diff,"limit":1000}),
            deadline(),
        )
        .unwrap();
    assert_eq!(result["contract_summary"]["change_count"], 2);
    assert_eq!(result["verification"]["status"], "violations_found");
    for rule in ["protobuf_field_number_changed", "protobuf_rpc_removed"] {
        assert!(
            result["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["rule_id"] == rule)
        );
    }
    assert_eq!(result["contract_impacts"][0]["certainty"], "candidate");
    assert_eq!(
        result["contract_impacts"][0]["impact_level"],
        "field_candidate"
    );
    let fixed = json!({"base":diff["base"],"head":{"kind":"snapshot","snapshot_id":result["snapshots"]["head"]["snapshot_id"],"semantic_epoch":result["snapshots"]["head"]["epochs"]["semantic"]}});
    let mut page = service
        .execute(
            "contract_impact",
            json!({"diff":fixed,"limit":1}),
            deadline(),
        )
        .unwrap();
    let mut changes = vec![];
    loop {
        assert_eq!(page["contract_summary"], result["contract_summary"]);
        changes.extend(page["contract_changes"].as_array().unwrap().iter().cloned());
        let Some(cursor) = page["output"]["continuation"]["next_cursor"].as_str() else {
            break;
        };
        page = service
            .execute("contract_impact", json!({"cursor":cursor}), deadline())
            .unwrap();
    }
    assert_eq!(changes, *result["contract_changes"].as_array().unwrap());
    fs::write(root.join("proto/stock.proto"), "message Invalid {").unwrap();
    let invalid = service
        .execute("contract_impact", json!({"diff":diff}), deadline())
        .unwrap();
    assert_eq!(invalid["contract_changes"], json!([]));
    assert!(
        invalid["completeness"]["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["kind"] == "protobuf_parse_failed")
    );
    engine.shutdown();
}
