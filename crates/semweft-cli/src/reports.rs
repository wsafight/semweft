use crate::daemon;
use anyhow::{Context, Result, bail};
use semweft_query::config::Resolved;
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt::Write, path::Path};

pub fn load(path: &Path) -> Result<Value> {
    let report: Value = serde_json::from_slice(
        &std::fs::read(path)
            .with_context(|| format!("invalid_argument: cannot read report {}", path.display()))?,
    )
    .context("invalid_argument: input is not a JSON report")?;
    if report["schema_version"] != "0.1"
        || (report.get("snapshot_id").is_none() && report.get("error").is_none())
    {
        bail!("invalid_argument: report input must be a Semweft snapshot or error JSON response");
    }
    Ok(report)
}

pub fn set_source_root(value: &mut Value, root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .context("invalid_argument: source workspace does not exist")?;
    if !root.is_dir() {
        bail!("invalid_argument: SARIF source workspace must be a directory");
    }
    value["_semweft_source_root"] = json!(root);
    Ok(())
}

fn list_mut<'a>(value: &'a mut Value, key: &str) -> &'a mut Value {
    match key.split_once('.') {
        Some((parent, child)) => &mut value[parent][child],
        None => &mut value[key],
    }
}

pub fn collect(resolved: &Resolved, tool: &str, mut result: Value) -> Result<Value> {
    let mut seen = BTreeSet::new();
    let mut returned = result["output"]["returned_items"].as_u64().unwrap_or(0);
    let mut returned_relations = result["output"]["returned_relations"].as_u64().unwrap_or(0);
    while let Some(cursor) = result["output"]["continuation"]["next_cursor"]
        .as_str()
        .map(str::to_owned)
    {
        if !seen.insert(cursor.clone()) {
            bail!("internal_error: report pagination repeated a cursor");
        }
        let mut page = daemon::call(resolved, tool, json!({"cursor":cursor}))?;
        if page.get("error").is_some() {
            return Ok(page);
        }
        for key in [
            "snapshot_id",
            "epochs",
            "snapshots",
            "verification",
            "diff_summary",
            "test_summary",
            "risk_summary",
            "architecture",
            "guard",
            "diff_identity",
            "contract_summary",
            "api_summary",
            "api_surface_summary",
            "service_mapping",
            "service_impact_summary",
        ] {
            if page[key] != result[key] {
                bail!("internal_error: report pages do not share {key}");
            }
        }
        for key in [
            "findings",
            "test_candidates",
            "relations",
            "relation_changes.added",
            "relation_changes.removed",
            "affected_modules",
            "symbols",
            "modules",
            "dependencies",
            "entries",
            "cycles",
            "paths",
            "contract_changes",
            "contract_impacts",
            "api_changes",
            "affected_services",
        ] {
            if let Some(items) = list_mut(&mut page, key).as_array_mut() {
                list_mut(&mut result, key)
                    .as_array_mut()
                    .context("internal_error: report page lacks list")?
                    .append(items);
            }
        }
        returned += page["output"]["returned_items"].as_u64().unwrap_or(0);
        returned_relations += page["output"]["returned_relations"].as_u64().unwrap_or(0);
        result["output"] = page["output"].clone();
    }
    result["output"]["returned_items"] = json!(returned);
    result["output"]["returned_relations"] = json!(returned_relations);
    result["output"]["all_pages"] = json!(true);
    Ok(result)
}

