use crate::model::{Relation, Symbol};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{path::Path, process::Command};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Navigation {
    pub target: Symbol,
    pub nodes: Vec<Symbol>,
    pub relations: Vec<Relation>,
    pub output: Value,
}

pub struct Query {
    pub symbol: String,
    pub snapshot: String,
    pub epoch: u64,
    pub direction: String,
    pub depth: usize,
    pub references: bool,
}

#[derive(Clone)]
pub struct PositionQuery {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub file_hash: String,
    pub snapshot: String,
    pub epoch: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Definition {
    #[serde(flatten)]
    pub symbol: Symbol,
    pub certainty: String,
    pub evidence: Vec<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Definitions {
    pub symbols: Vec<Definition>,
    pub occurrence: Value,
    pub completeness: Value,
    pub output: Value,
}

fn pinned_command(repository: &Path, workspace: &Path, snapshot: &str, epoch: u64) -> Command {
    let mut command = Command::new(repository.join("target/debug/semweft"));
    command
        .env(
            "SEMWEFT_CACHE_DIR",
            repository.join("target/desktop-preview/cache"),
        )
        .env(
            "SEMWEFT_CONFIG_DIR",
            repository.join("target/desktop-preview/config"),
        )
        .arg("--workspace")
        .arg(workspace)
        .args([
            "--format",
            "json",
            "--timeout-ms",
            "60000",
            "--limit",
            "1000",
            "--snapshot-id",
            snapshot,
            "--semantic-epoch",
            &epoch.to_string(),
        ]);
    command
}

fn pinned_output(command: &mut Command, snapshot: &str, epoch: u64) -> Result<Value> {
    let output = command.output().context("无法调用本地 Semweft 引擎")?;
    let value: Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "引擎未返回 JSON：{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    if !output.status.success() || value.get("error").is_some() {
        bail!("{}", value.get("error").unwrap_or(&value));
    }
    ensure!(
        value["snapshot_id"].as_str() == Some(snapshot),
        "引擎返回了不同的快照"
    );
    ensure!(
        value["epochs"]["semantic"].as_u64() == Some(epoch),
        "引擎返回了不同的语义版本"
    );
    Ok(value)
}

pub fn definitions(
    repository: &Path,
    workspace: &Path,
    query: &PositionQuery,
) -> Result<Definitions> {
    ensure!(!query.file_hash.is_empty(), "缺少捕获源码的内容哈希");
    let mut result = Definitions::default();
    let mut cursor = None;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let mut command = pinned_command(repository, workspace, &query.snapshot, query.epoch);
        if let Some(cursor) = &cursor {
            command.arg("--cursor").arg(cursor);
        }
        command
            .arg("symbol-at")
            .arg(format!("{}:{}:{}", query.file, query.line, query.column))
            .arg("--file-hash")
            .arg(&query.file_hash);
        let value = pinned_output(&mut command, &query.snapshot, query.epoch)?;
        let page: Definitions = serde_json::from_value(value.clone())?;
        result.symbols.extend(page.symbols);
        result.occurrence = page.occurrence;
        result.completeness = page.completeness;
        result.output = page.output;
        cursor = value["output"]["continuation"]["next_cursor"]
            .as_str()
            .map(str::to_owned);
        let Some(ref next) = cursor else {
            break;
        };
        ensure!(seen.insert(next.clone()), "引擎重复了定义分页游标");
    }
    // Multiple relations can resolve to the same declaration. Preserve all evidence.
    let mut candidates = std::collections::BTreeMap::<String, Definition>::new();
    for candidate in result.symbols {
        let existing = candidates
            .entry(candidate.symbol.symbol_id.clone())
            .or_insert_with(|| candidate.clone());
        if candidate.certainty == "definite" {
            existing.certainty = candidate.certainty;
        }
        for evidence in candidate.evidence {
            if !existing.evidence.contains(&evidence) {
                existing.evidence.push(evidence);
            }
        }
    }
    result.symbols = candidates.into_values().collect();
    Ok(result)
}

pub fn navigate(repository: &Path, workspace: &Path, query: &Query) -> Result<Navigation> {
    let mut cursor = None;
    let mut result = Navigation::default();
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let mut command = pinned_command(repository, workspace, &query.snapshot, query.epoch);
        if let Some(cursor) = &cursor {
            command.arg("--cursor").arg(cursor);
        }
        command
            .arg(if query.references { "refs" } else { "calls" })
            .arg(&query.symbol);
        if !query.references {
            command.args([
                "--direction",
                &query.direction,
                "--depth",
                &query.depth.to_string(),
                "--max-nodes",
                "5000",
            ]);
        }
        let value = pinned_output(&mut command, &query.snapshot, query.epoch)?;
        let page: Navigation = serde_json::from_value(value.clone())?;
        if cursor.is_none() {
            result.target = page.target;
            result.nodes = page.nodes;
        }
        result.relations.extend(page.relations);
        result.output = page.output;
        cursor = value["output"]["continuation"]["next_cursor"]
            .as_str()
            .map(str::to_owned);
        let Some(ref next) = cursor else {
            break;
        };
        ensure!(seen.insert(next.clone()), "引擎重复了分页游标");
    }
    Ok(result)
}
