//! Shared query schemas, validation, errors and local IPC request envelope.
//! This crate has no daemon, language frontend or filesystem execution dependency.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Component, Path, PathBuf};

pub const DAEMON_PROTOCOL: &str = "0.1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopEndpoint {
    pub protocol: String,
    pub workspace: PathBuf,
    pub workspace_id: String,
    pub config_hash: String,
    pub endpoint: String,
}

#[derive(Serialize, Deserialize)]
pub struct DaemonRequest {
    pub protocol: String,
    pub workspace_id: String,
    pub config_hash: String,
    pub method: String,
    pub args: Value,
    #[serde(default)]
    pub request_id: Option<String>,
}
pub const TOOL_NAMES: [&str; 19] = [
    "capabilities",
    "index_status",
    "repo_map",
    "search_symbols",
    "symbol_at",
    "get_symbol",
    "find_references",
    "call_graph",
    "impact_of",
    "context_pack",
    "find_implementations",
    "verify_change",
    "find_paths",
    "suggest_tests",
    "arch_check",
    "agent_preflight",
    "agent_verify",
    "get_review_result",
    "contract_impact",
];
pub const MIN_BUDGET: u64 = 4096;

#[derive(Debug)]
pub struct QueryError {
    pub code: String,
    pub message: String,
    pub details: Value,
}
impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for QueryError {}
pub fn failure(code: &str, message: &str, details: Value) -> anyhow::Error {
    QueryError {
        code: code.into(),
        message: message.into(),
        details,
    }
    .into()
}
pub fn error_value(error: &anyhow::Error) -> Value {
    if let Some(error) = error.downcast_ref::<QueryError>() {
        return json!({"schema_version":"0.1","error":{"code":error.code,"message":error.message,"details":error.details}});
    }
    let message = format!("{error:#}");
    let candidate = message.split(':').next().unwrap_or("");
    let code = if [
        "invalid_argument",
        "symbol_not_found",
        "ambiguous_symbol",
        "file_not_found",
        "content_mismatch",
        "budget_too_small",
        "snapshot_expired",
        "cursor_expired",
        "timeout",
        "cancelled",
        "indexer_unavailable",
        "capability_unsupported",
        "daemon_unavailable",
        "daemon_incompatible",
        "config_conflict",
        "trust_required",
        "index_refresh_failed",
        "index_store_error",
        "review_not_found",
    ]
    .contains(&candidate)
    {
        candidate
    } else {
        "internal_error"
    };
    json!({"schema_version":"0.1","error":{"code":code,"message":message}})
}

fn string() -> Value {
    json!({"type":"string","minLength":1})
}
fn integer(default: u64, min: u64, max: u64) -> Value {
    json!({"type":"integer","default":default,"minimum":min,"maximum":max})
}
fn choice(values: &[&str], default: &str) -> Value {
    json!({"type":"string","enum":values,"default":default})
}
fn array(values: &[&str]) -> Value {
    json!({"type":"array","items":{"type":"string","enum":values}})
}
fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

