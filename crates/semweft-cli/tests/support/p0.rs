use super::*;
use protobuf::Message;
use scip::types::{Document, Index, Metadata, Occurrence, PositionEncoding, ToolInfo};

fn write_scip(ws: &Workspace, source: &str, occurrences: Vec<Occurrence>, utf16: bool) -> PathBuf {
    let mut index = Index::new();
    let mut metadata = Metadata::new();
    let mut tool = ToolInfo::new();
    tool.name = "fixture-indexer".into();
    tool.version = "1.2.3".into();
    metadata.tool_info = Some(tool).into();
    index.metadata = Some(metadata).into();
    let mut document = Document::new();
    document.relative_path = "src/lib.rs".into();
    document.text = source.into();
    document.position_encoding = if utf16 {
        PositionEncoding::UTF16CodeUnitOffsetFromLineStart
    } else {
        PositionEncoding::UTF8CodeUnitOffsetFromLineStart
    }
    .into();
    document.occurrences = occurrences;
    index.documents.push(document);
    let path = ws.temp.path().join("index.scip");
    fs::write(&path, index.write_to_bytes().unwrap()).unwrap();
    path
}

fn occurrence(symbol: &str, range: &[i32], definition: bool) -> Occurrence {
    let mut value = Occurrence::new();
    value.symbol = symbol.into();
    value.range = range.into();
    value.symbol_roles = i32::from(definition);
    value
}

fn manifest(ws: &Workspace) -> PathBuf {
    let path = ws.temp.path().join("inputs.json");
    ws.ok(&[
        "index",
        "--syntax-only",
        "--export-manifest",
        path.to_str().unwrap(),
    ]);
    path
}

fn import(ws: &Workspace, index: &std::path::Path, inputs: &std::path::Path) -> Value {
    ws.ok(&[
        "index",
        "--scip",
        index.to_str().unwrap(),
        "--scip-manifest",
        inputs.to_str().unwrap(),
    ])
}

#[test]
fn historical_supplemental_scip_evidence_survives_zero_cache_budget() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\nquery_cache_views=0\nquery_cache_bytes=0\n",
    )
    .unwrap();
    let inputs = manifest(&ws);
    let index = write_scip(
        &ws,
        SOURCE,
        vec![
            occurrence("stock", &[1, 7, 12], true),
            occurrence("order", &[2, 7, 12], true),
            occurrence("stock", &[2, 24, 29], false),
        ],
        false,
    );
    let published = import(&ws, &index, &inputs);
    let id = published["snapshot_id"].as_str().unwrap();
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 2 }")).unwrap();
    ws.ok(&["symbol", "stock"]);
    ws.stop_and_wait();
    let refs = ws.ok(&["refs", "stock", "--source", "L3", "--snapshot-id", id]);
    assert_eq!(refs["relations"].as_array().unwrap().len(), 1, "{refs}");
    assert_eq!(refs["relations"][0]["certainty"], "definite");
}

