use crate::{lsp, workspace};
use anyhow::{Context, Result, bail};
use semweft_core::*;
use semweft_lang_rust::RustParser;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    steps: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    name: String,
    edits: Vec<Edit>,
    #[serde(default)]
    fault: Option<Fault>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    path: String,
    /// New content; `null` or absent without `replace` deletes the file.
    content: Option<String>,
    /// Replace one exact occurrence, so large real files need not be inlined.
    replace: Option<Replace>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replace {
    old: String,
    new: String,
}
#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Fault {
    /// Kill the persistent backend before this step's refresh.
    CrashBackend,
    /// Stop this step's refresh halfway; the next step's edit supersedes it.
    InterruptRefresh,
}

const MAX_BACKEND_RESTARTS: u32 = 2;

pub fn scan(root: &Path) -> Result<Value> {
    let root = root
        .canonicalize()
        .context("invalid_argument: workspace does not exist")?;
    let workspace_id = identity(
        "ws",
        &[root.to_str().context("invalid_argument: non-UTF-8 root")?],
    );
    let captured = workspace::capture(&root, &workspace_id, "not_enabled")?;
    let mut syntax = RustParser::new()
        .map_err(anyhow::Error::msg)?
        .analyze(&captured.manifest)
        .map_err(anyhow::Error::msg)?;
    syntax.diagnostics.extend(captured.diagnostics);
    syntax.diagnostics.push(Diagnostic {
        code: "semantic_not_requested".into(),
        file: None,
        message:
            "Syntax-only prototype; L3 is available through explicit prototype check authorization"
                .into(),
    });
    let view = RefreshCoordinator::default()
        .publish_inputs(captured.manifest, syntax)
        .map_err(anyhow::Error::msg)?;
    Ok(
        json!({"schema_version":"0.1", "stage":"prototype", "snapshot_id":view.snapshot.snapshot_id,
        "epochs":view.epochs, "manifest":view.snapshot.manifest,
        "symbols":view.snapshot.syntax.symbols, "call_sites":view.snapshot.syntax.calls,
        "completeness":{"status":view.completeness_status(),"scope":view.snapshot.manifest.scope,
            "known_gaps":view.known_gaps,"pending_units":[],"L3":"not_requested"},
        "output":{"truncated":false}}),
    )
}

