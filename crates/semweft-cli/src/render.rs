use anyhow::{Result, bail};
use clap::ValueEnum;
use semweft_core::digest;
use serde_json::Value;
use std::fmt::Write;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Format {
    Json,
    Text,
    Markdown,
    Mermaid,
    Dot,
}
fn location(value: &Value) -> String {
    format!(
        "{}:{}:{}",
        value["file"].as_str().unwrap_or("?"),
        value["line"].as_u64().unwrap_or(1),
        value["column"].as_u64().unwrap_or(1)
    )
}
fn label(value: &Value) -> &str {
    value["qualified_name"]
        .as_str()
        .or_else(|| value["name"].as_str())
        .unwrap_or("?")
}
pub fn render(
    value: &Value,
    format: Format,
    max_chars: Option<usize>,
    gaps_all: bool,
) -> Result<String> {
    if matches!(format, Format::Json) {
        return Ok(if value.get("token_count").is_some() {
            serde_json::to_string(value)?
        } else {
            serde_json::to_string_pretty(value)?
        });
    }
    let markdown = matches!(format, Format::Markdown);
    let mut out = String::new();
    if let Some(error) = value.get("error") {
        return Ok(format!(
            "{}: {}",
            error["code"].as_str().unwrap_or("error"),
            error["message"].as_str().unwrap_or("")
        ));
    }
    if matches!(format, Format::Mermaid | Format::Dot) {
        return graph(value, format);
    }
    writeln!(
        out,
        "Semweft | snapshot {} | epochs {}/{} | {}",
        value["snapshot_id"].as_str().unwrap_or("not indexed"),
        value["epochs"]["syntax"],
        value["epochs"]["semantic"],
        value["completeness"]["status"].as_str().unwrap_or("status")
    )?;
    let pending = value["completeness"]["pending_units"]
        .as_array()
        .map_or(0, Vec::len);
    writeln!(
        out,
        "Pending: {pending} | truncated: {}{}",
        value["output"]["truncated"].as_bool().unwrap_or(false),
        value["output"]["truncation_reason"]
            .as_str()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default()
    )?;
    if value.get("fresh_requirement").is_some() {
        writeln!(
            out,
            "Semantic freshness: {}{}",
            value["fresh_requirement"]["satisfied"],
            value["fresh_requirement"]["reason"]
                .as_str()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        )?;
    }
    let header_len = out.len();
    if value.get("evidence_summary").is_some() {
        writeln!(
            out,
            "Relations: {} | evidence L1/L2/L3: {}/{}/{} | definite/candidate: {}/{}",
            value["evidence_summary"]["relation_count"],
            value["evidence_summary"]["source"]["L1"],
            value["evidence_summary"]["source"]["L2"],
            value["evidence_summary"]["source"]["L3"],
            value["evidence_summary"]["certainty"]["definite"],
            value["evidence_summary"]["certainty"]["candidate"]
        )?;
    }
    if let Some(health) = value.get("health") {
        writeln!(
            out,
            "Semantic backend: {}{}",
            health["refresh"]["state"].as_str().unwrap_or("unknown"),
            health["refresh"]["failure_reason"]
                .as_str()
                .map(|r| format!(" — {r}"))
                .unwrap_or_default()
        )?;
    }
    if let Some(health) = value.get("analysis_health") {
        writeln!(
            out,
            "Source files: {} | symbols: {} | calls: {} | occurrences without current semantic binding: {}",
            health["source_files"],
            health["syntax_symbols"],
            health["syntax_calls"],
            health["unresolved_occurrence_count"]
        )?;
        for file in health["parse_failed_files"]
            .as_array()
            .into_iter()
            .flatten()
        {
            writeln!(out, "- parse failed: {}", file.as_str().unwrap_or("?"))?;
        }
    }
    let impact = value.get("changes").is_some();
    if impact {
        heading(&mut out, "摘要", markdown)?;
        writeln!(
            out,
            "候选影响：直接调用方 {}，可达调用方 {}。候选关系不表示调用方一定需要修改。",
            value["summary"]["direct_callers"], value["summary"]["transitive_callers"]
        )?;
        heading(&mut out, "变更符号", markdown)?;
        if let Some(changes) = value["changes"].as_array() {
            for change in changes {
                writeln!(
                    out,
                    "- {}: {} → {} ({})",
                    change["kind"].as_str().unwrap_or("?"),
                    label(&change["base_symbol"]),
                    label(&change["head_symbol"]),
                    change["matching"].as_str().unwrap_or("")
                )?;
            }
        }
        heading(&mut out, "候选影响", markdown)?;
    }
    if let Some(symbol) = value.get("symbol").or_else(|| value.get("target")) {
        writeln!(
            out,
            "\n{} — {}\n{}",
            label(symbol),
            location(&symbol["location"]),
            symbol["signature"].as_str().unwrap_or("")
        )?;
        if let Some(docs) = symbol["docs"].as_str().filter(|s| !s.is_empty()) {
            writeln!(out, "{docs}")?;
        }
        if let Some(body) = symbol["body"].as_str() {
            writeln!(out, "\n{body}")?;
        }
    }
    if let Some(symbols) = value["symbols"].as_array() {
        for s in symbols {
            writeln!(
                out,
                "- {} [{}] — {}",
                label(s),
                s["kind"].as_str().unwrap_or("?"),
                location(&s["location"])
            )?;
        }
    }
    if let Some(modules) = value["modules"].as_array() {
        let mut previous_crate = "";
        for module in modules {
            if let Some(name) = module["crate"].as_str()
                && name != previous_crate
            {
                writeln!(out, "\ncrate {name}")?;
                previous_crate = name;
            }
            writeln!(
                out,
                "\n{} ({} symbols)",
                module["path"].as_str().unwrap_or("?"),
                module["symbol_count"]
            )?;
            for symbol in module["symbols"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "  {} — {}",
                    label(symbol),
                    symbol["signature"].as_str().unwrap_or("")
                )?;
            }
            if module["omitted_symbols"].as_u64().unwrap_or(0) > 0 {
                writeln!(
                    out,
                    "  {} symbols omitted; use map --max-symbols-per-module 1000",
                    module["omitted_symbols"]
                )?;
            }
        }
    }
    if let Some(entries) = value["entries"].as_array() {
        for entry in entries {
            writeln!(
                out,
                "- Cargo {} {} — {}",
                entry["kind"].as_str().unwrap_or("target"),
                entry["target"].as_str().unwrap_or("?"),
                entry["file"].as_str().unwrap_or("?")
            )?;
        }
    }
    if let Some(deps) = value["dependencies"].as_array() {
        for dep in deps {
            writeln!(
                out,
                "- {} → {} [{} {}]",
                dep["from"].as_str().unwrap_or("?"),
                dep["to"].as_str().unwrap_or("?"),
                dep["source"].as_str().unwrap_or("?"),
                dep["certainty"].as_str().unwrap_or("?")
            )?;
        }
    }
    if let Some(relations) = value["relations"].as_array() {
        let mut test = vec![];
        let mut grouped = std::collections::BTreeMap::<String, Vec<&Value>>::new();
        for r in relations {
            if r["scope"] == "test" {
                test.push(r);
                continue;
            }
            grouped
                .entry(r["location"]["file"].as_str().unwrap_or("?").into())
                .or_default()
                .push(r);
        }
        for (module, edges) in grouped {
            writeln!(out, "\n{module}")?;
            for r in edges {
                edge(&mut out, r)?;
            }
        }
        if !test.is_empty() {
            writeln!(out, "\n测试作用域：{} 条候选关系", test.len())?;
            if markdown {
                writeln!(out, "\n<details><summary>测试关系</summary>\n")?;
            }
            for r in test {
                edge(&mut out, r)?;
            }
            if markdown {
                writeln!(out, "\n</details>")?;
            }
        }
    }
    if let Some(history) = value.get("history") {
        heading(&mut out, "历史引用", markdown)?;
        writeln!(
            out,
            "来源 {}，历史关系单独标为 stale。",
            history["snapshot_id"].as_str().unwrap_or("?")
        )?;
        for r in history["relations"].as_array().into_iter().flatten() {
            edge(&mut out, r)?;
        }
    }
    if let Some(delta) = value.get("relation_changes") {
        writeln!(
            out,
            "\n两侧关系变化：新增 {}，移除 {}，保留 {}（{}）。",
            delta["added_count"],
            delta["removed_count"],
            delta["unchanged_count"],
            delta["basis"].as_str().unwrap_or("unknown")
        )?;
        for (key, title) in [("added", "head 新增关系"), ("removed", "base 移除关系")] {
            let items = delta[key].as_array().map(Vec::as_slice).unwrap_or(&[]);
            if !items.is_empty() {
                writeln!(out, "\n{title}")?;
                for relation in items {
                    edge(&mut out, relation)?;
                }
            }
        }
    }
    if let Some(residual) = value["residual_references"]
        .as_array()
        .filter(|v| !v.is_empty())
    {
        writeln!(out, "\n残留名称引用（L1 candidate，尚未解析绑定）：")?;
        for r in residual {
            writeln!(
                out,
                "- {} — {}",
                r["name"].as_str().unwrap_or("?"),
                location(&r["location"])
            )?;
        }
    }
    if let Some(items) = value["items"].as_array() {
        for item in items {
            writeln!(
                out,
                "\n{} [{}] — {}\n{}",
                label(item),
                item["granularity"].as_str().unwrap_or("?"),
                location(&item["location"]),
                item["content"].as_str().unwrap_or("")
            )?;
        }
        writeln!(
            out,
            "\nTokens ({}): {} / {}, omitted: {}",
            value["token_count"]["mode"].as_str().unwrap_or("estimated"),
            value["token_count"]["total"],
            value["token_count"]["budget"],
            value["omitted_count"]
        )?;
    }
    if impact {
        heading(&mut out, "已知缺口", markdown)?;
    }
    let gaps = value["completeness"]["known_gaps"].as_array();
    if let Some(gaps) = gaps {
        let mut grouped = std::collections::BTreeMap::<String, usize>::new();
        for gap in gaps {
            *grouped
                .entry(gap["kind"].as_str().unwrap_or("unknown").into())
                .or_default() += gap["count"].as_u64().unwrap_or(1) as usize;
        }
        for (kind, count) in grouped {
            writeln!(out, "- {kind}: {count}")?;
        }
        if gaps_all {
            for gap in gaps {
                writeln!(
                    out,
                    "- {}: {}",
                    gap["file"].as_str().unwrap_or("workspace"),
                    gap["reason"].as_str().unwrap_or("")
                )?;
            }
        } else if !gaps.is_empty() {
            let files = value["relations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r["location"]["file"].as_str())
                .chain(
                    value["changes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|c| {
                            [
                                c["base_symbol"]["location"]["file"].as_str(),
                                c["head_symbol"]["location"]["file"].as_str(),
                            ]
                        })
                        .flatten(),
                )
                .chain(
                    value
                        .get("target")
                        .and_then(|s| s["location"]["file"].as_str()),
                )
                .chain(
                    value
                        .get("symbol")
                        .and_then(|s| s["location"]["file"].as_str()),
                )
                .collect::<std::collections::BTreeSet<_>>();
            for gap in gaps
                .iter()
                .filter(|g| g["file"].as_str().is_some_and(|file| files.contains(file)))
                .take(20)
            {
                writeln!(
                    out,
                    "- {}: {}",
                    gap["file"].as_str().unwrap_or("workspace"),
                    gap["reason"].as_str().unwrap_or("")
                )?;
            }
            writeln!(
                out,
                "完整缺口：同一命令加 --gaps all，或 semweft status --snapshot-id {} --format json",
                value["snapshot_id"].as_str().unwrap_or("")
            )?;
        }
    }
    if impact {
        heading(&mut out, "未检查项与截断", markdown)?;
        writeln!(
            out,
            "外部消费者、行为正确性、运行时分派完整性、兼容性核验与测试建议未包含。"
        )?;
    }
    if value["output"]["truncated"] == true {
        writeln!(out, "续查：{}", value["output"]["continuation"])?;
    }
    if impact {
        heading(&mut out, "版本信息", markdown)?;
        writeln!(out, "{}", value["snapshots"])?;
    }
    if value.get("completeness").is_none() {
        writeln!(out, "\n{}", serde_json::to_string_pretty(value)?)?;
    }
    if markdown {
        writeln!(out, "\nReport hash: {}", report_hash(value)?)?;
    }
    if let Some(max) = max_chars
        && out.chars().count() > max
    {
        let command = if value.get("snapshots").is_some() {
            "同一 diff 命令加 --format json；固定两侧版本见响应 snapshots".to_owned()
        } else {
            format!(
                "同一命令加 --format json --snapshot-id {} --semantic-epoch {}",
                value["snapshot_id"].as_str().unwrap_or(""),
                value["epochs"]["semantic"]
            )
        };
        let suffix = format!(
            "\n\n输出已裁剪。完整结果：{command}。\n{}",
            if markdown {
                format!("Report hash: {}\n", report_hash(value)?)
            } else {
                String::new()
            }
        );
        let header = &out[..header_len];
        let notice = "展示裁剪：max_chars\n";
        if max < header.chars().count() + suffix.chars().count() + notice.chars().count() {
            bail!(
                "invalid_argument: max_chars cannot hold the version header and continuation; minimum {}",
                header.chars().count() + suffix.chars().count() + notice.chars().count()
            );
        }
        let available = max.saturating_sub(suffix.chars().count());
        let paragraphs = out[header_len..].split("\n\n");
        let mut shortened = format!("{header}{notice}");
        for p in paragraphs {
            if shortened.chars().count() + p.chars().count() + 2 > available {
                break;
            }
            shortened.push_str(p);
            shortened.push_str("\n\n");
        }
        shortened.push_str(&suffix);
        return Ok(shortened);
    }
    Ok(out)
}
fn heading(out: &mut String, text: &str, markdown: bool) -> Result<()> {
    writeln!(out, "\n{}{text}\n", if markdown { "## " } else { "" })?;
    Ok(())
}
fn edge(out: &mut String, r: &Value) -> Result<()> {
    writeln!(
        out,
        "- {} → {} [{} {} {}] — {}",
        r["from_name"].as_str().unwrap_or("?"),
        r["to_name"].as_str().unwrap_or("?"),
        r["kind"].as_str().unwrap_or("?"),
        r["certainty"].as_str().unwrap_or("?"),
        r["freshness"].as_str().unwrap_or("?"),
        location(&r["location"])
    )?;
    if let Some(origin) = r["origin"]["snapshot_id"].as_str() {
        writeln!(out, "  该位置属于旧快照 {origin}，尚未验证当前坐标。")?;
    }
    Ok(())
}
fn graph(value: &Value, format: Format) -> Result<String> {
    let dependencies = value["dependencies"].as_array().ok_or_else(|| {
        anyhow::anyhow!("invalid_argument: graph formats are available for map only")
    })?;
    if value.get("error").is_some() {
        bail!("invalid_argument: graph output requires successful map result");
    }
    let mut out = String::new();
    let mermaid = matches!(format, Format::Mermaid);
    if mermaid {
        writeln!(
            out,
            "flowchart LR\n  %% snapshot {} epochs {}/{} completeness {} pending {} truncated {}",
            value["snapshot_id"],
            value["epochs"]["syntax"],
            value["epochs"]["semantic"],
            value["completeness"]["status"],
            value["completeness"]["pending_units"]
                .as_array()
                .map_or(0, Vec::len),
            value["output"]["truncated"]
        )?;
    } else {
        writeln!(
            out,
            "digraph semweft {{\n  // snapshot {} epochs {}/{} completeness {} truncated {}",
            value["snapshot_id"],
            value["epochs"]["syntax"],
            value["epochs"]["semantic"],
            value["completeness"]["status"],
            value["output"]["truncated"]
        )?;
    }
    let mut nodes = std::collections::BTreeSet::new();
    if value["dependency_kind"] == "module" {
        for module in value["modules"].as_array().into_iter().flatten() {
            if let Some(path) = module["path"].as_str() {
                nodes.insert(path.to_owned());
            }
        }
    } else {
        for entry in value["entries"].as_array().into_iter().flatten() {
            if let Some(name) = entry["crate"].as_str() {
                nodes.insert(name.to_owned());
            }
        }
    }
    for dep in dependencies {
        for key in ["from", "to"] {
            nodes.insert(dep[key].as_str().unwrap_or("?").to_owned());
        }
    }
    writeln!(
        out,
        "  {} remaining items: {}; omitted modules by depth: {}",
        if mermaid { "%%" } else { "//" },
        value["output"]["remaining_items"],
        value["omitted_modules"].as_array().map_or(0, Vec::len)
    )?;
    let ids = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), format!("n{i}")))
        .collect::<std::collections::BTreeMap<_, _>>();
    for (label, id) in &ids {
        if mermaid {
            writeln!(
                out,
                "  {id}[\"{}\"]",
                label
                    .replace('&', "&amp;")
                    .replace('"', "&quot;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace(['\n', '\r'], " ")
            )?;
        } else {
            writeln!(out, "  {id} [label={}];", serde_json::to_string(label)?)?;
        }
    }
    for dep in dependencies {
        let a = &ids[dep["from"].as_str().unwrap_or("?")];
        let b = &ids[dep["to"].as_str().unwrap_or("?")];
        let label = format!(
            "{} {}",
            dep["source"].as_str().unwrap_or("?"),
            dep["certainty"].as_str().unwrap_or("?")
        );
        let definite = dep["certainty"] == "definite";
        if mermaid {
            writeln!(
                out,
                "  {a} {}|{label}| {b}",
                if definite { "-->" } else { "-.->" }
            )?;
        } else {
            writeln!(
                out,
                "  {a} -> {b} [label={},style={}];",
                serde_json::to_string(&label)?,
                if definite { "solid" } else { "dashed" }
            )?;
        }
    }
    if !mermaid {
        out.push_str("}\n");
    }
    Ok(out)
}