pub fn tool_schema(name: &str) -> Value {
    let mut props = Map::from_iter([("timeout_ms".into(), integer(5000, 1, 60000))]);
    let mut required = vec![];
    if name != "capabilities" {
        props.insert("snapshot_id".into(), string());
        if name != "index_status" {
            props.extend(Map::from_iter([
                ("semantic_epoch".into(), integer(0, 0, u64::MAX)),
                (
                    "require_fresh".into(),
                    json!({"type":"boolean","default":false}),
                ),
                ("fresh_wait_ms".into(), integer(2000, 0, 30000)),
                ("limit".into(), integer(100, 1, 1000)),
                ("cursor".into(), string()),
            ]));
        }
    }
    match name {
        "get_review_result" => {
            required.push("review_id");
            props.insert("review_id".into(), string());
        }
        "repo_map" => {
            props.insert("cycles".into(), json!({"type":"boolean","default":false}));
            props.extend(Map::from_iter([
                ("path_prefix".into(), string()),
                ("depth".into(), integer(2, 0, 10)),
                ("max_symbols_per_module".into(), integer(20, 0, 1000)),
                (
                    "dependencies".into(),
                    choice(&["crate", "module", "service", "none"], "crate"),
                ),
            ]));
        }
        "search_symbols" => {
            required.push("query");
            props.insert(
                "api_scope".into(),
                choice(&["declarations", "external"], "declarations"),
            );
            props.extend(Map::from_iter([
                ("query".into(), string()),
                ("kind".into(), string()),
                ("path_prefix".into(), string()),
                (
                    "visibility".into(),
                    choice(&["public", "restricted", "private", "all"], "all"),
                ),
                (
                    "match".into(),
                    choice(&["fuzzy", "exact", "prefix"], "fuzzy"),
                ),
            ]));
        }
        "symbol_at" => {
            required.extend(["file", "line", "column"]);
            props.extend(Map::from_iter([
                ("file".into(), string()),
                ("line".into(), integer(1, 1, u32::MAX as u64)),
                ("column".into(), integer(1, 1, u32::MAX as u64)),
                ("file_hash".into(), string()),
            ]));
        }
        "get_symbol" => {
            required.push("symbol");
            props.extend(Map::from_iter([
                ("symbol".into(), string()),
                (
                    "include_body".into(),
                    json!({"type":"boolean","default":false}),
                ),
                (
                    "include_docs".into(),
                    json!({"type":"boolean","default":true}),
                ),
            ]));
        }
        "find_references" => {
            required.push("symbol");
            props.extend(Map::from_iter([
                ("symbol".into(), string()),
                (
                    "kinds".into(),
                    array(&["read", "write", "call", "type", "import"]),
                ),
                (
                    "include_stale".into(),
                    json!({"type":"boolean","default":false}),
                ),
                (
                    "stale_from".into(),
                    object(
                        json!({"snapshot_id":string(),"semantic_epoch":integer(0,0,u64::MAX)}),
                        &["snapshot_id", "semantic_epoch"],
                    ),
                ),
            ]));
        }
        "arch_check" => {
            let strings = json!({"type":"array","items":string()});
            let layer = object(
                json!({"name":string(),"paths":strings,"allow":strings}),
                &["name", "paths"],
            );
            props.insert("policy".into(), object(json!({"layers":{"type":"array","items":layer},"deny_unassigned":{"type":"boolean"}}), &["layers"]));
            props.insert("baseline".into(), object(json!({"schema_version":string(),"rules_digest":string(),"fingerprints":strings,"snapshot_id":string(),"semantic_epoch":integer(0,0,u64::MAX)}), &["schema_version","rules_digest","fingerprints","snapshot_id","semantic_epoch"]));
            let selector = object(
                json!({"kind":choice(&["revision","snapshot","workspace"],"workspace"),"revision":string(),"snapshot_id":string(),"semantic_epoch":integer(0,0,u64::MAX),"commit_context":string()}),
                &["kind"],
            );
            props.insert(
                "diff".into(),
                object(json!({"base":selector,"head":selector}), &["base", "head"]),
            );
        }
        "agent_preflight" => {
            let strings = json!({"type":"array","items":string()});
            props.insert("files".into(), strings.clone());
            props.insert("symbols".into(), strings);
            props.insert("depth".into(), integer(3, 1, 10));
            props.insert("max_nodes".into(), integer(1000, 1, 100000));
        }
        "call_graph" | "impact_of" | "verify_change" | "suggest_tests" | "agent_verify"
        | "contract_impact" => {
            props.extend(Map::from_iter([
                ("depth".into(), integer(3, 1, 10)),
                ("max_nodes".into(), integer(1000, 1, 100000)),
            ]));
            if !matches!(name, "verify_change" | "agent_verify" | "contract_impact") {
                props.insert("symbol".into(), string());
            } else {
                required.push("diff");
            }
            if name == "agent_verify" {
                let architecture = tool_schema("arch_check");
                props.insert(
                    "baseline".into(),
                    architecture["properties"]["baseline"].clone(),
                );
                props.insert(
                    "policy".into(),
                    architecture["properties"]["policy"].clone(),
                );
            }
            if name == "call_graph" {
                required.push("symbol");
                props.insert(
                    "direction".into(),
                    choice(&["callers", "callees", "both"], "callers"),
                );
            } else {
                let selector = object(
                    json!({"kind":choice(&["revision","snapshot","workspace"],"workspace"),"revision":string(),"snapshot_id":string(),"semantic_epoch":integer(0,0,u64::MAX),"commit_context":string()}),
                    &["kind"],
                );
                props.insert(
                    "diff".into(),
                    object(json!({"base":selector,"head":selector}), &["base", "head"]),
                );
            }
        }
        "find_paths" => {
            required.extend(["from", "to"]);
            props.insert(
                "scope".into(),
                choice(&["symbol", "module", "service"], "symbol"),
            );
            props.extend(Map::from_iter([
                ("from".into(), string()),
                ("to".into(), string()),
                ("max_depth".into(), integer(5, 1, 10)),
                ("max_paths".into(), integer(100, 1, 1000)),
                ("max_nodes".into(), integer(1000, 1, 100000)),
            ]));
        }
        "context_pack" => {
            required.push("budget");
            props.extend(Map::from_iter([
                ("budget".into(), integer(20000, 1, 10000000)),
                ("symbol".into(), string()),
                ("task".into(), string()),
                ("tokenizer".into(), string()),
                (
                    "include".into(),
                    array(&[
                        "definition",
                        "callers",
                        "callees",
                        "types",
                        "tests",
                        "config",
                    ]),
                ),
            ]));
        }
        _ => {}
    }
    if matches!(
        name,
        "impact_of"
            | "verify_change"
            | "suggest_tests"
            | "agent_verify"
            | "arch_check"
            | "contract_impact"
            | "agent_preflight"
    ) {
        let line = object(
            json!({"line":integer(1,1,u32::MAX as u64),"hits":integer(0,0,u64::MAX)}),
            &["line", "hits"],
        );
        let file = object(
            json!({"path":string(),"file_hash":string(),"lines":{"type":"array","items":line},"history_changes":integer(0,0,1000)}),
            &["path", "file_hash"],
        );
        let coverage = object(
            json!({"format":choice(&["lcov"],"lcov"),"report_digest":string(),"source_manifest_digest":string(),"omitted_records":integer(0,0,u32::MAX as u64)}),
            &[
                "format",
                "report_digest",
                "source_manifest_digest",
                "omitted_records",
            ],
        );
        let history = object(
            json!({"anchor_commit":string(),"commits_observed":integer(0,0,1000),"limit":integer(100,1,1000),"window_limited":{"type":"boolean"},"shallow":{"type":"boolean"}}),
            &[
                "anchor_commit",
                "commits_observed",
                "limit",
                "window_limited",
                "shallow",
            ],
        );
        let artifact = object(
            json!({"schema_version":choice(&["0.1"],"0.1"),"workspace_id":string(),"files":{"type":"array","items":file},"coverage":coverage,"history":history}),
            &["schema_version", "workspace_id", "files"],
        );
        props.insert(
            "risk_inputs".into(),
            if name == "agent_preflight" {
                object(json!({"head":artifact}), &["head"])
            } else {
                object(json!({"base":artifact,"head":artifact}), &[])
            },
        );
    }
    if name == "find_implementations" {
        required.push("symbol");
        props.insert("symbol".into(), string());
    }
    if [
        "find_references",
        "call_graph",
        "impact_of",
        "find_implementations",
        "find_paths",
        "suggest_tests",
    ]
    .contains(&name)
    {
        props.extend(Map::from_iter([
            ("path_prefix".into(), string()),
            ("source".into(), array(&["L1", "L2", "L3"])),
            ("certainty".into(), array(&["definite", "candidate"])),
            ("freshness".into(), array(&["current", "pending", "stale"])),
        ]));
    }
    let mut schema = object(Value::Object(props), &required);
    if schema["properties"].get("cursor").is_some() {
        schema["anyOf"] = json!([{"required":["cursor"]},{"required":required}]);
        schema["required"] = json!([]);
    }
    schema
}

