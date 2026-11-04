use crate::{
    architecture,
    engine::StoredView,
    query::{self, QueryService},
    verify,
};
use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use semweft_core::identity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub protected: Vec<Region>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    pub name: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    pub reason: String,
}

fn patterns(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).context("invalid_argument: invalid protection glob")?);
    }
    Ok(builder.build()?)
}

impl Policy {
    pub(crate) fn path_globs(&self) -> Result<GlobSet> {
        patterns(
            &self
                .protected
                .iter()
                .flat_map(|region| region.paths.iter().cloned())
                .collect::<Vec<_>>(),
        )
    }
    pub fn validate(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        for region in &self.protected {
            if region.name.is_empty()
                || region.reason.is_empty()
                || !names.insert(&region.name)
                || (region.paths.is_empty() && region.symbols.is_empty())
            {
                bail!(
                    "invalid_argument: protection needs unique name, reason and path or symbol patterns"
                );
            }
            for path in &region.paths {
                if path.starts_with('/') || path.split('/').any(|part| part == "..") {
                    bail!("invalid_argument: protected paths must be workspace-relative globs");
                }
            }
            patterns(&region.paths)?;
            patterns(&region.symbols)?;
        }
        Ok(())
    }
}

fn captured_policy(stored: &StoredView) -> Result<Policy> {
    if let Some(policy) = stored
        .view
        .snapshot
        .manifest
        .analysis_versions
        .get("guard_policy")
    {
        let policy: Policy = serde_json::from_str(policy)
            .context("invalid_argument: invalid frozen guard policy")?;
        policy.validate()?;
        return Ok(policy);
    }
    let policy = stored
        .view
        .snapshot
        .manifest
        .files
        .get("semweft.toml")
        .map(|file| {
            toml::from_str::<crate::config::Config>(&file.content).map(|config| config.guard)
        })
        .transpose()
        .context("invalid_argument: invalid captured protection policy")?
        .unwrap_or_default();
    policy.validate()?;
    Ok(policy)
}

fn protected(
    stored: &StoredView,
    files: &BTreeSet<String>,
    policy: &Policy,
    side: &str,
    deadline: Instant,
) -> Result<Vec<Value>> {
    let mut findings = vec![];
    for region in &policy.protected {
        let paths = patterns(&region.paths)?;
        let symbols = patterns(&region.symbols)?;
        for file in files {
            query::check_deadline(deadline)?;
            let mut matching = vec![];
            for symbol in stored
                .view
                .snapshot
                .syntax
                .symbols
                .iter()
                .filter(|symbol| &symbol.file == file)
            {
                query::check_deadline(deadline)?;
                if symbols.is_match(&symbol.qualified_name)
                    || symbols.is_match(query::qualified_name(symbol, stored))
                    || symbols.is_match(query::public_name(symbol))
                {
                    matching.push(symbol.symbol_id.clone());
                }
            }
            if paths.is_match(file) || !matching.is_empty() {
                findings.push(json!({"rule_id":"protected_region","rule_version":"1","severity":"error","status":"violations_found",
                    "fingerprint":identity("protection", &[&region.name,file,side]),"message":format!("Protected region {}: {}",region.name,region.reason),
                    "evidence":{"file":file,"side":side,"symbol_ids":matching,"region":region.name,
                        "snapshot_id":stored.view.snapshot.snapshot_id,"scope":"whole_file_conservative"}}));
            }
        }
    }
    Ok(findings)
}

fn verification(result: &mut Value, findings: Vec<Value>, rules: Vec<Value>, not_run: Vec<Value>) {
    let violations = findings
        .iter()
        .filter(|finding| {
            finding["status"] == "violations_found" && finding["baseline_status"] != "existing"
        })
        .count();
    let warnings = findings
        .iter()
        .filter(|finding| {
            finding["status"] == "insufficient_evidence" && finding["baseline_status"] != "existing"
        })
        .count();
    let incomplete =
        result["completeness"]["status"] != "within_scope" || result["output"]["truncated"] == true;
    let status = if violations > 0 {
        "violations_found"
    } else if warnings > 0 || incomplete {
        "insufficient_evidence"
    } else {
        "no_violations_found"
    };
    result["verification"] = json!({"status":status,"finding_count":findings.len(),"violation_count":violations,"warning_count":warnings,"rules":rules,"not_run":not_run,
        "scope":{"automatic_modification":true,"compiler_and_tests_executed":false}});
    result["guard"]["decision"] = json!(if violations > 0 {
        "block"
    } else if status == "insufficient_evidence" {
        "review"
    } else {
        "allow"
    });
    result["findings"] = json!(findings);
}