pub fn check(
    root: &Path,
    edits_path: Option<&Path>,
    executable: &Path,
    timeout_ms: u64,
) -> Result<Value> {
    let root = root
        .canonicalize()
        .context("invalid_argument: workspace does not exist")?;
    if !root.join("Cargo.toml").is_file() {
        bail!("invalid_argument: prototype check requires a Cargo.toml at workspace root");
    }
    let executable = if executable.components().count() > 1 {
        executable
            .canonicalize()
            .context("indexer_unavailable: executable path does not exist")?
    } else {
        executable.into()
    };
    let workspace_id = identity(
        "ws",
        &[root.to_str().context("invalid_argument: non-UTF-8 root")?],
    );
    let scenario = if let Some(path) = edits_path {
        serde_json::from_slice::<Scenario>(
            &fs::read(path).context("invalid_argument: cannot read edit scenario")?,
        )
        .context("invalid_argument: invalid edit scenario")?
    } else {
        Scenario { steps: vec![] }
    };
    validate(&scenario)?;
    let temporary = tempfile::tempdir()?;
    let scratch = temporary.path().join("workspace");
    fs::create_dir(&scratch)?;
    let scratch = scratch.canonicalize()?;
    workspace::capture(&root, &workspace_id, "")?.materialize(&scratch)?;
    // Toolchain proxies (rustup) select the binary per directory, so the
    // version must come from where the backend runs, not from our own cwd.
    let backend = lsp::version(&executable, &scratch)?;
    let captured = workspace::capture(&root, &workspace_id, &backend)?;
    let mut parser = RustParser::new().map_err(anyhow::Error::msg)?;
    let mut coordinator = RefreshCoordinator::default();
    let mut reports = vec![];
    let mut persistent: Option<lsp::LspClient> = None;
    let initial = Step {
        name: "initial".into(),
        edits: vec![],
        fault: None,
    };
    let mut pinned = vec![];
    // Syntax epochs still waiting for semantic publication, with edit times.
    let mut waiting: Vec<(u64, Instant)> = vec![];
    let mut pending_ms = vec![];
    for step in std::iter::once(initial).chain(scenario.steps) {
        eprintln!("prototype: checking {}", step.name);
        let started = Instant::now();
        for edit in &step.edits {
            apply(&scratch, edit)?;
        }
        let input = workspace::capture(&scratch, &workspace_id, &backend)?;
        let mut syntax = parser
            .analyze(&input.manifest)
            .map_err(anyhow::Error::msg)?;
        syntax.diagnostics.extend(input.diagnostics.clone());
        let pending = coordinator
            .publish_inputs(input.manifest.clone(), syntax)
            .map_err(anyhow::Error::msg)?;
        let syntax_ms = started.elapsed().as_secs_f64() * 1000.0;
        waiting.push((pending.epochs.syntax_epoch, started));
        pinned.push(pending.clone());
        let plan = coordinator.plan_refresh().map_err(anyhow::Error::msg)?;
        let plan_report = json!({"scope":plan.scope, "base_syntax_epoch":plan.base_syntax_epoch,
            "queried_callsites":plan.query.len(), "reused_callsites":plan.reused.len()});
        let ticket = coordinator.begin_refresh().map_err(anyhow::Error::msg)?;
        let semantic_started = Instant::now();
        let deadline = semantic_started + Duration::from_millis(timeout_ms);
        if step.fault == Some(Fault::CrashBackend) {
            persistent
                .as_mut()
                .context("invalid_argument: crash_backend needs a running backend")?
                .kill();
        }
        let interrupt_after =
            (step.fault == Some(Fault::InterruptRefresh)).then(|| plan.query.len().div_ceil(2));
        let mut queried = 0;
        let mut cancel = || {
            let stop = interrupt_after.is_some_and(|limit| queried >= limit);
            queried += 1;
            stop
        };
        let mut restarts = 0;
        let outcome = loop {
            if persistent.is_none() {
                persistent = Some(lsp::LspClient::start(&executable, &scratch, deadline)?);
            }
            let client = persistent.as_mut().unwrap();
            match client.refresh(
                &pending.snapshot,
                &backend,
                deadline,
                &plan.query,
                &mut cancel,
            ) {
                Ok(outcome) => break outcome,
                // A dead backend loses its open documents; a new session is
                // synchronized from the pinned snapshot, never from disk state.
                Err(error) if !client.alive() && restarts < MAX_BACKEND_RESTARTS => {
                    eprintln!("prototype: backend exited ({error:#}); restarting");
                    restarts += 1;
                    persistent = None;
                }
                Err(error) => return Err(error),
            }
        };
        let Some(mut batch) = outcome else {
            coordinator.cancel_refresh();
            // Whatever was gathered cannot be published for the cancelled ticket.
            let rejected = coordinator
                .complete(
                    &ticket,
                    SemanticBatch {
                        snapshot_id: pending.snapshot.snapshot_id.clone(),
                        syntax_epoch: pending.epochs.syntax_epoch,
                        input_hash: pending.snapshot.input_hash.clone(),
                        backend: backend.clone(),
                        resolutions: vec![],
                        diagnostics: vec![],
                    },
                )
                .expect_err("cancelled refresh must not publish");
            eprintln!("prototype: {}: refresh interrupted", step.name);
            reports.push(
                json!({"name":step.name, "snapshot_id":pending.snapshot.snapshot_id,
                "epochs":pending.epochs, "syntax_ms":syntax_ms, "plan":plan_report,
                "interrupted":true, "published":false, "publication_rejected":rejected,
                "pending_units":pending.pending_units.len()}),
            );
            continue;
        };
        let backend_ms = semantic_started.elapsed().as_secs_f64() * 1000.0;
        batch.resolutions.extend(plan.reused);
        let verify_started = Instant::now();
        ensure_unchanged(&scratch, &workspace_id, &backend, &input.manifest)?;
        let verify_ms = verify_started.elapsed().as_secs_f64() * 1000.0;
        let current = coordinator
            .complete(&ticket, batch)
            .map_err(anyhow::Error::msg)?;
        let incremental_ms = semantic_started.elapsed().as_secs_f64() * 1000.0;
        let published = Instant::now();
        waiting.retain(|(epoch, since)| {
            let done = *epoch <= current.epochs.semantic_epoch;
            if done {
                pending_ms.push(published.duration_since(*since).as_secs_f64() * 1000.0);
            }
            !done
        });

        // A separate tree-sitter parser, coordinator AND LSP process form the cold path.
        let cold_started = Instant::now();
        let mut cold_parser = RustParser::new().map_err(anyhow::Error::msg)?;
        let mut cold_syntax = cold_parser
            .analyze(&input.manifest)
            .map_err(anyhow::Error::msg)?;
        cold_syntax.diagnostics.extend(input.diagnostics);
        let syntax_matches = cold_syntax == current.snapshot.syntax;
        let mut cold_coordinator = RefreshCoordinator::default();
        let cold_pending = cold_coordinator
            .publish_inputs(input.manifest, cold_syntax)
            .map_err(anyhow::Error::msg)?;
        let cold_plan = cold_coordinator
            .plan_refresh()
            .map_err(anyhow::Error::msg)?;
        let cold_ticket = cold_coordinator
            .begin_refresh()
            .map_err(anyhow::Error::msg)?;
        let deadline = cold_started + Duration::from_millis(timeout_ms);
        let mut cold_backend = lsp::LspClient::start(&executable, &scratch, deadline)?;
        let cold_batch = cold_backend
            .refresh(
                &cold_pending.snapshot,
                &backend,
                deadline,
                &cold_plan.query,
                &mut || false,
            )?
            .context("index_refresh_failed: cold refresh was cancelled")?;
        ensure_unchanged(
            &scratch,
            &workspace_id,
            &backend,
            &cold_pending.snapshot.manifest,
        )?;
        let cold = cold_coordinator
            .complete(&cold_ticket, cold_batch)
            .map_err(anyhow::Error::msg)?;
        drop(cold_backend);
        let cold_ms = cold_started.elapsed().as_secs_f64() * 1000.0;
        let (missing, extra, changed) = compare_relations(&current, &cold);
        let gaps_match = current.known_gaps == cold.known_gaps;
        let equal = syntax_matches
            && missing.is_empty()
            && extra.is_empty()
            && changed.is_empty()
            && gaps_match;
        let names: BTreeMap<_, _> = current
            .snapshot
            .syntax
            .symbols
            .iter()
            .map(|s| (s.symbol_id.as_str(), s.qualified_name.as_str()))
            .collect();
        let resolved_calls: Vec<_> = current.relations.iter().map(|r| json!({
            "caller":names.get(r.from_symbol.as_str()),"target":names.get(r.to_symbol.as_str()),
            "certainty":r.certainty,"derivation":r.evidence[0].derivation,"file":r.file,"span":r.span
        })).collect();
        eprintln!(
            "prototype: {}: {} relations; matches cold = {}",
            step.name,
            current.relations.len(),
            equal
        );
        reports.push(json!({"name":step.name, "snapshot_id":current.snapshot.snapshot_id,
            "epochs":current.epochs, "syntax_ms":syntax_ms,"incremental_semantic_ms":incremental_ms,
            "cold_ms":cold_ms, "semantic_phases_ms":{"backend":backend_ms,"verify_inputs":verify_ms,
                "publish":incremental_ms - backend_ms - verify_ms}, "plan":plan_report, "backend_restarts":restarts, "published":true,
            "relations":current.relations.len(),"callsites":current.snapshot.syntax.calls.len(),
            "matches_cold":equal,"syntax_matches":syntax_matches,"known_gaps_match":gaps_match,
            "missing_relation_ids":missing,"extra_relation_ids":extra,"changed_relation_ids":changed,
            "known_gaps":current.known_gaps,"resolved_calls":resolved_calls}));
    }
    let matched = reports
        .iter()
        .filter(|r| r["published"] == true)
        .all(|r| r["matches_cold"] == true);
    let report = json!({"schema_version":"0.1","stage":"prototype","backend":backend,
        "workspace":root,"edits_applied_to":"temporary_copy",
        "source_inputs_unchanged":workspace::capture(&root, &workspace_id, &backend)?.manifest == captured.manifest,
        "matches_cold":matched,
        "steps":reports,"pending_duration_ms":percentiles(pending_ms),"retained_pending_views":pinned.len(),
        "limitations":["Single persistent LSP probe; embedded ra_ap_* not compared", "Definition and trait-implementation queries only; no references refresh or inherited default-method dispatch", "Structural edits fall back to a full re-query; finer invalidation is not attempted", "Faults are injected between queries, not during an in-flight request", "Timings are local observations, not product performance claims"]});
    Ok(report)
}