#[test]
fn scip_import_merges_bindings_reuses_revision_and_rejects_old_inputs() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    let inputs = manifest(&ws);
    let index = write_scip(
        &ws,
        SOURCE,
        vec![
            occurrence("stock", &[1, 7, 12], true),
            occurrence("order", &[2, 7, 12], true),
            occurrence("stock", &[2, 24, 29], false),
        ],
        false,
    );
    let status = import(&ws, &index, &inputs);
    let refs = ws.ok(&["refs", "stock", "--source", "L3"]);
    assert_eq!(refs["relations"].as_array().unwrap().len(), 1, "{refs}");
    assert_eq!(refs["relations"][0]["certainty"], "definite");
    assert_eq!(
        refs["relations"][0]["evidence"][0]["derivation"],
        "scip_occurrence+syntax_callsite"
    );
    let merged = ws.ok(&["refs", "stock"]);
    assert_eq!(merged["relations"].as_array().unwrap().len(), 1);
    assert_eq!(
        merged["relations"][0]["evidence"].as_array().unwrap().len(),
        2
    );
    assert_eq!(merged["snapshot_id"], status["snapshot_id"]);
    let valid_bytes = fs::read(&index).unwrap();
    let mut invalid = Index::parse_from_bytes(&valid_bytes).unwrap();
    invalid.documents[0].text = "pub fn unrelated() {}".into();
    fs::write(&index, invalid.write_to_bytes().unwrap()).unwrap();
    assert_eq!(
        ws.run(&[
            "index",
            "--scip",
            index.to_str().unwrap(),
            "--scip-manifest",
            inputs.to_str().unwrap()
        ])
        .1["error"]["code"],
        "content_mismatch"
    );
    assert_eq!(
        ws.ok(&["refs", "stock", "--source", "L3"])["snapshot_id"],
        status["snapshot_id"]
    );
    fs::write(&index, valid_bytes).unwrap();
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 2 }")).unwrap();
    let diff = ws.ok(&["impact", "--diff", "HEAD"]);
    assert_eq!(
        diff["snapshots"]["base"]["snapshot_id"], status["snapshot_id"],
        "{diff}"
    );
    let failed = ws.run(&[
        "index",
        "--scip",
        index.to_str().unwrap(),
        "--scip-manifest",
        inputs.to_str().unwrap(),
    ]);
    assert_eq!(failed.1["error"]["code"], "content_mismatch");
}

#[test]
fn scip_utf16_ranges_align_unicode_and_report_invalid_positions() {
    let ws = Workspace::new();
    let source =
        "pub struct Thing;\npub fn use_it() { let _s = \"😀\"; let _: Option<Thing> = None; }\n";
    fs::write(ws.root.join("src/lib.rs"), source).unwrap();
    let inputs = manifest(&ws);
    let line = source.lines().nth(1).unwrap();
    let start = line[..line.find("Thing").unwrap()].encode_utf16().count() as i32;
    let index = write_scip(
        &ws,
        source,
        vec![
            occurrence("thing", &[0, 11, 16], true),
            occurrence("thing", &[1, start, start + 5], false),
            occurrence("thing", &[1, -1, 2], false),
        ],
        true,
    );
    import(&ws, &index, &inputs);
    let refs = ws.ok(&["refs", "Thing", "--source", "L3"]);
    assert_eq!(refs["relations"].as_array().unwrap().len(), 1, "{refs}");
    assert_eq!(
        refs["relations"][0]["span"]["start"]["column"],
        line.find("Thing").unwrap() + 1
    );
    assert!(
        refs["completeness"]["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["kind"] == "scip_position_mapping_gap")
    );
}

#[test]
fn exact_token_budgets_cover_unicode_and_every_omission_page() {
    let ws = Workspace::new();
    let mut source = "/// 库存😀\npub fn stock() {}\n".to_owned();
    for n in 0..90 {
        source.push_str(&format!("pub fn caller_{n}() {{ stock(); }}\n"));
    }
    fs::write(ws.root.join("src/lib.rs"), source).unwrap();
    for tokenizer in [
        "cl100k_base@tiktoken-rs-0.12.1",
        "o200k_base@tiktoken-rs-0.12.1",
    ] {
        let mut page = ws.ok(&[
            "context",
            "stock",
            "--budget",
            "4096",
            "--tokenizer",
            tokenizer,
        ]);
        let expected = page["omitted_count"].as_u64().unwrap() as usize;
        assert!(expected > 0, "{page}");
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let serialized = serde_json::to_string(&page).unwrap();
            let measured = if tokenizer.starts_with("cl100k") {
                tiktoken_rs::cl100k_base_singleton()
                    .encode_ordinary(&serialized)
                    .len()
            } else {
                tiktoken_rs::o200k_base_singleton()
                    .encode_ordinary(&serialized)
                    .len()
            };
            assert_eq!(page["token_count"]["total"], measured);
            assert!(measured <= 4096);
            for item in page["omitted"].as_array().unwrap() {
                assert!(seen.insert(serde_json::to_string(item).unwrap()));
            }
            let cursor = page["output"]["continuation"]["next_cursor"]
                .as_str()
                .or_else(|| page["omitted_cursor"].as_str())
                .map(str::to_owned);
            match cursor {
                Some(cursor) => page = ws.ok(&["context", "--cursor", &cursor]),
                None => break,
            }
            assert!(seen.len() <= expected);
        }
        assert_eq!(seen.len(), expected);
    }
}

