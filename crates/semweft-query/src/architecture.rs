use crate::{engine::StoredView, query, structure};
use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use semweft_core::identity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Instant};

pub const RULE_VERSION: &str = "1";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub layers: Vec<Layer>,
    pub deny_unassigned: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub name: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub allow: Vec<String>,
}

fn globs(paths: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for path in paths {
        if path.starts_with('/') || path.split('/').any(|part| part == "..") || path.is_empty() {
            bail!("invalid_argument: layer paths must be workspace-relative globs");
        }
        builder.add(Glob::new(path).context("invalid_argument: invalid layer glob")?);
    }
    Ok(builder.build()?)
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        if self.deny_unassigned && self.layers.is_empty() {
            bail!("invalid_argument: deny_unassigned requires at least one architecture layer");
        }
        let names = self
            .layers
            .iter()
            .map(|layer| layer.name.as_str())
            .collect::<BTreeSet<_>>();
        if names.len() != self.layers.len() || names.contains("") {
            bail!("invalid_argument: architecture layer names must be unique and nonempty");
        }
        for layer in &self.layers {
            if layer.paths.is_empty()
                || layer
                    .allow
                    .iter()
                    .any(|name| !names.contains(name.as_str()))
            {
                bail!("invalid_argument: each layer needs paths and valid allowed layer names");
            }
            globs(&layer.paths)?;
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        Ok(identity(
            "arch_rules",
            &[RULE_VERSION, &serde_json::to_string(self)?],
        ))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub schema_version: String,
    pub rules_digest: String,
    pub fingerprints: BTreeSet<String>,
    pub snapshot_id: String,
    pub semantic_epoch: u64,
}

pub fn baseline(report: &Value) -> Result<Baseline> {
    if report["architecture"]["analysis_complete"] != true
        || report["architecture"]["policy"]["layers"]
            .as_array()
            .is_none_or(Vec::is_empty)
    {
        bail!("invalid_argument: cannot baseline incomplete architecture traversal");
    }
    if report["output"]["continuation"]["next_cursor"].is_string() {
        bail!("invalid_argument: collect all architecture pages before writing a baseline");
    }
    Ok(Baseline {
        schema_version: "0.1".into(),
        rules_digest: report["architecture"]["rules_digest"]
            .as_str()
            .context("invalid_argument: no architecture policy")?
            .into(),
        fingerprints: report["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|finding| finding["fingerprint"].as_str().map(str::to_owned))
            .collect(),
        snapshot_id: report["snapshot_id"]
            .as_str()
            .context("invalid_argument: missing snapshot")?
            .into(),
        semantic_epoch: report["epochs"]["semantic"].as_u64().unwrap_or(0),
    })
}

pub(crate) fn policy(stored: &StoredView, args: &Value) -> Result<Policy> {
    let policy = if let Some(value) = args.get("policy") {
        serde_json::from_value(value.clone())
            .context("invalid_argument: invalid architecture policy")?
    } else if let Some(policy) = stored
        .view
        .snapshot
        .manifest
        .analysis_versions
        .get("architecture_policy")
    {
        serde_json::from_str(policy)
            .context("invalid_argument: invalid frozen architecture policy")?
    } else if let Some(file) = stored.view.snapshot.manifest.files.get("semweft.toml") {
        toml::from_str::<crate::config::Config>(&file.content)
            .context("invalid_argument: invalid captured architecture configuration")?
            .architecture
    } else {
        Policy::default()
    };
    policy.validate()?;
    Ok(policy)
}

fn collect_findings(
    stored: &StoredView,
    policy: &Policy,
    deadline: Instant,
) -> Result<(Vec<Value>, Vec<String>)> {
    let layers = policy
        .layers
        .iter()
        .map(|layer| Ok((layer, globs(&layer.paths)?)))
        .collect::<Result<Vec<_>>>()?;
    let mut graph = query::envelope(stored, json!({"satisfied":true}));
    structure::repo_map(
        &mut graph,
        stored,
        &json!({"dependencies":"module","depth":10,"max_symbols_per_module":0}),
        deadline,
    )?;
    let mut findings = vec![];
    let mut unassigned = BTreeSet::new();
    for edge in graph["dependencies"].as_array().into_iter().flatten() {
        query::check_deadline(deadline)?;
        if edge["basis"]
            .as_str()
            .is_some_and(|basis| basis.starts_with("conventional Rust module"))
        {
            continue;
        }
        let from = edge["from"].as_str().unwrap_or("");
        let to = edge["to"].as_str().unwrap_or("");
        let assigned = |path: &str| -> Result<Option<&Layer>> {
            let matching = layers
                .iter()
                .filter(|(_, globs)| globs.is_match(path))
                .map(|(layer, _)| *layer)
                .collect::<Vec<_>>();
            if matching.len() > 1 {
                bail!("invalid_argument: {path} matches more than one architecture layer");
            }
            Ok(matching.first().copied())
        };
        let from_layer = assigned(from)?;
        let to_layer = assigned(to)?;
        for (path, layer) in [(from, from_layer), (to, to_layer)] {
            if layer.is_none() {
                unassigned.insert(path.to_owned());
            }
        }
        let forbidden = match (from_layer, to_layer) {
            (Some(from), Some(to)) => from.name != to.name && !from.allow.contains(&to.name),
            _ => policy.deny_unassigned,
        };
        if !forbidden {
            continue;
        }
        let from_name = from_layer
            .map(|layer| layer.name.as_str())
            .unwrap_or("unassigned");
        let to_name = to_layer
            .map(|layer| layer.name.as_str())
            .unwrap_or("unassigned");
        let definite = edge["certainty"] == "definite";
        let mut evidence = vec![];
        for id in edge["relation_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            query::check_deadline(deadline)?;
            if let Some(relation) = stored.index().relation(id) {
                evidence.push(query::relation_value(relation, stored));
            }
        }
        findings.push(json!({"rule_id":"architecture_layer_direction","rule_version":RULE_VERSION,
            "fingerprint":identity("arch_finding", &[from,to,from_name,to_name]),
            "severity":if definite {"error"} else {"warning"},
            "status":if definite {"violations_found"} else {"insufficient_evidence"},
            "message":format!("{from_name} -> {to_name} is not allowed: {from} -> {to}"),
            "from_layer":from_name,"to_layer":to_name,"dependency":edge,
            "evidence":{"relations":evidence,"dependency_path":[from,to],"snapshot_id":stored.view.snapshot.snapshot_id}}));
    }
    findings.sort_by(|left, right| {
        left["fingerprint"]
            .as_str()
            .cmp(&right["fingerprint"].as_str())
    });
    findings.dedup_by(|left, right| left["fingerprint"] == right["fingerprint"]);
    Ok((findings, unassigned.into_iter().collect()))
}

pub(crate) fn report(
    result: &mut Value,
    head: &StoredView,
    base: Option<&StoredView>,
    args: &Value,
    deadline: Instant,
) -> Result<()> {
    let policy = policy(head, args)?;
    let digest = policy.digest()?;
    let baseline = args
        .get("baseline")
        .map(|value| {
            serde_json::from_value::<Baseline>(value.clone())
                .context("invalid_argument: invalid architecture baseline")
        })
        .transpose()?;
    if let Some(baseline) = &baseline
        && (baseline.schema_version != "0.1" || baseline.rules_digest != digest)
    {
        bail!(
            "invalid_argument: architecture baseline schema or rules digest differs; regenerate explicitly"
        );
    }
    let (mut findings, unassigned) = collect_findings(head, &policy, deadline)?;
    let prior = if let Some(baseline) = &baseline {
        baseline.fingerprints.clone()
    } else if let Some(base) = base {
        collect_findings(base, &policy, deadline)?
            .0
            .iter()
            .filter_map(|finding| finding["fingerprint"].as_str().map(str::to_owned))
            .collect()
    } else {
        BTreeSet::new()
    };
    let current = findings
        .iter()
        .filter_map(|finding| finding["fingerprint"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    for finding in &mut findings {
        finding["baseline_status"] = json!(if prior
            .contains(finding["fingerprint"].as_str().unwrap())
        {
            "existing"
        } else {
            "new"
        });
    }
    let new_violations = findings
        .iter()
        .filter(|finding| {
            finding["baseline_status"] == "new" && finding["status"] == "violations_found"
        })
        .count();
    let warnings = findings
        .iter()
        .filter(|finding| {
            finding["baseline_status"] == "new" && finding["status"] == "insufficient_evidence"
        })
        .count();
    let incomplete = policy.layers.is_empty() || result["completeness"]["status"] != "within_scope";
    let status = if new_violations > 0 {
        "violations_found"
    } else if warnings > 0 || incomplete {
        "insufficient_evidence"
    } else {
        "no_violations_found"
    };
    result["architecture"] = json!({"rules_digest":digest,"rule_version":RULE_VERSION,"policy":policy,
        "analysis_complete":true,"unassigned_dependency_files":unassigned,"comparison":if baseline.is_some() {"baseline"} else if base.is_some() {"diff_base"} else {"none"},
        "new_count":current.difference(&prior).count(),"existing_count":current.intersection(&prior).count(),
        "resolved_fingerprints":prior.difference(&current).collect::<Vec<_>>(),"baseline":baseline,
        "limitations":["Only indexed file-level dependencies", "Candidate bindings are warnings, not definite violations", "No service or transitive policy inference"]});
    result["verification"] = json!({"status":status,"finding_count":findings.len(),"violation_count":new_violations,"warning_count":warnings,
        "rules":[{"id":"architecture_layer_direction","version":RULE_VERSION,"status":status,"finding_count":findings.len()}],
        "not_run":if policy.layers.is_empty() {vec!["architecture policy is not configured"]} else {vec![]},
        "scope":{"dependency_kind":"module","new_findings_only":true,"external_consumers_included":false}});
    result["findings"] = json!(findings);
    Ok(())
}
