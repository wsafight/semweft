//! Shared query schemas, validation, errors and local IPC request envelope.
//! This crate has no daemon, language frontend or filesystem execution dependency.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Component, Path, PathBuf};

pub const DAEMON_PROTOCOL: &str = "0.1";

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
pub const TOOL_NAMES: [&str; 10] = [
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
        "repo_map" => {
            props.extend(Map::from_iter([
                ("path_prefix".into(), string()),
                ("depth".into(), integer(2, 0, 10)),
                ("max_symbols_per_module".into(), integer(20, 0, 1000)),
                (
                    "dependencies".into(),
                    choice(&["crate", "module", "none"], "crate"),
                ),
            ]));
        }
        "search_symbols" => {
            required.push("query");
            props.extend(Map::from_iter([
                ("query".into(), string()),
                ("kind".into(), string()),
                ("path_prefix".into(), string()),
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
        "call_graph" | "impact_of" => {
            props.extend(Map::from_iter([
                ("symbol".into(), string()),
                ("depth".into(), integer(3, 1, 10)),
                ("max_nodes".into(), integer(1000, 1, 100000)),
            ]));
            if name == "call_graph" {
                required.push("symbol");
                props.insert(
                    "direction".into(),
                    choice(&["callers", "callees", "both"], "callers"),
                );
            } else {
                let selector = object(
                    json!({"kind":choice(&["revision","snapshot","workspace"],"workspace"),"revision":string(),"snapshot_id":string(),"semantic_epoch":integer(0,0,u64::MAX)}),
                    &["kind"],
                );
                props.insert(
                    "diff".into(),
                    object(json!({"base":selector,"head":selector}), &["base", "head"]),
                );
            }
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
    if ["find_references", "call_graph", "impact_of"].contains(&name) {
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
    if tool == "impact_of" {
        if args.get("symbol").is_some() == args.get("diff").is_some() {
            bail!("invalid_argument: exactly one of symbol and diff is required");
        }
        if args.get("diff").is_some()
            && (args.get("snapshot_id").is_some() || args.get("semantic_epoch").is_some())
        {
            bail!(
                "invalid_argument: diff selectors cannot be combined with top-level snapshot parameters"
            );
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
