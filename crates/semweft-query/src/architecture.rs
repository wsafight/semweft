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
            if let Some(relation) = stored.full_index().relation(id) {
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

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn layer(name: &str, paths: &[&str], allow: &[&str]) -> Layer {
        Layer {
            name: name.into(),
            paths: paths.iter().map(|path| (*path).into()).collect(),
            allow: allow.iter().map(|name| (*name).into()).collect(),
        }
    }

    #[test]
    fn valid_policies_accept_glob_paths_and_known_layers() {
        let policy = Policy {
            layers: vec![
                layer("core", &["crates/core/**"], &[]),
                layer("ui", &["src/ui/**", "src/*.rs"], &["core"]),
            ],
            deny_unassigned: true,
        };
        policy.validate().unwrap();
        let digest = policy.digest().unwrap();
        assert!(digest.starts_with("arch_rules_"));
        assert_eq!(policy.digest().unwrap(), digest);

        let mut changed = policy.clone();
        changed.deny_unassigned = false;
        assert_ne!(changed.digest().unwrap(), digest);

        Policy::default().validate().unwrap();
    }

    #[test]
    fn policies_reject_incoherent_layer_definitions() {
        let cases = [
            Policy {
                layers: vec![],
                deny_unassigned: true,
            },
            Policy {
                layers: vec![layer("", &["src/**"], &[]), layer("ui", &["ui/**"], &[])],
                deny_unassigned: false,
            },
            Policy {
                layers: vec![layer("ui", &["src/**"], &[]), layer("ui", &["ui/**"], &[])],
                deny_unassigned: false,
            },
            Policy {
                layers: vec![layer("ui", &[], &[])],
                deny_unassigned: false,
            },
            Policy {
                layers: vec![layer("ui", &["src/**"], &["missing"])],
                deny_unassigned: false,
            },
        ];
        for policy in cases {
            let error = policy.validate().unwrap_err();
            assert_eq!(
                semweft_protocol::error_value(&error)["error"]["code"],
                "invalid_argument",
                "{policy:?}"
            );
        }
    }

    #[test]
    fn policies_reject_escaping_or_invalid_globs() {
        for path in ["/absolute/**", "../outside/**", "", "a/../b"] {
            let policy = Policy {
                layers: vec![layer("ui", &[path], &[])],
                deny_unassigned: false,
            };
            assert!(policy.validate().is_err(), "{path}");
        }
        let policy = Policy {
            layers: vec![layer("ui", &["["], &[])],
            deny_unassigned: false,
        };
        assert!(policy.validate().is_err(), "invalid glob syntax");
    }

    fn report() -> Value {
        json!({
            "architecture": {
                "analysis_complete": true,
                "policy": {"layers": [{"name": "ui", "paths": ["src/ui/**"]}]},
                "rules_digest": "rules",
            },
            "findings": [
                {"fingerprint": "a"},
                {"fingerprint": "b"},
                {"fingerprint": "a"},
                {"other": true},
            ],
            "snapshot_id": "snap",
            "epochs": {"semantic": 4},
            "output": {"continuation": {}},
        })
    }

    #[test]
    fn baselines_capture_distinct_fingerprints_and_version_bindings() {
        let snapshot = baseline(&report()).unwrap();
        assert_eq!(snapshot.schema_version, "0.1");
        assert_eq!(snapshot.rules_digest, "rules");
        assert_eq!(snapshot.snapshot_id, "snap");
        assert_eq!(snapshot.semantic_epoch, 4);
        assert_eq!(
            snapshot.fingerprints,
            BTreeSet::from(["a".to_string(), "b".to_string()])
        );

        let mut missing_epoch = report();
        missing_epoch["epochs"] = json!({});
        assert_eq!(baseline(&missing_epoch).unwrap().semantic_epoch, 0);
    }

    #[test]
    fn baselines_require_complete_collected_traversals_and_bindings() {
        let mut cases = vec![];
        let mut incomplete = report();
        incomplete["architecture"]["analysis_complete"] = json!(false);
        cases.push(incomplete);
        let mut no_layers = report();
        no_layers["architecture"]["policy"]["layers"] = json!([]);
        cases.push(no_layers);
        let mut continued = report();
        continued["output"]["continuation"]["next_cursor"] = json!("page-2");
        cases.push(continued);
        let mut no_rules = report();
        no_rules["architecture"]["rules_digest"] = Value::Null;
        cases.push(no_rules);
        let mut no_snapshot = report();
        no_snapshot["snapshot_id"] = Value::Null;
        cases.push(no_snapshot);
        for report in cases {
            let error = baseline(&report).unwrap_err();
            assert_eq!(
                semweft_protocol::error_value(&error)["error"]["code"],
                "invalid_argument"
            );
        }
    }

    #[test]
    fn policies_and_baselines_reject_unknown_fields() {
        let policy: Result<Policy, _> = serde_json::from_value(json!({
            "layers": [],
            "unknown": true,
        }));
        assert!(policy.is_err());
        let baseline: Result<Baseline, _> = serde_json::from_value(json!({
            "schema_version": "0.1",
            "rules_digest": "d",
            "fingerprints": [],
            "snapshot_id": "s",
            "semantic_epoch": 0,
            "unknown": true,
        }));
        assert!(baseline.is_err());
        let decoded: Baseline = serde_json::from_value(json!({
            "schema_version": "0.1",
            "rules_digest": "d",
            "fingerprints": ["f"],
            "snapshot_id": "s",
            "semantic_epoch": 2,
        }))
        .unwrap();
        assert_eq!(decoded.fingerprints, BTreeSet::from(["f".to_string()]));
    }
}
