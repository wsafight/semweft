use crate::{config::Resolved, engine, frontend::Frontends, workspace};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{process::Command, time::Instant};

pub use semweft_bench::recover;

pub fn capture(mut resolved: Resolved, revisions: u32) -> Result<semweft_bench::StorageInput> {
    resolved.config.index.dependency_depth = 0;
    let start = Instant::now();
    let captured = workspace::capture_config(&resolved, "not_enabled")?;
    let capture_ms = start.elapsed().as_secs_f64() * 1000.;
    let manifest = captured.manifest;
    let edited_file = manifest
        .files
        .keys()
        .find(|path| path.ends_with(".rs") || path.ends_with(".go") || crate::web::is_source(path))
        .cloned()
        .context("invalid_argument: benchmark needs a captured source file")?;
    Ok(semweft_bench::StorageInput {
        workspace: resolved.root,
        capture_ms,
        manifest,
        edited_file,
        revisions,
    })
}

pub fn run_one(input: semweft_bench::StorageInput) -> Result<Value> {
    let mut parser = Frontends::new().map_err(anyhow::Error::msg)?;
    let mut deriver = engine::RelationDeriver::default();
    semweft_bench::run_storage(
        input,
        |manifest| parser.analyze(manifest).map_err(anyhow::Error::msg),
        |view| deriver.derive(view),
        |index, workspace_id| {
            let child = Command::new(std::env::current_exe()?)
                .arg("__benchmark-recovery")
                .arg(index)
                .arg(workspace_id)
                .output()?;
            ensure!(
                child.status.success(),
                "index_store_error: benchmark recovery failed: {} {}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
            Ok(serde_json::from_slice(&child.stdout)?)
        },
    )
}

pub fn run(
    input: semweft_bench::StorageInput,
    runs: u32,
    save: Option<&std::path::Path>,
    baseline: Option<&std::path::Path>,
) -> Result<Value> {
    if let Some(path) = save {
        semweft_bench::save_input(path, &input)?;
    }
    let baseline = baseline
        .map(|path| -> Result<Value> { Ok(serde_json::from_slice(&std::fs::read(path)?)?) })
        .transpose()?;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("input.json");
    semweft_bench::save_input(&path, &input)?;
    semweft_bench::run_suite(
        &input,
        runs,
        || {
            let child = Command::new(std::env::current_exe()?)
                .arg("__benchmark-run")
                .arg(&path)
                .output()?;
            ensure!(
                child.status.success(),
                "index_store_error: benchmark trial failed: {} {}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
            Ok(serde_json::from_slice(&child.stdout)?)
        },
        baseline.as_ref(),
    )
}
