use crate::{config, engine::StoredView, git, process, query, workspace};
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use semweft_core::{Manifest, digest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
    time::Instant,
};

pub const MODEL: &str = "heuristic_v2";
const MAX_FILES: usize = 10_000;
const MAX_LINES: usize = 100_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    pub line: u32,
    pub hits: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub path: String,
    pub file_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<Line>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_changes: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub format: String,
    pub report_digest: String,
    pub source_manifest_digest: String,
    pub omitted_records: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub anchor_commit: String,
    pub commits_observed: u32,
    pub limit: u32,
    pub window_limited: bool,
    pub shallow: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub schema_version: String,
    pub workspace_id: String,
    pub files: Vec<File>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<History>,
}

fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Artifact {
    pub fn load(path: &Path) -> Result<Self> {
        let artifact: Self = serde_json::from_slice(&read_limited(path, 16 * 1024 * 1024)?)
            .context("invalid_argument: malformed risk input artifact")?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == "0.1" && !self.workspace_id.is_empty(),
            "invalid_argument: invalid risk input identity"
        );
        ensure!(
            self.files.len() <= MAX_FILES,
            "invalid_argument: risk inputs exceed 10000 files"
        );
        if let Some(coverage) = &self.coverage {
            ensure!(
                coverage.format == "lcov"
                    && hash(&coverage.report_digest)
                    && hash(&coverage.source_manifest_digest),
                "invalid_argument: risk coverage requires LCOV and content digests"
            );
        }
        if let Some(history) = &self.history {
            ensure!(
                (1..=1000).contains(&history.limit)
                    && history.commits_observed <= history.limit
                    && matches!(history.anchor_commit.len(), 40 | 64)
                    && history
                        .anchor_commit
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit()),
                "invalid_argument: invalid risk history window"
            );
        }
        let mut paths = BTreeSet::new();
        let mut total_lines = 0;
        for file in &self.files {
            semweft_protocol::safe_relative(&file.path)?;
            ensure!(
                !file.path.contains(['\\', ':', '\0'])
                    && paths.insert(&file.path)
                    && hash(&file.file_hash),
                "invalid_argument: invalid or duplicate risk source path/hash"
            );
            if let Some(lines) = &file.lines {
                ensure!(
                    self.coverage.is_some(),
                    "invalid_argument: coverage lines require provenance"
                );
                total_lines += lines.len();
                let mut seen = BTreeSet::new();
                ensure!(
                    lines
                        .iter()
                        .all(|line| line.line > 0 && seen.insert(line.line)),
                    "invalid_argument: coverage lines must be positive and unique within a file"
                );
            }
            if let Some(changes) = file.history_changes {
                ensure!(
                    self.history
                        .as_ref()
                        .is_some_and(|history| changes <= history.commits_observed),
                    "invalid_argument: history count exceeds its declared window"
                );
            }
        }
        ensure!(
            total_lines <= MAX_LINES,
            "invalid_argument: risk coverage exceeds 100000 line observations"
        );
        Ok(())
    }

    fn bind(&self, stored: &StoredView, revision: Option<&str>, deadline: Instant) -> Result<()> {
        self.validate()?;
        ensure!(
            self.workspace_id == stored.view.snapshot.manifest.workspace_id,
            "content_mismatch: risk inputs belong to a different workspace"
        );
        for file in &self.files {
            query::check_deadline(deadline)?;
            let captured = stored
                .view
                .snapshot
                .manifest
                .files
                .get(&file.path)
                .context("content_mismatch: risk source is outside the selected snapshot")?;
            ensure!(
                captured.file_hash == file.file_hash,
                "content_mismatch: risk source changed: {}",
                file.path
            );
            let line_count = captured.content.lines().count().max(1);
            ensure!(
                file.lines
                    .as_ref()
                    .is_none_or(|lines| lines.iter().all(|line| line.line as usize <= line_count)),
                "content_mismatch: coverage line is outside source: {}",
                file.path
            );
        }
        if let Some(history) = &self.history {
            let revision = revision.context(
                "content_mismatch: selected snapshot has no verifiable risk history anchor",
            )?;
            ensure!(
                history.anchor_commit == revision,
                "content_mismatch: risk history anchor differs from selected snapshot revision"
            );
        }
        Ok(())
    }
}