pub(crate) fn before(
    stored: &StoredView,
    fresh: Value,
    args: &Value,
    deadline: Instant,
) -> Result<Value> {
    let mut result = query::envelope(stored, fresh);
    let mut files = args["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let explicit_files = files.clone();
    for file in &files {
        semweft_protocol::safe_relative(file)?;
    }
    let mut selected = BTreeSet::new();
    for name in args["symbols"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let symbol = query::resolve(stored, name)?;
        files.insert(symbol.file.clone());
        selected.insert(symbol.symbol_id.clone());
    }
    if files.is_empty() {
        bail!("invalid_argument: preflight requires files or symbols");
    }
    let max_nodes = args["max_nodes"].as_u64().unwrap_or(1000) as usize;
    let mut targets = vec![];
    let mut relations = BTreeMap::new();
    let mut raw = BTreeMap::new();
    let mut public_count = 0;
    let api = stored.api_surface(deadline)?;
    for symbol in &stored.view.snapshot.syntax.symbols {
        query::check_deadline(deadline)?;
        if !files.contains(&symbol.file)
            || (!explicit_files.contains(&symbol.file) && !selected.contains(&symbol.symbol_id))
        {
            continue;
        }
        if targets.len() + relations.len() >= max_nodes {
            result["output"]["truncated"] = json!(true);
            result["output"]["truncation_reason"] = json!("node_limit");
            break;
        }
        let mut value = query::symbol_value(symbol, stored, false, false);
        value["api_exposure"] = api.exposure(symbol);
        public_count += usize::from(value["visibility"] == "public");
        targets.push(value);
        let graph = query::traverse(stored, &symbol.symbol_id, args, true, deadline);
        for (relation, depth, path) in graph.edges {
            if targets.len() + relations.len() >= max_nodes {
                result["output"]["truncated"] = json!(true);
                result["output"]["truncation_reason"] = json!("node_limit");
                break;
            }
            let mut value = query::relation_value(&relation, stored);
            value["depth"] = json!(depth);
            value["path"] = json!(path);
            raw.insert(relation.relation_id.clone(), relation.clone());
            relations.insert(relation.relation_id, value);
        }
        if let Some(reason) = graph.reason {
            result["output"]["truncated"] = json!(true);
            result["output"]["truncation_reason"] = json!(reason);
        }
    }
    let policy = captured_policy(stored)?;
    let findings = protected(stored, &files, &policy, "head", deadline)?;
    let rule = json!({"id":"protected_region","version":"1","status":if findings.is_empty() {"no_violations_found"} else {"violations_found"},"finding_count":findings.len()});
    result["guard"] = json!({"phase":"before","requested_files":files,"public_declarations_observed":public_count,
        "observed_impact_relations":relations.len(),"hotspots":"unavailable","coverage":"unavailable","protection_policy":policy,
        "unconfirmed_risks":["Public visibility and captured API paths require semantic validation", "Impact paths are candidates, not required edits", "Imported coverage and history describe captured existing inputs, not future edits or executed tests"]});
    let observation = crate::risk_inputs::preflight(stored, &targets, &files, args, deadline)?;
    result["guard"]["hotspots"] = observation["hotspots"]["status"].clone();
    result["guard"]["coverage"] = observation["coverage"]["status"].clone();
    result["guard"]["risk_observations"] = observation;
    result["api_summary"] = api.summary(
        targets
            .iter()
            .filter(|target| target["api_exposure"]["external_reachability"] == "candidate")
            .count(),
    );
    result["symbols"] = json!(targets);
    result["relations"] = json!(relations.into_values().collect::<Vec<_>>());
    result["evidence_summary"] = query::summary(
        &raw.into_values().collect::<Vec<_>>(),
        if result["output"]["truncated"] == true {
            "partial"
        } else {
            "complete"
        },
    );
    verification(&mut result, findings, vec![rule], vec![]);
    Ok(result)
}

pub(crate) fn after(
    service: &QueryService,
    impact: Value,
    args: &Value,
    deadline: Instant,
) -> Result<Value> {
    let mut result = verify::report(impact, args);
    let head_meta = &result["snapshots"]["head"];
    let base_meta = &result["snapshots"]["base"];
    let head = service.engine.historical(
        head_meta["snapshot_id"].as_str().unwrap(),
        head_meta["epochs"]["semantic"].as_u64(),
    )?;
    let base = service.engine.historical(
        base_meta["snapshot_id"].as_str().unwrap(),
        base_meta["epochs"]["semantic"].as_u64(),
    )?;
    let mut findings = result["findings"].as_array().cloned().unwrap_or_default();
    let mut rules = result["verification"]["rules"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut not_run = vec![];
    let architecture = architecture::policy(&head, args)?;
    if !architecture.layers.is_empty() {
        architecture::report(&mut result, &head, Some(&base), args, deadline)?;
        findings.extend(result["findings"].as_array().into_iter().flatten().cloned());
        rules.extend(
            result["verification"]["rules"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
    } else {
        not_run.push(json!("architecture_layer_direction: no configured policy"));
    }
    let files = base
        .view
        .snapshot
        .manifest
        .files
        .keys()
        .chain(head.view.snapshot.manifest.files.keys())
        .filter(|path| {
            base.view
                .snapshot
                .manifest
                .files
                .get(*path)
                .map(|file| &file.file_hash)
                != head
                    .view
                    .snapshot
                    .manifest
                    .files
                    .get(*path)
                    .map(|file| &file.file_hash)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut protections = vec![];
    let head_policy = captured_policy(&head)?;
    let base_policy = captured_policy(&base)?;
    for (side, stored) in [("base", &base), ("head", &head)] {
        for policy in [&base_policy, &head_policy] {
            protections.extend(protected(stored, &files, policy, side, deadline)?);
        }
    }
    protections.sort_by(|left, right| {
        left["fingerprint"]
            .as_str()
            .cmp(&right["fingerprint"].as_str())
    });
    protections.dedup_by(|left, right| left["fingerprint"] == right["fingerprint"]);
    rules.push(json!({"id":"protected_region","version":"1","status":if protections.is_empty() {"no_violations_found"} else {"violations_found"},"finding_count":protections.len()}));
    findings.extend(protections);
    result["guard"] = json!({"phase":"after","changed_files":files,"protection_basis":"union of captured base and head policies; symbol protections conservatively cover the whole file"});
    verification(&mut result, findings, rules, not_run);
    Ok(result)
}
