use super::*;

fn current_calls(ws: &Workspace, symbol: &str) -> BTreeSet<String> {
    let report = ws.ok(&[
        "calls",
        symbol,
        "--direction",
        "callees",
        "--source",
        "L3",
        "--certainty",
        "definite",
        "--freshness",
        "current",
        "--all-pages",
    ]);
    assert!(
        !report["relations"].as_array().unwrap().is_empty(),
        "{report}"
    );
    report["relations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|relation| {
            serde_json::to_string(&json!({
                "relation_id":relation["relation_id"],
                "from":relation["from_name"], "to":relation["to_name"],
                "kind":relation["kind"], "file":relation["location"]["file"],
                "span":relation["span"]
            }))
            .unwrap()
        })
        .collect()
}

fn real_producer_refresh_and_cold_consistency(web: bool) {
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_file(ws.root.join("src/lib.rs")).unwrap();
    let (inventory, order, source, caller, original, replacement) = if web {
        fs::write(
            ws.root.join("package.json"),
            r#"{"name":"semweft-producer-test","version":"1.0.0","private":true}"#,
        )
        .unwrap();
        fs::write(ws.root.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"baseUrl":".","paths":{"@shop/*":["src/*"]}},"include":["src/**/*.ts"]}"#).unwrap();
        (
            "src/inventory.ts",
            "src/order.ts",
            "export function stock(): number { return 1; }\nexport function backup(): number { return 2; }\n",
            "order",
            "import { stock, backup } from '@shop/inventory';\nexport function order(): number { const label = '😀'; return stock(); }\n",
            "import { stock, backup } from '@shop/inventory';\nexport function order(): number { const label = '😀'; return backup(); }\n",
        )
    } else {
        fs::write(ws.root.join("go.mod"), "module example.com/shop\ngo 1.22\n").unwrap();
        (
            "inventory.go",
            "order.go",
            "package shop\nfunc Stock() int { return 1 }\nfunc Backup() int { return 2 }\n",
            "Order",
            "package shop\nfunc Order() int { _ = \"😀\"; return Stock() }\n",
            "package shop\nfunc Order() int { _ = \"😀\"; return Backup() }\n",
        )
    };
    fs::write(ws.root.join(inventory), source).unwrap();
    fs::write(ws.root.join(order), original).unwrap();
    ws.ok(&["index", "--syntax-only"]);
    ws.ok(&["trust", "--accept-build-execution"]);
    ws.ok(&["index"]);
    let before = current_calls(&ws, caller);
    assert_eq!(before.len(), 1, "{before:?}");
    assert!(
        before
            .iter()
            .all(|relation| relation.contains(if web { "stock" } else { "Stock" }))
    );
    fs::write(ws.root.join(order), replacement).unwrap();
    ws.ok(&["index"]);
    let refreshed = current_calls(&ws, caller);
    assert_eq!(refreshed.len(), 1, "{refreshed:?}");
    assert!(refreshed.iter().all(|relation| relation.contains(if web {
        "backup"
    } else {
        "Backup"
    })));
    assert_ne!(before, refreshed);
    ws.stop_and_wait();
    fs::rename(
        ws.temp.path().join("cache"),
        ws.temp.path().join("warm-cache"),
    )
    .unwrap();
    ws.ok(&["index"]);
    assert_eq!(current_calls(&ws, caller), refreshed);
    ws.ok(&["trust", "--revoke"]);
    assert_eq!(ws.run(&["index"]).1["error"]["code"], "trust_required");
}

#[cfg(feature = "go-experimental")]
#[test]
#[ignore = "Requires a real scip-go on PATH or SEMWEFT_SCIP_GO; explicitly trusts a temporary project"]
fn real_go_scip_refresh_matches_cold() {
    real_producer_refresh_and_cold_consistency(false);
}

#[cfg(feature = "web-experimental")]
#[test]
#[ignore = "Requires a real scip-typescript on PATH or SEMWEFT_SCIP_TYPESCRIPT; explicitly trusts a temporary project"]
fn real_web_scip_refresh_matches_cold() {
    real_producer_refresh_and_cold_consistency(true);
}

