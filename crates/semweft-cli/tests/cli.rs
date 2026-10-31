use serde_json::Value;
use std::{path::PathBuf, process::Command};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_semweft"))
}
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/rust-refresh")
}

#[test]
fn scan_exposes_syntax_and_boundaries_without_starting_l3() {
    let output = binary()
        .args(["prototype", "scan", "--workspace"])
        .arg(fixture())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["epochs"]["syntax_epoch"], 1);
    assert_eq!(response["epochs"]["semantic_epoch"], 0);
    assert!(response["symbols"].as_array().unwrap().len() >= 8);
    assert!(response["call_sites"].as_array().unwrap().len() >= 4);
    assert_eq!(response["completeness"]["status"], "partial");
    assert_eq!(response["completeness"]["L3"], "not_requested");
    assert_eq!(
        response["completeness"]["pending_units"],
        serde_json::json!([])
    );
}

#[test]
fn missing_trust_fails_before_starting_an_external_tool() {
    let output = binary()
        .args([
            "prototype",
            "check",
            "--rust-analyzer",
            "/does/not/exist",
            "--workspace",
        ])
        .arg(fixture())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["error"]["code"], "trust_required");
}

#[test]
fn capabilities_does_not_need_a_workspace_or_indexer() {
    let empty = tempfile::tempdir().unwrap();
    let output = binary()
        .arg("capabilities")
        .current_dir(empty.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["languages"]["rust"]["L2"]["status"], "unsupported");
    assert_eq!(response["stage"], "mvp_experimental");
    assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0);
}

#[test]
#[ignore = "Requires rust-analyzer and rust-src; runs the trusted local fixture only"]
fn lsp_refresh_matches_cold_with_annotated_targets() {
    let root = fixture();
    let original = std::fs::read(root.join("src/inventory.rs")).unwrap();
    let output = binary()
        .args(["prototype", "check", "--workspace"])
        .arg(&root)
        .arg("--edits")
        .arg(root.join("../rust-refresh-edits.json"))
        .arg("--accept-build-execution")
        .arg("--timeout-ms")
        .arg("60000")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["matches_cold"], true);
    assert_eq!(response["source_inputs_unchanged"], true);
    let steps = response["steps"].as_array().unwrap();
    let published: Vec<_> = steps.iter().filter(|s| s["published"] == true).collect();
    let counts: Vec<_> = published
        .iter()
        .map(|step| step["relations"].as_u64().unwrap())
        .collect();
    assert_eq!(counts, [5, 5, 5, 8, 5, 7, 8, 8, 8]);
    let resolves = |step: &Value, caller: &str, target: &str, certainty: &str| {
        step["resolved_calls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|call| {
                call["caller"] == caller
                    && call["target"] == target
                    && call["certainty"] == certainty
            })
    };
    let dispatches_to = |step: &Value, target: &str| {
        step["resolved_calls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|call| {
                call["caller"] == "src/lib.rs::dispatch"
                    && call["target"] == target
                    && call["certainty"] == "candidate"
                    && call["derivation"] == "lsp_implementation+syntax_callsite"
            })
    };
    // The persistent session must follow implementation-set changes.
    let alternate = "src/alternate.rs::<Deduct for Alternate>::deduct";
    assert!(!dispatches_to(&steps[0], alternate));
    assert!(dispatches_to(&steps[3], alternate));
    assert!(!dispatches_to(&steps[4], alternate));
    for step in &published {
        assert!(dispatches_to(
            step,
            "src/lib.rs::<Deduct for Stock>::deduct"
        ));
        // Independently annotated direct binding. Equality with a second run
        // alone would also pass if both backends returned an empty graph.
        assert!(resolves(
            step,
            "src/order.rs::create",
            "src/inventory.rs::available",
            "definite"
        ));
        assert!(resolves(
            step,
            "src/lib.rs::dispatch",
            "src/lib.rs::Deduct::deduct",
            "candidate"
        ));
    }
    let mode = |step: &Value| step["plan"]["scope"]["mode"].clone();
    assert_eq!(mode(&steps[1]), "body_local");
    assert_eq!(steps[1]["plan"]["queried_callsites"], 0);
    assert_eq!(mode(&steps[2]), "full");
    // An interrupted refresh never publishes; the next plan covers its edits.
    assert_eq!(steps[5]["published"], false);
    assert_eq!(steps[5]["publication_rejected"], "superseded");
    assert_eq!(
        steps[6]["plan"]["base_syntax_epoch"],
        steps[4]["epochs"]["syntax_epoch"]
    );
    assert_eq!(
        steps[6]["plan"]["scope"]["changed_files"],
        serde_json::json!(["src/inventory.rs", "src/lib.rs", "src/order.rs"])
    );
    // A crashed backend is restarted; cfg-inactive code is only a candidate
    // until the Cargo feature is enabled behind the reload barrier.
    assert_eq!(steps[7]["backend_restarts"], 1);
    let boost = ("src/extra.rs::boost", "src/inventory.rs::available");
    assert!(resolves(&steps[7], boost.0, boost.1, "candidate"));
    assert!(resolves(&steps[8], boost.0, boost.1, "definite"));
    assert_eq!(mode(&steps[9]), "body_local");
    assert!(steps[9]["plan"]["reused_callsites"].as_u64().unwrap() > 0);
    assert_eq!(
        std::fs::read(root.join("src/inventory.rs")).unwrap(),
        original
    );
}
