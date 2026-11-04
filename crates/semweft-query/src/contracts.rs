use crate::{
    engine::StoredView,
    query::{self, QueryService},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mappings: Vec<Mapping>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub idl: String,
    pub contract: String,
    pub symbol: String,
    pub generator: String,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        for mapping in &self.mappings {
            semweft_protocol::safe_relative(&mapping.idl)?;
            if !mapping.idl.ends_with(".proto")
                || mapping.contract.is_empty()
                || mapping.symbol.is_empty()
                || !mapping
                    .generator
                    .split_once('@')
                    .is_some_and(|(name, version)| !name.is_empty() && !version.is_empty())
            {
                bail!(
                    "invalid_argument: contract mapping needs .proto path, contract id, symbol and generator@version"
                );
            }
        }
        Ok(())
    }
}

fn captured(stored: &StoredView) -> Result<Config> {
    if let Some(config) = stored
        .view
        .snapshot
        .manifest
        .analysis_versions
        .get("contracts_policy")
    {
        let config: Config = serde_json::from_str(config)
            .context("invalid_argument: invalid frozen contract mappings")?;
        config.validate()?;
        return Ok(config);
    }
    let config = stored
        .view
        .snapshot
        .manifest
        .files
        .get("semweft.toml")
        .map(|file| {
            toml::from_str::<crate::config::Config>(&file.content).map(|config| config.contracts)
        })
        .transpose()
        .context("invalid_argument: invalid captured contract configuration")?
        .unwrap_or_default();
    config.validate()?;
    Ok(config)
}

type Declarations = BTreeMap<(String, String), Value>;

fn declarations(
    stored: &StoredView,
    deadline: Instant,
) -> Result<(Declarations, BTreeSet<String>, Vec<Value>)> {
    let mut declarations = BTreeMap::new();
    let mut failed = BTreeSet::new();
    let mut gaps = vec![];
    for (path, file) in stored
        .view
        .snapshot
        .manifest
        .files
        .iter()
        .filter(|(path, _)| path.ends_with(".proto"))
    {
        query::check_deadline(deadline)?;
        let descriptor = match protox_parse::parse(path, &file.content) {
            Ok(descriptor) => descriptor,
            Err(error) => {
                failed.insert(path.clone());
                gaps.push(
                    json!({"kind":"protobuf_parse_failed","file":path,"reason":error.to_string()}),
                );
                continue;
            }
        };
        let locations = descriptor.source_code_info.as_ref().into_iter().flat_map(|info|info.location.iter()).map(|location| {
            (location.path.clone(),json!({"file":path,"line":location.span.first().map(|line|line+1),"column":location.span.get(1).map(|column|column+1),"file_hash":file.file_hash,"coordinate_snapshot_id":stored.view.snapshot.snapshot_id}))
        }).collect::<BTreeMap<_,_>>();
        let package = descriptor.package.as_deref().unwrap_or("");
        let qualified = |prefix: &str, name: &str| {
            if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}.{name}")
            }
        };
        let mut messages = descriptor
            .message_type
            .iter()
            .enumerate()
            .map(|(index, message)| (package.to_owned(), message, vec![4, index as i32]))
            .collect::<Vec<_>>();
        while let Some((parent, message, location)) = messages.pop() {
            query::check_deadline(deadline)?;
            let name = qualified(&parent, message.name.as_deref().unwrap_or(""));
            declarations.insert((path.clone(),name.clone()),json!({"contract_id":name,"kind":"message","file":path,"location":locations.get(&location)}));
            for (index, field) in message.field.iter().enumerate() {
                query::check_deadline(deadline)?;
                let id = qualified(&name, field.name.as_deref().unwrap_or(""));
                let mut location = location.clone();
                location.extend([2, index as i32]);
                let oneof = field
                    .oneof_index
                    .and_then(|index| message.oneof_decl.get(index as usize))
                    .and_then(|oneof| oneof.name.as_deref());
                declarations.insert((path.clone(),id.clone()),json!({"contract_id":id,"container":name,"kind":"field","file":path,
                    "number":field.number,"type":field.r#type,"type_name":field.type_name,"label":field.label,
                    "oneof":oneof,"proto3_optional":field.proto3_optional.unwrap_or(false),"json_name":field.json_name,
                    "location":locations.get(&location)}));
            }
            for (index, nested) in message.nested_type.iter().enumerate() {
                let mut location = location.clone();
                location.extend([3, index as i32]);
                messages.push((name.clone(), nested, location));
            }
        }
        for (service_index, service) in descriptor.service.iter().enumerate() {
            let service_name = qualified(package, service.name.as_deref().unwrap_or(""));
            for (method_index, method) in service.method.iter().enumerate() {
                query::check_deadline(deadline)?;
                let id = qualified(&service_name, method.name.as_deref().unwrap_or(""));
                declarations.insert((path.clone(),id.clone()),json!({"contract_id":id,"container":service_name,"kind":"rpc","file":path,
                    "input":method.input_type,"output":method.output_type,"client_streaming":method.client_streaming.unwrap_or(false),"server_streaming":method.server_streaming.unwrap_or(false),
                    "location":locations.get(&vec![6,service_index as i32,2,method_index as i32])}));
            }
        }
        if !descriptor.enum_type.is_empty() || !descriptor.extension.is_empty() {
            gaps.push(json!({"kind":"protobuf_unmodeled_declarations","file":path,"reason":"Enum values, extensions, custom options and cross-file descriptor linking are outside the current compatibility rules"}));
        }
    }
    Ok((declarations, failed, gaps))
}