#[test]
fn map_retains_empty_files_and_inherited_conditional_dependencies() {
    let ws = Workspace::new();
    fs::remove_dir_all(ws.root.join("src")).unwrap();
    fs::write(ws.root.join("Cargo.toml"), "[workspace]\nmembers=[\"a\",\"b\"]\n[workspace.dependencies]\nrenamed={package=\"b\",path=\"b\"}\n").unwrap();
    for name in ["a", "b"] {
        fs::create_dir_all(ws.root.join(format!("{name}/src"))).unwrap();
        fs::write(
            ws.root.join(format!("{name}/Cargo.toml")),
            format!(
                "[package]\nname=\"{name}\"\nversion=\"0.1.0\"\n{}",
                if name == "a" {
                    "[target.'cfg(unix)'.dependencies]\nrenamed.workspace=true\n"
                } else {
                    ""
                }
            ),
        )
        .unwrap();
        fs::write(
            ws.root.join(format!("{name}/src/lib.rs")),
            "pub fn entry() {}\n",
        )
        .unwrap();
    }
    fs::write(ws.root.join("a/src/empty.rs"), "").unwrap();
    let map = ws.ok(&["map", "--dependencies", "crate"]);
    assert!(
        map["modules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["path"] == "a/src/empty.rs"),
        "{map}"
    );
    assert!(
        map["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["from"] == "a"
                && d["to"] == "b"
                && d["alias"] == "renamed"
                && d["condition"] == "cfg(unix)"
                && d["workspace_inherited"] == true)
    );
    assert_eq!(map["analysis_health"]["source_files"], 3);
    assert!(!map["core_symbols"].as_array().unwrap().is_empty());
}

#[test]
fn graph_module_filter_and_diff_relation_pages_keep_fixed_versions() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("src/other.rs"),
        "pub fn other() { stock(); }\n",
    )
    .unwrap();
    let graph = ws.ok(&["calls", "stock", "--path", "src/other.rs"]);
    assert_eq!(graph["relations"].as_array().unwrap().len(), 1);
    assert_eq!(graph["relations"][0]["location"]["file"], "src/other.rs");
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("{ stock() }", "{ 0 }"),
    )
    .unwrap();
    let mut page = ws.ok(&["impact", "--diff", "HEAD", "--limit", "1"]);
    assert_eq!(page["relation_changes"]["removed_count"], 1, "{page}");
    let snapshots = page["snapshots"].clone();
    let mut removed = 0;
    loop {
        assert_eq!(page["snapshots"], snapshots);
        removed += page["relation_changes"]["removed"]
            .as_array()
            .unwrap()
            .len();
        if let Some(cursor) = page["output"]["continuation"]["next_cursor"]
            .as_str()
            .map(str::to_owned)
        {
            page = ws.ok(&["impact", "--cursor", &cursor]);
        } else {
            break;
        }
    }
    assert_eq!(removed, 1);
}