pub fn tools() -> Vec<Value> {
    let descriptions = [
        "Installed tools, languages and limits",
        "Saved index and trust state",
        "Cargo targets, modules and dependencies",
        "Search workspace symbols",
        "Resolve declaration or reference at UTF-8 position",
        "Symbol signature, documentation and source",
        "References and explicit historical evidence",
        "Bounded caller/callee graph",
        "Candidate impact of a symbol or explicit diff",
        "Context with a serialized-response budget",
        "Indexed implementations of a trait or trait method",
        "Verify an explicit diff with versioned built-in review rules and evidence gaps",
        "Bounded simple call paths between indexed symbols, with evidence and explicit truncation",
        "Suggested declared Rust tests from bounded impact paths, with snapshot evidence and coverage gaps",
        "Check versioned file-layer dependency rules and classify new findings against a diff or explicit baseline",
        "Before-edit public declaration, candidate impact and protected-region guardrails for explicit files or symbols",
        "After-edit diff, configured architecture and protected-region verification using captured base and head policies",
        "Load an immutable local human review and check its snapshot and rule bindings against the selected analysis view",
        "Protobuf field/RPC diff, selected compatibility rules and configured generated-code candidate impacts",
    ];
    TOOL_NAMES.iter().zip(descriptions).map(|(name,description)|json!({"name":name,"description":description,"version":"0.1","input_schema":tool_schema(name)})).collect()
}

