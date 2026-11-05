use crate::{architecture, config, engine::StoredView, verify};
use anyhow::{Context, Result, bail, ensure};
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

fn build(
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
    let record = json!({"schema_version":"0.1","workspace_id":resolved.workspace_id,"conclusion":conclusion,"note":note,
        "created_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
        "snapshot_id":report["snapshot_id"],"epochs":report["epochs"],"snapshots":report["snapshots"],
        "diff_digest":report["diff_identity"]["digest"],"rule_bindings":binding,
        "rules_digest":identity("review_rules", &[&serde_json::to_string(&binding)?]),
        "report_digest":identity("report", &[&serde_json::to_string(&report)?]),"report":report,
        "authority":"local human record; not a signature or a change to analysis certainty"});
    Ok(record)
}

pub fn save(
    resolved: &config::Resolved,
    report: Value,
    conclusion: &str,
    note: &str,
) -> Result<Value> {
    let mut record = build(resolved, report, conclusion, note)?;
    persist(resolved, &mut record)?;
    Ok(record)
}

fn persist(resolved: &config::Resolved, record: &mut Value) -> Result<()> {
    record.as_object_mut().unwrap().remove("review_id");
    let id = identity("review", &[&serde_json::to_string(&record)?]);
    record["review_id"] = json!(id);
    let directory = resolved.index_dir.join("reviews");
    config::private_dir(&directory)?;
    config::atomic_write(
        &directory.join(format!("{id}.json")),
        &serde_json::to_vec_pretty(&record)?,
    )?;
    Ok(())
}

pub fn create(resolved: &config::Resolved, report: Value, title: &str) -> Result<Value> {
    ensure!(
        title.len() <= 512,
        "invalid_argument: review title is too long"
    );
    let mut record = build(resolved, report, "comment", "")?;
    record["kind"] = json!("request");
    record["title"] = json!(title);
    record["conclusion"] = json!("pending");
    record["items"] = json!(record["report"]["findings"].as_array().into_iter().flatten().enumerate().map(|(index, finding)| {
        json!({"item_id":identity("finding", &[&index.to_string(), &finding.to_string()]),"finding_index":index})
    }).collect::<Vec<_>>());
    persist(resolved, &mut record)?;
    record["revision"] = json!(0);
    record["state"] = json!("pending");
    Ok(record)
}

fn events(resolved: &config::Resolved, record: &Value) -> Result<Vec<Value>> {
    let id = record["review_id"]
        .as_str()
        .context("invalid_argument: missing review id")?;
    let path = resolved
        .index_dir
        .join("reviews")
        .join(format!("{id}.events.json"));
    let entries: Vec<Value> = match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("index_store_error: malformed review events")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(error) => return Err(error.into()),
    };
    let mut previous = id.to_owned();
    for (index, entry) in entries.iter().enumerate() {
        let mut payload = entry.clone();
        let event_id = payload
            .as_object_mut()
            .context("index_store_error: malformed review event")?
            .remove("event_id");
        ensure!(
            entry["review_id"] == id
                && entry["report_digest"] == record["report_digest"]
                && entry["revision"] == index + 1
                && entry["previous"] == previous
                && event_id == Some(json!(identity("review_event", &[&payload.to_string()]))),
            "index_store_error: review event identity mismatch"
        );
        previous = entry["event_id"].as_str().unwrap().to_owned();
    }
    Ok(entries)
}