fn validate(scenario: &Scenario) -> Result<()> {
    // Validate the entire scenario before launching any external tools.
    let last = scenario.steps.len().saturating_sub(1);
    for (index, step) in scenario.steps.iter().enumerate() {
        if step.fault == Some(Fault::InterruptRefresh) && index == last {
            bail!("invalid_argument: interrupt_refresh needs a later step to supersede it");
        }
        let mut paths = std::collections::BTreeSet::new();
        for edit in &step.edits {
            let path = workspace::safe_relative(&edit.path)?;
            let cargo = path.file_name().is_some_and(|name| name == "Cargo.toml");
            if !edit.path.ends_with(".rs") && !cargo {
                bail!("capability_unsupported: this probe accepts only .rs and Cargo.toml edits");
            }
            if edit.content.is_some() && edit.replace.is_some() {
                bail!(
                    "invalid_argument: use either content or replace: {}",
                    edit.path
                );
            }
            if cargo && edit.content.is_none() && edit.replace.is_none() {
                bail!("capability_unsupported: deleting a Cargo manifest is not modeled");
            }
            if !paths.insert(&edit.path) {
                bail!(
                    "invalid_argument: duplicate path in edit step: {}",
                    edit.path
                );
            }
        }
    }
    Ok(())
}

fn apply(scratch: &Path, edit: &Edit) -> Result<()> {
    let path = scratch.join(workspace::safe_relative(&edit.path)?);
    match (&edit.content, &edit.replace) {
        (Some(content), _) => {
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, content)?;
        }
        (None, Some(replace)) => {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("invalid_argument: cannot read {}", edit.path))?;
            if text.matches(&replace.old).count() != 1 {
                bail!(
                    "invalid_argument: replace.old must occur exactly once in {}",
                    edit.path
                );
            }
            fs::write(&path, text.replacen(&replace.old, &replace.new, 1))?;
        }
        (None, None) => fs::remove_file(&path)
            .with_context(|| format!("invalid_argument: cannot delete {}", edit.path))?,
    }
    Ok(())
}

