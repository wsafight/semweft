use crate::{architecture, config, engine::StoredView, verify};
use anyhow::{Context, Result, bail};
use semweft_core::identity;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

fn rules(report: &Value) -> Value {
    json!({"versions":report["verification"]["rules"].as_array().into_iter().flatten().filter_map(|rule| Some((rule["id"].as_str()?,rule["version"].as_str()?))).collect::<BTreeMap<_,_>>(),
        "architecture":report["architecture"]["rules_digest"],"risk_model":report["risk_summary"]["scoring"]})
}

pub fn save(
    resolved: &config::Resolved,
    mut report: Value,
    conclusion: &str,
    note: &str,
) -> Result<Value> {
    if !matches!(conclusion, "approve" | "reject" | "comment") {
        bail!("invalid_argument: review conclusion must be approve, reject or comment");
    }
    if report["schema_version"] != "0.1"
        || !report["verification"].is_object()
        || !report["snapshot_id"].is_string()
        || report["completeness"]["scope"]["workspace_id"] != resolved.workspace_id
    {
        bail!("invalid_argument: review requires a verification report from this workspace");
    }
    if report["output"]["continuation"]["next_cursor"].is_string()
        || report["findings"].as_array().map(Vec::len)
            != report["verification"]["finding_count"]
                .as_u64()
                .map(|count| count as usize)
    {
        bail!("invalid_argument: review requires all findings, not a partial page");
    }
    report
        .as_object_mut()
        .unwrap()
        .remove("_semweft_source_root");
    let binding = rules(&report);
    let mut record = json!({"schema_version":"0.1","workspace_id":resolved.workspace_id,"conclusion":conclusion,"note":note,
        "created_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
        "snapshot_id":report["snapshot_id"],"epochs":report["epochs"],"snapshots":report["snapshots"],
        "diff_digest":report["diff_identity"]["digest"],"rule_bindings":binding,
        "rules_digest":identity("review_rules", &[&serde_json::to_string(&binding)?]),
        "report_digest":identity("report", &[&serde_json::to_string(&report)?]),"report":report,
        "authority":"local human record; not a signature or a change to analysis certainty"});
    let id = identity("review", &[&serde_json::to_string(&record)?]);
    record["review_id"] = json!(id);
    let directory = resolved.index_dir.join("reviews");
    config::private_dir(&directory)?;
    config::atomic_write(
        &directory.join(format!("{id}.json")),
        &serde_json::to_vec_pretty(&record)?,
    )?;
    Ok(record)
}

pub(crate) fn get(resolved: &config::Resolved, stored: &StoredView, id: &str) -> Result<Value> {
    if !id.starts_with("review_")
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        || id.len() > 128
    {
        bail!("invalid_argument: invalid review id");
    }
    let path = resolved
        .index_dir
        .join("reviews")
        .join(format!("{id}.json"));
    let mut record: Value = serde_json::from_slice(
        &std::fs::read(path).context("review_not_found: no such local review")?,
    )
    .context("invalid_argument: malformed review record")?;
    let recorded_id = record
        .as_object_mut()
        .context("invalid_argument: invalid review record")?
        .remove("review_id");
    if recorded_id != Some(json!(id))
        || identity("review", &[&serde_json::to_string(&record)?]) != id
        || record["workspace_id"] != resolved.workspace_id
    {
        bail!("invalid_argument: review identity or workspace mismatch");
    }
    let current_versions = verify::rules()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|rule| {
            Some((
                rule["id"].as_str()?.to_owned(),
                rule["version"].as_str()?.to_owned(),
            ))
        })
        .chain([
            (
                "architecture_layer_direction".into(),
                architecture::RULE_VERSION.into(),
            ),
            ("protected_region".into(), "1".into()),
        ])
        .chain(
            crate::contracts::rules()
                .into_iter()
                .map(|rule| (rule.to_owned(), "1".into())),
        )
        .collect::<BTreeMap<_, _>>();
    let versions_match = record["rule_bindings"]["versions"]
        .as_object()
        .is_some_and(|rules| {
            rules.iter().all(|(id, version)| {
                current_versions
                    .get(id)
                    .is_some_and(|current| version == current)
            })
        });
    let policy_matches = record["rule_bindings"]["architecture"].is_null()
        || record["rule_bindings"]["architecture"]
            == architecture::policy(stored, &json!({}))?.digest()?;
    let snapshot_matches = record["snapshot_id"] == stored.view.snapshot.snapshot_id
        && record["epochs"]["semantic"] == stored.view.epochs.semantic_epoch;
    let risk_matches = record["rule_bindings"]["risk_model"].is_null()
        || record["rule_bindings"]["risk_model"] == crate::risk_inputs::MODEL;
    record["review_id"] = json!(id);
    record["validity"] = json!({"snapshot_matches":snapshot_matches,"rules_match":versions_match && policy_matches && risk_matches,
        "approval_applies":record["conclusion"] == "approve" && snapshot_matches && versions_match && policy_matches && risk_matches,
        "current_snapshot_id":stored.view.snapshot.snapshot_id,"current_semantic_epoch":stored.view.epochs.semantic_epoch,
        "reason":"Approval applies only to the bound analysis view and rule versions, never to later edits"});
    Ok(record)
}
