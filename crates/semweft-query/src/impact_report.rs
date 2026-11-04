use crate::{engine::StoredView, query, structure};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

#[derive(Default)]
struct ModuleImpact {
    changed: BTreeSet<String>,
    affected: BTreeSet<String>,
    relations: BTreeSet<String>,
    residual: usize,
}

pub(crate) fn enrich(
    result: &mut Value,
    base: &StoredView,
    head: &StoredView,
    args: &Value,
    deadline: Instant,
) -> Result<()> {
    for change in result["changes"].as_array_mut().into_iter().flatten() {
        let before = change["base_symbol"]["visibility"] == "public";
        let after = change["head_symbol"]["visibility"] == "public";
        if before || after {
            change["public_api_review"] = json!({"base_public":before,"head_public":after,
                "external_reachability":"not_established","compatibility":"not_evaluated",
                "basis":"syntax_public_visibility"});
        }
    }
    crate::api_surface::diff(result, base, head, deadline)?;
    let mut modules = BTreeMap::<(&str, String), ModuleImpact>::new();
    let mut kinds = BTreeMap::<String, usize>::new();
    let mut public_changes = 0;
    for change in result["changes"].as_array().into_iter().flatten() {
        let kind = change["kind"].as_str().unwrap_or("unknown");
        *kinds.entry(kind.into()).or_default() += 1;
        public_changes += usize::from(
            ["base_symbol", "head_symbol"]
                .iter()
                .any(|key| change[key]["visibility"] == "public"),
        );
    }
    let mut complete = true;
    let computed = (|| -> Result<()> {
        for change in result["changes"].as_array().into_iter().flatten() {
            query::check_deadline(deadline)?;
            for (side, key) in [("base", "base_symbol"), ("head", "head_symbol")] {
                if let (Some(file), Some(id)) = (
                    change[key]["location"]["file"].as_str(),
                    change[key]["symbol_id"].as_str(),
                ) {
                    let module = modules.entry((side, file.into())).or_default();
                    module.changed.insert(id.into());
                    module.affected.insert(id.into());
                }
            }
        }
        for (default_side, relations) in [
            ("head", &result["relations"]),
            ("head", &result["relation_changes"]["added"]),
            ("base", &result["relation_changes"]["removed"]),
        ] {
            for relation in relations.as_array().into_iter().flatten() {
                query::check_deadline(deadline)?;
                let side = if relation["side"].as_str().unwrap_or(default_side) == "base" {
                    "base"
                } else {
                    "head"
                };
                let stored = if side == "base" { base } else { head };
                for key in ["from", "to"] {
                    if let Some(symbol) = relation[key]
                        .as_str()
                        .and_then(|id| stored.index().symbol(id))
                    {
                        let module = modules.entry((side, symbol.file.clone())).or_default();
                        module.affected.insert(symbol.symbol_id.clone());
                        if let Some(id) = relation["relation_id"].as_str() {
                            module.relations.insert(id.into());
                        }
                    }
                }
            }
        }
        for reference in result["residual_references"]
            .as_array()
            .into_iter()
            .flatten()
        {
            query::check_deadline(deadline)?;
            if let Some(file) = reference["location"]["file"].as_str() {
                modules.entry(("head", file.into())).or_default().residual += 1;
            }
        }
        Ok(())
    })();
    if let Err(error) = computed {
        if query::error_value(&error)["error"]["code"] != "timeout" {
            return Err(error);
        }
        complete = false;
        result["output"]["truncated"] = json!(true);
        result["output"]["truncation_reason"] = json!("timeout");
        result["evidence_summary"]["basis"] = json!("partial");
        result["evidence_summary"]["exact"] = json!(false);
        result["output"]["remaining_relations"] = Value::Null;
    }
    let count = |key: &str| result[key].as_array().map_or(0, Vec::len);
    let diff_summary = json!({"changed_symbols":count("changes"), "change_kinds":kinds,
        "external_api_candidate_path_changes":count("api_changes"),
        "configuration_changes":count("configuration_changes"), "residual_references":count("residual_references"),
        "affected_modules_observed":modules.len(), "module_aggregation_complete":complete,
        "module_scope":"source files; base/head counted separately; inline modules and services are not expanded",
        "analysis_basis":result["evidence_summary"]["basis"]});
    let mut signals = vec![];
    for (kind, observed, reason) in [
        (
            "deleted_symbols",
            kinds.get("deleted").copied().unwrap_or(0),
            "Declarations were deleted; review consumers in both versions",
        ),
        (
            "signature_changes",
            kinds.get("signature_changed").copied().unwrap_or(0),
            "Declaration signatures or attributes changed; compatibility is not established",
        ),
        (
            "public_declaration_changes",
            public_changes,
            "Declarations marked public changed; external reachability and compatibility are not established",
        ),
        (
            "external_api_candidate_path_changes",
            count("api_changes"),
            "Captured Rust module/reexport paths changed; compiler validation and compatibility review are required",
        ),
        (
            "residual_references",
            count("residual_references"),
            "Old-name occurrences remain; lexical binding is unresolved",
        ),
        (
            "configuration_changes",
            count("configuration_changes"),
            "Build or analysis inputs changed; unchanged declarations may be affected",
        ),
        (
            "indexed_impact_relations",
            count("relations"),
            "Indexed relations indicate candidate impact, not required caller edits",
        ),
        (
            "analysis_gaps",
            result["completeness"]["known_gaps"]
                .as_array()
                .map_or(0, Vec::len),
            "Analysis has disclosed missing evidence",
        ),
        (
            "pending_units",
            base.view.pending_units.len() + head.view.pending_units.len(),
            "Semantic units are pending in one or both versions",
        ),
        (
            "truncated_analysis",
            usize::from(result["output"]["truncated"] == true),
            "Traversal or aggregation was limited; observed counts may be lower bounds",
        ),
    ] {
        if observed > 0 {
            signals.push(json!({"kind":kind,"observed_count":observed,"reason":reason}));
        }
    }
    let changed_symbols = count("changes");
    let affected_paths = modules
        .keys()
        .map(|(_, path)| path.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let relations = result["relations"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let callers = result["summary"]["transitive_callers"]
        .as_u64()
        .unwrap_or(0) as usize;
    let call_edges = relations
        .iter()
        .filter(|relation| relation["kind"] == "call")
        .filter_map(|relation| {
            Some((
                (
                    relation["side"].as_str().unwrap_or("head"),
                    relation["relation_id"].as_str()?,
                ),
                relation,
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let max_call_depth = relations
        .iter()
        .filter(|relation| relation["kind"] == "call")
        .filter(|relation| {
            relation["path"].as_array().is_some_and(|path| {
                path.iter().all(|id| {
                    id.as_str().is_some_and(|id| {
                        call_edges.contains_key(&(relation["side"].as_str().unwrap_or("head"), id))
                    })
                })
            })
        })
        .filter_map(|relation| relation["depth"].as_u64())
        .max()
        .unwrap_or(0) as usize;
    let indexed_test_callers = relations
        .iter()
        .filter(|relation| relation["scope"] == "test")
        .filter_map(|relation| relation["from"].as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let gap_count = result["completeness"]["known_gaps"]
        .as_array()
        .map_or(0, Vec::len);
    let pending_units = base.view.pending_units.len() + head.view.pending_units.len();
    let configuration_changes = count("configuration_changes");
    let residual_references = count("residual_references");
    let truncated = result["output"]["truncated"] == true;
    let has_change = changed_symbols > 0
        || configuration_changes > 0
        || residual_references > 0
        || count("api_changes") > 0;
    let (mut coverage, hotspot, sources) =
        crate::risk_inputs::observe(result, base, head, args, deadline)?;
    coverage["indexed_test_callers"] = json!(indexed_test_callers);
    let coverage_score = coverage["score"].as_u64().unwrap() as usize;
    let hotspot_score = hotspot["score"].as_u64().unwrap() as usize;
    let missing_coverage = coverage["status"] != "available";
    let missing_history = hotspot["status"] != "available";
    let scope_score = (changed_symbols * 3 + affected_paths * 2).min(20);
    let public_api_score = (public_changes * 10).min(20);
    let call_impact_score = (callers * 2).min(9) + (max_call_depth * 2).min(6);
    let evidence_gap_score = (gap_count * 2).min(6)
        + pending_units.min(3)
        + usize::from(truncated) * 5
        + configuration_changes.min(3)
        + residual_references.min(3)
        + usize::from(missing_coverage) * 3
        + usize::from(missing_history) * 3;
    let evidence_gap_score = evidence_gap_score.min(15);
    let score = has_change.then_some(
        (scope_score
            + public_api_score
            + call_impact_score
            + coverage_score
            + hotspot_score
            + evidence_gap_score)
            .min(100),
    );
    let level = score.map(|score| match score {
        0..=19 => "low",
        20..=44 => "moderate",
        45..=69 => "high",
        _ => "critical",
    });
    let confidence =
        if truncated || gap_count > 0 || pending_units > 0 || missing_coverage || missing_history {
            "low"
        } else {
            "medium"
        };
    let risk_summary = json!({"scoring":crate::risk_inputs::MODEL,"score":score,"level":level,
        "confidence":if has_change {Some(confidence)} else {None},
        "score_basis":"review_priority_not_probability",
        "dimensions":{
            "impact_scope":{"score":scope_score,"max_score":20,"changed_symbols":changed_symbols,"affected_source_files":affected_paths},
            "public_api":{"score":public_api_score,"max_score":20,"changed_declarations":public_changes,
                "external_candidate_paths_changed":count("api_changes"),
                "basis":"syntax_visibility_only; external reachability and semver compatibility are not inferred"},
            "call_impact":{"score":call_impact_score,"max_score":15,"observed_callers":callers,"max_call_depth":max_call_depth,"depth_basis":"complete all-call witness paths only"},
            "test_coverage":coverage,
            "hotspot":hotspot,
            "evidence_gaps":{"score":evidence_gap_score,"max_score":15,"known_gaps":gap_count,
                "pending_units":pending_units,"truncated":truncated,"configuration_changes":configuration_changes,
                "residual_references":residual_references,"missing_or_partial_coverage":missing_coverage,"missing_or_partial_history":missing_history}
        },
        "signals":signals,"sources":sources,"coverage":coverage["status"],"per_test_coverage":"unavailable","external_consumers_included":false,
        "conclusion":"Heuristic review priority only; it is not a probability, compatibility verdict, or behavioral-safety claim"});
    result["diff_summary"] = diff_summary;
    if result["summary"].is_object() {
        result["summary"]["risk"] = score.map_or(Value::Null, |score| json!(score));
    }
    result["risk_summary"] = risk_summary;
    result["affected_modules"] = json!(modules.into_iter().map(|((side,path), module)| {
        let stored = if side == "base" { base } else { head };
        json!({"side":side,"path":path,"crate":structure::package(stored,&path).map(|(_,name)|name),
            "snapshot_id":stored.view.snapshot.snapshot_id,"semantic_epoch":stored.view.epochs.semantic_epoch,
            "changed_symbol_ids":module.changed,"affected_symbol_ids":module.affected,
            "relation_ids":module.relations,"residual_reference_count":module.residual})
    }).collect::<Vec<_>>());
    crate::services::enrich_diff(result, base, head, deadline)?;
    Ok(())
}