fn ensure_unchanged(
    root: &Path,
    workspace_id: &str,
    backend: &str,
    expected: &Manifest,
) -> Result<()> {
    if workspace::capture(root, workspace_id, backend)?.manifest != *expected {
        bail!(
            "content_mismatch: semantic backend changed captured inputs (for example Cargo.lock); materialize those inputs before running the probe"
        );
    }
    Ok(())
}

fn compare_relations(
    current: &AnalysisView,
    cold: &AnalysisView,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let current: BTreeMap<_, _> = current
        .relations
        .iter()
        .map(|r| (&r.relation_id, r))
        .collect();
    let cold: BTreeMap<_, _> = cold.relations.iter().map(|r| (&r.relation_id, r)).collect();
    let missing = cold
        .keys()
        .filter(|id| !current.contains_key(*id))
        .map(|id| (*id).clone())
        .collect();
    let extra = current
        .keys()
        .filter(|id| !cold.contains_key(*id))
        .map(|id| (*id).clone())
        .collect();
    // evidence_id includes the snapshot identity; compare semantic dimensions.
    let changed = current
        .iter()
        .filter_map(|(id, relation)| {
            cold.get(id)
                .filter(|other| {
                    relation.certainty != other.certainty
                        || relation.from_symbol != other.from_symbol
                        || relation.to_symbol != other.to_symbol
                })
                .map(|_| (*id).clone())
        })
        .collect();
    (missing, extra, changed)
}

fn percentiles(mut values: Vec<f64>) -> Value {
    values.sort_by(f64::total_cmp);
    let at = |p: f64| values[((values.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    if values.is_empty() {
        return json!({"samples":0});
    }
    json!({"p50":at(0.50),"p95":at(0.95),"p99":at(0.99),"samples":values.len(),
        "method":"nearest_rank; edit applied to semantic publication; initial startup included"})
}
