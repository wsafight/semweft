use crate::{
    engine::StoredView,
    query::{self, QueryService},
};
use anyhow::{Context, Result, bail};
use semweft_core::{Certainty, Relation, Symbol};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Instant};

fn test_attribute(symbol: &Symbol, stored: &StoredView) -> Option<String> {
    if !symbol.file.ends_with(".rs") || symbol.kind != "function" {
        return None;
    }
    stored
        .view
        .snapshot
        .syntax
        .declarations
        .get(&symbol.symbol_id)?
        .attributes
        .iter()
        .find_map(|attribute| {
            let normalized = attribute
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>();
            let name = normalized.strip_prefix("#[")?.split(['(', ']']).next()?;
            matches!(name, "test" | "tokio::test" | "async_std::test").then(|| attribute.clone())
        })
}

fn ignored(symbol: &Symbol, stored: &StoredView) -> bool {
    stored
        .view
        .snapshot
        .syntax
        .declarations
        .get(&symbol.symbol_id)
        .is_some_and(|info| {
            info.attributes.iter().any(|attribute| {
                let normalized = attribute
                    .chars()
                    .filter(|character| !character.is_whitespace())
                    .collect::<String>();
                normalized == "#[ignore]"
                    || normalized.starts_with("#[ignore(")
                    || normalized.starts_with("#[ignore=")
            })
        })
}

fn add_candidate(
    candidates: &mut BTreeMap<(String, String), Value>,
    symbol: &Symbol,
    side: &str,
    sides: &BTreeMap<&str, StoredView>,
    head_names: &BTreeMap<(&str, &str), Vec<&Symbol>>,
    witness: Value,
) {
    let Some(attribute) = test_attribute(symbol, &sides[side]) else {
        return;
    };
    let head_match = head_names
        .get(&(symbol.qualified_name.as_str(), symbol.kind.as_str()))
        .filter(|matches| matches.len() == 1)
        .map(|matches| matches[0])
        .filter(|candidate| test_attribute(candidate, &sides["head"]).is_some());
    let (test, test_side, availability) = if side == "head" {
        (symbol, "head", "head")
    } else if let Some(head) = head_match {
        (head, "head", "head")
    } else {
        (symbol, "base", "base_only")
    };
    let stored = &sides[test_side];
    let candidate = candidates.entry((test_side.into(), test.symbol_id.clone())).or_insert_with(|| json!({
        "test":query::symbol_value(test,stored,false,false),"availability":availability,
        "test_snapshot":{"snapshot_id":stored.view.snapshot.snapshot_id,
            "epochs":{"syntax":stored.view.epochs.syntax_epoch,"semantic":stored.view.epochs.semantic_epoch}},
        "detection":{"kind":"rust_test_attribute","attribute":test_attribute(test,stored).unwrap_or(attribute)},
        "ignored":ignored(test,stored),"executed":false,"certainty":"candidate","witnesses":[]
    }));
    let witnesses = candidate["witnesses"].as_array_mut().unwrap();
    if !witnesses.contains(&witness) {
        witnesses.push(witness);
    }
}

#[derive(Clone, Copy)]
enum ReportRelation<'a> {
    Json(&'a Value),
    Typed(&'a (Relation, u64, Vec<String>)),
}

impl ReportRelation<'_> {
    fn text(&self, field: &str) -> Option<&str> {
        match self {
            Self::Json(value) => value[field].as_str(),
            Self::Typed((relation, _, _)) => match field {
                "relation_id" => Some(&relation.relation_id),
                "from" => Some(&relation.from_symbol),
                "to" => Some(&relation.to_symbol),
                "kind" => Some(&relation.kind),
                "certainty" => Some(if relation.certainty == Certainty::Definite {
                    "definite"
                } else {
                    "candidate"
                }),
                _ => None,
            },
        }
    }

    fn side(&self) -> &str {
        self.text("side").unwrap_or("head")
    }

    fn path(&self) -> Vec<Option<&str>> {
        match self {
            Self::Json(value) => value["path"]
                .as_array()
                .into_iter()
                .flatten()
                .map(Value::as_str)
                .collect(),
            Self::Typed((_, _, path)) => path.iter().map(|id| Some(id.as_str())).collect(),
        }
    }

    fn value(&self, stored: &StoredView) -> Value {
        match self {
            Self::Json(value) => (*value).clone(),
            Self::Typed(edge) => query::graph_value(edge, stored),
        }
    }
}