fn uri(path: &str) -> Result<String> {
    semweft_protocol::safe_relative(path)?;
    let mut encoded = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}")?;
        }
    }
    Ok(encoded)
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn html(value: &Value) -> Result<String> {
    let text = crate::render::render(value, crate::render::Format::Text, None, true)?;
    let mut out = String::from(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\"><title>Semweft 审查报告</title><style>body{font:16px/1.6 system-ui,sans-serif;max-width:1100px;margin:32px auto;padding:0 20px;color:#18273a;background:#f5f7fa}section,details{background:white;border:1px solid #d5dde8;border-radius:10px;margin:16px 0;padding:16px}pre{white-space:pre-wrap;overflow-wrap:anywhere;font:13px/1.6 ui-monospace,monospace}summary{cursor:pointer;font-weight:600}a{color:#1557b0}h1{font-size:26px}.warning{border-left:5px solid #b36a00;padding-left:16px}</style><h1>Semweft 审查报告</h1>",
    );
    out.push_str("<p class=\"warning\">静态证据报告，不代表行为安全或兼容性。保留分析缺口与截断。源码链接指向报告目录下的当前文件，并非历史快照内容；历史坐标不链接到新源码。</p><section><pre>");
    out.push_str(&escape(&text));
    out.push_str("</pre></section>");
    fn locations(value: &Value, side: &str, output: &mut BTreeSet<(String, u64, u64, String)>) {
        if let Some(location) = value.get("location")
            && let Some(file) = location["file"].as_str()
        {
            output.insert((
                file.into(),
                location["line"].as_u64().unwrap_or(1),
                location["column"].as_u64().unwrap_or(1),
                side.into(),
            ));
        }
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    let side = child["side"].as_str().unwrap_or(
                        if matches!(
                            key.as_str(),
                            "base_symbol" | "base_exports" | "base_exposure" | "removed"
                        ) {
                            "base"
                        } else {
                            side
                        },
                    );
                    locations(child, side, output);
                }
            }
            Value::Array(array) => {
                for child in array {
                    locations(child, child["side"].as_str().unwrap_or(side), output);
                }
            }
            _ => {}
        }
    }
    let mut source_locations = BTreeSet::new();
    locations(value, "head", &mut source_locations);
    out.push_str("<details open><summary>代码位置</summary><ul>");
    for (file, line, column, side) in source_locations {
        let label = escape(&format!("{file}:{line}:{column} ({side})"));
        if side != "base"
            && let Ok(path) = uri(&file)
        {
            write!(
                out,
                "<li><a href=\"./{}#L{line}\">{label}</a></li>",
                escape(&path)
            )?;
            continue;
        }
        write!(out, "<li>{label}</li>")?;
    }
    out.push_str("</ul></details>");
    for (key, title) in [
        ("risk_summary", "风险评分及依据"),
        ("changes", "变更与公开声明"),
        ("relations", "影响关系与完整证据"),
        ("paths", "调用路径"),
        ("relation_changes", "关系增减"),
        ("completeness", "缺口与分析范围"),
        ("findings", "规则结果"),
        ("architecture", "分层与基线"),
        ("test_candidates", "测试候选"),
        ("review", "人工审查记录"),
        ("contract_changes", "Protobuf 字段与 RPC 变更"),
        ("contract_impacts", "生成代码与调用方候选"),
        ("output", "截断与续查"),
    ] {
        if let Some(section) = value.get(key) {
            write!(
                out,
                "<details><summary>{title}</summary><pre>{}</pre></details>",
                escape(&serde_json::to_string_pretty(section)?)
            )?;
        }
    }
    write!(
        out,
        "<details><summary>完整固定 JSON</summary><pre>{}</pre></details></html>",
        escape(&serde_json::to_string_pretty(value)?)
    )?;
    Ok(out)
}