fn source_manifest(manifest: &Manifest) -> Value {
    json!({"schema_version":"0.1","workspace_id":manifest.workspace_id,
        "analysis_config":manifest.analysis_versions.get("analysis_config"),
        "files":manifest.files.iter().map(|(path,file)|(path.clone(),file.file_hash.clone())).collect::<BTreeMap<_,_>>(),
        "binary_files":manifest.binary_files})
}

fn lcov(
    root: &Path,
    manifest: &Manifest,
    text: &str,
    deadline: Instant,
) -> Result<(BTreeMap<String, Vec<Line>>, u32)> {
    let mut files = BTreeMap::<String, BTreeMap<u32, u64>>::new();
    let mut active = false;
    let mut selected = None;
    let mut omitted = 0;
    let mut observations = 0;
    let mut line_count = 0;
    let mut source_lines = Vec::new();
    for record in text.lines() {
        query::check_deadline(deadline)?;
        if let Some(path) = record.strip_prefix("SF:") {
            ensure!(
                !active,
                "invalid_argument: LCOV record is missing end_of_record"
            );
            active = true;
            let path = Path::new(path);
            let path = if path.is_absolute() {
                path.strip_prefix(root).ok()
            } else {
                Some(path)
            };
            selected = path
                .and_then(|path| path.to_str())
                .map(|path| path.trim_start_matches("./").replace('\\', "/"))
                .filter(|path| {
                    semweft_protocol::safe_relative(path).is_ok()
                        && manifest.files.contains_key(path)
                });
            if let Some(path) = &selected {
                source_lines = manifest.files[path]
                    .content
                    .split_terminator('\n')
                    .collect::<Vec<_>>();
                line_count = source_lines.len().max(1);
                files.entry(path.clone()).or_default();
            } else {
                omitted += 1;
            }
        } else if let Some(value) = record.strip_prefix("DA:") {
            ensure!(
                active,
                "invalid_argument: LCOV DA occurs outside a source record"
            );
            observations += 1;
            ensure!(
                observations <= MAX_LINES,
                "invalid_argument: LCOV exceeds 100000 line observations"
            );
            let mut fields = value.split(',');
            let line = fields
                .next()
                .context("invalid_argument: missing LCOV line")?
                .parse::<u32>()
                .context("invalid_argument: invalid LCOV line")?;
            let hits = fields
                .next()
                .context("invalid_argument: missing LCOV hits")?
                .parse::<u64>()
                .context("invalid_argument: invalid LCOV hits")?;
            let checksum = fields.next().filter(|checksum| !checksum.is_empty());
            let end = fields
                .next()
                .filter(|end| !end.is_empty())
                .map(str::parse::<u32>)
                .transpose()
                .context("invalid_argument: invalid LCOV statement end")?;
            ensure!(
                fields.next().is_none() && end.is_none_or(|end| end >= line && end - line < 128),
                "invalid_argument: invalid LCOV statement span or excess DA fields"
            );
            ensure!(line > 0, "invalid_argument: LCOV lines are one-based");
            if let Some(path) = &selected {
                ensure!(
                    line as usize <= line_count && end.is_none_or(|end| end as usize <= line_count),
                    "content_mismatch: LCOV line is outside captured source"
                );
                if let Some(checksum) = checksum {
                    let text = source_lines
                        .get(line as usize - 1)
                        .context("content_mismatch: missing checksummed source line")?
                        .replacen('\r', "", 1);
                    let expected = base64::engine::general_purpose::STANDARD_NO_PAD
                        .encode(md5::compute(text.as_bytes()).0);
                    ensure!(
                        expected == checksum,
                        "content_mismatch: LCOV checksum differs from captured source at {}:{}",
                        path,
                        line
                    );
                }
                let count = files.get_mut(path).unwrap().entry(line).or_default();
                *count = count
                    .checked_add(hits)
                    .context("invalid_argument: LCOV hit count overflow")?;
            }
        } else if record == "end_of_record" {
            ensure!(
                active,
                "invalid_argument: LCOV terminator occurs outside a source record"
            );
            active = false;
            selected = None;
        }
    }
    ensure!(!active, "invalid_argument: incomplete LCOV record");
    ensure!(
        !files.is_empty(),
        "invalid_argument: LCOV has no captured source records"
    );
    Ok((
        files
            .into_iter()
            .map(|(path, lines)| {
                (
                    path,
                    lines
                        .into_iter()
                        .map(|(line, hits)| Line { line, hits })
                        .collect(),
                )
            })
            .collect(),
        omitted,
    ))
}