fn check(schema: &Value, value: &Value, path: &str, skip_required: bool) -> Result<()> {
    let valid = match schema["type"].as_str() {
        Some("string") => value.as_str().is_some_and(|s| {
            schema["minLength"]
                .as_u64()
                .is_none_or(|n| s.len() as u64 >= n)
        }),
        Some("integer") => value.as_u64().is_some_and(|n| {
            schema["minimum"].as_u64().is_none_or(|m| n >= m)
                && schema["maximum"].as_u64().is_none_or(|m| n <= m)
        }),
        Some("boolean") => value.is_boolean(),
        Some("array") => value.is_array(),
        Some("object") => value.is_object(),
        _ => true,
    };
    if !valid
        || schema["enum"]
            .as_array()
            .is_some_and(|a| !a.contains(value))
    {
        bail!("invalid_argument: invalid value for {path}");
    }
    if let Some(values) = value.as_array() {
        for item in values {
            check(&schema["items"], item, path, false)?;
        }
    }
    if let Some(values) = value.as_object() {
        let props = schema["properties"]
            .as_object()
            .context("internal_error: schema lacks properties")?;
        for (key, item) in values {
            check(
                props
                    .get(key)
                    .with_context(|| format!("invalid_argument: unknown parameter {path}.{key}"))?,
                item,
                &format!("{path}.{key}"),
                false,
            )?;
        }
        if !skip_required {
            for key in schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !values.contains_key(key) {
                    bail!("invalid_argument: missing {path}.{key}");
                }
            }
        }
    }
    Ok(())
}
pub fn validate(tool: &str, args: &Value) -> Result<()> {
    if !TOOL_NAMES.contains(&tool) {
        bail!("capability_unsupported: unknown tool {tool}");
    }
    let cursor = args.get("cursor").is_some();
    let schema = tool_schema(tool);
    check(&schema, args, tool, cursor)?;
    if cursor {
        return Ok(());
    }
    if args.get("risk_inputs").is_some()
        && ((tool != "agent_preflight" && args.get("diff").is_none())
            || args["risk_inputs"].as_object().is_none_or(Map::is_empty))
    {
        bail!(
            "invalid_argument: risk inputs require a diff or preflight and at least one side artifact"
        );
    }
    if args["cycles"] == true && args["dependencies"] == "none" {
        bail!("invalid_argument: cycles require crate or module dependencies");
    }
    if args["api_scope"] == "external"
        && args["visibility"]
            .as_str()
            .is_some_and(|visibility| !["public", "all"].contains(&visibility))
    {
        bail!(
            "invalid_argument: external API inventory cannot filter private or restricted declarations"
        );
    }
    for key in schema["anyOf"][1]["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if args.get(key).is_none() {
            bail!("invalid_argument: missing {tool}.{key}");
        }
    }
    if args.get("semantic_epoch").is_some() && args.get("snapshot_id").is_none() {
        bail!("invalid_argument: semantic_epoch requires snapshot_id");
    }
    let timeout = args["timeout_ms"].as_u64().unwrap_or(5000);
    if let Some(path) = args["path_prefix"].as_str().filter(|p| *p != ".") {
        safe_relative(path)?;
    }
    if args["fresh_wait_ms"]
        .as_u64()
        .is_some_and(|n| n > timeout / 2)
    {
        bail!("invalid_argument: fresh_wait_ms cannot exceed half of timeout_ms");
    }
    if matches!(tool, "impact_of" | "suggest_tests")
        && args.get("symbol").is_some() == args.get("diff").is_some()
    {
        bail!("invalid_argument: exactly one of symbol and diff is required");
    }
    if let Some(diff) = args.get("diff") {
        if args.get("snapshot_id").is_some() || args.get("semantic_epoch").is_some() {
            bail!(
                "invalid_argument: diff selectors cannot be combined with top-level snapshot parameters"
            );
        }
        for side in ["base", "head"] {
            let selector = &diff[side];
            let keys = selector.as_object().unwrap();
            let valid = match selector["kind"].as_str() {
                Some("revision") => keys.len() == 2 && keys.contains_key("revision"),
                Some("snapshot") => {
                    keys.contains_key("snapshot_id")
                        && keys.len() <= 4
                        && !keys.contains_key("revision")
                }
                Some("workspace") => side == "head" && keys.len() == 1,
                _ => false,
            };
            if !valid {
                bail!("invalid_argument: invalid diff {side} selector");
            }
        }
    }
    if tool == "context_pack" && (args.get("symbol").is_some() == args.get("task").is_some()) {
        bail!("invalid_argument: exactly one of symbol and task is required");
    }
    let stale = args["include_stale"].as_bool().unwrap_or(false);
    if stale != args.get("stale_from").is_some() {
        bail!("invalid_argument: include_stale requires stale_from and vice versa");
    }
    if args["freshness"]
        .as_array()
        .is_some_and(|a| a.contains(&json!("stale")))
        && !stale
    {
        bail!("invalid_argument: stale filtering requires find_references history");
    }
    Ok(())
}

