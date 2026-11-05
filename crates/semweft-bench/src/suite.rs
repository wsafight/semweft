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

#[cfg(test)]
mod tests {
    use super::*;
    use semweft_core::{Manifest, SourceFile};
    use std::collections::BTreeMap;

    fn manifest() -> Manifest {
        Manifest {
            workspace_id: "ws_suite".into(),
            files: BTreeMap::from([("src/lib.rs".into(), SourceFile::new("pub fn entry() {}\n"))]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        }
    }

    fn input(revisions: u32) -> StorageInput {
        StorageInput {
            workspace: PathBuf::from("/tmp/semweft-suite-workspace"),
            capture_ms: 1.25,
            manifest: manifest(),
            edited_file: "src/lib.rs".into(),
            revisions,
        }
    }

    fn sample(retained: u64, value: f64, checksum: u64) -> Value {
        json!({
            "retained_views": retained,
            "parse_publish_ms": value,
            "query_checksum": checksum,
        })
    }

    fn report(corpus: &str, samples: Vec<Value>) -> Value {
        json!({"corpus_id": corpus, "samples": samples})
    }

    fn corpus() -> String {
        corpus_id(&input(2)).unwrap()
    }

    #[test]
    fn percentile_uses_the_nearest_rank_and_handles_small_samples() {
        assert_eq!(percentile(&[5.0], 0.0), 5.0);
        assert_eq!(percentile(&[5.0], 0.5), 5.0);
        assert_eq!(percentile(&[5.0], 1.0), 5.0);
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 0.0), 1.0);
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 0.5), 2.0);
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 1.0), 3.0);
        let values = (1..=20).map(f64::from).collect::<Vec<_>>();
        assert_eq!(percentile(&values, 0.95), 19.0);
        assert_eq!(percentile(&values, 1.0), 20.0);
        // NaNs sort after finite values and are still addressable.
        assert!(percentile(&[f64::NAN, 1.0], 1.0).is_nan());
    }

    #[test]
    fn corpus_id_is_deterministic_and_covers_every_input_component() {
        let original = input(2);
        let digest = corpus_id(&original).unwrap();
        assert_eq!(corpus_id(&original.clone()).unwrap(), digest);
        assert_eq!(digest.len(), 64);

        let mut revisions = original.clone();
        revisions.revisions = 3;
        assert_ne!(corpus_id(&revisions).unwrap(), digest);

        let mut edited = original.clone();
        edited.edited_file = "src/other.rs".into();
        assert_ne!(corpus_id(&edited).unwrap(), digest);

        let mut content = original.clone();
        content.manifest.files.insert(
            "src/lib.rs".into(),
            SourceFile::new("pub fn changed() {}\n"),
        );
        assert_ne!(corpus_id(&content).unwrap(), digest);

        let mut versions = original.clone();
        versions
            .manifest
            .analysis_versions
            .insert("cargo".into(), "debug".into());
        assert_ne!(corpus_id(&versions).unwrap(), digest);
    }

    #[test]
    fn save_and_load_round_trip_the_frozen_input() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/input.json");
        let original = input(4);
        save_input(&path, &original).unwrap();
        let frozen: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(frozen["schema_version"], "0.1");
        assert_eq!(
            frozen["input"],
            serde_json::to_value(&original).unwrap(),
            "the persisted envelope keeps the complete input"
        );
        let loaded = load_input(&path).unwrap();
        assert_eq!(
            serde_json::to_value(loaded).unwrap(),
            serde_json::to_value(original).unwrap()
        );
    }

    #[test]
    fn save_refuses_to_overwrite_an_existing_frozen_input() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("input.json");
        save_input(&path, &input(2)).unwrap();
        let before = fs::read(&path).unwrap();
        let error = save_input(&path, &input(5)).unwrap_err();
        assert!(
            format!("{error:#}").contains("invalid_argument"),
            "{error:#}"
        );
        assert_eq!(fs::read(&path).unwrap(), before, "the first input survives");
    }

    #[test]
    fn load_rejects_unknown_schema_versions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("input.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "schema_version": "9.9",
                "input": serde_json::to_value(input(2)).unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
        let error = load_input(&path).err().expect("invalid input must fail");
        assert!(
            format!("{error:#}").contains("unsupported benchmark input"),
            "{error:#}"
        );
    }

    #[test]
    fn load_rejects_corrupt_source_hashes_and_unknown_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("corrupt.json");
        let mut frozen = json!({
            "schema_version": "0.1",
            "input": serde_json::to_value(input(2)).unwrap(),
        });
        frozen["input"]["manifest"]["files"]["src/lib.rs"]["content"] = json!("tampered");
        fs::write(&path, serde_json::to_vec(&frozen).unwrap()).unwrap();
        let error = load_input(&path).err().expect("invalid input must fail");
        assert!(
            format!("{error:#}").contains("corrupt benchmark source hash"),
            "{error:#}"
        );

        let strict = temp.path().join("strict.json");
        let mut frozen = json!({
            "schema_version": "0.1",
            "input": serde_json::to_value(input(2)).unwrap(),
            "extra": true,
        });
        frozen["schema_version"] = json!("0.1");
        fs::write(&strict, serde_json::to_vec(&frozen).unwrap()).unwrap();
        assert!(load_input(&strict).is_err(), "unknown fields are rejected");
    }

    #[test]
    fn run_suite_rejects_unbounded_run_counts() {
        let input = input(2);
        for runs in [0, 101, u32::MAX] {
            let error =
                run_suite(&input, runs, || unreachable!("no trial should run"), None).unwrap_err();
            assert!(
                format!("{error:#}").contains("runs must be 1..=100"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn run_suite_rejects_a_baseline_for_another_corpus() {
        let input = input(2);
        let baseline = json!({"corpus_id": "different"});
        let error = run_suite(
            &input,
            1,
            || Ok(report(&corpus(), vec![sample(1, 1.0, 7)])),
            Some(&baseline),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("baseline uses a different frozen input"),
            "{error:#}"
        );
    }

    #[test]
    fn run_suite_rejects_trial_reports_for_another_corpus() {
        let input = input(2);
        let error = run_suite(
            &input,
            1,
            || Ok(report("other", vec![sample(1, 1.0, 7)])),
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("benchmark child input mismatch"),
            "{error:#}"
        );
    }

    #[test]
    fn run_suite_detects_query_checksum_drift_between_trials() {
        let input = input(2);
        let corpus = corpus();
        let mut trial = 0;
        let error = run_suite(
            &input,
            3,
            || {
                trial += 1;
                Ok(report(&corpus, vec![sample(1, 10.0, trial)]))
            },
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("benchmark query checksum changed"),
            "{error:#}"
        );
    }

    #[test]
    fn run_suite_reports_counts_percentiles_and_every_run() {
        let input = input(2);
        let corpus = corpus();
        let mut values = [10.0, 11.0, 12.0].into_iter();
        let result = run_suite(
            &input,
            3,
            || Ok(report(&corpus, vec![sample(1, values.next().unwrap(), 7)])),
            None,
        )
        .unwrap();
        assert_eq!(result["corpus_id"], corpus);
        assert_eq!(result["runs"].as_array().unwrap().len(), 3);
        assert_eq!(result["runs"][1]["samples"][0]["parse_publish_ms"], 11.0);
        let stats = &result["summary"]["1"]["parse_publish_ms"];
        assert_eq!(stats["count"], 3);
        assert_eq!(stats["min"], 10.0);
        assert_eq!(stats["p50"], 11.0);
        assert_eq!(stats["p95"], 12.0);
        assert_eq!(stats["max"], 12.0);
        // Samples without the metric contribute no statistics.
        assert!(
            result["summary"]["1"]
                .get("catalog_bytes_written")
                .is_none()
        );
    }

    #[test]
    fn run_suite_omits_metrics_that_are_missing_or_not_numeric() {
        let input = input(2);
        let corpus = corpus();
        let mut report = report(&corpus, vec![sample(1, 1.0, 7)]);
        report["samples"][0]["persist_ms"] = json!("slow");
        report["samples"][0]["disk_bytes"] = Value::Null;
        let result = run_suite(&input, 2, || Ok(report.clone()), None).unwrap();
        assert!(result["summary"]["1"].get("persist_ms").is_none());
        assert!(result["summary"]["1"].get("disk_bytes").is_none());
        assert!(result["summary"]["1"].get("parse_publish_ms").is_some());
    }

    #[test]
    fn baseline_comparison_uses_p95_ratios_and_skips_non_positive_values() {
        let input = input(2);
        let corpus = corpus();
        let baseline = json!({
            "corpus_id": corpus,
            "samples": [sample(1, 100.0, 7)],
            "summary": {
                "1": {
                    "parse_publish_ms": {"p95": 100.0},
                    "persist_ms": {"p95": 0.0},
                }
            }
        });
        let mut trial = 0;
        let result = run_suite(
            &input,
            1,
            || {
                trial += 1;
                Ok(report(&corpus, vec![sample(1, 50.0 + trial as f64, 7)]))
            },
            Some(&baseline),
        )
        .unwrap();
        let comparison = result["comparison"].as_object().unwrap();
        assert_eq!(comparison.len(), 1, "{comparison:?}");
        assert_eq!(comparison["1.parse_publish_ms"]["p95_ratio"], 0.51);
    }

    #[test]
    fn disk_bytes_sums_nested_files_and_ignores_directories() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(disk_bytes(temp.path()).unwrap(), 0);
        fs::create_dir_all(temp.path().join("a/b")).unwrap();
        fs::write(temp.path().join("root.txt"), b"12345").unwrap();
        fs::write(temp.path().join("a/nested.txt"), b"abc").unwrap();
        fs::write(temp.path().join("a/b/deep.bin"), [0u8; 17]).unwrap();
        assert_eq!(disk_bytes(temp.path()).unwrap(), 5 + 3 + 17);
    }
}
