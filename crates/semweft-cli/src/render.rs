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
    Sarif,
    Html,
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
    if matches!(format, Format::Html) {
        if max_chars.is_some() {
            bail!("invalid_argument: HTML cannot be clipped with max_chars");
        }
        return crate::reports::html(value);
    }
    if matches!(format, Format::Sarif) {
        if max_chars.is_some() {
            bail!("invalid_argument: SARIF cannot be clipped with max_chars");
        }
        return crate::reports::sarif(value);
    }
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
    if let Some(verification) = value.get("verification") {
        writeln!(
            out,
            "核验状态：{} | findings（全量）：{} | 违规：{} | 警告：{}",
            verification["status"].as_str().unwrap_or("unknown"),
            verification["finding_count"],
            verification["violation_count"],
            verification["warning_count"]
        )?;
    }
    if let Some(summary) = value.get("test_summary") {
        writeln!(
            out,
            "测试候选（全量）：{} | 当前版本：{} | 当前版本非 ignored：{} | 仅旧版本：{} | ignored：{}",
            summary["candidate_count"],
            summary["head_candidates"],
            summary["head_nonignored_candidates"],
            summary["base_only_candidates"],
            summary["ignored_candidates"]
        )?;
        writeln!(
            out,
            "覆盖信息：不可用 | 已执行测试：false | 关系遍历完成：{}",
            summary["traversal_complete"]
        )?;
    }
    if let Some(summary) = value.get("diff_summary") {
        writeln!(
            out,
            "Diff（全量）：变更符号 {} | 配置 {} | 残留引用 {} | 已观察文件模块 {} | 模块聚合完成 {} | {}",
            summary["changed_symbols"],
            summary["configuration_changes"],
            summary["residual_references"],
            summary["affected_modules_observed"],
            summary["module_aggregation_complete"],
            summary["analysis_basis"].as_str().unwrap_or("unknown")
        )?;
    }
    let header_len = out.len();
    if value.get("test_summary").is_some() {
        heading(&mut out, "测试候选推荐（当前页）", markdown)?;
        writeln!(
            out,
            "依据已索引调用/引用路径推荐，未确认构建或运行可用性；零候选不表示无需测试。仅识别 Rust test、tokio::test、async_std::test 属性入口。"
        )?;
        for candidate in value["test_candidates"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "\n- {} — {} | {} | ignored {} | candidate | snapshot {}",
                label(&candidate["test"]),
                location(&candidate["test"]["location"]),
                candidate["availability"].as_str().unwrap_or("?"),
                candidate["ignored"],
                candidate["test_snapshot"]["snapshot_id"]
                    .as_str()
                    .unwrap_or("?")
            )?;
            for witness in candidate["witnesses"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "  依据：{} | side {} | target {} | 路径绑定 {}",
                    witness["reason"].as_str().unwrap_or("?"),
                    witness["side"].as_str().unwrap_or("?"),
                    witness["target_symbol_id"].as_str().unwrap_or("?"),
                    witness["path_certainty"]
                        .as_str()
                        .unwrap_or("not_applicable")
                )?;
                for relation in witness["path"].as_array().into_iter().flatten() {
                    edge(&mut out, relation)?;
                }
            }
        }
    }
    if let Some(verification) = value.get("verification") {
        heading(&mut out, "修改核验", markdown)?;
        writeln!(
            out,
            "范围：已安装的内置审查规则；不执行编译或测试。状态与规则计数为分页前全量结果。"
        )?;
        for rule in verification["rules"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "- {}@{}: {}（{} 条 finding）",
                rule["id"].as_str().unwrap_or("?"),
                rule["version"].as_str().unwrap_or("?"),
                rule["status"].as_str().unwrap_or("unknown"),
                rule["finding_count"]
            )?;
        }
        heading(&mut out, "核验 findings（当前页）", markdown)?;
        for finding in value["findings"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "- [{} / {}] {}@{}: {}",
                finding["severity"].as_str().unwrap_or("?"),
                finding["status"].as_str().unwrap_or("?"),
                finding["rule_id"].as_str().unwrap_or("?"),
                finding["rule_version"].as_str().unwrap_or("?"),
                finding["message"].as_str().unwrap_or("")
            )?;
            let evidence = &finding["evidence"];
            if let Some(file) = evidence["file"].as_str() {
                writeln!(out, "  证据：{file}")?;
            }
            if evidence.get("location").is_some() {
                writeln!(
                    out,
                    "  证据：{} | side {} | certainty {}",
                    location(&evidence["location"]),
                    evidence["side"].as_str().unwrap_or("head"),
                    evidence["certainty"].as_str().unwrap_or("unknown")
                )?;
            }
            if let Some(relation) = evidence["relation_id"].as_str() {
                writeln!(out, "  relation_id: {relation}")?;
            }
        }
    }
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
    if let Some(summary) = value.get("path_summary") {
        heading(&mut out, "调用路径", markdown)?;
        writeln!(
            out,
            "{} → {} | 已找到 {} 条 | 枚举完成：{} | 访问节点：{}",
            label(&value["from"]),
            label(&value["to"]),
            summary["path_count"],
            summary["enumeration_complete"],
            summary["visited_nodes"]
        )?;
        writeln!(
            out,
            "范围：{}；按简单路径枚举，路径中的 candidate 证据仍为候选。",
            summary["scope"].as_str().unwrap_or("indexed symbol calls")
        )?;
        writeln!(
            out,
            "限制：深度 {}，路径 {}，节点 {}。枚举完成仅针对已索引图，分析缺口仍见 completeness。",
            summary["max_depth"], summary["max_paths"], summary["max_nodes"]
        )?;
        if let Some(reason) = summary["reason"].as_str() {
            writeln!(
                out,
                "路径枚举截断：{reason}；未找到路径不能据此证明两端无关系。"
            )?;
        }
        if let Some(interpretation) = summary["interpretation"].as_str() {
            writeln!(out, "{interpretation}")?;
        }
        for path in value["paths"].as_array().into_iter().flatten() {
            let nodes = path
                .get("symbols")
                .or_else(|| path.get("nodes"))
                .unwrap_or(&Value::Null);
            let names = nodes
                .as_array()
                .into_iter()
                .flatten()
                .map(label)
                .collect::<Vec<_>>();
            writeln!(
                out,
                "\n{} [{} {}] | {} 步 | {}",
                names.join(" → "),
                path["certainty"].as_str().unwrap_or("?"),
                path["freshness"].as_str().unwrap_or("?"),
                path["length"],
                path["path_id"].as_str().unwrap_or("?")
            )?;
            for relation in path["relations"].as_array().into_iter().flatten() {
                edge(&mut out, relation)?;
            }
        }
    }
    let impact = value.get("changes").is_some();
    if impact {
        if let Some(summary) = value.get("risk_summary") {
            heading(&mut out, "风险审查信号（全量）", markdown)?;
            let score = summary["score"]
                .as_u64()
                .map(|score| {
                    format!(
                        "{score}/100（{}，启发式；置信度 {}）",
                        summary["level"].as_str().unwrap_or("?"),
                        summary["confidence"].as_str().unwrap_or("?")
                    )
                })
                .unwrap_or_else(|| "未提供".into());
            writeln!(
                out,
                "风险评分：{score} | 行覆盖观测：{} | 热点历史：{} | 逐测试归因不可用、外部消费者未纳入；分数仅用于审查优先级。",
                summary["dimensions"]["test_coverage"]["status"]
                    .as_str()
                    .unwrap_or("unavailable"),
                summary["dimensions"]["hotspot"]["status"]
                    .as_str()
                    .unwrap_or("unavailable")
            )?;
            for (key, label) in [
                ("impact_scope", "影响范围"),
                ("public_api", "公开声明"),
                ("call_impact", "调用影响"),
                ("test_coverage", "行覆盖观测"),
                ("hotspot", "历史热点"),
                ("evidence_gaps", "证据缺口"),
            ] {
                let dimension = &summary["dimensions"][key];
                if !dimension["score"].is_number() {
                    continue;
                }
                let observation = match key {
                    "test_coverage" => format!(
                        "报告行 {}、命中 {}、缺少观测声明 {}",
                        dimension["observed_lines"],
                        dimension["covered_lines"],
                        dimension["declarations_without_line_observations"]
                    ),
                    "hotspot" => format!(
                        "最大窗口触及提交 {}/{}、来源 {}",
                        dimension["max_window_changes"],
                        dimension["max_window_commits"],
                        dimension["max_frequency_source"]
                    ),
                    _ => String::new(),
                };
                writeln!(
                    out,
                    "- {label}: {}/{} {observation}",
                    dimension["score"], dimension["max_score"]
                )?;
            }
            for signal in summary["signals"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "- {}: {} — {}",
                    signal["kind"].as_str().unwrap_or("?"),
                    signal["observed_count"],
                    signal["reason"].as_str().unwrap_or("")
                )?;
            }
        }
        if value.get("affected_modules").is_some() {
            heading(&mut out, "受影响文件模块（当前页）", markdown)?;
            writeln!(
                out,
                "按源文件聚合，base/head 分别计数；未展开内联模块或服务。"
            )?;
            for module in value["affected_modules"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "- {} | {} | crate {} | 变更符号 {} | 影响符号 {} | 关系 {} | 残留引用 {}",
                    module["path"].as_str().unwrap_or("?"),
                    module["side"].as_str().unwrap_or("?"),
                    module["crate"].as_str().unwrap_or("unknown"),
                    module["changed_symbol_ids"].as_array().map_or(0, Vec::len),
                    module["affected_symbol_ids"].as_array().map_or(0, Vec::len),
                    module["relation_ids"].as_array().map_or(0, Vec::len),
                    module["residual_reference_count"]
                )?;
            }
        }
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
    if let Some(review) = value
        .get("review")
        .or_else(|| value.get("review_id").map(|_| value))
    {
        heading(&mut out, "人工审查记录", markdown)?;
        writeln!(
            out,
            "{} | 结论 {} | 批准适用于当前视图 {}",
            review["review_id"].as_str().unwrap_or("?"),
            review["conclusion"].as_str().unwrap_or("?"),
            review["validity"]["approval_applies"]
        )?;
        writeln!(out, "{}", review["note"].as_str().unwrap_or(""))?;
    }
    if let Some(summary) = value.get("contract_summary") {
        heading(&mut out, "Protobuf 契约影响", markdown)?;
        writeln!(
            out,
            "变更 {} | 生成映射候选 {} | {}",
            summary["change_count"],
            summary["mapping_count"],
            summary["compatibility_scope"]
                .as_str()
                .unwrap_or("limited scope")
        )?;
        for change in value["contract_changes"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "- {} [{}] {}",
                change["contract_id"].as_str().unwrap_or("?"),
                change["kind"].as_str().unwrap_or("?"),
                change["file"].as_str().unwrap_or("?")
            )?;
        }
        for impact in value["contract_impacts"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "- {} → {} [{} / {}]",
                impact["contract_id"].as_str().unwrap_or("?"),
                label(&impact["symbol"]),
                impact["side"].as_str().unwrap_or("?"),
                impact["impact_level"].as_str().unwrap_or("candidate")
            )?;
            for relation in impact["relations"].as_array().into_iter().flatten() {
                edge(&mut out, relation)?;
            }
        }
    }
    if let Some(architecture) = value.get("architecture") {
        heading(&mut out, "架构分层检查", markdown)?;
        writeln!(
            out,
            "新增 {} | 历史 {} | 规则 {} | 比较 {}",
            architecture["new_count"],
            architecture["existing_count"],
            architecture["rules_digest"].as_str().unwrap_or("?"),
            architecture["comparison"].as_str().unwrap_or("none")
        )?;
    }
    if let Some(observation) = value["guard"].get("risk_observations") {
        heading(&mut out, "修改前覆盖与热点观测", markdown)?;
        writeln!(
            out,
            "覆盖 {}：观测 {} 行，覆盖 {} 行；热点 {}：窗口内最高 {}/{} 次提交。未执行测试。",
            observation["coverage"]["status"]
                .as_str()
                .unwrap_or("unavailable"),
            observation["coverage"]["observed_lines"],
            observation["coverage"]["covered_lines"],
            observation["hotspots"]["status"]
                .as_str()
                .unwrap_or("unavailable"),
            observation["hotspots"]["max_window_changes"],
            observation["hotspots"]["max_window_commits"]
        )?;
    }
    if let Some(api) = value.get("api_summary") {
        heading(&mut out, "公开声明清单", markdown)?;
        if api["external_reachability"] == "candidate" {
            writeln!(
                out,
                "Rust 库 API 候选 {} 个，导出路径 {} 条；已展开捕获的模块与本地重导出，条件编译未求值，需编译器确认。",
                api["declaration_count"], api["export_path_count"]
            )?;
            for gap in api["known_gaps"].as_array().into_iter().flatten() {
                writeln!(out, "- API 缺口：{}", gap.as_str().unwrap_or("unknown"))?;
            }
        } else {
            writeln!(
                out,
                "语法公开声明 {} 个；外部可达性未确认，未展开重导出和生成物。",
                api["declaration_count"]
            )?;
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
            for export in s["api_exposure"]["paths"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "  API: {}{}",
                    export["path"].as_str().unwrap_or("?"),
                    if export["conditional"] == true {
                        " (条件候选)"
                    } else {
                        " (候选)"
                    }
                )?;
            }
        }
    }
    if let Some(summary) = value.get("api_surface_summary") {
        heading(&mut out, "外部 API 候选变更", markdown)?;
        writeln!(
            out,
            "{} 条路径变更；基于冻结 Rust 库模块/重导出图，不判定兼容性。",
            summary["change_count"]
        )?;
        for change in value["api_changes"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "- {}: {}",
                change["kind"].as_str().unwrap_or("?"),
                change["path"].as_str().unwrap_or("?")
            )?;
        }
    }
    if let Some(summary) = value.get("cycle_summary") {
        heading(&mut out, "循环依赖（候选图）", markdown)?;
        writeln!(
            out,
            "强连通分量 {} 个；依赖范围 {}，未索引依赖不纳入结论。",
            summary["component_count"],
            summary["dependency_kind"].as_str().unwrap_or("unknown")
        )?;
        for cycle in value["cycles"].as_array().into_iter().flatten() {
            let nodes = cycle["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            writeln!(
                out,
                "- {} [{}]",
                nodes.join(" ↔ "),
                cycle["certainty"].as_str().unwrap_or("candidate")
            )?;
        }
    }
    if let Some(summary) = value.get("service_impact_summary") {
        heading(&mut out, "服务影响（显式映射）", markdown)?;
        writeln!(
            out,
            "观察到 {} 个受影响服务；只聚合已捕获文件与索引关系。",
            summary["affected_services_observed"]
        )?;
        for service in value["affected_services"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "- {} ({}): {} 个文件，{} 个候选符号",
                service["name"].as_str().unwrap_or("?"),
                service["side"].as_str().unwrap_or("head"),
                service["files"].as_array().map_or(0, Vec::len),
                service["affected_symbol_ids"]
                    .as_array()
                    .map_or(0, Vec::len)
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
    if let Some(counts) = value.get("implementation_counts") {
        writeln!(
            out,
            "\n实现计数（过滤后全量）：总计 {}，生产 {}，测试 {}，语义证据 {}，blanket {}，默认方法继承 {}，排除 {}。",
            counts["total"],
            counts["production"],
            counts["test"],
            counts["with_semantic_evidence"],
            counts["blanket"],
            counts["inherited_defaults"],
            counts["excluded"]
        )?;
        writeln!(
            out,
            "当前页：{} 条实现关系；默认方法继承与排除候选为过滤后全量快照元数据，随页重复。",
            value["relations"].as_array().map_or(0, Vec::len)
        )?;
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
    if let Some(defaults) = value["inherited_defaults"]
        .as_array()
        .filter(|defaults| !defaults.is_empty())
    {
        heading(&mut out, "继承默认方法（过滤后全量，随页重复）", markdown)?;
        for inherited in defaults {
            writeln!(
                out,
                "- impl {} for {} 继承 {} [{} {}] — {}",
                inherited["trait_path"].as_str().unwrap_or("?"),
                inherited["self_type"].as_str().unwrap_or("?"),
                label(&value["target"]),
                inherited["scope"].as_str().unwrap_or("?"),
                inherited["certainty"].as_str().unwrap_or("?"),
                location(&inherited["location"])
            )?;
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
            let details = gaps.iter().flat_map(|gap| match gap["samples"].as_array() {
                Some(samples) => samples.iter().collect::<Vec<_>>(),
                None => vec![gap],
            });
            for gap in details {
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
    fn verification_reports_verdict_rules_and_evidence_in_all_human_formats() {
        let mut value = impact();
        value["verification"] = json!({"status":"insufficient_evidence","finding_count":8,"violation_count":0,"warning_count":8,
            "rules":[{"id":"residual_reference_requires_review","version":"0.1","status":"insufficient_evidence","finding_count":8}]});
        value["findings"] = json!([{"rule_id":"residual_reference_requires_review","rule_version":"0.1",
            "severity":"warning","status":"insufficient_evidence","message":"Review old-name binding",
            "evidence":{"certainty":"candidate","location":{"file":"src/lib.rs","line":3,"column":4}}}]);
        for format in [Format::Text, Format::Markdown] {
            let report = render(&value, format, None, false).unwrap();
            assert!(report.contains("核验状态：insufficient_evidence"));
            assert!(report.contains("findings（全量）：8"));
            assert!(report.contains("residual_reference_requires_review@0.1"));
            assert!(report.contains("src/lib.rs:3:4"));
            let cropped = render(&value, format, Some(600), false).unwrap();
            assert!(cropped.contains("核验状态：insufficient_evidence"));
            assert!(cropped.contains("输出已裁剪"));
        }
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
    fn impls_reports_distinguish_page_relations_from_full_snapshot_metadata() {
        let value = json!({
            "snapshot_id":"snap_impls","epochs":{"syntax":1,"semantic":0},
            "completeness":{"status":"partial","pending_units":[]},
            "output":{"truncated":true,"truncation_reason":"limit",
                "continuation":{"next_cursor":"impls_page"}},
            "target":{"qualified_name":"shop::Deduct::name","signature":"fn name(&self) -> &str",
                "location":{"file":"src/lib.rs","line":2,"column":5}},
            "implementation_counts":{"total":4,"production":3,"test":1,
                "with_semantic_evidence":2,"blanket":1,"inherited_defaults":2,"excluded":1},
            "relations":[{"from_name":"<Deduct for Fake>::name","to_name":"Deduct::name",
                "kind":"implements","certainty":"candidate","freshness":"current","scope":"test",
                "location":{"file":"tests/impls.rs","line":3,"column":5}}],
            "inherited_defaults":[
                {"trait_path":"Deduct","self_type":"Stock","scope":"production","certainty":"candidate",
                    "location":{"file":"src/lib.rs","line":8,"column":1}},
                {"trait_path":"super::Deduct","self_type":"DefaultFake","scope":"test","certainty":"candidate",
                    "location":{"file":"src/lib.rs","line":20,"column":1}}
            ],
            "excluded_candidates":[{"impl_id":"excluded","self_type":"Other","reason":"different trait"}]
        });
        for format in [Format::Text, Format::Markdown] {
            let text = render(&value, format, None, false).unwrap();
            assert!(text.contains("实现计数（过滤后全量）：总计 4，生产 3，测试 1，语义证据 2，blanket 1，默认方法继承 2，排除 1。"), "{text}");
            assert!(text.contains("当前页：1 条实现关系"), "{text}");
            assert!(text.contains("默认方法继承与排除候选为过滤后全量快照元数据，随页重复。"));
            assert!(text.contains("继承默认方法（过滤后全量，随页重复）"));
            assert!(text.contains("impl Deduct for Stock 继承 shop::Deduct::name [production candidate] — src/lib.rs:8:1"));
            assert!(text.contains("impl super::Deduct for DefaultFake 继承 shop::Deduct::name [test candidate] — src/lib.rs:20:1"));
            assert!(text.contains("测试作用域：1 条候选关系"));
            assert!(text.contains("- <Deduct for Fake>::name → Deduct::name [implements candidate current] — tests/impls.rs:3:5"));
            assert_eq!(text.matches(" 继承 shop::Deduct::name").count(), 2);
            if matches!(format, Format::Markdown) {
                assert!(text.contains("## 继承默认方法"));
                assert!(text.contains("<details><summary>测试关系</summary>"));
            }
        }
        let rendered = render(&value, Format::Json, None, false).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&rendered).unwrap(), value);
    }

    #[test]
    fn impls_empty_relations_still_report_full_default_metadata_and_zero_counts() {
        let mut value = json!({
            "snapshot_id":"snap_impls","epochs":{"syntax":1,"semantic":0},
            "completeness":{"status":"partial","pending_units":[]},
            "output":{"truncated":false},
            "target":{"qualified_name":"Deduct::name"},
            "implementation_counts":{"total":0,"production":0,"test":0,
                "with_semantic_evidence":0,"blanket":0,"inherited_defaults":1,"excluded":0},
            "relations":[],"excluded_candidates":[],
            "inherited_defaults":[{"trait_path":"Deduct","self_type":"Stock","scope":"production",
                "certainty":"candidate","location":{"file":"src/lib.rs","line":8,"column":1}}]
        });
        for format in [Format::Text, Format::Markdown] {
            let text = render(&value, format, None, false).unwrap();
            assert!(text.contains("实现计数（过滤后全量）：总计 0，生产 0，测试 0，语义证据 0，blanket 0，默认方法继承 1，排除 0。"));
            assert!(text.contains("当前页：0 条实现关系"));
            assert!(text.contains("impl Deduct for Stock 继承 Deduct::name"));
        }
        value["inherited_defaults"] = json!([]);
        value["implementation_counts"]["inherited_defaults"] = json!(0);
        let text = render(&value, Format::Text, None, false).unwrap();
        assert!(text.contains("默认方法继承 0，排除 0。"));
        assert!(!text.contains("继承默认方法（过滤后全量"));
        assert!(!text.contains("[production candidate]"));
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
