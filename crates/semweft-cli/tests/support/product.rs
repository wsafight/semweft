use super::*;

fn hook(ws: &Workspace, host: &str, event: &str, tool_id: &str) -> Value {
    use std::io::Write;
    let tool = if host == "claude" {
        "Edit"
    } else {
        "apply_patch"
    };
    let input = if host == "claude" {
        json!({"file_path":"src/lib.rs"})
    } else {
        json!({"command":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch"})
    };
    let mut child = ws
        .command()
        .args(["hook", "--host", host])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&serde_json::to_vec(&json!({"hook_event_name":event,"cwd":ws.root,"session_id":"test-session","tool_use_id":tool_id,"tool_name":tool,"tool_input":input})).unwrap()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn public_inventory_baseline_review_and_offline_html_work_through_cli() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[[architecture.layers]]\nname=\"domain\"\npaths=[\"src/**\"]\n",
    )
    .unwrap();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    let inventory = ws.ok(&[
        "search",
        "--visibility",
        "public",
        "--all-pages",
        "--limit",
        "1",
    ]);
    assert_eq!(inventory["symbols"].as_array().unwrap().len(), 3);
    assert_eq!(inventory["api_summary"]["declaration_count"], 3);
    let external = ws.ok(&[
        "search",
        "--api-scope",
        "external",
        "--all-pages",
        "--limit",
        "1",
    ]);
    assert_eq!(
        external["api_summary"]["external_reachability"],
        "candidate"
    );
    assert_eq!(external["symbols"].as_array().unwrap().len(), 3);
    assert!(external["symbols"][0]["api_exposure"]["paths"][0]["path"].is_string());
    let cycles = ws.ok(&["map", "--cycles", "--all-pages"]);
    assert_eq!(cycles["cycle_summary"]["component_count"], 0);
    let baseline = ws.root.join("baseline.json");
    let (code, arch) = ws.run(&["arch-check", "--write-baseline", baseline.to_str().unwrap()]);
    assert_eq!(code, 3, "{arch}");
    assert!(baseline.is_file());
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 2 }")).unwrap();
    let (code, report) = ws.run(&["guard", "after", "--diff", "WORKTREE", "--all-pages"]);
    assert_eq!(code, 3, "{report}");
    let input = ws.temp.path().join("review.json");
    fs::write(&input, serde_json::to_vec(&report).unwrap()).unwrap();
    let record = ws.ok(&[
        "review",
        "save",
        "--input",
        input.to_str().unwrap(),
        "--conclusion",
        "approve",
        "--note",
        "manual review",
    ]);
    let id = record["review_id"].as_str().unwrap();
    let review = ws.ok(&["review", "get", id]);
    assert_eq!(review["review"]["validity"]["approval_applies"], true);
    let html = ws
        .command()
        .args([
            "report",
            "--input",
            input.to_str().unwrap(),
            "--format",
            "html",
        ])
        .output()
        .unwrap();
    assert_eq!(html.status.code(), Some(3));
    assert!(
        String::from_utf8(html.stdout)
            .unwrap()
            .starts_with("<!doctype html>")
    );
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 3 }")).unwrap();
    assert_eq!(
        ws.ok(&["review", "get", id])["review"]["validity"]["approval_applies"],
        false
    );
}