#[cfg(test)]
mod html_tests {
    use super::*;
    #[test]
    fn static_html_escapes_untrusted_data_and_keeps_historical_coordinates_separate() {
        let value = json!({"schema_version":"0.1","snapshot_id":"snap","epochs":{"syntax":1,"semantic":0},
            "findings":[{"message":"<script>alert('x')</script>","evidence":{"side":"base","location":{"file":"src/old.rs","line":4}}}],
            "relations":[{"side":"head","location":{"file":"src/current.rs","line":8}},{"location":{"file":"javascript:alert(1)"}}],
            "completeness":{"status":"partial","known_gaps":[{"reason":"& untrusted <html>"}]}});
        let html = html(&value).unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(!html.contains("href=\"./src/old.rs"));
        assert!(html.contains("href=\"./src/current.rs#L8"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("default-src 'none'"));
    }
}

pub fn sarif(value: &Value) -> Result<String> {
    let mut notifications = vec![];
    let mut rules = vec![];
    let mut results = vec![];
    let error = value.get("error");
    if let Some(error) = error {
        notifications.push(json!({"level":"error","descriptor":{"id":error["code"].as_str().unwrap_or("analysis_error")},
            "message":{"text":error["message"].as_str().unwrap_or("Analysis failed")}}));
    } else {
        if value.get("verification").is_none() {
            bail!("invalid_argument: SARIF is supported for verify and saved verification reports");
        }
        if value["output"]["continuation"]["next_cursor"].is_string()
            || value["findings"].as_array().map(Vec::len)
                != value["verification"]["finding_count"]
                    .as_u64()
                    .map(|count| count as usize)
        {
            bail!(
                "invalid_argument: SARIF requires every finding; export verify --all-pages before rendering a saved report"
            );
        }
        for rule in value["verification"]["rules"]
            .as_array()
            .into_iter()
            .flatten()
        {
            rules.push(json!({"id":rule["id"],"shortDescription":{"text":rule["id"]},
                "defaultConfiguration":{"level":"warning"},
                "properties":{"semweftRuleVersion":rule["version"],"semweftStatus":rule["status"]}}));
        }
        for finding in value["findings"].as_array().into_iter().flatten() {
            let evidence = &finding["evidence"];
            let side = evidence["side"].as_str().unwrap_or("head");
            let level = if finding["status"] == "violations_found" {
                "error"
            } else {
                "warning"
            };
            let mut result = json!({"ruleId":finding["rule_id"],"level":level,
                "message":{"text":finding["message"]},
                "properties":{"semweftStatus":finding["status"],"semweftRuleVersion":finding["rule_version"],
                    "side":side,"evidence":evidence}});
            let file = evidence["location"]["file"]
                .as_str()
                .or_else(|| evidence["file"].as_str());
            if let Some(file) = file {
                let mut physical =
                    json!({"artifactLocation":{"uri":uri(file)?,"uriBaseId":"%SRCROOT%"}});
                if let Some(line) = evidence["location"]["line"]
                    .as_u64()
                    .filter(|line| *line > 0)
                {
                    physical["region"] = json!({"startLine":line});
                }
                if side == "base" {
                    result["properties"]["historicalLocation"] = physical;
                    result["message"]["text"] = json!(format!(
                        "{} [base snapshot: {}]",
                        finding["message"].as_str().unwrap_or(""),
                        file
                    ));
                } else {
                    result["locations"] = json!([{"physicalLocation":physical}]);
                }
            }
            results.push(result);
        }
        if value["verification"]["status"] == "insufficient_evidence"
            || value["completeness"]["status"] != "within_scope"
            || value["output"]["truncated"] == true
        {
            notifications.push(json!({"level":"warning","descriptor":{"id":"semweft_incomplete_analysis"},
                "message":{"text":"Analysis has missing evidence or traversal limits; no finding is not proof of safety. Per-test coverage is unavailable; Semweft did not execute tests."}}));
        }
    }
    let root = if let Some(root) = value["_semweft_source_root"].as_str() {
        std::path::PathBuf::from(root)
    } else {
        std::env::current_dir()?
    }
    .canonicalize()?;
    let source_root = url::Url::from_directory_path(root).map_err(|_| {
        anyhow::anyhow!("invalid_argument: cannot represent workspace as a file URI")
    })?;
    let mut properties = json!({"coverage":value["risk_summary"]["dimensions"]["test_coverage"]["status"].as_str().unwrap_or("unavailable"),"perTestCoverage":"unavailable","testsExecuted":false});
    for key in [
        "schema_version",
        "snapshots",
        "verification",
        "completeness",
        "output",
        "risk_summary",
    ] {
        if !value[key].is_null() {
            properties[key] = value[key].clone();
        }
    }
    let report = json!({"$schema":"https://docs.oasis-open.org/sarif/sarif/v2.1.0/os/schemas/sarif-schema-2.1.0.json","version":"2.1.0",
        "runs":[{"tool":{"driver":{"name":"Semweft","version":env!("CARGO_PKG_VERSION"),"rules":rules}},
            "columnKind":"unicodeCodePoints","results":results,
            "invocations":[{"executionSuccessful":error.is_none(),"toolExecutionNotifications":notifications}],
            "originalUriBaseIds":{"%SRCROOT%":{"uri":source_root}},
            "properties":properties}]});
    Ok(serde_json::to_string_pretty(&report)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sarif_retains_old_coordinates_as_historical_and_escapes_current_uris() {
        let value = json!({"verification":{"status":"insufficient_evidence","finding_count":2,"rules":[{"id":"review","version":"0.1"}]},
            "findings":[{"rule_id":"review","rule_version":"0.1","status":"insufficient_evidence","message":"candidate",
                "evidence":{"side":"head","location":{"file":"src/a b#文.rs","line":4,"column":9},"certainty":"candidate"}},
                {"rule_id":"review","rule_version":"0.1","status":"insufficient_evidence","message":"deleted call",
                "evidence":{"side":"base","location":{"file":"src/old.rs","line":10}}}],
            "output":{"truncated":false,"continuation":{}},"completeness":{"status":"partial"}});
        let report: Value = serde_json::from_str(&sarif(&value).unwrap()).unwrap();
        assert_eq!(report["version"], "2.1.0");
        assert!(
            report["runs"][0]["originalUriBaseIds"]["%SRCROOT%"]["uri"]
                .as_str()
                .unwrap()
                .starts_with("file:")
        );
        let results = &report["runs"][0]["results"];
        assert_eq!(
            results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "src/a%20b%23%E6%96%87.rs"
        );
        assert_eq!(
            results[0]["locations"][0]["physicalLocation"]["region"],
            json!({"startLine":4})
        );
        assert_eq!(results[0]["level"], "warning");
        assert!(results[1].get("locations").is_none());
        assert_eq!(
            results[1]["properties"]["historicalLocation"]["region"]["startLine"],
            10
        );
        assert_eq!(
            report["runs"][0]["invocations"][0]["executionSuccessful"],
            true
        );
        assert_eq!(
            report["runs"][0]["invocations"][0]["toolExecutionNotifications"][0]["level"],
            "warning"
        );
        let mut clipped = value;
        clipped["findings"].as_array_mut().unwrap().pop();
        assert!(
            sarif(&clipped)
                .unwrap_err()
                .to_string()
                .contains("every finding")
        );
    }

    #[test]
    fn sarif_reports_failed_analysis_and_rejects_other_queries() {
        let report: Value = serde_json::from_str(
            &sarif(&json!({"error":{"code":"timeout","message":"no evidence"}})).unwrap(),
        )
        .unwrap();
        assert_eq!(
            report["runs"][0]["invocations"][0]["executionSuccessful"],
            false
        );
        assert_eq!(report["runs"][0]["results"], json!([]));
        assert!(sarif(&json!({"relations":[]})).is_err());
    }
}
