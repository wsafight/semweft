use serde_json::{Value, json};
#[cfg(all(feature = "go-experimental", feature = "web-experimental"))]
#[path = "support/acceptance.rs"]
mod acceptance;
#[path = "support/hosts.rs"]
mod hosts;
#[path = "support/p0.rs"]
mod p0;
#[cfg(any(feature = "go-experimental", feature = "web-experimental"))]
#[path = "support/producers.rs"]
mod producers;
#[path = "support/product.rs"]
mod product;
#[path = "support/stability.rs"]
mod stability;
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

const SOURCE: &str = "/// Available stock.\npub fn stock() -> i32 { 1 }\npub fn order() -> i32 { stock() }\npub fn entry() -> i32 { order() }\n";
#[test]
fn storage_benchmark_uses_real_inputs_without_editing_or_indexing_the_workspace() {
    let ws = Workspace::new();
    let result = ws.ok(&["benchmark-storage", "--revisions", "2"]);
    assert_eq!(result["files"], 2);
    assert_eq!(result["samples"][1]["retained_views"], 2);
    assert_eq!(result["samples"][1]["recovery"]["loaded_views"], 1);
    assert!(
        result["samples"][1]["object_bytes_written"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        fs::read_to_string(ws.root.join("src/lib.rs")).unwrap(),
        SOURCE
    );
    assert!(!ws.temp.path().join("cache").exists());
}
struct Workspace {
    temp: tempfile::TempDir,
    root: PathBuf,
}
impl Workspace {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), SOURCE).unwrap();
        Self { temp, root }
    }
    fn implementations() -> Self {
        let ws = Self::new();
        fs::write(
            ws.root.join("src/lib.rs"),
            r#"pub trait Deduct {
    fn deduct(&self) -> i32;
    fn name(&self) -> &'static str { "default" }
}
pub trait Empty {}
pub struct Stock;
impl Deduct for Stock {
    fn deduct(&self) -> i32 { 1 }
}
pub mod production;
#[cfg(test)]
mod tests {
    struct Fake;
    impl super::Deduct for Fake {
        fn deduct(&self) -> i32 { 0 }
    }
}
"#,
        )
        .unwrap();
        fs::write(
            ws.root.join("src/production.rs"),
            r#"use crate::Deduct;
pub struct Other;
impl crate::Deduct for Other {
    fn deduct(&self) -> i32 { 2 }
    fn name(&self) -> &'static str { "other" }
}
impl<T> Deduct for T {
    fn deduct(&self) -> i32 { 3 }
    fn name(&self) -> &'static str { "generic" }
}
"#,
        )
        .unwrap();
        fs::create_dir_all(ws.root.join("tests")).unwrap();
        fs::write(
            ws.root.join("tests/impls.rs"),
            r#"use shop::Deduct;
struct Integration;
impl Deduct for Integration {
    fn deduct(&self) -> i32 { 4 }
    fn name(&self) -> &'static str { "integration" }
}
"#,
        )
        .unwrap();
        ws
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_semweft"));
        cmd.current_dir(&self.root)
            .arg("--workspace")
            .arg(&self.root)
            .env("SEMWEFT_CACHE_DIR", self.temp.path().join("cache"))
            .env("SEMWEFT_CONFIG_DIR", self.temp.path().join("config"));
        cmd
    }
    fn run(&self, args: &[&str]) -> (i32, Value) {
        let output = self.command().args(args).output().unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "args {args:?}: stdout {} stderr {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), value)
    }
    fn ok(&self, args: &[&str]) -> Value {
        let (code, value) = self.run(args);
        assert_eq!(code, 0, "args {args:?}: {value}");
        value
    }
    fn stop_and_wait(&self) {
        self.ok(&["__stop"]);
        let cache = fs::read_dir(self.temp.path().join("cache/workspaces"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(cache.join("daemon.lock"))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if lock.try_lock().is_ok() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "daemon did not stop");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "MVP Test")
            .env("GIT_AUTHOR_EMAIL", "mvp@example.invalid")
            .env("GIT_COMMITTER_NAME", "MVP Test")
            .env("GIT_COMMITTER_EMAIL", "mvp@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = self.command().arg("__stop").output();
    }
}

#[test]
fn impls_traits_and_methods_report_scope_counts_and_inherited_defaults() {
    let ws = Workspace::implementations();
    let result = ws.ok(&["impls", "Deduct", "--format", "json"]);
    assert_eq!(result["mode"], "trait");
    assert_eq!(result["target"]["kind"], "trait");
    assert_eq!(result["target"]["symbol_id"], result["trait"]["symbol_id"]);
    assert_eq!(
        result["implementation_counts"],
        json!({"total":5,"production":3,"test":2,"with_semantic_evidence":0,
            "blanket":1,"inherited_defaults":0,"excluded":0})
    );
    let relations = result["relations"].as_array().unwrap();
    assert_eq!(relations.len(), 5);
    assert!(relations.iter().all(|relation| {
        relation["kind"] == "implements"
            && relation["certainty"] == "candidate"
            && relation["freshness"] == "current"
            && relation["evidence"][0]["source"] == "L1"
            && relation["evidence"][0]["derivation"]
                .as_str()
                .unwrap()
                .starts_with("syntax_impl_header+name_match")
            && relation["to"] == result["trait"]["symbol_id"]
    }));
    assert_eq!(
        relations
            .iter()
            .filter(|relation| relation["scope"] == "test")
            .count(),
        2
    );
    for (self_type, scope) in [
        ("Stock", "production"),
        ("Other", "production"),
        ("Fake", "test"),
        ("Integration", "test"),
    ] {
        assert!(relations.iter().any(|relation| {
            relation["implementation"]["self_type"] == self_type && relation["scope"] == scope
        }));
    }
    assert_eq!(
        relations
            .iter()
            .filter(|relation| relation["implementation"]["blanket"] == true)
            .count(),
        1
    );
    assert_eq!(result["inherited_defaults"], json!([]));
    assert_eq!(result["excluded_candidates"], json!([]));
    for kind in [
        "external_implementations_not_indexed",
        "generated_impls_not_extracted",
        "blanket_impl_coverage_unknown",
        "implementation_semantic_evidence_missing",
    ] {
        assert!(
            result["completeness"]["known_gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == kind)
        );
    }

    let required = ws.ok(&["impls", "Deduct::deduct"]);
    assert_eq!(required["mode"], "trait_method");
    assert_eq!(required["target"]["kind"], "trait_method");
    assert_eq!(required["relations"].as_array().unwrap().len(), 5);
    assert_eq!(
        required["implementation_counts"],
        result["implementation_counts"]
    );
    assert_eq!(required["inherited_defaults"], json!([]));
    assert!(
        required["relations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|relation| {
                relation["from_name"]
                    .as_str()
                    .unwrap()
                    .ends_with("::deduct")
                    && relation["to"] == required["target"]["symbol_id"]
            })
    );

    let defaults = ws.ok(&["impls", "Deduct::name"]);
    assert_eq!(defaults["mode"], "trait_method");
    assert_eq!(defaults["relations"].as_array().unwrap().len(), 3);
    assert_eq!(
        defaults["implementation_counts"],
        json!({"total":3,"production":2,"test":1,"with_semantic_evidence":0,
            "blanket":1,"inherited_defaults":2,"excluded":0})
    );
    let inherited = defaults["inherited_defaults"].as_array().unwrap();
    assert_eq!(inherited.len(), 2);
    assert!(
        inherited
            .iter()
            .any(|entry| entry["self_type"] == "Stock" && entry["scope"] == "production")
    );
    assert!(
        inherited
            .iter()
            .any(|entry| entry["self_type"] == "Fake" && entry["scope"] == "test")
    );
    assert!(
        defaults["relations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|relation| {
                !inherited
                    .iter()
                    .any(|entry| entry["impl_id"] == relation["implementation"]["impl_id"])
            })
    );
}