#[test]
fn both_hook_json_adapters_pair_receipts_and_deny_protected_edits() {
    for host in ["claude", "codex"] {
        let ws = Workspace::new();
        let before = hook(&ws, host, "PreToolUse", "one");
        assert_ne!(
            before["hookSpecificOutput"]["permissionDecision"], "deny",
            "{before}"
        );
        fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 2 }")).unwrap();
        let after = hook(&ws, host, "PostToolUse", "one");
        assert!(
            after["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("PostToolUse")
        );
        assert!(!after.to_string().contains("failed closed"), "{after}");
        fs::write(ws.root.join("semweft.toml"),"schema_version=\"0.1\"\n[[guard.protected]]\nname=\"core\"\npaths=[\"src/**\"]\nreason=\"manual only\"\n").unwrap();
        let denied = hook(&ws, host, "PreToolUse", "two");
        assert_eq!(
            denied["hookSpecificOutput"]["permissionDecision"], "deny",
            "{denied}"
        );
        assert!(
            denied["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("Protected region"),
            "{denied}"
        );
    }
}

#[test]
fn frozen_risk_artifacts_flow_through_cli_mcp_and_human_reports() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 2 }")).unwrap();
    let manifest = ws.temp.path().join("sources.json");
    ws.ok(&[
        "index",
        "--syntax-only",
        "--export-manifest",
        manifest.to_str().unwrap(),
    ]);
    let coverage = ws.temp.path().join("coverage.info");
    fs::write(&coverage, "SF:src/lib.rs\nDA:2,0\nend_of_record\n").unwrap();
    let artifact = ws.ok(&[
        "risk-inputs",
        "--coverage",
        coverage.to_str().unwrap(),
        "--source-manifest",
        manifest.to_str().unwrap(),
        "--history-commits",
        "100",
    ]);
    assert_eq!(artifact["history"]["commits_observed"], 1);
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
    assert_eq!(report["risk_summary"]["scoring"], "heuristic_v2");
    assert_eq!(
        report["risk_summary"]["dimensions"]["test_coverage"]["status"],
        "partial"
    );
    assert_eq!(
        report["risk_summary"]["dimensions"]["test_coverage"]["uncovered_lines"],
        1
    );
    assert_eq!(report["risk_summary"]["dimensions"]["hotspot"]["score"], 15);
    let (code, preflight) = ws.run(&[
        "guard",
        "before",
        "--file",
        "src/lib.rs",
        "--risk-inputs",
        path.to_str().unwrap(),
        "--all-pages",
    ]);
    assert!(matches!(code, 0 | 3), "{preflight}");
    assert_eq!(preflight["guard"]["hotspots"], "available");
    assert_eq!(
        preflight["guard"]["risk_observations"]["tests_executed"],
        false
    );
    for format in ["text", "markdown", "html"] {
        let output = ws
            .command()
            .args([
                "impact",
                "--diff",
                "WORKTREE",
                "--risk-inputs",
                path.to_str().unwrap(),
                "--format",
                format,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("partial"), "{text}");
        assert!(text.contains("行覆盖观测"), "{text}");
    }
    let sarif = ws
        .command()
        .args([
            "verify",
            "--diff",
            "WORKTREE",
            "--risk-inputs",
            path.to_str().unwrap(),
            "--format",
            "sarif",
        ])
        .output()
        .unwrap();
    assert_eq!(sarif.status.code(), Some(3));
    let sarif: Value = serde_json::from_slice(&sarif.stdout).unwrap();
    assert_eq!(sarif["runs"][0]["properties"]["coverage"], "partial");
    assert_eq!(sarif["runs"][0]["properties"]["testsExecuted"], false);
    assert_eq!(
        sarif["runs"][0]["properties"]["risk_summary"],
        report["risk_summary"]
    );
    let mut client = Mcp::new(&ws);
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"risk-test","version":"1"}}}));
    assert!(client.reply().get("result").is_some());
    client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"impact_of","arguments":{"diff":semweft_query::git::diff_selectors(&ws.root,"WORKTREE").unwrap(),"risk_inputs":{"head":artifact},"limit":1000}}}));
    assert_eq!(
        client.reply()["result"]["structuredContent"]["risk_summary"],
        report["risk_summary"]
    );
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 3 }")).unwrap();
    let (code, stale) = ws.run(&[
        "impact",
        "--diff",
        "WORKTREE",
        "--risk-inputs",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{stale}");
    assert_eq!(stale["error"]["code"], "content_mismatch");
}

#[test]
fn mapped_services_and_aggregate_paths_flow_through_cli_and_html() {
    let ws = Workspace::new();
    fs::write(ws.root.join("semweft.toml"),"schema_version=\"0.1\"\n[[services]]\nname=\"front\"\npaths=[\"src/lib.rs\"]\n[[services]]\nname=\"back\"\npaths=[\"src/store.rs\"]\n").unwrap();
    fs::write(
        ws.root.join("src/lib.rs"),
        "mod store; pub fn entry() { stock(); }\n",
    )
    .unwrap();
    fs::write(
        ws.root.join("src/store.rs"),
        "pub fn stock() { entry(); }\n",
    )
    .unwrap();
    let map = ws.ok(&[
        "map",
        "--dependencies",
        "service",
        "--cycles",
        "--all-pages",
        "--limit",
        "1",
    ]);
    assert_eq!(map["cycles"][0]["nodes"], json!(["back", "front"]));
    let path = ws.ok(&[
        "path",
        "front",
        "back",
        "--scope",
        "service",
        "--all-pages",
        "--limit",
        "1",
    ]);
    assert_eq!(path["paths"][0]["length"], 1);
    assert!(path["paths"][0]["relations"][0]["evidence"].is_array());
    let html = ws
        .command()
        .args([
            "path", "front", "back", "--scope", "service", "--format", "html",
        ])
        .output()
        .unwrap();
    assert!(html.status.success());
    let html = String::from_utf8(html.stdout).unwrap();
    assert!(html.contains("front → back"), "{html}");
    assert!(html.contains("need not share"));
}
