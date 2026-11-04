use crate::{
    engine::{self, StoredView},
    query,
};
use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};
use semweft_core::{Certainty, Relation, identity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub paths: Vec<String>,
}

pub fn validate(services: &[Service]) -> Result<()> {
    if services.len() > 256 {
        bail!("invalid_argument: at most 256 services are supported");
    }
    let mut names = BTreeSet::new();
    for service in services {
        if service.name.trim().is_empty()
            || !names.insert(&service.name)
            || service.paths.is_empty()
            || service.paths.len() > 256
        {
            bail!("invalid_argument: services need unique names and 1..256 path globs");
        }
        for path in &service.paths {
            if path.starts_with('/')
                || path.contains(['\\', ':'])
                || path.split('/').any(|part| part == "..")
            {
                bail!("invalid_argument: service paths must be workspace-relative globs");
            }
            Glob::new(path).context("invalid_argument: invalid service path glob")?;
        }
    }
    Ok(())
}

pub(crate) struct Mapping {
    pub services: Vec<Service>,
    pub owners: BTreeMap<String, String>,
    pub digest: String,
}

pub(crate) fn configured(manifest: &semweft_core::Manifest) -> Result<Vec<Service>> {
    let services = if let Some(frozen) = manifest.analysis_versions.get("service_mapping") {
        serde_json::from_str(frozen).context("invalid_argument: invalid frozen service mapping")?
    } else if let Some(file) = manifest.files.get("semweft.toml") {
        toml::from_str::<crate::config::Config>(&file.content)
            .context("invalid_argument: invalid captured service configuration")?
            .services
    } else {
        vec![]
    };
    validate(&services)?;
    Ok(services)
}

impl Mapping {
    pub fn load(stored: &StoredView, deadline: Instant) -> Result<Self> {
        let manifest = &stored.view.snapshot.manifest;
        let services = configured(manifest)?;
        let mut builder = GlobSetBuilder::new();
        let mut names = vec![];
        for service in &services {
            for path in &service.paths {
                builder.add(Glob::new(path)?);
                names.push(service.name.clone());
            }
        }
        let patterns = builder.build()?;
        let mut owners = BTreeMap::new();
        for file in manifest.files.keys() {
            query::check_deadline(deadline)?;
            let matches = patterns
                .matches(file)
                .into_iter()
                .map(|index| names[index].clone())
                .collect::<BTreeSet<_>>();
            if matches.len() > 1 {
                bail!(
                    "invalid_argument: file {file} belongs to multiple services: {}",
                    matches.into_iter().collect::<Vec<_>>().join(", ")
                );
            }
            if let Some(owner) = matches.into_iter().next() {
                owners.insert(file.clone(), owner);
            }
        }
        let digest = identity("services", &[&serde_json::to_string(&services)?]);
        Ok(Self {
            services,
            owners,
            digest,
        })
    }

    fn endpoint(&self, file: &str, scope: &str) -> Option<String> {
        if scope == "module" {
            Some(file.into())
        } else {
            self.owners.get(file).cloned()
        }
    }

    pub fn summary(&self) -> Value {
        json!({"mapping_digest":self.digest,"services":self.services,"mapped_files":self.owners.len(),
            "basis":"explicit captured path ownership; no inferred network or runtime dependencies"})
    }
}

fn endpoints(
    stored: &StoredView,
    mapping: &Mapping,
    relation: &Relation,
    scope: &str,
) -> Option<(String, String)> {
    let from = stored.index().symbol(&relation.from_symbol)?;
    let to = stored.index().symbol(&relation.to_symbol)?;
    let from = mapping.endpoint(&from.file, scope)?;
    let to = mapping.endpoint(&to.file, scope)?;
    (from != to).then_some((from, to))
}