#[test]
fn impls_filters_apply_to_relations_counts_and_default_metadata() {
    let ws = Workspace::implementations();
    let full = ws.ok(&["impls", "Deduct"]);
    let filtered = ws.ok(&[
        "impls",
        "Deduct",
        "--path",
        "src/production.rs",
        "--source",
        "L1,L3",
        "--certainty",
        "candidate",
        "--freshness",
        "current",
        "--snapshot-id",
        full["snapshot_id"].as_str().unwrap(),
        "--semantic-epoch",
        "0",
    ]);
    assert_eq!(filtered["snapshot_id"], full["snapshot_id"]);
    assert_eq!(
        filtered["implementation_counts"],
        json!({"total":2,"production":2,"test":0,"with_semantic_evidence":0,
            "blanket":1,"inherited_defaults":0,"excluded":0})
    );
    assert_eq!(filtered["relations"].as_array().unwrap().len(), 2);
    assert!(
        filtered["relations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|relation| {
                relation["location"]["file"] == "src/production.rs"
                    && relation["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|evidence| evidence["source"] == "L1")
            })
    );
    let tests = ws.ok(&["impls", "Deduct", "--path", "tests"]);
    assert_eq!(tests["implementation_counts"]["total"], 1);
    assert_eq!(tests["implementation_counts"]["test"], 1);
    assert_eq!(
        tests["relations"][0]["implementation"]["self_type"],
        "Integration"
    );
    let inherited = ws.ok(&[
        "impls",
        "Deduct::name",
        "--path",
        "src/lib.rs",
        "--source",
        "L1",
        "--certainty",
        "candidate",
        "--freshness",
        "current",
    ]);
    assert_eq!(inherited["relations"], json!([]));
    assert_eq!(inherited["implementation_counts"]["total"], 0);
    assert_eq!(inherited["implementation_counts"]["inherited_defaults"], 2);
    assert_eq!(inherited["inherited_defaults"].as_array().unwrap().len(), 2);
    let overrides = ws.ok(&["impls", "Deduct::name", "--path", "src/production.rs"]);
    assert_eq!(overrides["implementation_counts"]["total"], 2);
    assert_eq!(overrides["inherited_defaults"], json!([]));

    for (flag, value) in [
        ("--path", "src/missing"),
        ("--source", "L2"),
        ("--source", "L3"),
        ("--certainty", "definite"),
        ("--freshness", "pending"),
    ] {
        let empty = ws.ok(&["impls", "Deduct::name", flag, value]);
        assert_eq!(empty["relations"], json!([]), "{flag} {value}: {empty}");
        assert_eq!(
            empty["inherited_defaults"],
            json!([]),
            "{flag} {value}: {empty}"
        );
        assert_eq!(empty["excluded_candidates"], json!([]));
        assert_eq!(
            empty["implementation_counts"],
            json!({"total":0,"production":0,"test":0,"with_semantic_evidence":0,
                "blanket":0,"inherited_defaults":0,"excluded":0}),
            "{flag} {value}: {empty}"
        );
    }
    for (flag, value) in [
        ("--source", "L4"),
        ("--certainty", "certain"),
        ("--freshness", "fresh"),
        ("--freshness", "stale"),
    ] {
        let (code, error) = ws.run(&["impls", "Deduct", flag, value]);
        assert_eq!(code, 2);
        assert_eq!(error["error"]["code"], "invalid_argument");
    }
}

#[test]
fn impls_empty_traits_succeed_and_non_trait_targets_exit_two() {
    let ws = Workspace::implementations();
    let result = ws.ok(&["impls", "Empty"]);
    assert_eq!(result["mode"], "trait");
    assert_eq!(result["relations"], json!([]));
    assert_eq!(result["inherited_defaults"], json!([]));
    assert_eq!(result["excluded_candidates"], json!([]));
    assert_eq!(
        result["implementation_counts"],
        json!({"total":0,"production":0,"test":0,"with_semantic_evidence":0,
            "blanket":0,"inherited_defaults":0,"excluded":0})
    );
    let (code, error) = ws.run(&["impls", "Stock"]);
    assert_eq!(code, 2);
    assert_eq!(error["error"]["code"], "invalid_argument");
    let (code, error) = ws.run(&["impls"]);
    assert_eq!(code, 2);
    assert_eq!(error["error"]["code"], "invalid_argument");
}