pub(crate) fn report(
    service: &QueryService,
    mut impact: Value,
    args: &Value,
    deadline: Instant,
    graph: Option<&query::Graph>,
) -> Result<Value> {
    query::check_deadline(deadline)?;
    let mut sides = BTreeMap::new();
    for side in ["base", "head"] {
        let metadata = if impact.get("snapshots").is_some() {
            &impact["snapshots"][side]
        } else if side == "head" {
            &impact
        } else {
            continue;
        };
        let snapshot = metadata["snapshot_id"]
            .as_str()
            .context("internal_error: test suggestions lack a fixed snapshot")?;
        let epoch = metadata["epochs"]["semantic"]
            .as_u64()
            .context("internal_error: test suggestions lack a semantic epoch")?;
        sides.insert(side, service.engine.historical(snapshot, Some(epoch))?);
    }
    let mut head_names = BTreeMap::<_, Vec<_>>::new();
    for symbol in &sides["head"].view.snapshot.syntax.symbols {
        query::check_deadline(deadline)?;
        head_names
            .entry((symbol.qualified_name.as_str(), symbol.kind.as_str()))
            .or_default()
            .push(symbol);
    }
    let relations = if let Some(graph) = graph {
        graph
            .edges
            .iter()
            .map(ReportRelation::Typed)
            .collect::<Vec<_>>()
    } else {
        impact["relations"]
            .as_array()
            .into_iter()
            .flatten()
            .map(ReportRelation::Json)
            .collect()
    };
    let lookup = relations
        .iter()
        .filter_map(|relation| Some(((relation.side(), relation.text("relation_id")?), relation)))
        .collect::<BTreeMap<_, _>>();
    let mut candidates = BTreeMap::new();
    let mut roots = vec![];
    for change in impact["changes"].as_array().into_iter().flatten() {
        for (side, key) in [("base", "base_symbol"), ("head", "head_symbol")] {
            if let Some(id) = change[key]["symbol_id"].as_str() {
                roots.push((side, id));
            }
        }
    }
    if let Some(id) = impact["target"]["symbol_id"].as_str() {
        roots.push(("head", id));
    }
    let mut timed_out = false;
    let mut missing_paths = 0;
    for (side, id) in roots {
        query::check_cancelled()?;
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        if let Some(symbol) = sides[side].index().symbol(id) {
            add_candidate(
                &mut candidates,
                symbol,
                side,
                &sides,
                &head_names,
                json!({"side":side,"reason":"selected_test","target_symbol_id":id,"path":[]}),
            );
        }
    }
    if !timed_out {
        for relation in &relations {
            query::check_cancelled()?;
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            let side = relation.side();
            let Some(symbol) = relation
                .text("from")
                .and_then(|id| sides[side].index().symbol(id))
            else {
                continue;
            };
            if test_attribute(symbol, &sides[side]).is_none() {
                continue;
            }
            let ids = relation.path();
            let path = ids
                .iter()
                .filter_map(|id| lookup.get(&(side, (*id)?)).copied())
                .collect::<Vec<_>>();
            if path.is_empty()
                || path.len() != ids.len()
                || path[0].text("from") != Some(symbol.symbol_id.as_str())
                || path
                    .windows(2)
                    .any(|pair| pair[0].text("to") != pair[1].text("from"))
            {
                missing_paths += 1;
                continue;
            }
            add_candidate(
                &mut candidates,
                symbol,
                side,
                &sides,
                &head_names,
                json!({
                    "side":side,"reason":if path.iter().all(|edge| edge.text("kind") == Some("call")) { "call_path" } else { "reference_path" },
                    "target_symbol_id":path.last().unwrap().text("to"),
                    "path":path.iter().map(|edge| edge.value(&sides[side])).collect::<Vec<_>>(),
                    "path_certainty":if path.iter().all(|edge| edge.text("certainty") == Some("definite")) { "definite" } else { "candidate" }
                }),
            );
        }
    }
    if timed_out && candidates.is_empty() {
        bail!("timeout: no test candidates computed within timeout_ms");
    }
    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        (
            left["availability"] != "head",
            left["test"]["qualified_name"].as_str(),
            left["test"]["symbol_id"].as_str(),
        )
            .cmp(&(
                right["availability"] != "head",
                right["test"]["qualified_name"].as_str(),
                right["test"]["symbol_id"].as_str(),
            ))
    });
    let current = candidates
        .iter()
        .filter(|candidate| candidate["availability"] == "head")
        .count();
    let ignored = candidates
        .iter()
        .filter(|candidate| candidate["ignored"] == true)
        .count();
    let nonignored = candidates
        .iter()
        .filter(|candidate| candidate["availability"] == "head" && candidate["ignored"] == false)
        .count();
    let gaps = impact["completeness"]["known_gaps"].as_array_mut().unwrap();
    gaps.push(json!({"kind":"test_selection_without_coverage","reason":"Recommendations follow indexed impact paths, not runtime or per-test coverage; an empty list does not establish that no tests are needed"}));
    gaps.push(json!({"kind":"test_entrypoint_detection_scope","reason":"Only explicit Rust #[test], #[tokio::test] and #[async_std::test] functions are recognized; custom/generated tests, other languages and build/run availability are not established"}));
    if missing_paths > 0 {
        gaps.push(json!({"kind":"test_evidence_path_incomplete","count":missing_paths,"reason":"Truncated impact omitted edges needed for a complete test witness; those candidates were not added"}));
    }
    impact["completeness"]["status"] = json!("partial");
    if timed_out {
        impact["output"]["truncated"] = json!(true);
        impact["output"]["truncation_reason"] = json!("timeout");
        impact["evidence_summary"]["basis"] = json!("partial");
        impact["evidence_summary"]["exact"] = json!(false);
        impact["output"]["remaining_relations"] = Value::Null;
    }
    if impact["output"]["truncated"] == true {
        impact["output"]["continuation"]["kind"] = json!("requery");
        impact["output"]["continuation"]["suggested"] = query::fixed_requery(&impact, args);
    }
    impact["test_summary"] = json!({"candidate_count":candidates.len(),"head_candidates":current,
        "base_only_candidates":candidates.len()-current,"ignored_candidates":ignored,
        "head_nonignored_candidates":nonignored,
        "traversal_complete":impact["output"]["truncated"] != true && missing_paths == 0,"coverage":"unavailable","executed":false,
        "method":"bounded reverse indexed call/reference paths; explicit Rust test attributes"});
    impact["summary"]["tests_to_run"] = json!(nonignored);
    impact["test_candidates"] = json!(candidates);
    Ok(impact)
}