pub(crate) fn dependencies(
    stored: &StoredView,
    args: &Value,
    deadline: Instant,
) -> Result<(Vec<Value>, Value)> {
    let mapping = Mapping::load(stored, deadline)?;
    if mapping.services.is_empty() {
        bail!("capability_unsupported: service dependencies require configured services");
    }
    let mut grouped = BTreeMap::<(String, String), Vec<Relation>>::new();
    let mut unmapped = 0;
    for relation in query::filtered(engine::relations_with_deadline(stored, deadline)?, args) {
        query::check_deadline(deadline)?;
        if let Some(pair) = endpoints(stored, &mapping, &relation, "service") {
            grouped.entry(pair).or_default().push(relation);
        } else if !mapping.owners.contains_key(&relation.file)
            || stored
                .index()
                .symbol(&relation.to_symbol)
                .is_none_or(|symbol| !mapping.owners.contains_key(&symbol.file))
        {
            unmapped += 1;
        }
    }
    let dependencies = grouped.into_iter().map(|((from,to),mut edges)| {
        edges.sort_by(|left,right|left.relation_id.cmp(&right.relation_id));
        json!({"from":from,"to":to,"source":if edges.iter().any(|edge|edge.evidence.iter().any(|evidence|evidence.source=="L3")) {"L3"} else {"L1"},
            "certainty":if edges.iter().any(|edge|edge.certainty==Certainty::Definite && edge.freshness=="current") {"definite"} else {"candidate"},
            "basis":"indexed cross-service symbol relations and explicit path ownership","mapping_digest":mapping.digest,
            "relation_ids":edges.iter().map(|edge|&edge.relation_id).collect::<Vec<_>>(),
            "evidence":edges.iter().map(|edge|query::relation_value(edge,stored)).collect::<Vec<_>>()})
    }).collect();
    let mut summary = mapping.summary();
    summary["unmapped_indexed_relations"] = json!(unmapped);
    Ok((dependencies, summary))
}

pub(crate) fn find_paths(
    mut result: Value,
    stored: &StoredView,
    args: &Value,
    deadline: Instant,
) -> Result<Value> {
    use semweft_algo::{
        graph::{Adjacency, Interrupt},
        paths::{Limits, StopReason, simple_paths_filtered},
    };
    let scope = args["scope"].as_str().unwrap();
    let from = args["from"].as_str().unwrap().to_string();
    let to = args["to"].as_str().unwrap().to_string();
    let mapping = Mapping::load(stored, deadline)?;
    for endpoint in [&from, &to] {
        if scope == "service" {
            if !mapping
                .services
                .iter()
                .any(|service| service.name == *endpoint)
            {
                bail!("invalid_argument: unknown configured service {endpoint}");
            }
        } else {
            semweft_protocol::safe_relative(endpoint)?;
            if !stored.view.snapshot.manifest.files.contains_key(endpoint) {
                bail!("file_not_found: module file is not captured: {endpoint}");
            }
        }
    }
    let mut graph = Adjacency::<String, String>::new();
    let mut edges = BTreeMap::new();
    for relation in query::filtered(engine::relations_with_deadline(stored, deadline)?, args) {
        query::check_deadline(deadline)?;
        if relation.kind != "call" {
            continue;
        }
        if let Some((from, to)) = endpoints(stored, &mapping, &relation, scope) {
            graph
                .entry(from)
                .or_default()
                .insert((relation.relation_id.clone(), to));
            edges.insert(relation.relation_id.clone(), relation);
        }
    }
    let limits = Limits {
        max_depth: args["max_depth"].as_u64().unwrap_or(5),
        max_paths: args["max_paths"].as_u64().unwrap_or(100) as usize,
        max_nodes: args["max_nodes"].as_u64().unwrap_or(1000) as usize,
    };
    let traversal = simple_paths_filtered(
        &graph,
        from.clone(),
        to.clone(),
        limits,
        || {
            if query::check_cancelled().is_err() {
                Err(Interrupt::Cancelled)
            } else if Instant::now() >= deadline {
                Err(Interrupt::Timeout)
            } else {
                Ok(())
            }
        },
        |_| true,
    );
    if traversal.reason == Some(StopReason::Cancelled) {
        bail!("cancelled: path query cancelled");
    }
    if traversal.reason == Some(StopReason::Timeout) && traversal.paths.is_empty() {
        bail!("timeout: no paths computed within timeout_ms");
    }
    let reason = traversal.reason.map(|reason| match reason {
        StopReason::PathLimit => "path_limit",
        StopReason::DepthLimit => "depth_limit",
        StopReason::NodeLimit => "node_limit",
        StopReason::Timeout => "timeout",
        StopReason::Cancelled => "cancelled",
    });
    let mut observed = BTreeMap::new();
    let paths = traversal.paths.iter().map(|path| {
        let witnesses = path.edges.iter().map(|id|&edges[id]).collect::<Vec<_>>();
        for witness in &witnesses {observed.insert(witness.relation_id.clone(),(*witness).clone());}
        json!({"path_id":identity("aggregate_path", &[&stored.view.snapshot.snapshot_id,&stored.view.epochs.semantic_epoch.to_string(),scope,&from,&to,&path.edges.join("\n")]),
            "node_ids":path.nodes,"nodes":path.nodes.iter().map(|name|json!({"name":name,"kind":scope})).collect::<Vec<_>>(),
            "length":witnesses.len(),"certainty":if witnesses.iter().all(|edge|edge.certainty==Certainty::Definite) {"definite"} else {"candidate"},
            "freshness":if witnesses.iter().any(|edge|edge.freshness=="pending") {"pending"} else {"current"},
            "relations":witnesses.iter().map(|edge|query::relation_value(edge,stored)).collect::<Vec<_>>()})
    }).collect::<Vec<_>>();
    let basis = match traversal.reason {
        None => "complete",
        Some(StopReason::DepthLimit | StopReason::PathLimit) => "within_limits",
        _ => "partial",
    };
    result["evidence_summary"] = query::summary(&observed.into_values().collect::<Vec<_>>(), basis);
    result["from"] = json!({"name":from,"kind":scope});
    result["to"] = json!({"name":to,"kind":scope});
    result["path_summary"] = json!({"path_count":paths.len(),"enumeration_complete":reason.is_none(),"reason":reason,"visited_nodes":traversal.visited.len(),"basis":basis,"max_depth":limits.max_depth,"max_paths":limits.max_paths,"max_nodes":limits.max_nodes,
        "cycle_policy":"simple aggregate paths; repeated nodes excluded","scope":scope,
        "interpretation":"Each hop has an indexed call witness; adjacent hops need not share an intermediate symbol or form an executable call chain"});
    result["paths"] = json!(paths);
    if scope == "service" {
        result["service_mapping"] = mapping.summary();
    }
    result["frontier"] = json!(traversal.frontier);
    if let Some(reason) = reason {
        let mut suggested = query::fixed_requery(&result, args);
        suggested["max_depth"] = json!((limits.max_depth + 1).min(10));
        suggested["max_paths"] = json!((limits.max_paths * 2).min(1000));
        suggested["max_nodes"] = json!((limits.max_nodes * 2).min(100000));
        result["output"] = json!({"truncated":true,"truncation_reason":reason,"remaining_relations":null,"continuation":{"kind":"requery","frontier":traversal.frontier,"suggested":suggested}});
    }
    Ok(result)
}