#[test]
fn docs_attributes_and_nested_test_scopes_are_preserved() {
    let ws = Workspace::new();
    let source = "/** 库存文档 */\n#[inline]\n#[must_use]\n#[allow(dead_code)]\n#[cold]\npub fn stock() {}\n#[cfg(test)]\nmod arbitrary {\n#[allow(dead_code)]\n#[inline]\n#[cold]\n#[allow(unused)]\nfn nested() { stock(); }\n}\npub fn production() { stock(); }\n";
    fs::write(ws.root.join("src/lib.rs"), source).unwrap();
    assert!(
        ws.ok(&["symbol", "stock"])["symbol"]["docs"]
            .as_str()
            .unwrap()
            .contains("库存文档")
    );
    assert_eq!(ws.ok(&["symbol", "nested"])["symbol"]["scope"], "test");
    let refs = ws.ok(&["refs", "stock"]);
    assert!(
        refs["completeness"]["pending_units"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(refs["relations"].as_array().unwrap().len(), 2);
}

#[test]
fn cargo_dependency_source_depth_tracks_external_transitive_sources() {
    let ws = Workspace::new();
    for (name, dependency, symbol) in [
        ("dep_a", "dep_b", "DependencyA"),
        ("dep_b", "", "DependencyB"),
    ] {
        let root = ws.temp.path().join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname=\"{name}\"\nversion=\"0.1.0\"\n{}",
                if dependency.is_empty() {
                    String::new()
                } else {
                    format!("[dependencies]\n{dependency}={{path=\"../{dependency}\"}}\n")
                }
            ),
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), format!("pub struct {symbol};\n")).unwrap();
    }
    fs::write(
        ws.root.join("Cargo.toml"),
        "[package]\nname=\"shop\"\nversion=\"0.1.0\"\n[dependencies]\ndep_a={path=\"../dep_a\"}\n",
    )
    .unwrap();
    assert!(
        Command::new("cargo")
            .current_dir(&ws.root)
            .args(["generate-lockfile", "--offline"])
            .output()
            .unwrap()
            .status
            .success()
    );
    // A missing resident backend keeps this test focused on captured dependency sources.
    assert!(
        ws.command()
            .env("SEMWEFT_RUST_ANALYZER", "/missing/semweft-test-ra")
            .args(["index", "--syntax-only"])
            .output()
            .unwrap()
            .status
            .success()
    );
    for (depth, a, b) in [(1, true, false), (2, true, true), (0, false, false)] {
        fs::write(
            ws.root.join("semweft.toml"),
            format!("schema_version=\"0.1\"\n[index]\ndependency_depth={depth}\n"),
        )
        .unwrap();
        ws.ok(&["trust", "--accept-build-execution"]);
        let path = manifest(&ws);
        let captured: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let paths = captured["files"].as_object().unwrap();
        assert_eq!(
            paths.keys().any(|p| p.starts_with("dependencies/dep_a-")),
            a
        );
        assert_eq!(
            paths.keys().any(|p| p.starts_with("dependencies/dep_b-")),
            b
        );
        if depth == 2 {
            let map = ws.ok(&["map", "--dependencies", "crate"]);
            assert!(
                map["dependencies"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["from"] == "shop" && e["to"] == "dep_a"),
                "{map}"
            );
            assert!(
                map["dependencies"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["from"] == "dep_a" && e["to"] == "dep_b"),
                "{map}"
            );
        }
        assert_eq!(
            !ws.ok(&["search", "DependencyA", "--match", "exact"])["symbols"]
                .as_array()
                .unwrap()
                .is_empty(),
            a
        );
        assert_eq!(
            !ws.ok(&["search", "DependencyB", "--match", "exact"])["symbols"]
                .as_array()
                .unwrap()
                .is_empty(),
            b
        );
    }
}