pub fn safe_relative(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("invalid_argument: expected a normalized workspace-relative path");
    }
    Ok(path.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_inputs_are_structured_and_bound_to_diff_or_preflight() {
        let artifact = json!({"schema_version":"0.1","workspace_id":"ws_risk","files":[{"path":"src/lib.rs","file_hash":"a".repeat(64),"lines":[{"line":1,"hits":0}]}],
            "coverage":{"format":"lcov","report_digest":"b".repeat(64),"source_manifest_digest":"c".repeat(64),"omitted_records":0}});
        let diff =
            json!({"base":{"kind":"revision","revision":"HEAD"},"head":{"kind":"workspace"}});
        for tool in [
            "impact_of",
            "verify_change",
            "suggest_tests",
            "agent_verify",
            "arch_check",
            "contract_impact",
        ] {
            validate(tool, &json!({"diff":diff,"risk_inputs":{"head":artifact}})).unwrap();
        }
        assert!(
            validate(
                "impact_of",
                &json!({"symbol":"entry","risk_inputs":{"head":artifact}})
            )
            .is_err()
        );
        assert!(validate("impact_of", &json!({"diff":diff,"risk_inputs":{}})).is_err());
        validate(
            "agent_preflight",
            &json!({"files":["src/lib.rs"],"risk_inputs":{"head":artifact}}),
        )
        .unwrap();
        assert!(
            validate(
                "agent_preflight",
                &json!({"files":["src/lib.rs"],"risk_inputs":{"base":artifact}})
            )
            .is_err()
        );
        let mut invalid = artifact;
        invalid["files"][0]["lines"][0]["line"] = json!(0);
        assert!(
            validate(
                "impact_of",
                &json!({"diff":diff,"risk_inputs":{"head":invalid}})
            )
            .is_err()
        );
    }

    #[test]
    fn tool_registry_exposes_implementations_and_change_verification() {
        let expected = [
            "capabilities",
            "index_status",
            "repo_map",
            "search_symbols",
            "symbol_at",
            "get_symbol",
            "find_references",
            "call_graph",
            "impact_of",
            "context_pack",
            "find_implementations",
            "verify_change",
            "find_paths",
            "suggest_tests",
            "arch_check",
            "agent_preflight",
            "agent_verify",
            "get_review_result",
            "contract_impact",
        ];
        assert_eq!(TOOL_NAMES, expected);
        let registered = tools();
        assert_eq!(registered.len(), 19);
        for (tool, name) in registered.iter().zip(expected) {
            assert_eq!(tool["name"], name);
            assert_eq!(tool["version"], "0.1");
            assert!(!tool["description"].as_str().unwrap().is_empty());
            assert_eq!(tool["input_schema"], tool_schema(name));
        }
    }

    #[test]
    fn test_suggestions_accept_one_target_mode_and_reject_invalid_combinations() {
        let diff =
            json!({"base":{"kind":"revision","revision":"HEAD"},"head":{"kind":"workspace"}});
        for args in [
            json!({"symbol":"stock"}),
            json!({"diff":diff}),
            json!({"symbol":"stock","source":["L1"],"path_prefix":"src","depth":10,"max_nodes":100000}),
            json!({"cursor":"page"}),
        ] {
            validate("suggest_tests", &args).unwrap();
        }
        for args in [
            json!({}),
            json!({"symbol":""}),
            json!({"symbol":"stock","diff":diff}),
            json!({"diff":diff,"snapshot_id":"head"}),
            json!({"symbol":"stock","depth":0}),
            json!({"symbol":"stock","source":["L4"]}),
        ] {
            let error = validate("suggest_tests", &args).unwrap_err();
            assert_eq!(
                error_value(&error)["error"]["code"],
                "invalid_argument",
                "{args}"
            );
        }
    }

    #[test]
    fn paths_require_two_symbols_or_a_cursor_and_bounded_limits() {
        let schema = tool_schema("find_paths");
        assert_eq!(schema["anyOf"][1]["required"], json!(["from", "to"]));
        for args in [
            json!({"from":"entry","to":"stock"}),
            json!({"from":"entry","to":"stock","max_depth":10,"max_paths":1000,"max_nodes":100000,
                "source":["L1"],"certainty":["candidate"],"freshness":["current"],"path_prefix":"src"}),
            json!({"cursor":"next"}),
        ] {
            validate("find_paths", &args).unwrap();
        }
        for args in [
            json!({}),
            json!({"from":"entry"}),
            json!({"to":"stock"}),
            json!({"from":"","to":"stock"}),
            json!({"from":"entry","to":"stock","max_depth":0}),
            json!({"from":"entry","to":"stock","max_depth":11}),
            json!({"from":"entry","to":"stock","max_paths":0}),
            json!({"from":"entry","to":"stock","max_paths":1001}),
            json!({"from":"entry","to":"stock","max_nodes":0}),
            json!({"from":"entry","to":"stock","path_prefix":"../src"}),
            json!({"from":"entry","to":"stock","freshness":["stale"]}),
        ] {
            let error = validate("find_paths", &args).unwrap_err();
            assert_eq!(
                error_value(&error)["error"]["code"],
                "invalid_argument",
                "{args}"
            );
        }
    }

    #[test]
    fn verification_requires_a_diff_or_cursor_and_validates_both_selectors() {
        let schema = tool_schema("verify_change");
        assert_eq!(schema["anyOf"][1]["required"], json!(["diff"]));
        assert!(schema["properties"].get("symbol").is_none());
        let diff =
            json!({"base":{"kind":"revision","revision":"HEAD"},"head":{"kind":"workspace"}});
        validate("verify_change", &json!({"diff":diff})).unwrap();
        validate("verify_change", &json!({"cursor":"page"})).unwrap();
        validate("verify_change", &json!({"diff":{"base":{"kind":"snapshot","snapshot_id":"base","semantic_epoch":0},"head":{"kind":"snapshot","snapshot_id":"head"}}})).unwrap();
        for args in [
            json!({}),
            json!({"symbol":"stock"}),
            json!({"diff":diff,"snapshot_id":"head"}),
            json!({"diff":diff,"semantic_epoch":0}),
            json!({"diff":diff,"rules":["unknown"]}),
            json!({"diff":diff,"depth":0}),
            json!({"diff":diff,"max_nodes":0}),
            json!({"diff":{"base":{"kind":"workspace"},"head":{"kind":"workspace"}}}),
            json!({"diff":{"base":{"kind":"revision"},"head":{"kind":"workspace"}}}),
            json!({"diff":{"base":{"kind":"snapshot","semantic_epoch":0},"head":{"kind":"workspace"}}}),
            json!({"diff":{"base":{"kind":"revision","revision":"HEAD","snapshot_id":"base"},"head":{"kind":"workspace"}}}),
            json!({"diff":{"base":{"kind":"revision","revision":"HEAD"},"head":{"kind":"workspace","revision":"HEAD"}}}),
        ] {
            let error = validate("verify_change", &args).unwrap_err();
            assert_eq!(
                error_value(&error)["error"]["code"],
                "invalid_argument",
                "{args}"
            );
        }
    }

    #[test]
    fn implementations_schema_requires_symbol_or_cursor_and_shares_filters() {
        let schema = tool_schema("find_implementations");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!([]));
        assert_eq!(
            schema["anyOf"],
            json!([{"required":["cursor"]},{"required":["symbol"]}])
        );
        assert_eq!(schema["properties"]["symbol"], string());
        let references = tool_schema("find_references");
        for parameter in [
            "path_prefix",
            "source",
            "certainty",
            "freshness",
            "snapshot_id",
            "semantic_epoch",
            "require_fresh",
            "fresh_wait_ms",
            "timeout_ms",
            "limit",
            "cursor",
        ] {
            assert_eq!(
                schema["properties"][parameter], references["properties"][parameter],
                "{parameter}"
            );
            assert!(!schema["properties"][parameter].is_null());
        }
        assert!(schema["properties"].get("include_stale").is_none());
        assert!(schema["properties"].get("stale_from").is_none());
    }

    #[test]
    fn implementations_accepts_symbol_filters_versions_and_cursor_only_requests() {
        for args in [
            json!({"symbol":"Draw"}),
            json!({"symbol":"Draw::draw","path_prefix":"."}),
            json!({"symbol":"Draw","path_prefix":"src","source":["L1","L2","L3"],
                "certainty":["definite","candidate"],"freshness":["current","pending"],
                "snapshot_id":"snapshot","semantic_epoch":0,"limit":1,
                "timeout_ms":1000,"require_fresh":true,"fresh_wait_ms":500}),
            json!({"cursor":"continuation"}),
            json!({"cursor":"continuation","timeout_ms":60000}),
        ] {
            validate("find_implementations", &args).unwrap();
        }
    }

    #[test]
    fn implementations_rejects_invalid_arguments_and_unsupported_history() {
        for args in [
            json!({}),
            json!({"symbol":""}),
            json!({"symbol":false}),
            json!({"symbol":"Draw","unknown":true}),
            json!({"symbol":"Draw","path_prefix":"../src"}),
            json!({"symbol":"Draw","path_prefix":"/src"}),
            json!({"symbol":"Draw","source":"L1"}),
            json!({"symbol":"Draw","source":["L4"]}),
            json!({"symbol":"Draw","certainty":["unknown"]}),
            json!({"symbol":"Draw","freshness":["unknown"]}),
            json!({"symbol":"Draw","freshness":["stale"]}),
            json!({"symbol":"Draw","include_stale":true}),
            json!({"symbol":"Draw","stale_from":{"snapshot_id":"old","semantic_epoch":0}}),
            json!({"symbol":"Draw","semantic_epoch":0}),
            json!({"symbol":"Draw","snapshot_id":"snapshot","semantic_epoch":-1}),
            json!({"symbol":"Draw","timeout_ms":1000,"fresh_wait_ms":501}),
            json!({"symbol":"Draw","limit":0}),
            json!({"symbol":"Draw","limit":1001}),
            json!({"cursor":""}),
            json!({"cursor":1}),
            json!({"cursor":"continuation","source":["L4"]}),
        ] {
            let error = validate("find_implementations", &args).unwrap_err();
            assert_eq!(
                error_value(&error)["error"]["code"],
                "invalid_argument",
                "{args}"
            );
        }
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn protocol_constants_stay_stable() {
        assert_eq!(DAEMON_PROTOCOL, "0.1");
        assert_eq!(MIN_BUDGET, 4096);
        assert_eq!(TOOL_NAMES.len(), 19);
        assert_eq!(
            TOOL_NAMES
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            TOOL_NAMES.len(),
            "tool names are unique"
        );
    }

    #[test]
    fn structured_query_errors_keep_their_code_and_details() {
        let error = failure("symbol_not_found", "missing", json!({"symbol": "x"}));
        let value = error_value(&error);
        assert_eq!(value["schema_version"], "0.1");
        assert_eq!(value["error"]["code"], "symbol_not_found");
        assert_eq!(value["error"]["message"], "missing");
        assert_eq!(value["error"]["details"]["symbol"], "x");
    }

    #[test]
    fn plain_errors_map_through_known_codes_or_internal_error() {
        for (message, expected) in [
            ("timeout: too slow", "timeout"),
            ("invalid_argument: bad", "invalid_argument"),
            ("daemon_incompatible: version", "daemon_incompatible"),
            ("cancelled: stopped", "cancelled"),
            ("something_else: bad", "internal_error"),
            ("no prefix", "internal_error"),
        ] {
            let value = error_value(&anyhow::anyhow!(message));
            assert_eq!(value["error"]["code"], expected, "{message}");
            assert_eq!(value["error"]["message"], message);
            assert!(value["error"].get("details").is_none());
        }
    }

    #[test]
    fn request_and_endpoint_envelopes_round_trip() {
        let request: DaemonRequest = serde_json::from_value(json!({
            "protocol": "0.1",
            "workspace_id": "ws",
            "config_hash": "hash",
            "method": "search_symbols",
            "args": {"query": "x"},
        }))
        .unwrap();
        assert_eq!(request.request_id, None);
        assert_eq!(request.args["query"], "x");
        let encoded = serde_json::to_value(DaemonRequest {
            request_id: Some("desktop-1".into()),
            ..request
        })
        .unwrap();
        assert_eq!(encoded["request_id"], "desktop-1");
        assert_eq!(encoded["protocol"], "0.1");

        let endpoint = DesktopEndpoint {
            protocol: DAEMON_PROTOCOL.into(),
            workspace: PathBuf::from("/workspace"),
            workspace_id: "ws".into(),
            config_hash: "hash".into(),
            endpoint: "/tmp/socket".into(),
        };
        let decoded: DesktopEndpoint =
            serde_json::from_value(serde_json::to_value(&endpoint).unwrap()).unwrap();
        assert_eq!(decoded.protocol, DAEMON_PROTOCOL);
        assert_eq!(decoded.workspace, PathBuf::from("/workspace"));
        assert_eq!(decoded.endpoint, "/tmp/socket");
    }

    #[test]
    fn tool_schemas_share_pagination_and_add_tool_specific_parameters() {
        let capabilities = tool_schema("capabilities");
        assert!(
            capabilities["properties"].get("snapshot_id").is_none(),
            "capabilities need no snapshot"
        );
        assert!(capabilities["properties"].get("timeout_ms").is_some());

        let status = tool_schema("index_status");
        assert!(status["properties"].get("snapshot_id").is_some());
        assert!(status["properties"].get("semantic_epoch").is_none());
        assert!(status["properties"].get("limit").is_none());

        let search = tool_schema("search_symbols");
        assert_eq!(search["required"], json!([]));
        assert_eq!(search["anyOf"][1]["required"], json!(["query"]));
        assert_eq!(search["properties"]["match"]["default"], "fuzzy");
        assert_eq!(
            search["properties"]["api_scope"]["enum"],
            json!(["declarations", "external"])
        );
        assert_eq!(
            search["properties"]["visibility"]["enum"],
            json!(["public", "restricted", "private", "all"])
        );
        assert_eq!(search["additionalProperties"], false);

        let review = tool_schema("get_review_result");
        assert_eq!(review["anyOf"][1]["required"], json!(["review_id"]));

        let symbol = tool_schema("get_symbol");
        assert_eq!(symbol["anyOf"][1]["required"], json!(["symbol"]));
        assert_eq!(symbol["properties"]["include_body"]["default"], false);
        assert_eq!(symbol["properties"]["include_docs"]["default"], true);

        let paths = tool_schema("find_paths");
        assert_eq!(paths["anyOf"][0]["required"], json!(["cursor"]));
        assert_eq!(paths["properties"]["max_paths"]["maximum"], 1000);
        assert_eq!(paths["properties"]["max_nodes"]["maximum"], 100000);
    }

    #[test]
    fn validate_rejects_unknown_tools_and_unknown_parameters() {
        let error = validate("no_such_tool", &json!({})).unwrap_err();
        assert_eq!(
            error_value(&error)["error"]["code"],
            "capability_unsupported"
        );
        let error = validate("capabilities", &json!({"unknown": true})).unwrap_err();
        assert_eq!(error_value(&error)["error"]["code"], "invalid_argument");
        assert!(validate("capabilities", &json!({})).is_ok());
        assert!(validate("capabilities", &json!({"timeout_ms": 1})).is_ok());
        assert!(validate("capabilities", &json!({"timeout_ms": 0})).is_err());
        assert!(validate("capabilities", &json!({"timeout_ms": 60001})).is_err());
    }

    #[test]
    fn cursor_requests_skip_tool_specific_requirements() {
        assert!(validate("get_symbol", &json!({"cursor": "next"})).is_ok());
        assert!(validate("find_paths", &json!({"cursor": "next"})).is_ok());
        assert!(validate("search_symbols", &json!({"cursor": "next"})).is_ok());
        assert!(validate("find_paths", &json!({"from": "a"})).is_err());
        assert!(validate("search_symbols", &json!({})).is_err());
        // Cursor-only requests still enforce the common parameter types.
        assert!(validate("find_paths", &json!({"cursor": "", "limit": 0})).is_err());
        assert!(validate("find_paths", &json!({"cursor": "x", "timeout_ms": 60001})).is_err());
    }

    #[test]
    fn validation_enforces_cross_field_relationships() {
        assert!(validate("index_status", &json!({"semantic_epoch": 0})).is_err());
        assert!(validate("index_status", &json!({"snapshot_id": "s"})).is_ok());
        assert!(
            validate(
                "index_status",
                &json!({"snapshot_id": "s", "semantic_epoch": 0})
            )
            .is_err(),
            "index_status has no semantic epoch parameter"
        );
        assert!(
            validate(
                "search_symbols",
                &json!({"query": "x", "timeout_ms": 1000, "fresh_wait_ms": 500})
            )
            .is_ok()
        );
        assert!(
            validate(
                "search_symbols",
                &json!({"query": "x", "timeout_ms": 1000, "fresh_wait_ms": 501})
            )
            .is_err()
        );
        assert!(
            validate(
                "search_symbols",
                &json!({"query": "x", "path_prefix": "../x"})
            )
            .is_err()
        );
        assert!(
            validate(
                "search_symbols",
                &json!({"query": "x", "path_prefix": "/abs"})
            )
            .is_err()
        );
        assert!(validate("search_symbols", &json!({"query": "x", "path_prefix": "."})).is_ok());
    }

    #[test]
    fn repo_map_and_api_scope_validations_catch_incoherent_filters() {
        assert!(
            validate(
                "repo_map",
                &json!({"snapshot_id": "s", "cycles": true, "dependencies": "none"})
            )
            .is_err()
        );
        assert!(
            validate(
                "repo_map",
                &json!({"snapshot_id": "s", "cycles": true, "dependencies": "crate"})
            )
            .is_ok()
        );
        assert!(
            validate(
                "search_symbols",
                &json!({"query": "x", "api_scope": "external", "visibility": "private"})
            )
            .is_err()
        );
        assert!(
            validate(
                "search_symbols",
                &json!({"query": "x", "api_scope": "external", "visibility": "all"})
            )
            .is_ok()
        );
        assert!(
            validate(
                "search_symbols",
                &json!({"query": "x", "api_scope": "declarations", "visibility": "private"})
            )
            .is_ok()
        );
    }

    #[test]
    fn context_pack_requires_budget_and_exactly_one_of_symbol_and_task() {
        let base = json!({"snapshot_id": "s", "budget": 20000});
        assert!(validate("context_pack", &json!({"snapshot_id": "s", "symbol": "x"})).is_err());
        assert!(
            validate(
                "context_pack",
                &json!({"snapshot_id": "s", "budget": 20000, "symbol": "x"})
            )
            .is_ok()
        );
        assert!(
            validate(
                "context_pack",
                &json!({"snapshot_id": "s", "budget": 20000, "task": "x"})
            )
            .is_ok()
        );
        assert!(validate("context_pack", &base).is_err());
        assert!(
            validate(
                "context_pack",
                &json!({"snapshot_id": "s", "budget": 20000, "symbol": "x", "task": "y"})
            )
            .is_err()
        );
    }

    #[test]
    fn stale_history_parameters_must_be_paired_and_requested() {
        assert!(validate("find_references", &json!({"symbol": "x"})).is_ok());
        assert!(
            validate(
                "find_references",
                &json!({"symbol": "x", "include_stale": true, "stale_from": {"snapshot_id": "s", "semantic_epoch": 0}})
            )
            .is_ok()
        );
        assert!(
            validate(
                "find_references",
                &json!({"symbol": "x", "include_stale": true})
            )
            .is_err()
        );
        assert!(
            validate(
                "find_references",
                &json!({"symbol": "x", "stale_from": {"snapshot_id": "s", "semantic_epoch": 0}})
            )
            .is_err()
        );
        assert!(
            validate(
                "find_references",
                &json!({"symbol": "x", "freshness": ["stale"], "include_stale": true, "stale_from": {"snapshot_id": "s", "semantic_epoch": 0}})
            )
            .is_ok()
        );
        assert!(
            validate(
                "find_references",
                &json!({"symbol": "x", "freshness": ["stale"]})
            )
            .is_err()
        );
    }

    #[test]
    fn safe_relative_accepts_normalized_paths_and_rejects_traversal() {
        assert_eq!(
            safe_relative("src/lib.rs").unwrap(),
            PathBuf::from("src/lib.rs")
        );
        assert_eq!(safe_relative("a").unwrap(), PathBuf::from("a"));
        assert_eq!(safe_relative("a/b/c").unwrap(), PathBuf::from("a/b/c"));
        assert_eq!(safe_relative("a//b").unwrap(), PathBuf::from("a//b"));
        for path in ["", ".", "..", "./a", "../a", "a/../b", "/abs"] {
            let error = safe_relative(path).unwrap_err();
            assert_eq!(
                error_value(&error)["error"]["code"],
                "invalid_argument",
                "{path}"
            );
        }
    }
}