#[test]
fn impls_pagination_keeps_the_snapshot_metadata_and_all_relations() {
    for (symbol, expected) in [("Deduct", 5), ("Deduct::name", 3)] {
        let ws = Workspace::implementations();
        let complete = ws.ok(&["impls", symbol]);
        let mut page = ws.ok(&[
            "impls",
            symbol,
            "--limit",
            "1",
            "--snapshot-id",
            complete["snapshot_id"].as_str().unwrap(),
            "--semantic-epoch",
            "0",
        ]);
        let original = fs::read_to_string(ws.root.join("src/lib.rs")).unwrap();
        fs::write(ws.root.join("src/lib.rs"), format!("{original}\npub struct Added;\nimpl Deduct for Added {{\n    fn deduct(&self) -> i32 {{ 5 }}\n    fn name(&self) -> &'static str {{ \"added\" }}\n}}\n")).unwrap();
        let mut seen = BTreeSet::new();
        let mut pages = 0;
        loop {
            assert_eq!(page["snapshot_id"], complete["snapshot_id"]);
            assert_eq!(page["epochs"], complete["epochs"]);
            for field in [
                "implementation_counts",
                "inherited_defaults",
                "excluded_candidates",
            ] {
                assert_eq!(page[field], complete[field], "{symbol}: {field}");
            }
            let relations = page["relations"].as_array().unwrap();
            assert_eq!(relations.len(), 1, "{page}");
            assert!(
                seen.insert(relations[0]["relation_id"].as_str().unwrap().to_owned()),
                "duplicate: {page}"
            );
            pages += 1;
            assert!(pages <= expected, "pagination did not finish: {page}");
            let Some(cursor) = page["output"]["continuation"]["next_cursor"].as_str() else {
                assert_eq!(page["output"]["truncated"], false);
                break;
            };
            assert_eq!(page["output"]["truncated"], true);
            assert_eq!(page["output"]["remaining_relations"], expected - pages);
            page = ws.ok(&["impls", "--cursor", cursor]);
        }
        assert_eq!(pages, expected);
        let expected_ids: BTreeSet<_> = complete["relations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|relation| relation["relation_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(seen, expected_ids, "{symbol}: missed relations");
        let latest = ws.ok(&["impls", symbol]);
        assert_ne!(latest["snapshot_id"], complete["snapshot_id"]);
        assert_eq!(latest["implementation_counts"]["total"], expected + 1);
    }
}

#[test]
fn navigation_pagination_and_budget_keep_a_fixed_view() {
    let ws = Workspace::new();
    assert!(ws.ok(&["status"])["snapshot_id"].is_null());
    let symbols = ws.ok(&["search", "s", "--limit", "1"]);
    assert_eq!(symbols["symbols"].as_array().unwrap().len(), 1);
    let target = ws.ok(&["symbol", "stock", "--include-body"]);
    assert_eq!(
        ws.ok(&["symbol", "shop::stock"])["symbol"]["qualified_name"],
        "shop::stock"
    );
    assert_eq!(target["symbol"]["docs"], "Available stock.");
    assert!(target["symbol"]["body"].as_str().unwrap().contains("{ 1 }"));
    let at = ws.ok(&["symbol-at", "src/lib.rs:3:26"]);
    assert_eq!(at["symbols"][0]["name"], "stock");
    assert_eq!(at["symbols"][0]["certainty"], "candidate");
    let mismatch = ws.run(&["symbol-at", "src/lib.rs:3:26", "--file-hash", "wrong"]);
    assert_eq!(mismatch.0, 2);
    assert_eq!(mismatch.1["error"]["code"], "content_mismatch");
    let refs = ws.ok(&["refs", "stock", "--source", "L1"]);
    assert_eq!(refs["relations"].as_array().unwrap().len(), 1);
    assert_eq!(refs["relations"][0]["from_name"], "order");
    let graph = ws.ok(&["calls", "stock", "--depth", "1"]);
    assert_eq!(graph["output"]["truncation_reason"], "depth_limit");
    assert_eq!(graph["evidence_summary"]["basis"], "within_limits");
    let graph = ws.ok(&["calls", "stock", "--depth", "3", "--limit", "1"]);
    assert_eq!(graph["evidence_summary"]["relation_count"], 2);
    assert_eq!(graph["output"]["remaining_relations"], 1);
    let cursor = graph["output"]["continuation"]["next_cursor"]
        .as_str()
        .unwrap();
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 2 }")).unwrap();
    let next = ws.ok(&["calls", "--cursor", cursor]);
    assert_eq!(next["snapshot_id"], graph["snapshot_id"]);
    assert_eq!(next["relations"].as_array().unwrap().len(), 1);
    assert_ne!(
        next["relations"][0]["relation_id"],
        graph["relations"][0]["relation_id"]
    );
    assert_eq!(
        ws.run(&["calls", "--cursor", cursor, "--depth", "2"]).1["error"]["code"],
        "invalid_argument"
    );
    let current = ws.ok(&["symbol", "stock", "--include-body"]);
    assert_ne!(current["snapshot_id"], target["snapshot_id"]);
    let pinned = ws.ok(&[
        "symbol",
        "stock",
        "--include-body",
        "--snapshot-id",
        target["snapshot_id"].as_str().unwrap(),
        "--semantic-epoch",
        "0",
    ]);
    assert!(pinned["symbol"]["body"].as_str().unwrap().contains("{ 1 }"));
    let output = ws
        .command()
        .args(["context", "stock", "--budget", "8k"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.stdout.len() - 1 <= 8000);
    assert_eq!(
        value["token_count"]["total"].as_u64().unwrap() as usize,
        output.stdout.len() - 1
    );
    assert_eq!(
        ws.run(&["context", "stock", "--budget", "100"]).1["error"]["code"],
        "budget_too_small"
    );
    let markdown = ws
        .command()
        .args(["impact", "stock", "--format", "markdown"])
        .output()
        .unwrap();
    let markdown = String::from_utf8(markdown.stdout).unwrap();
    for section in [
        "摘要",
        "变更符号",
        "候选影响",
        "已知缺口",
        "未检查项与截断",
        "版本信息",
        "Report hash:",
    ] {
        assert!(markdown.contains(section), "{markdown}");
    }
}

#[test]
fn git_diff_uses_both_sides_and_history_is_separate() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "Cargo.toml", "src/lib.rs"]);
    ws.git(&["commit", "-qm", "base"]);
    let base = ws.ok(&["refs", "stock"]);
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 2 }")).unwrap();
    let diff = ws.ok(&["impact", "--diff", "WORKTREE"]);
    assert!(
        diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "modified" && c["head_symbol"]["name"] == "stock")
    );
    assert!(
        diff["relations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["side"] == "base" && r["from_name"] == "order")
    );
    assert!(
        diff["relations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["side"] == "head" && r["from_name"] == "order")
    );
    ws.git(&["add", "src/lib.rs"]);
    ws.git(&["commit", "-qm", "change"]);
    let revision = ws.ok(&["impact", "--diff", "HEAD~1..HEAD"]);
    assert_eq!(revision["changes"][0]["kind"], "modified");
    let latest = ws.ok(&["refs", "stock"]);
    assert_ne!(base["snapshot_id"], latest["snapshot_id"]);
    let history = ws.ok(&[
        "refs",
        "stock",
        "--include-stale",
        "--stale-from",
        base["snapshot_id"].as_str().unwrap(),
        "--stale-semantic-epoch",
        "0",
    ]);
    assert_eq!(history["relations"][0]["freshness"], "current");
    assert_eq!(history["history"]["relations"][0]["freshness"], "stale");
    assert_eq!(history["evidence_summary"]["relation_count"], 1);
    assert_eq!(history["history"]["evidence_summary"]["relation_count"], 1);
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE
            .replace("/// Available stock.", "// comment only")
            .replace("{ 1 }", "{ 2 }"),
    )
    .unwrap();
    assert!(
        ws.ok(&["impact", "--diff", "WORKTREE"])["changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ws.run(&["impact", "--diff", "HEAD.."]).1["error"]["code"],
        "invalid_argument"
    );
}

#[test]
fn git_diff_reserves_existing_symbols_before_inferring_renames() {
    let ws = Workspace::new();
    let source = (0..12)
        .map(|i| format!("pub fn unchanged_{i}() {{}}\n"))
        .collect::<String>();
    fs::write(ws.root.join("src/lib.rs"), source).unwrap();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    // The parser orders symbols by ID. Keep the last one so a streaming
    // matcher encounters every deletion before the surviving exact match.
    let symbols = ws.ok(&["search", "unchanged", "--match", "prefix"]);
    let survivor = symbols["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .max_by_key(|s| s["symbol_id"].as_str().unwrap())
        .unwrap();
    let name = survivor["name"].as_str().unwrap();
    fs::write(
        ws.root.join("src/lib.rs"),
        format!("pub fn {name}() {{}}\n"),
    )
    .unwrap();

    let diff = ws.ok(&["impact", "--diff", "WORKTREE"]);
    let changes = diff["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 11, "{diff}");
    assert!(changes.iter().all(|c| c["kind"] == "deleted"), "{diff}");
    assert!(
        changes.iter().all(|c| c["base_symbol"]["name"] != name),
        "{diff}"
    );
}

#[test]
fn git_diff_does_not_choose_between_ambiguous_rename_candidates() {
    for (base, head, deleted, added) in [
        (
            "pub fn first() {}\npub fn second() {}\n",
            "pub fn replacement() {}\n",
            2,
            1,
        ),
        (
            "pub fn original() {}\n",
            "pub fn first() {}\npub fn second() {}\n",
            1,
            2,
        ),
        (
            "#[cfg(test)]\npub fn same() {}\n#[cfg(not(test))]\npub fn same() {}\n",
            "pub fn same() {}\n",
            2,
            1,
        ),
    ] {
        let ws = Workspace::new();
        fs::write(ws.root.join("src/lib.rs"), base).unwrap();
        ws.git(&["init", "-q"]);
        ws.git(&["add", "."]);
        ws.git(&["commit", "-qm", "base"]);
        fs::write(ws.root.join("src/lib.rs"), head).unwrap();

        let diff = ws.ok(&["impact", "--diff", "WORKTREE"]);
        let changes = diff["changes"].as_array().unwrap();
        assert_eq!(changes.len(), deleted + added, "{diff}");
        assert_eq!(
            changes.iter().filter(|c| c["kind"] == "deleted").count(),
            deleted,
            "{diff}"
        );
        assert_eq!(
            changes.iter().filter(|c| c["kind"] == "added").count(),
            added,
            "{diff}"
        );
    }
}

#[test]
fn git_diff_keeps_unchanged_cfg_alternatives_out_of_the_change_set() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("src/conditional.rs"),
        "#[cfg(test)]\npub fn same() {}\n#[cfg(not(test))]\npub fn same() {}\n",
    )
    .unwrap();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 2 }")).unwrap();

    let diff = ws.ok(&["impact", "--diff", "WORKTREE"]);
    let changes = diff["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{diff}");
    assert_eq!(changes[0]["kind"], "modified");
    assert_eq!(changes[0]["head_symbol"]["name"], "stock");
}

#[test]
fn git_diff_matches_type_and_field_renames_and_file_moves() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("src/lib.rs"),
        "pub struct Old { pub old_field: u64 }\npub fn relocated() -> i32 { 7 }\n",
    )
    .unwrap();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        "pub struct New { pub old_field: u64 }\n",
    )
    .unwrap();
    fs::write(
        ws.root.join("src/relocated.rs"),
        "pub fn relocated() -> i32 { 7 }\n",
    )
    .unwrap();

    let diff = ws.ok(&["impact", "--diff", "WORKTREE"]);
    let changes = diff["changes"].as_array().unwrap();
    assert!(
        changes.iter().any(|c| c["kind"] == "renamed"
            && c["base_symbol"]["name"] == "Old"
            && c["head_symbol"]["name"] == "New"),
        "{diff}"
    );
    assert!(
        changes.iter().any(|c| c["kind"] == "moved"
            && c["base_symbol"]["name"] == "relocated"
            && c["head_symbol"]["location"]["file"] == "src/relocated.rs"),
        "{diff}"
    );

    // A field rename changes the enclosing type too; only the field has a
    // matching fingerprint after masking its own declaration name.
    fs::write(
        ws.root.join("src/lib.rs"),
        "pub struct Old { pub new_field: u64 }\npub fn relocated() -> i32 { 7 }\n",
    )
    .unwrap();
    fs::remove_file(ws.root.join("src/relocated.rs")).unwrap();
    let diff = ws.ok(&["impact", "--diff", "WORKTREE"]);
    assert!(
        diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "renamed"
                && c["base_symbol"]["name"] == "old_field"
                && c["head_symbol"]["name"] == "new_field"),
        "{diff}"
    );
}

#[test]
fn git_diff_rename_fingerprints_preserve_other_identifiers() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("src/lib.rs"),
        "pub fn old() -> i32 { let old_value = 1; old_value }\n",
    )
    .unwrap();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        "pub fn new() -> i32 { let new_value = 1; new_value }\n",
    )
    .unwrap();

    let diff = ws.ok(&["impact", "--diff", "WORKTREE"]);
    let changes = diff["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 2, "{diff}");
    assert!(changes.iter().any(|c| c["kind"] == "deleted"), "{diff}");
    assert!(changes.iter().any(|c| c["kind"] == "added"), "{diff}");
}