pub fn decide(resolved: &config::Resolved, engine: &crate::Engine, args: &Value) -> Result<Value> {
    engine.scan(false)?;
    let (stored, _) = engine.bind(
        &json!({}),
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )?;
    let id = args["review_id"]
        .as_str()
        .context("invalid_argument: missing review id")?;
    let mut record = get(resolved, &stored, id)?;
    ensure!(
        record["kind"] == "request",
        "invalid_argument: decisions require a review request"
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(
            resolved
                .index_dir
                .join("reviews")
                .join(format!("{id}.lock")),
        )?;
    lock.lock()?;
    let mut entries = events(resolved, &record)?;
    ensure!(
        args["revision"].as_u64() == Some(entries.len() as u64),
        "config_conflict: review changed; reload before deciding"
    );
    let target = args["item_id"].as_str().unwrap_or("overall");
    let conclusion = args["conclusion"].as_str().unwrap_or("");
    let reviewer = args["reviewer"].as_str().unwrap_or("").trim();
    let note = args["note"].as_str().unwrap_or("");
    ensure!(
        !reviewer.is_empty()
            && reviewer.len() <= 256
            && note.len() <= 16384
            && entries.len() < 10000,
        "invalid_argument: reviewer, note or event limit invalid"
    );
    if target == "overall" {
        ensure!(
            matches!(conclusion, "approve" | "reject" | "comment"),
            "invalid_argument: invalid overall conclusion"
        );
        if conclusion == "approve" {
            ensure!(
                record["validity"]["snapshot_matches"] == true
                    && record["validity"]["rules_match"] == true,
                "content_mismatch: this review is superseded; create a new request"
            );
            for item in record["items"].as_array().into_iter().flatten() {
                let latest = entries
                    .iter()
                    .rev()
                    .find(|event| event["item_id"] == item["item_id"]);
                ensure!(
                    latest.is_some_and(|event| matches!(
                        event["conclusion"].as_str(),
                        Some("accept" | "dismiss")
                    )),
                    "invalid_argument: acknowledge every finding before approval"
                );
            }
        }
    } else if target == "source" {
        ensure!(
            conclusion == "comment",
            "invalid_argument: source annotations are comments"
        );
        let location = &args["location"];
        let side = location["side"].as_str().unwrap_or("");
        ensure!(
            matches!(side, "base" | "head"),
            "invalid_argument: annotation side required"
        );
        let binding = &record["report"]["snapshots"][side];
        let snapshot = binding["snapshot_id"]
            .as_str()
            .context("invalid_argument: annotation requires a bound diff")?;
        let archived = engine.historical(snapshot, binding["epochs"]["semantic"].as_u64())?;
        let file = location["file"]
            .as_str()
            .context("invalid_argument: annotation file required")?;
        let source = archived
            .view
            .snapshot
            .manifest
            .files
            .get(file)
            .context("content_mismatch: annotation file is not in this view")?;
        ensure!(
            location["file_hash"] == source.file_hash,
            "content_mismatch: annotation source differs"
        );
        semweft_core::byte_offset(
            &source.content,
            semweft_core::Position {
                line: location["line"]
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .context("invalid_argument: annotation line required")?,
                column: 1,
            },
        )
        .map_err(anyhow::Error::msg)?;
    } else {
        ensure!(
            record["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["item_id"] == target))
                && matches!(conclusion, "accept" | "dismiss" | "comment"),
            "invalid_argument: invalid finding decision"
        );
    }
    let mut event = json!({"review_id":id,"report_digest":record["report_digest"],"revision":entries.len()+1,
        "previous":entries.last().map(|entry| &entry["event_id"]).cloned().unwrap_or_else(|| json!(id)),
        "item_id":target,"conclusion":conclusion,"reviewer":reviewer,"note":note,"location":args["location"],
        "created_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64});
    event["event_id"] = json!(identity("review_event", &[&event.to_string()]));
    entries.push(event);
    config::atomic_write(
        &resolved
            .index_dir
            .join("reviews")
            .join(format!("{id}.events.json")),
        &serde_json::to_vec_pretty(&entries)?,
    )?;
    apply_events(&mut record, entries);
    Ok(record)
}

fn apply_events(record: &mut Value, entries: Vec<Value>) {
    let latest = entries.last();
    let conclusion = latest
        .filter(|event| event["item_id"] == "overall")
        .and_then(|event| event["conclusion"].as_str())
        .unwrap_or("pending");
    record["conclusion"] = json!(conclusion);
    record["revision"] = json!(entries.len());
    record["state"] = json!(if record["validity"]["snapshot_matches"] == false
        || record["validity"]["rules_match"] == false
    {
        "superseded"
    } else {
        match conclusion {
            "approve" => "approved",
            "reject" => "changes_requested",
            _ if entries.is_empty() => "pending",
            _ => "in_review",
        }
    });
    record["validity"]["approval_applies"] = json!(record["state"] == "approved");
    record["events"] = json!(entries);
}

fn load(resolved: &config::Resolved, id: &str) -> Result<Value> {
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
    Ok(record)
}

/// Cheap wake-up signal for waiters: revision and coarse state, without a scan.
pub fn status(
    resolved: &config::Resolved,
    engine: &crate::Engine,
    id: &str,
) -> Result<Value> {
    let record = load(resolved, id)?;
    let entries = if record["kind"] == "request" {
        events(resolved, &record)?
    } else {
        vec![]
    };
    let revision = entries.len();
    let conclusion = entries
        .last()
        .filter(|event| event["item_id"] == "overall")
        .and_then(|event| event["conclusion"].as_str())
        .unwrap_or("pending");
    let snapshot_matches = engine.latest_snapshot().is_some_and(|(snapshot, epoch)| {
        record["snapshot_id"] == json!(snapshot) && record["epochs"]["semantic"] == json!(epoch)
    });
    let state = if !snapshot_matches {
        "superseded"
    } else {
        match conclusion {
            "approve" => "approved",
            "reject" => "changes_requested",
            _ if revision == 0 => "pending",
            _ => "in_review",
        }
    };
    Ok(json!({"schema_version":"0.1","review_id":id,"revision":revision,"state":state,"conclusion":conclusion}))
}

pub(crate) fn get(resolved: &config::Resolved, stored: &StoredView, id: &str) -> Result<Value> {
    let mut record = load(resolved, id)?;
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
    if record["kind"] == "request" {
        let entries = events(resolved, &record)?;
        apply_events(&mut record, entries);
    }
    Ok(record)
}