fn comparable(mut declaration: Value) -> Value {
    declaration.as_object_mut().unwrap().remove("location");
    declaration
}

fn wire_family(kind: i64) -> Option<u8> {
    match kind {
        3 | 4 | 5 | 8 | 13 | 14 | 17 | 18 => Some(0),
        1 | 6 | 16 => Some(1),
        9 | 11 | 12 => Some(2),
        2 | 7 | 15 => Some(5),
        _ => None,
    }
}

pub(crate) fn rules() -> Vec<&'static str> {
    vec![
        "protobuf_field_number_changed",
        "protobuf_wire_type_changed",
        "protobuf_rpc_removed",
        "protobuf_contract_review",
    ]
}

pub(crate) fn report(
    service: &QueryService,
    mut result: Value,
    args: &Value,
    deadline: Instant,
) -> Result<Value> {
    let mut sides = BTreeMap::new();
    for side in ["base", "head"] {
        let meta = &result["snapshots"][side];
        sides.insert(
            side,
            service.engine.historical(
                meta["snapshot_id"].as_str().unwrap(),
                meta["epochs"]["semantic"].as_u64(),
            )?,
        );
    }
    let (before, before_failed, mut gaps) = declarations(&sides["base"], deadline)?;
    let (after, after_failed, head_gaps) = declarations(&sides["head"], deadline)?;
    gaps.extend(head_gaps);
    let invalid = before_failed.union(&after_failed).collect::<BTreeSet<_>>();
    let mut changes = vec![];
    let mut findings = vec![];
    for key in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        query::check_deadline(deadline)?;
        if invalid.contains(&key.0) {
            continue;
        }
        let base = before.get(key).cloned().unwrap_or(Value::Null);
        let head = after.get(key).cloned().unwrap_or(Value::Null);
        if !base.is_null()
            && !head.is_null()
            && comparable(base.clone()) == comparable(head.clone())
        {
            continue;
        }
        let kind = if base.is_null() {
            "added"
        } else if head.is_null() {
            "removed"
        } else {
            "modified"
        };
        let declaration = if head.is_null() { &base } else { &head };
        let rule = if !base.is_null()
            && !head.is_null()
            && base["kind"] == "field"
            && base["number"] != head["number"]
        {
            "protobuf_field_number_changed"
        } else if !base.is_null()
            && !head.is_null()
            && base["kind"] == "field"
            && base["label"] != 3
            && head["label"] != 3
            && base["type"]
                .as_i64()
                .and_then(wire_family)
                .zip(head["type"].as_i64().and_then(wire_family))
                .is_some_and(|(before, after)| before != after)
        {
            "protobuf_wire_type_changed"
        } else if base["kind"] == "rpc" && head.is_null() {
            "protobuf_rpc_removed"
        } else {
            "protobuf_contract_review"
        };
        let definite = rule != "protobuf_contract_review";
        let change = json!({"kind":kind,"contract_id":key.1,"file":key.0,"declaration_kind":declaration["kind"],"base":base,"head":head});
        findings.push(json!({"rule_id":rule,"rule_version":"1","status":if definite{"violations_found"}else{"insufficient_evidence"},"severity":if definite{"error"}else{"warning"},
            "message":format!("{rule}: {} ({kind}); generated source, JSON and runtime consumers need separate review",key.1),
            "evidence":{"file":key.0,"side":if head.is_null(){"base"}else{"head"},"location":declaration["location"],"change":change}}));
        changes.push(change);
    }
    let mut impacts = vec![];
    let mut relation_count = 0;
    let max_nodes = args["max_nodes"].as_u64().unwrap_or(1000) as usize;
    for change in &changes {
        let mut mapped = false;
        for side in ["base", "head"] {
            let stored = &sides[side];
            let config = captured(stored)?;
            for mapping in config.mappings.iter().filter(|mapping| {
                mapping.idl == change["file"]
                    && (mapping.contract == change["contract_id"]
                        || mapping.contract == change[side]["container"])
            }) {
                query::check_deadline(deadline)?;
                let symbol = match query::resolve(stored, &mapping.symbol) {
                    Ok(symbol) => symbol,
                    Err(error) => {
                        gaps.push(json!({"kind":"generated_symbol_mapping_failed","side":side,"contract_id":change["contract_id"],"reason":error.to_string()}));
                        continue;
                    }
                };
                mapped = true;
                if relation_count >= max_nodes {
                    gaps.push(json!({"kind":"contract_impact_budget","reason":"Combined mapping relation budget reached"}));
                    result["output"]["truncated"] = json!(true);
                    result["output"]["truncation_reason"] = json!("node_limit");
                    break;
                }
                let graph = query::traverse(stored, &symbol.symbol_id, args, true, deadline);
                if graph.edges.len() > max_nodes - relation_count {
                    gaps.push(json!({"kind":"contract_impact_budget","contract_id":change["contract_id"],"reason":"Mapping omitted as a whole to preserve complete witness paths within the combined relation budget"}));
                    result["output"]["truncated"] = json!(true);
                    result["output"]["truncation_reason"] = json!("node_limit");
                    continue;
                }
                let mut relations = vec![];
                for (relation, depth, path) in graph.edges {
                    let mut value = query::relation_value(&relation, stored);
                    value["depth"] = json!(depth);
                    value["path"] = json!(path);
                    value["side"] = json!(side);
                    relations.push(value);
                }
                relation_count += relations.len();
                if graph.reason.is_some() {
                    gaps.push(json!({"kind":"contract_impact_truncated","side":side,"contract_id":change["contract_id"],"reason":graph.reason}));
                    result["output"]["truncated"] = json!(true);
                    result["output"]["truncation_reason"] = json!(query::truncation_reason(
                        result["output"]["truncation_reason"].as_str(),
                        graph.reason
                    ));
                }
                impacts.push(json!({"contract_id":change["contract_id"],"side":side,"mapping":mapping,"symbol":query::symbol_value(symbol,stored,false,false),"relations":relations,
                    "certainty":"candidate","mapping_basis":"explicit configuration, not generator attestation",
                    "impact_level":if mapping.contract == change["contract_id"] && symbol.kind == "field" {"field_candidate"}else{"type_or_contract_candidate"},
                    "runtime":"not_observed","snapshot_id":stored.view.snapshot.snapshot_id}));
            }
        }
        if !mapped {
            gaps.push(json!({"kind":"contract_generated_mapping_missing","contract_id":change["contract_id"],"reason":"No mapped generated declaration was resolved; field access and object passthrough consumers are unknown"}));
        }
    }
    gaps.push(json!({"kind":"contract_external_consumers_unknown","reason":"Unindexed services, generators, reflection and runtime consumers are not enumerated"}));
    result["completeness"]["known_gaps"]
        .as_array_mut()
        .unwrap()
        .extend(gaps);
    result["completeness"]["status"] = json!("partial");
    let violations = findings
        .iter()
        .filter(|finding| finding["status"] == "violations_found")
        .count();
    result["verification"] = json!({"status":if violations>0{"violations_found"}else{"insufficient_evidence"},"finding_count":findings.len(),"violation_count":violations,"warning_count":findings.len()-violations,
        "rules":rules().into_iter().map(|id|json!({"id":id,"version":"1","finding_count":findings.iter().filter(|finding|finding["rule_id"] == id).count()})).collect::<Vec<_>>(),
        "not_run":["Enum/extension/options compatibility", "Cross-file descriptor linking", "Generated source API and JSON compatibility", "Behavioral compatibility"]});
    result["contract_summary"] = json!({"format":"protobuf","parser":"protox-parse@0.10.0","change_count":changes.len(),"mapping_count":impacts.len(),"compatibility_scope":"selected binary-wire and RPC deletion rules; other changes require review","diff_digest":result["diff_identity"]["digest"]});
    result["contract_changes"] = json!(changes);
    result["contract_impacts"] = json!(impacts);
    result["findings"] = json!(findings);
    if result["output"]["truncated"] == true {
        result["evidence_summary"]["basis"] = json!("partial");
        result["evidence_summary"]["exact"] = json!(false);
        result["output"]["remaining_relations"] = Value::Null;
        result["output"]["continuation"]["kind"] = json!("requery");
        result["output"]["continuation"]["suggested"] = query::fixed_requery(&result, args);
    }
    Ok(result)
}