#[test]
fn verification_reports_review_findings_exit_codes_and_frozen_pages() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("fn stock()", "fn inventory()"),
    )
    .unwrap();
    fs::write(
        ws.root.join("Cargo.toml"),
        "[package]\nname=\"shop\"\nversion=\"0.2.0\"\nedition=\"2024\"\n",
    )
    .unwrap();
    let (code, first) = ws.run(&["verify", "--diff", "WORKTREE", "--limit", "1"]);
    assert_eq!(code, 3, "{first}");
    assert_eq!(first["verification"]["status"], "insufficient_evidence");
    assert_eq!(first["verification"]["violation_count"], 0);
    assert_eq!(first["verification"]["rules"].as_array().unwrap().len(), 4);
    assert_eq!(
        first["findings"][0]["rule_id"],
        "configuration_change_requires_review"
    );
    assert_eq!(first["findings"][0]["evidence"]["file"], "Cargo.toml");
    let cursor = first["output"]["continuation"]["next_cursor"]
        .as_str()
        .unwrap();
    fs::write(ws.root.join("src/lib.rs"), SOURCE).unwrap();
    let (code, next) = ws.run(&["verify", "--cursor", cursor]);
    assert_eq!(code, 3);
    assert_eq!(next["verification"], first["verification"]);
    assert_eq!(next["snapshots"], first["snapshots"]);
    assert_ne!(next["findings"], first["findings"]);
    for format in ["text", "markdown"] {
        let output = ws
            .command()
            .args(["verify", "--diff", "WORKTREE", "--format", format])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("核验状态：insufficient_evidence"), "{text}");
        assert!(
            text.contains("configuration_change_requires_review@0.1"),
            "{text}"
        );
        assert!(text.contains("Cargo.toml"), "{text}");
    }
    for args in [
        vec!["verify"],
        vec!["verify", "--diff", "HEAD.."],
        vec!["verify", "--diff", "WORKTREE", "--snapshot-id", "bad"],
        vec!["verify", "--diff", "WORKTREE", "--depth", "0"],
    ] {
        let (code, value) = ws.run(&args);
        assert_eq!(code, 2, "{value}");
        assert_eq!(value["error"]["code"], "invalid_argument");
    }
}

#[test]
fn git_diff_reports_old_name_references_after_a_rename() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("fn stock()", "fn inventory()"),
    )
    .unwrap();

    let diff = ws.ok(&["impact", "--diff", "WORKTREE"]);
    assert_eq!(diff["changes"][0]["kind"], "renamed", "{diff}");
    let residual = diff["residual_references"].as_array().unwrap();
    assert_eq!(residual.len(), 1, "{diff}");
    assert_eq!(residual[0]["name"], "stock");
    assert_eq!(residual[0]["certainty"], "candidate");
    assert_eq!(residual[0]["location"]["file"], "src/lib.rs");
    assert_eq!(residual[0]["location"]["line"], 3);
    assert!(
        diff["relations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["side"] == "base" && r["from_name"] == "order"),
        "{diff}"
    );

    let markdown = ws
        .command()
        .args(["impact", "--diff", "WORKTREE", "--format", "markdown"])
        .output()
        .unwrap();
    assert!(markdown.status.success());
    assert!(
        String::from_utf8(markdown.stdout)
            .unwrap()
            .contains("残留名称引用")
    );
}