#[cfg(feature = "web-experimental")]
#[test]
#[ignore = "Requires real scip-typescript; checks default-export bindings and producer gaps in JavaScript"]
fn real_javascript_default_exports_keep_body_bindings_and_anonymous_import_gap() {
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_file(ws.root.join("src/lib.rs")).unwrap();
    fs::write(
        ws.root.join("package.json"),
        r#"{"name":"anonymous-default","version":"1.0.0","private":true}"#,
    )
    .unwrap();
    fs::write(
        ws.root.join("tsconfig.json"),
        r#"{"compilerOptions":{"allowJs":true,"checkJs":true},"include":["src/**/*.js"]}"#,
    )
    .unwrap();
    fs::write(
        ws.root.join("src/parser.js"),
        "export default function () { return target(); }\nfunction target() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        ws.root.join("src/entry.js"),
        "import parse from './parser';\nimport named from './named';\nexport function run() { return parse() + named(); }\n",
    )
    .unwrap();
    fs::write(
        ws.root.join("src/named.js"),
        "export default function named() { return 1; }\n",
    )
    .unwrap();
    ws.ok(&["trust", "--accept-build-execution"]);
    ws.ok(&["index"]);
    let imported = current_calls(&ws, "run");
    assert_eq!(imported.len(), 1);
    assert!(imported.iter().all(|relation| relation.contains("named")));
    let report = ws.ok(&["calls", "run", "--source", "L3", "--certainty", "definite"]);
    assert!(
        report["completeness"]["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["kind"] == "scip_unresolved_call"
                && gap["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("parse")))
    );
    let body = current_calls(&ws, "default");
    assert_eq!(body.len(), 1);
    assert!(body.iter().all(|relation| relation.contains("target")));
}

#[cfg(feature = "go-experimental")]
#[test]
#[ignore = "Requires real scip-go and Go; checks configured build tags in a trusted temporary project"]
fn real_go_build_tags_change_bindings_and_preserve_pinned_views() {
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_file(ws.root.join("src/lib.rs")).unwrap();
    fs::write(ws.root.join("go.mod"), "module example.com/tags\ngo 1.22\n").unwrap();
    fs::write(
        ws.root.join("order.go"),
        "package tags\nfunc Order() int { return Selected() }\nfunc Stock() int { return 1 }\nfunc Backup() int { return 2 }\n",
    )
    .unwrap();
    fs::write(
        ws.root.join("default.go"),
        "//go:build !semweft_acceptance\n\npackage tags\nfunc Selected() int { return Stock() }\n",
    )
    .unwrap();
    fs::write(
        ws.root.join("selected.go"),
        "//go:build semweft_acceptance\n\npackage tags\nfunc Selected() int { return Backup() }\n",
    )
    .unwrap();
    fs::write(
        ws.root.join("semweft.toml"),
        "schema_version='0.1'\n[go]\ntags=[]\n",
    )
    .unwrap();
    ws.ok(&["trust", "--accept-build-execution"]);
    ws.ok(&["index"]);
    let before = current_calls(&ws, "Order");
    assert_eq!(before.len(), 2);
    assert!(before.iter().any(|relation| relation.contains("Stock")));
    assert!(!before.iter().any(|relation| relation.contains("Backup")));
    let check_activation = |active: &str, inactive: &str| {
        for (path, expected) in [(active, 1), (inactive, 0)] {
            let search = ws.ok(&["search", "Selected", "--match", "exact", "--path", path]);
            let symbols = search["symbols"].as_array().unwrap();
            assert_eq!(symbols.len(), 1, "{search}");
            let report = ws.ok(&[
                "calls",
                symbols[0]["symbol_id"].as_str().unwrap(),
                "--direction",
                "callees",
                "--source",
                "L3",
                "--certainty",
                "definite",
                "--all-pages",
            ]);
            assert_eq!(
                report["relations"].as_array().unwrap().len(),
                expected,
                "{path}: {report}"
            );
        }
    };
    check_activation("default.go", "selected.go");
    let pinned = ws.ok(&[
        "calls",
        "Order",
        "--source",
        "L3",
        "--certainty",
        "definite",
    ]);
    fs::write(
        ws.root.join("semweft.toml"),
        "schema_version='0.1'\n[go]\ntags=['semweft_acceptance']\n",
    )
    .unwrap();
    ws.ok(&["index"]);
    let after = current_calls(&ws, "Order");
    assert_eq!(after.len(), 2);
    assert!(after.iter().any(|relation| relation.contains("Backup")));
    assert!(!after.iter().any(|relation| relation.contains("Stock")));
    assert_ne!(before, after);
    check_activation("selected.go", "default.go");
    let history = ws.ok(&[
        "calls",
        "Order",
        "--source",
        "L3",
        "--certainty",
        "definite",
        "--snapshot-id",
        pinned["snapshot_id"].as_str().unwrap(),
    ]);
    assert_eq!(history["relations"], pinned["relations"]);
    ws.stop_and_wait();
    fs::rename(
        ws.temp.path().join("cache"),
        ws.temp.path().join("warm-cache"),
    )
    .unwrap();
    ws.ok(&["index"]);
    assert_eq!(current_calls(&ws, "Order"), after);
    check_activation("selected.go", "default.go");
    let output = Command::new("go")
        .current_dir(&ws.root)
        .env("GOFLAGS", "-tags=semweft_acceptance")
        .env("GOPROXY", "off")
        .args(["test", "./..."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "web-experimental")]
#[test]
#[ignore = "Requires Node.js with its LCOV test reporter; executes only a generated temporary test"]
fn real_node_lcov_binds_coverage_to_frozen_source_inputs() {
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_file(ws.root.join("src/lib.rs")).unwrap();
    fs::write(
        ws.root.join("package.json"),
        "{\"name\":\"coverage-fixture\",\"version\":\"1.0.0\",\"type\":\"module\"}",
    )
    .unwrap();
    let source =
        "export function stock(enabled) {\n  if (enabled) {\n    return 1;\n  }\n  return 0;\n}\n";
    let test_source = "import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { stock } from './inventory.mjs';\ntest('enabled stock', () => assert.equal(stock(true), 1));\n";
    fs::write(ws.root.join("src/inventory.mjs"), source).unwrap();
    fs::write(ws.root.join("src/inventory.test.mjs"), test_source).unwrap();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/inventory.mjs"),
        source.replace("return 1", "return 2"),
    )
    .unwrap();
    fs::write(
        ws.root.join("src/inventory.test.mjs"),
        test_source.replace("stock(true), 1", "stock(true), 2"),
    )
    .unwrap();
    let inputs = ws.temp.path().join("inputs.json");
    ws.ok(&[
        "index",
        "--syntax-only",
        "--export-manifest",
        inputs.to_str().unwrap(),
    ]);
    let coverage = ws.temp.path().join("coverage.info");
    let output = Command::new("node")
        .current_dir(&ws.root)
        .args([
            "--test",
            "--experimental-test-coverage",
            "--test-reporter=lcov",
        ])
        .arg(format!(
            "--test-reporter-destination={}",
            coverage.display()
        ))
        .arg("src/inventory.test.mjs")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let artifact = ws.ok(&[
        "risk-inputs",
        "--coverage",
        coverage.to_str().unwrap(),
        "--source-manifest",
        inputs.to_str().unwrap(),
        "--history-commits",
        "100",
    ]);
    let source = artifact["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "src/inventory.mjs")
        .unwrap();
    let lines = source["lines"].as_array().unwrap();
    assert!(
        lines.iter().any(|line| line["hits"].as_u64().unwrap() > 0),
        "{artifact}"
    );
    assert!(lines.iter().any(|line| line["hits"] == 0), "{artifact}");
    let path = ws.temp.path().join("risk.json");
    fs::write(&path, serde_json::to_vec(&artifact).unwrap()).unwrap();
    let report = ws.ok(&[
        "impact",
        "--diff",
        "WORKTREE",
        "--risk-inputs",
        path.to_str().unwrap(),
        "--all-pages",
    ]);
    assert!(
        report["risk_summary"]["dimensions"]["test_coverage"]["uncovered_lines"]
            .as_u64()
            .unwrap()
            > 0,
        "{report}"
    );
    assert_eq!(
        report["risk_summary"]["dimensions"]["test_coverage"]["executes_tests"],
        false
    );
}