/// Ignore operational timing and ephemeral cursor IDs when hashing a report.
fn report_hash(value: &Value) -> Result<String> {
    fn normalize(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for key in [
                    "waited_ms",
                    "next_cursor",
                    "omitted_cursor",
                    "daemon_pid",
                    "last_refresh_ms",
                ] {
                    map.remove(key);
                }
                for child in map.values_mut() {
                    normalize(child);
                }
            }
            Value::Array(values) => {
                for child in values {
                    normalize(child);
                }
            }
            _ => {}
        }
    }
    let mut normalized = value.clone();
    normalize(&mut normalized);
    Ok(digest(serde_json::to_vec(&normalized)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn impact() -> Value {
        json!({
            "schema_version":"0.1",
            "snapshot_id":"snap_1",
            "epochs":{"syntax":2,"semantic":1},
            "completeness":{"status":"partial","pending_units":[],"known_gaps":[
                {"kind":"unresolved_call","file":"src/lib.rs","reason":"missing target","count":1}
            ]},
            "output":{"truncated":false},
            "evidence_summary":{"relation_count":1,"source":{"L1":1,"L2":0,"L3":0},"certainty":{"definite":0,"candidate":1}},
            "changes":[{
                "kind":"modified","matching":"exact",
                "base_symbol":{"qualified_name":"shop::stock"},
                "head_symbol":{"qualified_name":"shop::stock"}
            }],
            "summary":{"direct_callers":1,"transitive_callers":2},
            "snapshots":{"base":{"snapshot_id":"base"},"head":{"snapshot_id":"head"}}
        })
    }

    #[test]
    fn json_is_compact_only_for_budgeted_context() {
        let compact = render(
            &json!({"token_count":{"total":1},"symbols":[]}),
            Format::Json,
            None,
            false,
        )
        .unwrap();
        assert!(!compact.contains('\n'));
        let pretty = render(&json!({"symbols":[]}), Format::Json, None, false).unwrap();
        assert!(pretty.contains("\n  \"symbols\""));
    }

    #[test]
    fn errors_render_as_a_single_code_and_message_line() {
        let value = json!({"error":{"code":"invalid_argument","message":"bad input"}});
        for format in [Format::Text, Format::Markdown] {
            assert_eq!(
                render(&value, format, None, false).unwrap(),
                "invalid_argument: bad input"
            );
        }
    }

    #[test]
    fn impact_reports_include_sections_gaps_and_hashes() {
        let text = render(&impact(), Format::Text, None, false).unwrap();
        for section in [
            "摘要",
            "变更符号",
            "候选影响",
            "已知缺口",
            "未检查项与截断",
            "版本信息",
        ] {
            assert!(text.contains(section), "{text}");
        }
        assert!(text.contains("- modified: shop::stock → shop::stock (exact)"));
        assert!(text.contains("- unresolved_call: 1"));
        assert!(text.contains("--gaps all"));

        let verbose = render(&impact(), Format::Text, None, true).unwrap();
        assert!(verbose.contains("- src/lib.rs: missing target"));

        let markdown = render(&impact(), Format::Markdown, None, false).unwrap();
        assert!(markdown.contains("## 摘要"));
        assert!(markdown.contains("Report hash: "));
        assert!(markdown.contains("候选影响：直接调用方 1，可达调用方 2。"));
    }

    #[test]
    fn maps_render_as_mermaid_and_dot_with_certainty_styles() {
        let map = json!({
            "snapshot_id":"snap","epochs":{"syntax":1,"semantic":1},
            "completeness":{"status":"within_scope","pending_units":[]},
            "output":{"truncated":false},
            "dependency_kind":"crate",
            "entries":[{"crate":"shop"}],
            "dependencies":[
                {"from":"shop","to":"core","source":"L1","certainty":"definite"},
                {"from":"shop","to":"external dependencies","source":"L1","certainty":"candidate"}
            ]
        });
        let dot = render(&map, Format::Dot, None, false).unwrap();
        assert!(dot.starts_with("digraph semweft {"));
        assert!(dot.contains("style=solid"));
        assert!(dot.contains("style=dashed"));
        assert!(dot.ends_with("}\n"));

        let mermaid = render(&map, Format::Mermaid, None, false).unwrap();
        assert!(mermaid.starts_with("flowchart LR"));
        assert!(mermaid.contains("-->|L1 definite|"));
        assert!(mermaid.contains("-.->|L1 candidate|"));
        assert!(!mermaid.contains("digraph"));

        let modules = json!({
            "snapshot_id":"snap","epochs":{"syntax":1,"semantic":1},
            "completeness":{"status":"within_scope","pending_units":[]},
            "output":{"truncated":false},
            "dependency_kind":"module",
            "modules":[{"path":"src/a.rs"}],
            "dependencies":[{"from":"src/a.rs","to":"src/b.rs","source":"L3","certainty":"candidate"}]
        });
        let dot = render(&modules, Format::Dot, None, false).unwrap();
        assert!(dot.contains("\"src/a.rs\""));
        assert!(dot.contains("\"src/b.rs\""));
    }

    #[test]
    fn graph_formats_reject_non_map_results() {
        let error = render(
            &json!({"snapshot_id":"snap","completeness":{"status":"partial"}}),
            Format::Mermaid,
            None,
            false,
        )
        .unwrap_err();
        assert!(format!("{error}").contains("map only"));
    }

    #[test]
    fn max_chars_truncates_after_the_version_header_and_keeps_continuation() {
        let symbols = (0..40)
            .map(|i| {
                json!({"name":format!("symbol_{i}"),"kind":"function",
                    "location":{"file":"src/lib.rs","line":i+1,"column":1}})
            })
            .collect::<Vec<_>>();
        let value = json!({
            "snapshot_id":"snap_crop","epochs":{"syntax":1,"semantic":0},
            "completeness":{"status":"partial","pending_units":[]},
            "output":{"truncated":false},"symbols":symbols
        });
        let full = render(&value, Format::Text, None, false).unwrap();
        assert!(full.chars().count() > 400);
        let limit = full.chars().count() - 10;
        let cropped = render(&value, Format::Text, Some(limit), false).unwrap();
        assert!(
            cropped.chars().count() <= limit,
            "{}",
            cropped.chars().count()
        );
        assert!(cropped.contains("展示裁剪：max_chars"));
        assert!(cropped.contains("完整结果"));
        assert!(cropped.contains("--snapshot-id snap_crop --semantic-epoch 0"));
    }

    #[test]
    fn report_hash_ignores_operational_metadata_but_tracks_evidence() {
        let mut a = json!({"snapshot_id":"s","fresh_requirement":{"waited_ms":1},"output":{"continuation":{"next_cursor":"a"}},"relations":[{"relation_id":"r","certainty":"candidate"}]});
        let mut b = a.clone();
        b["fresh_requirement"]["waited_ms"] = json!(999);
        b["output"]["continuation"]["next_cursor"] = json!("b");
        assert_eq!(report_hash(&a).unwrap(), report_hash(&b).unwrap());
        a["relations"][0]["certainty"] = json!("definite");
        assert_ne!(report_hash(&a).unwrap(), report_hash(&b).unwrap());
    }

    #[test]
    fn impossible_max_chars_is_an_invalid_argument() {
        let value = json!({
            "snapshot_id":"snap","epochs":{"syntax":1,"semantic":0},
            "completeness":{"status":"partial","pending_units":[]},
            "output":{"truncated":false}
        });
        let error = render(&value, Format::Text, Some(1), false).unwrap_err();
        assert!(
            format!("{error}").starts_with("invalid_argument: max_chars"),
            "{error}"
        );
    }

    #[test]
    fn relation_history_and_module_omissions_are_rendered() {
        let value = json!({
            "snapshot_id":"snap","epochs":{"syntax":2,"semantic":2},
            "completeness":{"status":"within_scope","pending_units":[]},
            "output":{"truncated":false},
            "relations":[{"from_name":"a","to_name":"b","kind":"call","certainty":"candidate",
                "freshness":"current","location":{"file":"src/lib.rs","line":1,"column":1}}],
            "history":{"snapshot_id":"old","relations":[{"from_name":"c","to_name":"d","kind":"read",
                "certainty":"candidate","freshness":"stale","location":{"file":"src/lib.rs","line":2,"column":1},
                "origin":{"snapshot_id":"old"}}]},
            "modules":[{"path":"src/lib.rs","symbol_count":3,"symbols":[{"name":"a","signature":"fn a()"}],
                "omitted_symbols":2}]
        });
        let text = render(&value, Format::Text, None, false).unwrap();
        assert!(text.contains("src/lib.rs\n- a → b [call candidate current]"));
        assert!(text.contains("历史引用"));
        assert!(text.contains("- c → d [read candidate stale]"));
        assert!(text.contains("该位置属于旧快照 old"));
        assert!(text.contains("2 symbols omitted"));
    }
}