fn git_output(root: &Path, arguments: &[&str], deadline: Instant) -> Result<Vec<u8>> {
    let output = process::output(git::command(root).args(arguments), deadline)?;
    ensure!(
        output.status.success(),
        "invalid_argument: cannot collect Git risk history: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .context("invalid_argument: cannot open risk evidence input")?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .context("invalid_argument: cannot read risk evidence input")?;
    ensure!(
        bytes.len() as u64 <= limit,
        "invalid_argument: risk evidence input file exceeds size limit"
    );
    Ok(bytes)
}

pub fn collect(
    resolved: &config::Resolved,
    coverage: Option<(&Path, &Path)>,
    history_limit: Option<u32>,
    deadline: Instant,
) -> Result<Artifact> {
    ensure!(
        coverage.is_some() || history_limit.is_some(),
        "invalid_argument: risk inputs need coverage or history"
    );
    let capture = workspace::capture_config(resolved, "risk-inputs-readonly")?;
    let manifest = &capture.manifest;
    let mut artifact = Artifact {
        schema_version: "0.1".into(),
        workspace_id: manifest.workspace_id.clone(),
        files: manifest
            .files
            .iter()
            .map(|(path, file)| File {
                path: path.clone(),
                file_hash: file.file_hash.clone(),
                lines: None,
                history_changes: None,
            })
            .collect(),
        coverage: None,
        history: None,
    };
    if let Some((report, inputs)) = coverage {
        let report = String::from_utf8(read_limited(report, 32 * 1024 * 1024)?)
            .context("invalid_argument: cannot read UTF-8 LCOV")?;
        let inputs: Value = serde_json::from_slice(&read_limited(inputs, 8 * 1024 * 1024)?)
            .context("invalid_argument: malformed coverage source manifest")?;
        ensure!(
            inputs == source_manifest(manifest),
            "content_mismatch: coverage source manifest no longer matches captured inputs"
        );
        let (mut lines, omitted_records) = lcov(&resolved.root, manifest, &report, deadline)?;
        for file in &mut artifact.files {
            file.lines = lines.remove(&file.path);
        }
        artifact.coverage = Some(Coverage {
            format: "lcov".into(),
            report_digest: digest(report.as_bytes()),
            source_manifest_digest: digest(serde_json::to_vec(&inputs)?),
            omitted_records,
        });
    }
    if let Some(limit) = history_limit {
        ensure!(
            (1..=1000).contains(&limit),
            "invalid_argument: history commits must be 1..=1000"
        );
        let anchor = String::from_utf8(git_output(
            &resolved.root,
            &["rev-parse", "--verify", "HEAD^{commit}"],
            deadline,
        )?)?
        .trim()
        .to_owned();
        let prefix = String::from_utf8(git_output(
            &resolved.root,
            &["rev-parse", "--show-prefix"],
            deadline,
        )?)?
        .trim_end_matches('\n')
        .to_owned();
        let commits = String::from_utf8(git_output(
            &resolved.root,
            &[
                "rev-list",
                "--no-merges",
                &format!("--max-count={}", limit + 1),
                &anchor,
                "--",
                ".",
            ],
            deadline,
        )?)?;
        let commits = commits.lines().collect::<Vec<_>>();
        let mut counts = BTreeMap::<String, u32>::new();
        for commit in commits.iter().take(limit as usize) {
            let paths = git_output(
                &resolved.root,
                &[
                    "diff-tree",
                    "--root",
                    "--no-commit-id",
                    "--name-only",
                    "--no-renames",
                    "-r",
                    "-z",
                    commit,
                    "--",
                    ".",
                ],
                deadline,
            )?;
            for path in paths
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
            {
                let path = std::str::from_utf8(path)
                    .context("invalid_argument: non UTF-8 Git history path")?;
                if let Some(path) = path.strip_prefix(&prefix) {
                    *counts.entry(path.to_owned()).or_default() += 1;
                }
            }
        }
        for file in &mut artifact.files {
            if resolved.root.join(&file.path).is_file() {
                file.history_changes = Some(counts.get(&file.path).copied().unwrap_or(0));
            }
        }
        let shallow = git_output(
            &resolved.root,
            &["rev-parse", "--is-shallow-repository"],
            deadline,
        )?;
        artifact.history = Some(History {
            anchor_commit: anchor,
            commits_observed: commits.len().min(limit as usize) as u32,
            limit,
            window_limited: commits.len() > limit as usize,
            shallow: shallow.starts_with(b"true"),
        });
    }
    let after = workspace::capture_config(resolved, "risk-inputs-readonly")?;
    ensure!(
        source_manifest(manifest) == source_manifest(&after.manifest)
            && manifest.analysis_versions.get("repository_revision")
                == after.manifest.analysis_versions.get("repository_revision"),
        "content_mismatch: sources or Git HEAD changed while collecting risk inputs"
    );
    if let Some(history) = &artifact.history {
        let revision = git_output(
            &resolved.root,
            &["rev-parse", "--verify", "HEAD^{commit}"],
            deadline,
        )?;
        ensure!(
            history.anchor_commit == String::from_utf8(revision)?.trim(),
            "content_mismatch: Git HEAD changed while collecting risk inputs"
        );
    }
    artifact.validate()?;
    Ok(artifact)
}

pub(crate) fn observe(
    result: &Value,
    base: &StoredView,
    head: &StoredView,
    args: &Value,
    deadline: Instant,
) -> Result<(Value, Value, Value)> {
    let mut total = 0;
    let mut covered = 0;
    let mut declarations = 0;
    let mut missing = 0;
    let mut hotspot_files = 0;
    let mut missing_history = 0;
    let mut max_history_changes = 0_u32;
    let mut max_history_commits = 0_u32;
    let mut max_history_source = None;
    let mut incomplete_history = false;
    let mut sources = serde_json::Map::new();
    for (side, stored, key) in [("base", base, "base_symbol"), ("head", head, "head_symbol")] {
        let artifact = args["risk_inputs"]
            .get(side)
            .map(|value| {
                serde_json::from_value::<Artifact>(value.clone())
                    .context("invalid_argument: malformed risk artifact")
            })
            .transpose()?;
        if let Some(artifact) = &artifact {
            artifact.bind(
                stored,
                result["snapshots"][side]["commit"].as_str(),
                deadline,
            )?;
            incomplete_history |= artifact
                .history
                .as_ref()
                .is_some_and(|history| history.shallow);
            sources.insert(side.into(), json!({"artifact_digest":digest(serde_json::to_vec(artifact)?),"coverage":artifact.coverage,"history":artifact.history,
                "authority":"caller-provided evidence; source hashes are verified, test execution and history completeness are not attested"}));
        }
        let files = artifact
            .as_ref()
            .map(|artifact| {
                artifact
                    .files
                    .iter()
                    .map(|file| (file.path.as_str(), file))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let mut observed_lines = BTreeSet::new();
        let mut observed_files = BTreeSet::new();
        let coverage_lines = files
            .iter()
            .filter_map(|(path, file)| {
                file.lines.as_ref().map(|lines| {
                    (
                        *path,
                        lines
                            .iter()
                            .map(|line| (line.line, line.hits))
                            .collect::<BTreeMap<_, _>>(),
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        for change in result["changes"].as_array().into_iter().flatten() {
            query::check_deadline(deadline)?;
            let symbol = &change[key];
            let Some(path) = symbol["location"]["file"].as_str() else {
                continue;
            };
            declarations += 1;
            let start = symbol["span"]["start"]["line"].as_u64().unwrap_or(0);
            let end = symbol["span"]["end"]["line"].as_u64().unwrap_or(0);
            let end_column = symbol["span"]["end"]["column"].as_u64().unwrap_or(0);
            let mut matched = false;
            if let Some(lines) = coverage_lines.get(path) {
                // BTreeMap::range panics when start > end; clamp malformed spans instead.
                let first = u32::try_from(start).unwrap_or(u32::MAX);
                let last = u32::try_from(end).unwrap_or(u32::MAX);
                for (line, hits) in lines.range(first..=last.max(first)) {
                    query::check_deadline(deadline)?;
                    let position = u64::from(*line);
                    if position >= start && (position < end || position == end && end_column > 1) {
                        matched = true;
                        if observed_lines.insert((path, *line)) {
                            total += 1;
                            covered += usize::from(*hits > 0);
                        }
                    }
                }
            }
            missing += usize::from(!matched);
            observed_files.insert(path);
        }
        let hash_key = if side == "base" {
            "base_hash"
        } else {
            "head_hash"
        };
        for file in result["diff_identity"]["files"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if file[hash_key].is_string()
                && let Some(path) = file["path"].as_str()
            {
                observed_files.insert(path);
            }
        }
        for path in observed_files {
            query::check_deadline(deadline)?;
            if let Some((changes, history)) = files
                .get(path)
                .and_then(|file| file.history_changes)
                .zip(
                    artifact
                        .as_ref()
                        .and_then(|artifact| artifact.history.as_ref()),
                )
                .filter(|(_, history)| history.commits_observed > 0)
            {
                hotspot_files += 1;
                if max_history_commits == 0
                    || changes * max_history_commits
                        > max_history_changes * history.commits_observed
                {
                    max_history_changes = changes;
                    max_history_commits = history.commits_observed;
                    max_history_source = Some(json!({"side":side,"file":path}));
                }
            } else {
                missing_history += 1;
            }
        }
    }
    let coverage_score = if total > 0 {
        ((total - covered) * 15).div_ceil(total)
    } else {
        0
    };
    let coverage = json!({"status":if total == 0 {"unavailable"} else if missing > 0 {"partial"} else {"available"},
        "score":coverage_score,"max_score":15,"executed_coverage":if total > 0 {Some(covered as f64 * 100.0 / total as f64)} else {None},
        "observed_lines":total,"covered_lines":covered,"uncovered_lines":total-covered,"changed_declarations":declarations,"declarations_without_line_observations":missing,
        "basis":"imported LCOV executable lines intersecting changed declaration spans, separately on base/head; not changed-line or per-test coverage",
        "per_test_coverage":"unavailable","executes_tests":false});
    let hotspot_score = if max_history_commits > 0 {
        (max_history_changes as usize * 15).div_ceil(max_history_commits as usize)
    } else {
        0
    };
    let hotspot = json!({"status":if hotspot_files == 0 {"unavailable"} else if missing_history > 0 || incomplete_history {"partial"} else {"available"},
        "score":hotspot_score,"max_score":15,"max_change_frequency":if max_history_commits > 0 {Some(f64::from(max_history_changes) / f64::from(max_history_commits))} else {None},
        "max_window_changes":if max_history_commits > 0 {Some(max_history_changes)} else {None},"max_window_commits":if max_history_commits > 0 {Some(max_history_commits)} else {None},"max_frequency_source":max_history_source,
        "observed_file_versions":hotspot_files,"file_versions_without_history":missing_history,"shallow_history":incomplete_history,
        "basis":"maximum changed-file non-merge commit frequency within each explicit frozen history window; renames are not followed; shallow history may omit commits"});
    Ok((coverage, hotspot, Value::Object(sources)))
}

pub(crate) fn preflight(
    stored: &StoredView,
    targets: &[Value],
    files: &BTreeSet<String>,
    args: &Value,
    deadline: Instant,
) -> Result<Value> {
    let revision = stored
        .view
        .snapshot
        .manifest
        .analysis_versions
        .get("revision")
        .or_else(|| {
            stored
                .view
                .snapshot
                .manifest
                .analysis_versions
                .get("repository_revision")
        });
    let selected = json!({"changes":targets.iter().map(|symbol|json!({"head_symbol":symbol})).collect::<Vec<_>>(),
        "diff_identity":{"files":files.iter().filter_map(|path|stored.view.snapshot.manifest.files.get(path).map(|file|json!({"path":path,"head_hash":file.file_hash}))).collect::<Vec<_>>()},
        "snapshots":{"head":{"commit":revision}}});
    let (mut coverage, mut history, sources) = observe(&selected, stored, stored, args, deadline)?;
    coverage["basis"] = json!(
        "imported LCOV anchor lines intersecting selected existing declarations; a before-edit observation, not future-edit or per-test coverage"
    );
    coverage["selected_declarations"] = coverage["changed_declarations"].take();
    coverage
        .as_object_mut()
        .unwrap()
        .remove("changed_declarations");
    history["basis"] = json!(
        "maximum selected captured-file commit frequency within an explicit frozen window; not a prediction of edit risk"
    );
    Ok(
        json!({"coverage":coverage,"hotspots":history,"sources":sources,"scope":"selected existing declarations and captured files before editing","tests_executed":false}),
    )
}