#[test]
fn concurrent_clients_share_one_writer_and_recover_saved_snapshots() {
    let ws = Workspace::new();
    let handles = (0..4)
        .map(|_| {
            ws.command()
                .args(["search", "stock"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let ids = handles
        .into_iter()
        .map(|child| {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
            serde_json::from_slice::<Value>(&out.stdout).unwrap()["snapshot_id"].clone()
        })
        .collect::<Vec<_>>();
    assert!(ids.iter().all(|id| id == &ids[0]));
    assert_eq!(
        ws.ok(&["status"])["retained_views"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    ws.ok(&["__stop"]);
    std::thread::sleep(Duration::from_millis(150));
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 9 }")).unwrap();
    let latest = ws.ok(&["search", "stock"]);
    assert_ne!(latest["snapshot_id"], ids[0]);
    let old = ws.ok(&[
        "symbol",
        "stock",
        "--include-body",
        "--snapshot-id",
        ids[0].as_str().unwrap(),
    ]);
    assert!(old["symbol"]["body"].as_str().unwrap().contains("{ 1 }"));
    assert_eq!(ws.run(&["index"]).0, 4);
    let config = ws.ok(&["init"]);
    assert_eq!(config["created"], true);
    assert_eq!(ws.ok(&["init"])["created"], false);
    fs::write(
        ws.root.join("semweft.toml"),
        "schema_version=\"0.1\"\nunknown=true\n",
    )
    .unwrap();
    assert_eq!(ws.run(&["status"]).1["error"]["code"], "invalid_argument");
}

#[test]
fn warmed_relation_indexes_keep_pinned_views_across_edits_and_daemon_restart() {
    let ws = Workspace::new();
    let before = ws.ok(&["calls", "stock", "--depth", "3"]);
    assert_eq!(before["relations"].as_array().unwrap().len(), 2);
    let id = before["snapshot_id"].as_str().unwrap();
    let epoch = before["epochs"]["semantic"].as_u64().unwrap().to_string();
    let pinned = [
        "calls",
        "stock",
        "--depth",
        "3",
        "--snapshot-id",
        id,
        "--semantic-epoch",
        &epoch,
    ];
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("{ stock() }", "{ 0 }"),
    )
    .unwrap();
    let after = ws.ok(&["calls", "stock", "--depth", "3"]);
    assert_ne!(after["snapshot_id"], before["snapshot_id"]);
    assert!(after["relations"].as_array().unwrap().is_empty());
    assert_eq!(ws.ok(&pinned)["relations"], before["relations"]);
    ws.ok(&["__stop"]);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(ws.ok(&pinned)["relations"], before["relations"]);
    assert!(
        ws.ok(&["calls", "stock", "--depth", "3"])["relations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn snapshot_objects_are_reused_across_refreshes_and_daemon_restarts() {
    let ws = Workspace::new();
    let first = ws.ok(&["symbol", "stock", "--include-body"]);
    let index = ws
        .temp
        .path()
        .join("cache/workspaces")
        .join(
            first["completeness"]["scope"]["workspace_id"]
                .as_str()
                .unwrap(),
        )
        .join("index");
    let catalog = index.join("views.json");
    let catalog_before = fs::read(&catalog).unwrap();
    let modified = fs::metadata(&catalog).unwrap().modified().unwrap();
    let pointers: Value = serde_json::from_slice(&catalog_before).unwrap();
    assert_eq!(pointers["schema_version"], "0.2");
    let mut original_objects = vec![];
    for kind in ["sources", "snapshots", "views"] {
        for entry in fs::read_dir(index.join("objects").join(kind)).unwrap() {
            let path = entry.unwrap().path();
            original_objects.push((
                path.clone(),
                fs::read(&path).unwrap(),
                fs::metadata(&path).unwrap().modified().unwrap(),
            ));
        }
    }
    ws.ok(&["symbol", "stock"]);
    assert_eq!(fs::read(&catalog).unwrap(), catalog_before);
    assert_eq!(
        fs::metadata(&catalog).unwrap().modified().unwrap(),
        modified
    );

    for number in 2..=4 {
        fs::write(
            ws.root.join("src/lib.rs"),
            SOURCE.replace("{ 1 }", &format!("{{ {number} }}")),
        )
        .unwrap();
        let current = ws.ok(&["symbol", "stock", "--include-body"]);
        assert_ne!(current["snapshot_id"], first["snapshot_id"]);
        assert!(
            current["symbol"]["body"]
                .as_str()
                .unwrap()
                .contains(&format!("{{ {number} }}"))
        );
    }
    for (path, bytes, modified) in original_objects {
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
    }
    // One common Cargo.toml object, plus four versions of src/lib.rs.
    assert_eq!(
        fs::read_dir(index.join("objects/sources")).unwrap().count(),
        5
    );
    assert_eq!(
        fs::read_dir(index.join("objects/snapshots"))
            .unwrap()
            .count(),
        4
    );
    assert_eq!(
        fs::read_dir(index.join("objects/views")).unwrap().count(),
        4
    );
    let before_restart = fs::read(&catalog).unwrap();
    let modified = fs::metadata(&catalog).unwrap().modified().unwrap();
    ws.ok(&["__stop"]);
    std::thread::sleep(Duration::from_millis(150));
    let old = ws.ok(&[
        "symbol",
        "stock",
        "--include-body",
        "--snapshot-id",
        first["snapshot_id"].as_str().unwrap(),
        "--semantic-epoch",
        "0",
    ]);
    assert_eq!(old["symbol"], first["symbol"]);
    let latest = ws.ok(&["symbol", "stock", "--include-body"]);
    assert!(latest["symbol"]["body"].as_str().unwrap().contains("{ 4 }"));
    assert_eq!(fs::read(&catalog).unwrap(), before_restart);
    assert_eq!(
        fs::metadata(&catalog).unwrap().modified().unwrap(),
        modified
    );

    // Recovered object knowledge must also be reused for subsequent writes.
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 5 }")).unwrap();
    ws.ok(&["symbol", "stock"]);
    assert_eq!(
        fs::read_dir(index.join("objects/sources")).unwrap().count(),
        6
    );
    assert_eq!(
        fs::read_dir(index.join("objects/views")).unwrap().count(),
        5
    );
}

#[test]
fn daemon_migrates_a_legacy_index_and_keeps_pinned_queries() {
    let ws = Workspace::new();
    let first = ws.ok(&["calls", "stock", "--depth", "3"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("{ stock() }", "{ 0 }"),
    )
    .unwrap();
    let latest = ws.ok(&["calls", "stock", "--depth", "3"]);
    ws.ok(&["__stop"]);
    std::thread::sleep(Duration::from_millis(150));
    let workspace = first["completeness"]["scope"]["workspace_id"]
        .as_str()
        .unwrap();
    let index = ws
        .temp
        .path()
        .join("cache/workspaces")
        .join(workspace)
        .join("index");
    let recovered = semweft_store::SnapshotStore::open(&index, workspace)
        .unwrap()
        .1
        .unwrap();
    let legacy = json!({"schema_version":"0.1", "workspace_id":workspace, "latest":recovered.latest, "views":recovered.publications});
    fs::write(
        index.join("views.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    fs::remove_dir_all(index.join("objects")).unwrap();

    let status = ws.ok(&["status"]);
    assert_eq!(status["snapshot_id"], latest["snapshot_id"]);
    let old = ws.ok(&[
        "calls",
        "stock",
        "--depth",
        "3",
        "--snapshot-id",
        first["snapshot_id"].as_str().unwrap(),
        "--semantic-epoch",
        "0",
    ]);
    assert_eq!(old["relations"], first["relations"]);
    let catalog: Value =
        serde_json::from_slice(&fs::read(index.join("views.json")).unwrap()).unwrap();
    assert_eq!(catalog["schema_version"], "0.2");
    assert_eq!(catalog["views"].as_array().unwrap().len(), 2);
}

struct Mcp {
    child: Child,
    lines: mpsc::Receiver<String>,
}
impl Mcp {
    fn new(ws: &Workspace) -> Self {
        let mut child = ws
            .command()
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }
    fn send(&mut self, value: Value) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{value}").unwrap();
        self.child.stdin.as_mut().unwrap().flush().unwrap();
    }
    fn reply(&self) -> Value {
        serde_json::from_str(
            &self
                .lines
                .recv_timeout(Duration::from_secs(10))
                .expect("MCP response timeout"),
        )
        .unwrap()
    }
}
impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
#[test]
fn mcp_restores_its_watch_lease_after_daemon_crash() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[daemon]\nidle_timeout_ms=300\n",
    )
    .unwrap();
    let mut client = Mcp::new(&ws);
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"recovery-test","version":"1"}}}));
    client.reply();
    client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_symbols","arguments":{"query":"stock"}}}));
    assert_eq!(
        client.reply()["result"]["structuredContent"]["symbols"][0]["name"],
        "stock"
    );
    let status = ws.ok(&["status"]);
    assert_eq!(status["clients"]["watch_leases"], 1);
    let old_pid = status["clients"]["daemon_pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(old_pid, libc::SIGKILL) }, 0);
    // Also exercise immediate query recovery, before the lease monitor's next tick.
    std::thread::sleep(Duration::from_millis(30));
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_symbols","arguments":{"query":"stock"}}}));
    assert_eq!(
        client.reply()["result"]["structuredContent"]["symbols"][0]["name"],
        "stock"
    );
    let recovered = ws.ok(&["status"]);
    assert_eq!(recovered["clients"]["watch_leases"], 1);
    assert_ne!(
        recovered["clients"]["daemon_pid"],
        status["clients"]["daemon_pid"]
    );
    // Outlive several idle deadlines without issuing queries.
    std::thread::sleep(Duration::from_millis(1000));
    let alive = ws.ok(&["status"]);
    assert_eq!(
        alive["clients"]["daemon_pid"],
        recovered["clients"]["daemon_pid"]
    );
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("{ 1 }", "{ 42 }"),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = ws.ok(&["status"]);
        if status["snapshot_id"] != recovered["snapshot_id"] {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "recovered MCP session lost its watcher: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Recover while idle as well, without a tool call to trigger reconnection.
    let pid = recovered["clients"]["daemon_pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    std::thread::sleep(Duration::from_millis(1000));
    let idle_recovery = ws.ok(&["status"]);
    assert_eq!(idle_recovery["clients"]["watch_leases"], 1);
    assert_ne!(
        idle_recovery["clients"]["daemon_pid"],
        recovered["clients"]["daemon_pid"]
    );
    drop(client);
    ws.stop_and_wait();
}

#[cfg(unix)]
#[test]
fn revocation_stops_lsp_initialization_and_response_waits_and_descendants() {
    use std::os::unix::fs::PermissionsExt;
    for stage in ["initialize", "textDocument/definition"] {
        let ws = Workspace::new();
        let backend = ws.temp.path().join("controlled-lsp");
        let script = r##"#!/usr/bin/env python3
import json, os, sys, time
if sys.argv[1:] == ['--version']:
    print('controlled-lsp-1')
    sys.exit(0)
def send(value):
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(f'Content-Length: {len(body)}\r\n\r\n'.encode() + body)
    sys.stdout.buffer.flush()
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b'\r\n', b'\n'): break
        key, value = line.decode().split(':', 1)
        headers[key.lower()] = value.strip()
    request = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    method = request.get('method')
    if method == BLOCK_STAGE:
        if os.fork() == 0:
            time.sleep(2)
            open('executed_after_revoke', 'w').close()
            os._exit(0)
        open('backend_blocked', 'w').close()
        time.sleep(30)
    if 'id' in request:
        result = {'capabilities':{}} if method == 'initialize' else {'kind':'full','items':[]} if method == 'textDocument/diagnostic' else []
        send({'jsonrpc':'2.0', 'id':request['id'], 'result':result})
    if method == 'initialize':
        send({'jsonrpc':'2.0','method':'experimental/serverStatus','params':{'quiescent':True}})
"##;
        fs::write(
            &backend,
            script.replace(
                "import json, os, sys, time",
                &format!("import json, os, sys, time\nBLOCK_STAGE = {stage:?}"),
            ),
        )
        .unwrap();
        fs::set_permissions(&backend, fs::Permissions::from_mode(0o700)).unwrap();
        ws.ok(&["trust", "--accept-build-execution"]);
        assert!(
            ws.command()
                .env("SEMWEFT_RUST_ANALYZER", &backend)
                .args(["search", "stock"])
                .output()
                .unwrap()
                .status
                .success()
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ws.root.join("backend_blocked").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "backend did not reach {stage}: {}",
                ws.ok(&["status"])
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = std::time::Instant::now();
        assert_eq!(ws.ok(&["trust", "--revoke"])["trust"]["state"], "untrusted");
        assert!(started.elapsed() < Duration::from_millis(1500));
        let status = ws.ok(&["status"]);
        assert_eq!(status["trust"]["state"], "untrusted");
        assert_ne!(status["refresh"]["state"], "refreshing");
        std::thread::sleep(Duration::from_millis(2200));
        assert!(
            !ws.root.join("executed_after_revoke").exists(),
            "LSP descendant executed after revocation during {stage}"
        );
        ws.stop_and_wait();
    }
}

#[test]
fn mcp_exposes_product_tools_with_implementation_schema_and_queries_the_same_daemon() {
    let ws = Workspace::new();
    let mut client = Mcp::new(&ws);
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mvp-test","version":"1"}}}));
    assert!(client.reply().get("result").is_some());
    client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let tools = client.reply();
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 19);
    let names: BTreeSet<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "capabilities",
            "index_status",
            "repo_map",
            "search_symbols",
            "symbol_at",
            "get_symbol",
            "find_references",
            "find_implementations",
            "call_graph",
            "impact_of",
            "context_pack",
            "verify_change",
            "find_paths",
            "suggest_tests",
            "arch_check",
            "agent_preflight",
            "agent_verify",
            "get_review_result",
            "contract_impact",
        ])
    );
    let implementations = tools
        .iter()
        .find(|tool| tool["name"] == "find_implementations")
        .unwrap();
    assert!(!implementations["description"].as_str().unwrap().is_empty());
    let schema = &implementations["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!([]));
    for required in [json!(["symbol"]), json!(["cursor"])] {
        assert!(
            schema["anyOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|alternative| alternative["required"] == required)
        );
    }
    for property in ["symbol", "cursor", "path_prefix", "snapshot_id"] {
        assert_eq!(
            schema["properties"][property]["type"], "string",
            "{property}"
        );
    }
    for (property, values) in [
        ("source", json!(["L1", "L2", "L3"])),
        ("certainty", json!(["definite", "candidate"])),
        ("freshness", json!(["current", "pending", "stale"])),
    ] {
        assert_eq!(schema["properties"][property]["type"], "array");
        assert_eq!(schema["properties"][property]["items"]["enum"], values);
    }
    assert_eq!(schema["properties"]["limit"]["minimum"], 1);
    assert_eq!(schema["properties"]["limit"]["maximum"], 1000);
    assert_eq!(schema["properties"]["semantic_epoch"]["type"], "integer");
    assert_eq!(schema["properties"]["require_fresh"]["type"], "boolean");
    assert_eq!(schema["properties"]["fresh_wait_ms"]["type"], "integer");
    assert_eq!(schema["properties"]["timeout_ms"]["type"], "integer");
    assert!(ws.ok(&["status"])["snapshot_id"].is_null());
    client.send(json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"index","arguments":{}}}));
    assert_eq!(
        client.reply()["result"]["structuredContent"]["error"]["code"],
        "capability_unsupported"
    );
    assert!(ws.ok(&["status"])["snapshot_id"].is_null());
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_symbols","arguments":{"query":"stock"}}}));
    let reply = client.reply();
    let result = &reply["result"]["structuredContent"];
    assert_eq!(result["symbols"][0]["name"], "stock");
    assert_eq!(
        ws.ok(&["symbol", "stock"])["snapshot_id"],
        result["snapshot_id"]
    );
    client.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"call_graph","arguments":{"symbol":"stock","depth":0}}}));
    assert_eq!(
        client.reply()["result"]["structuredContent"]["error"]["code"],
        "invalid_argument"
    );
    fs::write(ws.root.join("src/lib.rs"), SOURCE.replace("{ 1 }", "{ 7 }")).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        let status = ws.ok(&["status"]);
        if status["snapshot_id"] != result["snapshot_id"] {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "watcher did not publish edit: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(client);
}