pub(crate) fn enrich_diff(
    result: &mut Value,
    base: &StoredView,
    head: &StoredView,
    deadline: Instant,
) -> Result<()> {
    #[derive(Default)]
    struct Impact {
        files: BTreeSet<String>,
        changed_files: BTreeSet<String>,
        symbols: BTreeSet<String>,
    }
    let before = Mapping::load(base, deadline)?;
    let after = Mapping::load(head, deadline)?;
    let mut groups = BTreeMap::<(String, String), Impact>::new();
    for module in result["affected_modules"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        query::check_deadline(deadline)?;
        let side = module["side"].as_str().unwrap_or("head");
        let mapping = if side == "base" { &before } else { &after };
        let file = module["path"].as_str().unwrap();
        if let Some(service) = mapping.owners.get(file) {
            let group = groups.entry((side.into(), service.clone())).or_default();
            group.files.insert(file.into());
            group.symbols.extend(
                module["affected_symbol_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
            module["service"] = json!(service);
        }
    }
    let mut unmapped = BTreeSet::new();
    for file in result["diff_identity"]["files"]
        .as_array()
        .into_iter()
        .flatten()
    {
        query::check_deadline(deadline)?;
        let path = file["path"].as_str().unwrap();
        for (side, mapping) in [("base", &before), ("head", &after)] {
            if file[format!("{side}_hash")].is_null() {
                continue;
            }
            if let Some(service) = mapping.owners.get(path) {
                let group = groups.entry((side.into(), service.clone())).or_default();
                group.files.insert(path.into());
                group.changed_files.insert(path.into());
            } else {
                unmapped.insert((side.to_owned(), path.to_owned()));
            }
        }
    }
    let service_count = groups
        .keys()
        .map(|(_, service)| service)
        .collect::<BTreeSet<_>>()
        .len();
    result["affected_services"] = json!(groups.into_iter().map(|((side,name),group)| {
        let mapping = if side == "base" {&before} else {&after};
        json!({"side":side,"name":name,"files":group.files,"changed_files":group.changed_files,"affected_symbol_ids":group.symbols,"mapping_digest":mapping.digest})
    }).collect::<Vec<_>>());
    result["service_impact_summary"] = json!({"base":before.summary(),"head":after.summary(),"affected_services_observed":service_count,
        "unmapped_changed_files":unmapped.iter().map(|(side,path)|json!({"side":side,"path":path})).collect::<Vec<_>>(),
        "basis":"explicit ownership of captured changed files and indexed candidate impact; not runtime service reachability"});
    if result["summary"].is_object() {
        result["summary"]["services"] = json!(service_count);
    }
    Ok(())
}