#[cfg(feature = "go-experimental")]
#[test]
fn go_workspaces_provide_navigation_maps_context_and_two_sided_diff() {
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_dir_all(ws.root.join("src")).unwrap();
    fs::write(
        ws.root.join("go.work"),
        "go 1.22\nuse (\n ./app\n ./lib\n)\n",
    )
    .unwrap();
    for name in ["app", "lib"] {
        fs::create_dir_all(ws.root.join(name)).unwrap();
        fs::write(
            ws.root.join(format!("{name}/go.mod")),
            format!("module example.com/{name}\ngo 1.22\n"),
        )
        .unwrap();
    }
    let lib = "package lib\n// Stock returns inventory.\nfunc Stock() int { return 1 }\n";
    fs::write(ws.root.join("lib/stock.go"), lib).unwrap();
    fs::write(ws.root.join("app/main.go"), "package main\nimport \"example.com/lib\"\nfunc Entry() int { return lib.Stock() }\nfunc main() { Entry() }\n").unwrap();
    let symbol = ws.ok(&["symbol", "Stock"]);
    assert_eq!(symbol["symbol"]["qualified_name"], "example.com/lib::Stock");
    assert_eq!(symbol["symbol"]["docs"], "Stock returns inventory.");
    let map = ws.ok(&["map"]);
    assert!(
        map["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "bin" && !e["main_symbol_id"].is_null()),
        "{map}"
    );
    assert!(
        map["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["from"] == "example.com/app" && e["to"] == "example.com/lib")
    );
    let context = ws.ok(&["context", "Stock", "--budget", "8k"]);
    assert!(
        context["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["location"]["file"] == "lib/go.mod"),
        "{context}"
    );
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("lib/stock.go"),
        lib.replace("return 1", "return 2"),
    )
    .unwrap();
    let diff = ws.ok(&["impact", "--diff", "HEAD"]);
    assert!(
        diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "modified" && c["head_symbol"]["name"] == "Stock"),
        "{diff}"
    );
    assert!(diff["configuration_changes"].as_array().unwrap().is_empty());
}

#[cfg(all(feature = "go-experimental", unix))]
#[test]
fn trusted_go_producer_publishes_bindings_and_recovers_after_failure() {
    use std::os::unix::fs::PermissionsExt;
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_dir_all(ws.root.join("src")).unwrap();
    fs::write(ws.root.join("go.mod"), "module example.com/shop\ngo 1.22\n").unwrap();
    let source =
        "package shop\nfunc Stock() int { return 1 }\nfunc Order() int { return Stock() }\n";
    fs::write(ws.root.join("shop.go"), source).unwrap();
    let index_path = write_scip(
        &ws,
        source,
        vec![
            occurrence("stock", &[1, 5, 10], true),
            occurrence("order", &[2, 5, 10], true),
            occurrence("stock", &[2, 26, 31], false),
        ],
        false,
    );
    let mut index = Index::parse_from_bytes(&fs::read(&index_path).unwrap()).unwrap();
    index.documents[0].relative_path = "shop.go".into();
    fs::write(&index_path, index.write_to_bytes().unwrap()).unwrap();
    let producer = ws.temp.path().join("producer.sh");
    fs::write(&producer, "#!/bin/sh\nif [ -f fail ]; then echo fixture-failure >&2; exit 1; fi\ncp \"$SCIP_FIXTURE\" \"$2\"\n").unwrap();
    fs::set_permissions(&producer, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        ws.command()
            .env("SEMWEFT_SCIP_GO", &producer)
            .env("SCIP_FIXTURE", &index_path)
            .args(["index", "--syntax-only"])
            .output()
            .unwrap()
            .status
            .success()
    );
    ws.ok(&["trust", "--accept-build-execution"]);
    ws.ok(&["index"]);
    let refs = ws.ok(&["refs", "Stock", "--source", "L3"]);
    assert_eq!(refs["relations"].as_array().unwrap().len(), 1, "{refs}");
    fs::write(ws.root.join("fail"), "").unwrap();
    assert_eq!(
        ws.run(&["index"]).1["error"]["code"],
        "index_refresh_failed"
    );
    assert_eq!(
        ws.ok(&["refs", "Stock", "--source", "L3"])["relations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fs::remove_file(ws.root.join("fail")).unwrap();
    ws.ok(&["index"]);
    ws.ok(&["trust", "--revoke"]);
    assert_eq!(ws.run(&["index"]).1["error"]["code"], "trust_required");
}

#[cfg(all(feature = "go-experimental", unix))]
#[test]
fn revoking_go_trust_cancels_the_running_producer_promptly() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_dir_all(ws.root.join("src")).unwrap();
    fs::write(ws.root.join("go.mod"), "module example.com/shop\ngo 1.22\n").unwrap();
    fs::write(ws.root.join("shop.go"), "package shop\nfunc Stock() {}\n").unwrap();
    let producer = ws.temp.path().join("slow-producer.sh");
    fs::write(
        &producer,
        "#!/bin/sh\ntouch started\nsleep 30\ntouch finished\n",
    )
    .unwrap();
    fs::set_permissions(&producer, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        ws.command()
            .env("SEMWEFT_SCIP_GO", &producer)
            .args(["index", "--syntax-only"])
            .output()
            .unwrap()
            .status
            .success()
    );
    ws.ok(&["trust", "--accept-build-execution"]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ws.root.join("started").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ws.root.join("started").exists());
    let started = Instant::now();
    ws.ok(&["trust", "--revoke"]);
    loop {
        let status = ws.ok(&["status"]);
        if status["refresh"]["state"] != "refreshing" {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(2), "{status}");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!ws.root.join("finished").exists());
}

#[cfg(feature = "go-experimental")]
#[test]
fn watcher_detects_changes_in_external_go_workspace_members() {
    use std::time::Instant;
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_dir_all(ws.root.join("src")).unwrap();
    let member = ws.temp.path().join("external");
    fs::create_dir(&member).unwrap();
    fs::write(
        member.join("go.mod"),
        "module example.com/external\ngo 1.22\n",
    )
    .unwrap();
    fs::write(
        member.join("code.go"),
        "package external\nfunc Before() {}\n",
    )
    .unwrap();
    fs::write(ws.root.join("go.work"), "go 1.22\nuse ../external\n").unwrap();
    let before = ws.ok(&["symbol", "Before"]);
    let watcher = ws
        .command()
        .args(["index", "--syntax-only", "--watch"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Stop(Child);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _stop = Stop(watcher);
    // A watch lease starts asynchronously; changing the external source is then
    // detected by the metadata fallback without a query that rescans inputs.
    let deadline = Instant::now() + Duration::from_secs(3);
    while ws.ok(&["status"])["clients"]["watch_leases"] == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(50));
    fs::write(
        member.join("code.go"),
        "package external\nfunc After() {}\n",
    )
    .unwrap();
    loop {
        let status = ws.ok(&["status"]);
        if status["snapshot_id"] != before["snapshot_id"] {
            break;
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(ws.ok(&["symbol", "After"])["symbol"]["name"], "After");
}

#[cfg(feature = "web-experimental")]
#[test]
fn web_projects_support_navigation_dependencies_and_git_diff_without_node() {
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_file(ws.root.join("src/lib.rs")).unwrap();
    fs::write(
        ws.root.join("package.json"),
        r#"{"name":"shop-web","main":"src/order.ts","scripts":{"postinstall":"exit 99"}}"#,
    )
    .unwrap();
    fs::write(
        ws.root.join("src/api.ts"),
        "export function send(n: number) { return n; }\n",
    )
    .unwrap();
    let source = "import {send} from './api.js';\nexport const order = (n: number) => { return send(n); };\n";
    fs::write(ws.root.join("src/order.ts"), source).unwrap();
    fs::write(
        ws.root.join("src/View.tsx"),
        "export function View() { return <button>Buy</button>; }\n",
    )
    .unwrap();
    fs::create_dir_all(ws.root.join("node_modules/vendor")).unwrap();
    fs::write(
        ws.root.join("node_modules/vendor/bad.ts"),
        "export function invisible() {}\n",
    )
    .unwrap();
    ws.ok(&["index", "--syntax-only"]);
    let symbols = ws.ok(&["search", "::"]);
    assert!(
        !symbols["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "invisible")
    );
    let calls = ws.ok(&["calls", "order", "--direction", "callees"]);
    assert!(
        calls["relations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "call" && r["certainty"] == "candidate"),
        "{calls}"
    );
    let refs = ws.ok(&["refs", "send"]);
    assert!(refs["relations"].as_array().unwrap().len() >= 2, "{refs}");
    let map = ws.ok(&["map", "--dependencies", "module"]);
    assert!(
        map["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["from"] == "src/order.ts" && d["to"] == "src/api.ts"),
        "{map}"
    );
    assert!(
        map["modules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["path"] == "src/View.tsx")
    );
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/order.ts"),
        source.replace("send(n)", "send(n + 1)"),
    )
    .unwrap();
    let diff = ws.ok(&["impact", "--diff", "HEAD"]);
    assert!(
        diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["head_symbol"]["name"] == "order"),
        "{diff}"
    );
    assert!(
        diff["configuration_changes"].as_array().unwrap().is_empty(),
        "{diff}"
    );
}