#[test]
fn call_paths_are_available_through_cli_mcp_and_human_reports() {
    let ws = Workspace::new();
    let result = ws.ok(&[
        "path",
        "entry",
        "stock",
        "--source",
        "L1",
        "--max-depth",
        "3",
        "--max-paths",
        "10",
    ]);
    assert_eq!(result["path_summary"]["path_count"], 1);
    assert_eq!(result["paths"][0]["length"], 2);
    assert_eq!(result["paths"][0]["certainty"], "candidate");
    for format in ["text", "markdown"] {
        let output = ws
            .command()
            .args(["path", "entry", "stock", "--format", format])
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("调用路径"), "{text}");
        assert!(
            text.contains("entry")
                && text.contains("order")
                && text.contains("stock")
                && text.contains(" → "),
            "{text}"
        );
        assert!(text.contains("src/lib.rs:3:"), "{text}");
    }
    let bounded = ws.ok(&["path", "entry", "stock", "--max-depth", "1"]);
    assert_eq!(bounded["output"]["truncation_reason"], "depth_limit");
    assert_eq!(ws.run(&["path", "missing", "stock"]).0, 2);
    assert_eq!(ws.run(&["path", "entry"]).0, 2);
    let mut client = Mcp::new(&ws);
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"paths-test","version":"1"}}}));
    assert!(client.reply().get("result").is_some());
    client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"find_paths","arguments":{"from":"entry","to":"stock","source":["L1"],"max_depth":3,"max_paths":10,"snapshot_id":result["snapshot_id"],"semantic_epoch":result["epochs"]["semantic"]}}}));
    let reply = client.reply();
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let mcp = &reply["result"]["structuredContent"];
    assert_eq!(mcp["paths"], result["paths"]);
    assert_eq!(mcp["path_summary"], result["path_summary"]);
}

#[test]
fn diff_reports_show_full_module_counts_and_risk_signals_in_human_formats() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("fn stock()", "fn inventory()"),
    )
    .unwrap();
    let result = ws.ok(&["impact", "--diff", "WORKTREE"]);
    assert_eq!(result["diff_summary"]["affected_modules_observed"], 2);
    assert!(result["risk_summary"]["score"].as_u64().is_some());
    for format in ["text", "markdown"] {
        let output = ws
            .command()
            .args(["impact", "--diff", "WORKTREE", "--format", format])
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        for expected in [
            "Diff（全量）",
            "风险审查信号（全量）",
            "风险评分：",
            "启发式",
            "受影响文件模块（当前页）",
            "src/lib.rs | base",
            "src/lib.rs | head",
            "residual_references",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
    }
}

#[test]
fn test_suggestions_are_consistent_through_cli_mcp_and_human_reports() {
    let ws = Workspace::new();
    fs::write(ws.root.join("src/lib.rs"), format!("{SOURCE}\n#[test]\nfn stock_test() {{ entry(); }}\n#[test]\n#[ignore]\nfn ignored_test() {{ stock(); }}\n")).unwrap();
    let result = ws.ok(&[
        "tests",
        "stock",
        "--suggested",
        "--source",
        "L1",
        "--depth",
        "4",
    ]);
    assert_eq!(result["test_summary"]["candidate_count"], 2);
    assert_eq!(result["test_summary"]["head_nonignored_candidates"], 1);
    assert_eq!(result["test_summary"]["executed"], false);
    for format in ["text", "markdown"] {
        let output = ws
            .command()
            .args(["tests", "stock", "--format", format])
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        for expected in [
            "测试候选推荐",
            "stock_test",
            "ignored_test",
            "src/lib.rs:",
            "call_path",
            "覆盖信息：不可用",
            "零候选不表示无需测试",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
    }
    assert_eq!(ws.run(&["tests"]).0, 2);
    let mut client = Mcp::new(&ws);
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"suggestions-test","version":"1"}}}));
    assert!(client.reply().get("result").is_some());
    client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"suggest_tests","arguments":{"symbol":"stock","source":["L1"],"depth":4,"snapshot_id":result["snapshot_id"],"semantic_epoch":result["epochs"]["semantic"]}}}));
    let reply = client.reply();
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let mcp = &reply["result"]["structuredContent"];
    assert_eq!(mcp["test_candidates"], result["test_candidates"]);
    assert_eq!(mcp["test_summary"], result["test_summary"]);
}

#[test]
fn diff_test_suggestions_and_cursor_requests_keep_both_snapshots() {
    let ws = Workspace::new();
    fs::write(ws.root.join("src/lib.rs"), format!("{SOURCE}\n#[test]\nfn stock_test() {{ stock(); }}\n#[test]\nfn order_test() {{ order(); }}\n")).unwrap();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    let source = fs::read_to_string(ws.root.join("src/lib.rs")).unwrap();
    fs::write(ws.root.join("src/lib.rs"), source.replace("{ 1 }", "{ 2 }")).unwrap();
    let first = ws.ok(&["tests", "--suggested", "--diff", "WORKTREE", "--limit", "1"]);
    assert_eq!(first["test_summary"]["candidate_count"], 2);
    assert_eq!(first["test_candidates"].as_array().unwrap().len(), 1);
    fs::write(ws.root.join("src/lib.rs"), "pub fn unrelated() {}\n").unwrap();
    let next = ws.ok(&[
        "tests",
        "--cursor",
        first["output"]["continuation"]["next_cursor"]
            .as_str()
            .unwrap(),
    ]);
    assert_eq!(first["snapshots"], next["snapshots"]);
    assert_eq!(first["test_summary"], next["test_summary"]);
    assert_eq!(next["test_candidates"].as_array().unwrap().len(), 1);
    assert_ne!(
        first["test_candidates"][0]["test"]["symbol_id"],
        next["test_candidates"][0]["test"]["symbol_id"]
    );
}

#[test]
fn mcp_verification_returns_insufficient_evidence_as_a_successful_tool_result() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("fn stock()", "fn inventory()"),
    )
    .unwrap();
    let (code, cli) = ws.run(&["verify", "--diff", "WORKTREE", "--limit", "1"]);
    assert_eq!(code, 3);
    let mut client = Mcp::new(&ws);
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"verify-test","version":"1"}}}));
    assert!(client.reply().get("result").is_some());
    client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    let diff = json!({"base":{"kind":"snapshot","snapshot_id":cli["snapshots"]["base"]["snapshot_id"],"semantic_epoch":cli["snapshots"]["base"]["epochs"]["semantic"]},
        "head":{"kind":"snapshot","snapshot_id":cli["snapshots"]["head"]["snapshot_id"],"semantic_epoch":cli["snapshots"]["head"]["epochs"]["semantic"]}});
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"verify_change","arguments":{"diff":diff,"limit":1}}}));
    let reply = client.reply();
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let first = &reply["result"]["structuredContent"];
    for field in [
        "status",
        "rules",
        "finding_count",
        "violation_count",
        "warning_count",
    ] {
        assert_eq!(
            first["verification"][field], cli["verification"][field],
            "{field}"
        );
    }
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"verify_change","arguments":{"cursor":first["output"]["continuation"]["next_cursor"]}}}));
    let reply = client.reply();
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let next = &reply["result"]["structuredContent"];
    assert_eq!(next["verification"], first["verification"]);
    assert_eq!(next["snapshots"], first["snapshots"]);
    assert_eq!(
        next["findings"][0]["rule_id"],
        "residual_reference_requires_review"
    );
}

