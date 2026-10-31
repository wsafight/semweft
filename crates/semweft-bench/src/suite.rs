use super::*;
use semweft_core::digest;
use serde::{Deserialize, Serialize};
use std::{fs, io::Write};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frozen {
    schema_version: String,
    input: StorageInput,
}

pub fn corpus_id(input: &StorageInput) -> Result<String> {
    Ok(digest(serde_json::to_vec(&(
        &input.manifest,
        &input.edited_file,
        input.revisions,
    ))?))
}
pub fn save_input(path: &Path, input: &StorageInput) -> Result<()> {
    let frozen = Frozen {
        schema_version: "0.1".into(),
        input: input.clone(),
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, &frozen)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .context("invalid_argument: frozen input already exists or cannot be saved")?;
    Ok(())
}
pub fn load_input(path: &Path) -> Result<StorageInput> {
    let frozen: Frozen = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        frozen.schema_version == "0.1",
        "invalid_argument: unsupported benchmark input"
    );
    ensure!(
        frozen
            .input
            .manifest
            .files
            .values()
            .all(|file| digest(&file.content) == file.file_hash),
        "invalid_argument: corrupt benchmark source hash"
    );
    Ok(frozen.input)
}
fn percentile(values: &[f64], quantile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() as f64 * quantile).ceil() as usize).saturating_sub(1)]
}

/// Each trial is provided by the host in a fresh process, with the same frozen
/// manifest and deterministic comment edits. No repository inputs are recaptured.
pub fn run_suite(
    input: &StorageInput,
    runs: u32,
    mut trial: impl FnMut() -> Result<Value>,
    baseline: Option<&Value>,
) -> Result<Value> {
    ensure!(
        (1..=100).contains(&runs),
        "invalid_argument: runs must be 1..=100"
    );
    let corpus = corpus_id(input)?;
    if let Some(baseline) = baseline {
        ensure!(
            baseline["corpus_id"] == corpus,
            "invalid_argument: baseline uses a different frozen input or edit sequence"
        );
    }
    let mut reports = Vec::new();
    for _ in 0..runs {
        let report = trial()?;
        ensure!(
            report["corpus_id"] == corpus,
            "index_store_error: benchmark child input mismatch"
        );
        if let Some(first) = reports.first().or(baseline) {
            let first: &Value = first;
            let expected: Vec<_> = first["samples"]
                .as_array()
                .context("invalid_argument: baseline lacks samples")?
                .iter()
                .map(|s| (&s["retained_views"], &s["query_checksum"]))
                .collect();
            let actual: Vec<_> = report["samples"]
                .as_array()
                .context("index_store_error: child lacks samples")?
                .iter()
                .map(|s| (&s["retained_views"], &s["query_checksum"]))
                .collect();
            ensure!(
                actual == expected,
                "index_store_error: benchmark query checksum changed"
            );
        }
        reports.push(report);
    }
    let metrics = [
        "parse_publish_ms",
        "persist_ms",
        "cold_index_and_query_ms",
        "hot_1000_queries_ms",
        "recovery.restore_ms",
        "recovery.peak_rss_bytes",
        "publisher_peak_rss_bytes",
        "disk_bytes",
        "object_bytes_written",
        "catalog_bytes_written",
    ];
    let mut summary = serde_json::Map::new();
    for (index, sample) in reports[0]["samples"].as_array().unwrap().iter().enumerate() {
        let mut stats = serde_json::Map::new();
        for metric in metrics {
            let pointer = format!("/{}", metric.replace('.', "/"));
            let values: Vec<_> = reports
                .iter()
                .filter_map(|r| {
                    r["samples"][index]
                        .pointer(&pointer)
                        .and_then(Value::as_f64)
                })
                .collect();
            if !values.is_empty() {
                stats.insert(metric.into(), json!({"count":values.len(),"min":percentile(&values,0.),"p50":percentile(&values,0.5),"p95":percentile(&values,0.95),"max":percentile(&values,1.)}));
            }
        }
        summary.insert(sample["retained_views"].to_string(), Value::Object(stats));
    }
    let mut result = reports[0].clone();
    result["summary"] = Value::Object(summary);
    result["runs"] = json!(reports);
    if let Some(baseline) = baseline {
        let mut comparison = serde_json::Map::new();
        for (revision, stats) in result["summary"].as_object().unwrap() {
            for (metric, stat) in stats.as_object().unwrap() {
                if let Some(old) = baseline["summary"][revision][metric]["p95"]
                    .as_f64()
                    .filter(|old| *old > 0.)
                {
                    comparison.insert(
                        format!("{revision}.{metric}"),
                        json!({"p95_ratio":stat["p95"].as_f64().unwrap()/old}),
                    );
                }
            }
        }
        result["comparison"] = json!(comparison);
    }
    Ok(result)
}

pub(super) fn disk_bytes(path: &Path) -> Result<u64> {
    let mut bytes = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            bytes += disk_bytes(&entry.path())?;
        } else if ty.is_file() {
            bytes += entry.metadata()?.len();
        }
    }
    Ok(bytes)
}