#[test]
fn ci_exports_all_verification_pages_and_renders_sarif_offline_with_verdict_exit_code() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("fn stock()", "fn inventory()"),
    )
    .unwrap();

    let (exit, complete) = ws.run(&[
        "verify",
        "--diff",
        "WORKTREE",
        "--limit",
        "1",
        "--all-pages",
        "--format",
        "json",
    ]);
    assert_eq!(exit, 3, "{complete}");
    assert_eq!(complete["output"]["all_pages"], true);
    assert_eq!(complete["output"]["truncated"], false);
    assert_eq!(
        complete["findings"].as_array().unwrap().len() as u64,
        complete["verification"]["finding_count"]
    );
    assert!(complete["output"]["continuation"]["next_cursor"].is_null());

    let input = ws.temp.path().join("verify.json");
    fs::write(&input, serde_json::to_vec(&complete).unwrap()).unwrap();
    let offline = ws
        .command()
        .args([
            "report",
            "--input",
            input.to_str().unwrap(),
            "--format",
            "sarif",
        ])
        .output()
        .unwrap();
    assert_eq!(offline.status.code(), Some(3));
    let sarif: Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(sarif["version"], "2.1.0");
    assert_eq!(
        sarif["runs"][0]["results"].as_array().unwrap().len() as u64,
        complete["verification"]["finding_count"]
    );
    assert_eq!(
        sarif["runs"][0]["properties"]["snapshots"],
        complete["snapshots"]
    );

    let direct = ws
        .command()
        .args([
            "verify", "--diff", "WORKTREE", "--limit", "1", "--format", "sarif",
        ])
        .output()
        .unwrap();
    assert_eq!(direct.status.code(), Some(3));
    let direct: Value = serde_json::from_slice(&direct.stdout).unwrap();
    assert_eq!(
        direct["runs"][0]["results"].as_array().unwrap().len() as u64,
        complete["verification"]["finding_count"]
    );
}

#[test]
fn ci_report_converts_index_errors_to_sarif_without_changing_failure_status() {
    let ws = Workspace::new();
    let input = ws.temp.path().join("error.json");
    fs::write(&input, br#"{"schema_version":"0.1","error":{"code":"index_refresh_failed","message":"index unavailable"}}"#).unwrap();
    let output = ws
        .command()
        .args([
            "report",
            "--input",
            input.to_str().unwrap(),
            "--format",
            "sarif",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
    let sarif: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        sarif["runs"][0]["invocations"][0]["executionSuccessful"],
        false
    );
    assert_eq!(sarif["runs"][0]["results"], json!([]));
}

#[test]
fn checked_in_ci_script_preserves_verdict_and_writes_all_report_formats() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "base"]);
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace("fn stock()", "fn inventory()"),
    )
    .unwrap();
    let reports = ws.temp.path().join("ci-reports");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/ci/semweft-ci.sh");
    let status = Command::new("sh")
        .arg(script)
        .current_dir(&ws.root)
        .env("SEMWEFT_BIN", env!("CARGO_BIN_EXE_semweft"))
        .env("SEMWEFT_DIFF", "WORKTREE")
        .env("SEMWEFT_REPORT_DIR", &reports)
        .env("SEMWEFT_CACHE_DIR", ws.temp.path().join("cache"))
        .env("SEMWEFT_CONFIG_DIR", ws.temp.path().join("config"))
        .output()
        .unwrap();
    assert_eq!(
        status.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        fs::read_to_string(reports.join("exit-code.txt"))
            .unwrap()
            .trim(),
        "3"
    );
    let verification: Value =
        serde_json::from_slice(&fs::read(reports.join("verify.json")).unwrap()).unwrap();
    assert_eq!(verification["output"]["all_pages"], true);
    assert_eq!(
        verification["findings"].as_array().unwrap().len() as u64,
        verification["verification"]["finding_count"]
    );
    let markdown = fs::read_to_string(reports.join("verify.markdown")).unwrap();
    assert!(markdown.contains("核验状态：insufficient_evidence"));
    let sarif: Value =
        serde_json::from_slice(&fs::read(reports.join("verify.sarif")).unwrap()).unwrap();
    assert_eq!(
        sarif["runs"][0]["results"].as_array().unwrap().len() as u64,
        verification["verification"]["finding_count"]
    );
}

#[test]
fn mcp_find_implementations_calls_match_cli_and_continue_without_a_symbol() {
    let ws = Workspace::implementations();
    let mut client = Mcp::new(&ws);
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"impls-test","version":"1"}}}));
    assert!(client.reply().get("result").is_some());
    client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"find_implementations","arguments":{"symbol":"Deduct","limit":1}}}));
    let reply = client.reply();
    assert_eq!(reply["id"], 2);
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let first = &reply["result"]["structuredContent"];
    assert_eq!(first["mode"], "trait");
    assert_eq!(first["relations"].as_array().unwrap().len(), 1);
    let cli = ws.ok(&[
        "impls",
        "Deduct",
        "--limit",
        "1",
        "--snapshot-id",
        first["snapshot_id"].as_str().unwrap(),
        "--semantic-epoch",
        "0",
    ]);
    for field in [
        "relations",
        "implementation_counts",
        "inherited_defaults",
        "excluded_candidates",
    ] {
        assert_eq!(first[field], cli[field], "{field}");
    }
    assert_eq!(first["implementation_counts"]["total"], 5);
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"find_implementations","arguments":{"cursor":first["output"]["continuation"]["next_cursor"]}}}));
    let reply = client.reply();
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let next = &reply["result"]["structuredContent"];
    assert_eq!(next["snapshot_id"], first["snapshot_id"]);
    assert_eq!(next["epochs"], first["epochs"]);
    assert_eq!(next["relations"].as_array().unwrap().len(), 1);
    assert_ne!(
        next["relations"][0]["relation_id"],
        first["relations"][0]["relation_id"]
    );
    for field in [
        "implementation_counts",
        "inherited_defaults",
        "excluded_candidates",
    ] {
        assert_eq!(next[field], first[field], "{field}");
    }
    client.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"find_implementations","arguments":{"symbol":"Deduct::name","path_prefix":"src/lib.rs","source":["L1"],"certainty":["candidate"],"freshness":["current"],"snapshot_id":first["snapshot_id"],"semantic_epoch":0}}}));
    let reply = client.reply();
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let defaults = &reply["result"]["structuredContent"];
    assert_eq!(defaults["mode"], "trait_method");
    assert_eq!(defaults["relations"], json!([]));
    assert_eq!(defaults["implementation_counts"]["total"], 0);
    assert_eq!(defaults["implementation_counts"]["inherited_defaults"], 2);
    assert_eq!(defaults["inherited_defaults"].as_array().unwrap().len(), 2);
    client.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"find_implementations","arguments":{"symbol":"Stock"}}}));
    let reply = client.reply();
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(
        reply["result"]["structuredContent"]["error"]["code"],
        "invalid_argument"
    );
}

#[test]
fn omitted_context_pages_fit_the_budget_and_cover_every_omission() {
    let ws = Workspace::new();
    let mut source = SOURCE.to_owned();
    for i in 0..80 {
        source.push_str(&format!("pub fn caller_{i}() -> i32 {{ stock() }}\n"));
    }
    fs::write(ws.root.join("src/lib.rs"), source).unwrap();
    let first = ws.ok(&["context", "stock", "--budget", "4096"]);
    assert!(first["omitted_count"].as_u64().unwrap() > 10);
    let total = first["omitted_count"].as_u64().unwrap() as usize;
    let mut seen = first["omitted"].as_array().unwrap().len();
    let mut cursor = first["omitted_cursor"].as_str().map(str::to_owned);
    while let Some(current) = cursor {
        let page = ws.ok(&["context", "--cursor", &current]);
        assert_eq!(page["snapshot_id"], first["snapshot_id"]);
        assert!(serde_json::to_vec(&page).unwrap().len() <= 4096);
        seen += page["omitted"].as_array().unwrap().len();
        cursor = page["output"]["continuation"]["next_cursor"]
            .as_str()
            .map(str::to_owned);
    }
    assert_eq!(seen, total);
}

#[test]
fn cargo_members_and_auto_targets_define_the_repository_map() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("Cargo.toml"),
        "[workspace]\nmembers=[\"crates/*\"]\nresolver=\"2\"\n",
    )
    .unwrap();
    for name in ["a", "b"] {
        let dir = ws.root.join("crates").join(name);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname=\"{name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n"),
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), format!("pub fn {name}() {{}}\n")).unwrap();
    }
    let outside = ws.root.join("fixtures/independent");
    fs::create_dir_all(outside.join("src")).unwrap();
    fs::write(
        outside.join("Cargo.toml"),
        "[package]\nname=\"independent\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(outside.join("src/lib.rs"), "pub fn ghost() {}\n").unwrap();
    fs::create_dir_all(ws.root.join("crates/a/src/bin")).unwrap();
    fs::write(ws.root.join("crates/a/src/bin/demo.rs"), "fn main() {}\n").unwrap();
    assert!(
        ws.ok(&["search", "ghost"])["symbols"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        ws.ok(&["search", "stock"])["symbols"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let map = ws.ok(&["map"]);
    assert!(
        map["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["target"] == "demo" && e["kind"] == "bin")
    );
    assert!(
        !ws.ok(&["status"])["manifest"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"].as_str().unwrap().starts_with("fixtures/"))
    );
}

#[cfg(unix)]
#[test]
fn mcp_cancellation_does_not_cancel_the_shared_refresh() {
    use std::os::unix::fs::PermissionsExt;
    let ws = Workspace::new();
    let backend = ws.temp.path().join("delayed-backend");
    fs::write(&backend,"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo delayed-test; exit 0; fi\nexec sleep 5\n").unwrap();
    fs::set_permissions(&backend, fs::Permissions::from_mode(0o700)).unwrap();
    ws.ok(&["trust", "--accept-build-execution"]);
    let output = ws
        .command()
        .env("SEMWEFT_RUST_ANALYZER", &backend)
        .args(["search", "stock"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut client = Mcp::new(&ws);
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"cancel-test","version":"1"}}}));
    client.reply();
    client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_symbol","arguments":{"symbol":"stock","require_fresh":true,"timeout_ms":5000,"fresh_wait_ms":2500}}}));
    std::thread::sleep(Duration::from_millis(100));
    client.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"test cancellation"}}));
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_symbols","arguments":{"query":"stock"}}}));
    let reply = client.reply();
    assert_eq!(reply["id"], 3);
    assert_eq!(
        reply["result"]["structuredContent"]["symbols"][0]["name"],
        "stock"
    );
    assert_ne!(ws.ok(&["status"])["refresh"]["state"], "current");
    assert_eq!(ws.ok(&["search", "stock"])["symbols"][0]["name"], "stock");
    drop(client);
}

#[test]
#[ignore = "Runs a real rust-analyzer against a generated, trusted temporary workspace"]
fn trusted_daemon_publishes_l3_and_refreshes_references() {
    let ws = Workspace::new();
    ws.ok(&["trust", "--accept-build-execution"]);
    let indexed = ws.ok(&["index", "--timeout-ms", "60000"]);
    assert_eq!(indexed["epochs"]["syntax"], indexed["epochs"]["semantic"]);
    let refs = ws.ok(&["refs", "stock", "--source", "L3"]);
    assert_eq!(refs["relations"].as_array().unwrap().len(), 1);
    assert_eq!(refs["relations"][0]["certainty"], "definite");
    fs::write(
        ws.root.join("src/lib.rs"),
        SOURCE.replace(
            "pub fn order() -> i32 { stock() }",
            "pub fn order() -> i32 { 0 }",
        ),
    )
    .unwrap();
    ws.ok(&["index", "--timeout-ms", "60000"]);
    assert!(
        ws.ok(&["refs", "stock", "--source", "L3"])["relations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    ws.ok(&["trust", "--revoke"]);
    assert_eq!(ws.ok(&["status"])["trust"]["state"], "untrusted");
}

#[test]
fn snapshot_pins_gc_and_storage_metrics_survive_restart_with_cache_disabled() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\nquery_cache_views=0\nquery_cache_bytes=0\n",
    )
    .unwrap();
    let first = ws.ok(&["symbol", "stock", "--include-body"]);
    let id = first["snapshot_id"].as_str().unwrap();
    ws.ok(&["snapshots", "pin", id]);
    let mut middle = String::new();
    for revision in 2..=3 {
        fs::write(
            ws.root.join("src/lib.rs"),
            SOURCE.replace("{ 1 }", &format!("{{ {revision} }}")),
        )
        .unwrap();
        let current = ws.ok(&["symbol", "stock"]);
        if revision == 2 {
            middle = current["snapshot_id"].as_str().unwrap().into();
        }
    }
    let before = ws.ok(&["status"]);
    assert!(
        before["storage"]["runtime"]["object_write_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(before["storage"]["query_cache"]["views"], 0);
    let usage = ws.ok(&["snapshots", "usage"]);
    assert!(usage["usage"]["total_bytes"].as_u64().unwrap() > 0);
    let preview = ws.ok(&["snapshots", "gc", "--keep-last", "0", "--dry-run"]);
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["gc"]["removed_views"], 1);
    assert_eq!(
        ws.ok(&["status"])["storage"]["journal_sequence"],
        before["storage"]["journal_sequence"]
    );
    let gc = ws.ok(&["snapshots", "gc", "--keep-last", "0"]);
    assert_eq!(gc["gc"]["removed_views"], 1);
    assert_eq!(
        ws.run(&["symbol", "stock", "--snapshot-id", &middle]).1["error"]["code"],
        "snapshot_expired"
    );
    ws.stop_and_wait();
    let old = ws.ok(&["symbol", "stock", "--include-body", "--snapshot-id", id]);
    assert!(old["symbol"]["body"].as_str().unwrap().contains("{ 1 }"));
    let restored = ws.ok(&["status"]);
    assert_eq!(restored["storage"]["runtime"]["recovered_views"], 1);
    assert!(
        restored["storage"]["runtime"]["historical_loads"]
            .as_u64()
            .unwrap()
            > 0
    );
    ws.ok(&["snapshots", "unpin", id]);
    ws.ok(&["snapshots", "gc", "--keep-last", "0"]);
    assert_eq!(
        ws.run(&["symbol", "stock", "--snapshot-id", id]).1["error"]["code"],
        "snapshot_expired"
    );
}

#[test]
fn configured_retention_collects_old_history_after_refresh() {
    for policy in ["retain_snapshots=1", "max_disk_bytes=0"] {
        let ws = Workspace::new();
        fs::write(
            ws.root.join("semweft.toml"),
            format!("schema_version=\"0.1\"\n[index]\nquery_cache_views=0\n{policy}\n"),
        )
        .unwrap();
        let first = ws.ok(&["symbol", "stock"]);
        for revision in 2..=3 {
            fs::write(
                ws.root.join("src/lib.rs"),
                SOURCE.replace("{ 1 }", &format!("{{ {revision} }}")),
            )
            .unwrap();
            ws.ok(&["symbol", "stock"]);
        }
        let status = ws.ok(&["status"]);
        assert_eq!(status["storage"]["retained_publications"], 1);
        assert_eq!(
            status["storage"]["disk_budget_unmet"],
            policy == "max_disk_bytes=0"
        );
        assert!(status["storage"]["runtime"]["gc_runs"].as_u64().unwrap() >= 3);
        assert_eq!(
            ws.run(&[
                "symbol",
                "stock",
                "--snapshot-id",
                first["snapshot_id"].as_str().unwrap()
            ])
            .1["error"]["code"],
            "snapshot_expired"
        );
        ws.stop_and_wait();
        let after = ws.ok(&["symbol", "stock"]);
        assert_eq!(after["snapshot_id"], status["snapshot_id"]);
    }
}

#[test]
fn frozen_benchmark_repeats_identical_inputs_and_rejects_incompatible_baselines() {
    let ws = Workspace::new();
    let input = ws.temp.path().join("frozen.json");
    let baseline = ws.temp.path().join("baseline.json");
    let first = ws.ok(&[
        "benchmark-storage",
        "--revisions",
        "2",
        "--runs",
        "2",
        "--save-input",
        input.to_str().unwrap(),
    ]);
    assert_eq!(first["runs"].as_array().unwrap().len(), 2);
    assert_eq!(first["summary"]["2"]["persist_ms"]["count"], 2);
    assert!(first["samples"][1]["disk_bytes"].as_u64().unwrap() > 0);
    fs::write(&baseline, serde_json::to_vec(&first).unwrap()).unwrap();
    fs::write(ws.root.join("src/lib.rs"), "new source, not valid Rust").unwrap();
    fs::write(ws.root.join("semweft.toml"), "invalid config").unwrap();
    let after = ws.ok(&[
        "benchmark-storage",
        "--input",
        input.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
    ]);
    assert_eq!(after["corpus_id"], first["corpus_id"]);
    assert_eq!(
        after["samples"][1]["query_checksum"],
        first["samples"][1]["query_checksum"]
    );
    assert!(after["comparison"]["2.persist_ms"]["p95_ratio"].is_number());
    let (_, invalid) = ws.run(&[
        "benchmark-storage",
        "--input",
        input.to_str().unwrap(),
        "--revisions",
        "3",
        "--baseline",
        baseline.to_str().unwrap(),
    ]);
    assert_eq!(invalid["error"]["code"], "invalid_argument");
    assert!(!ws.temp.path().join("cache").exists());
}
