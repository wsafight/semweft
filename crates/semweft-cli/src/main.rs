use semweft_query::{config, dependencies, engine, frontend, git, lsp, query, web, workspace};
mod benchmark;
mod daemon;
mod desktop;
mod hooks;
mod mcp;
mod prototype;
mod render;
mod reports;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use render::Format;
use serde_json::{Value, json};
use std::{io::IsTerminal, path::PathBuf};

#[derive(Parser)]
#[command(
    name = "semweft",
    version,
    about = "Local code evidence and candidate impact engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true, value_enum)]
    format: Option<Format>,
    #[arg(long, global = true)]
    snapshot_id: Option<String>,
    #[arg(long, global = true)]
    semantic_epoch: Option<u64>,
    #[arg(long, global = true)]
    require_fresh: bool,
    #[arg(long, global = true)]
    fresh_wait_ms: Option<u64>,
    #[arg(long,global=true,value_parser=clap::value_parser!(u64).range(1..=60000))]
    timeout_ms: Option<u64>,
    #[arg(long,global=true,value_parser=clap::value_parser!(u64).range(1..=1000))]
    limit: Option<u64>,
    #[arg(long, global = true)]
    cursor: Option<String>,
    #[arg(long, global = true, conflicts_with = "cursor")]
    all_pages: bool,
    #[arg(long, global = true, conflicts_with = "cursor")]
    risk_inputs: Option<PathBuf>,
    #[arg(long, global = true, conflicts_with = "cursor")]
    base_risk_inputs: Option<PathBuf>,
    #[arg(long, global = true, value_delimiter = ',')]
    source: Vec<String>,
    #[arg(long, global = true, value_delimiter = ',')]
    certainty: Vec<String>,
    #[arg(long, global = true, value_delimiter = ',')]
    freshness: Vec<String>,
    #[arg(long, global = true)]
    max_chars: Option<usize>,
    #[arg(long,global=true,default_value="relevant",value_parser=["relevant","all"])]
    gaps: String,
}
#[derive(Subcommand)]
enum Command {
    /// Freeze source-bound LCOV observations and/or a read-only Git history window.
    RiskInputs {
        #[arg(long, requires = "source_manifest")]
        coverage: Option<PathBuf>,
        #[arg(long, requires = "coverage")]
        source_manifest: Option<PathBuf>,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=1000))]
        history_commits: Option<u32>,
    },
    /// Measure real captured inputs in a temporary index, without editing the workspace.
    BenchmarkStorage {
        #[arg(long, value_parser=clap::value_parser!(u32).range(1..=1000))]
        revisions: Option<u32>,
        #[arg(long, default_value_t = 1, value_parser=clap::value_parser!(u32).range(1..=100))]
        runs: u32,
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long)]
        save_input: Option<PathBuf>,
        #[arg(long)]
        baseline: Option<PathBuf>,
    },
    #[command(name = "__benchmark-run", hide = true)]
    BenchmarkRun {
        input: PathBuf,
    },
    #[command(name = "__benchmark-recovery", hide = true)]
    BenchmarkRecovery {
        index: PathBuf,
        workspace_id: String,
    },
    /// Installed tools and limits; does not index or execute a build.
    Capabilities,
    /// Analyze Protobuf changes and explicitly mapped generated-code consumers.
    ContractImpact {
        #[arg(long)]
        diff: Option<String>,
        #[arg(long)]
        depth: Option<u64>,
        #[arg(long)]
        max_nodes: Option<u64>,
    },
    /// Save or retrieve a local human review bound to immutable analysis evidence.
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    /// Adapt an explicit Claude Code or Codex hook JSON event from stdin.
    Hook {
        #[arg(long,value_parser=["claude","codex"])]
        host: String,
    },
    /// Before/after-edit agent guardrails; no build or test execution.
    Guard {
        #[command(subcommand)]
        command: GuardCommand,
    },
    /// Check layer dependency directions and optionally compare or save a baseline.
    ArchCheck {
        #[arg(long)]
        diff: Option<String>,
        #[arg(long)]
        baseline: Option<PathBuf>,
        #[arg(long, conflicts_with = "cursor")]
        write_baseline: Option<PathBuf>,
    },
    /// Render a saved verification JSON response without indexing the workspace.
    Report {
        #[arg(long)]
        input: PathBuf,
    },
    /// Write initial configuration without overwriting existing files.
    Init {
        #[arg(long)]
        accept_build_execution: bool,
    },
    /// Grant or revoke authorization for Cargo scripts and procedural macros.
    Trust {
        #[arg(long, conflicts_with = "revoke")]
        accept_build_execution: bool,
        #[arg(long)]
        revoke: bool,
    },
    /// Rebuild the shared index; semantic indexing requires prior trust.
    Index {
        #[arg(long)]
        syntax_only: bool,
        #[arg(long)]
        watch: bool,
        /// Import an existing binary SCIP index; does not execute its producer.
        #[arg(long, conflicts_with = "syntax_only", requires = "scip_manifest")]
        scip: Option<PathBuf>,
        #[arg(long, requires = "scip")]
        scip_manifest: Option<PathBuf>,
        /// Save the captured input hashes for a subsequent SCIP producer run.
        #[arg(long)]
        export_manifest: Option<PathBuf>,
    },
    /// Pin a durable version, release a pin, or collect expired history.
    Snapshots {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    Status,
    Doctor,
    Map {
        #[arg(long)]
        cycles: bool,
        #[arg(long, alias = "path-prefix")]
        path: Option<String>,
        #[arg(long)]
        depth: Option<u64>,
        #[arg(long)]
        max_symbols_per_module: Option<u64>,
        #[arg(long,value_parser=["crate","module","service","none"])]
        dependencies: Option<String>,
    },
    Search {
        query: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long="match",value_parser=["fuzzy","exact","prefix"])]
        match_mode: Option<String>,
        #[arg(long,value_parser=["public","restricted","private","all"])]
        visibility: Option<String>,
        #[arg(long,value_parser=["declarations","external"])]
        api_scope: Option<String>,
    },
    Symbol {
        symbol: Option<String>,
        #[arg(long)]
        include_body: bool,
        #[arg(long)]
        no_docs: bool,
    },
    SymbolAt {
        location: Option<String>,
        #[arg(long)]
        file_hash: Option<String>,
    },
    Refs {
        symbol: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        #[arg(long)]
        include_stale: bool,
        #[arg(long)]
        stale_from: Option<String>,
        #[arg(long)]
        stale_semantic_epoch: Option<u64>,
    },
    /// Indexed implementations of a Rust trait or trait method.
    Impls {
        symbol: Option<String>,
        #[arg(long)]
        path: Option<String>,
    },
    Calls {
        symbol: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long,value_parser=["callers","callees","both"])]
        direction: Option<String>,
        #[arg(long)]
        depth: Option<u64>,
        #[arg(long)]
        max_nodes: Option<u64>,
    },
    /// Bounded simple call paths between two indexed symbols.
    Path {
        from: Option<String>,
        to: Option<String>,
        #[arg(long,value_parser=["symbol","module","service"])]
        scope: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        max_depth: Option<u64>,
        #[arg(long)]
        max_paths: Option<u64>,
        #[arg(long)]
        max_nodes: Option<u64>,
    },
    Impact {
        symbol: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        diff: Option<String>,
        #[arg(long)]
        depth: Option<u64>,
        #[arg(long)]
        max_nodes: Option<u64>,
    },
    /// Recommend test candidates from indexed impact paths without executing tests.
    Tests {
        symbol: Option<String>,
        #[arg(long)]
        suggested: bool,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        diff: Option<String>,
        #[arg(long)]
        depth: Option<u64>,
        #[arg(long)]
        max_nodes: Option<u64>,
    },
    /// Verify an explicit diff using the installed built-in review rules.
    Verify {
        #[arg(long)]
        diff: Option<String>,
        #[arg(long)]
        depth: Option<u64>,
        #[arg(long)]
        max_nodes: Option<u64>,
    },
    Context {
        symbol: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long,value_parser=parse_budget)]
        budget: Option<u64>,
        #[arg(long)]
        tokenizer: Option<String>,
        #[arg(long, value_delimiter = ',')]
        include: Vec<String>,
    },
    /// MCP stdio bridge to the shared local daemon.
    Mcp,
    #[command(name = "__daemon", hide = true)]
    Daemon,
    #[command(name = "__desktop", hide = true)]
    Desktop,
    #[command(name = "__stop", hide = true)]
    Stop,
    /// Semantic-refresh consistency experiments in a temporary workspace copy.
    Prototype {
        #[command(subcommand)]
        command: Prototype,
    },
}
#[derive(Subcommand)]
enum Prototype {
    Scan,
    Check {
        #[arg(long)]
        edits: Option<PathBuf>,
        #[arg(long, default_value = "rust-analyzer")]
        rust_analyzer: PathBuf,
        #[arg(long)]
        accept_build_execution: bool,
    },
}
fn parse_budget(text: &str) -> std::result::Result<u64, String> {
    let (number, multiplier) =
        if let Some(n) = text.strip_suffix('k').or_else(|| text.strip_suffix('K')) {
            (n, 1000)
        } else {
            (text, 1)
        };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|n| *n > 0)
        .ok_or_else(|| "budget must be positive, optionally ending in k".into())
}
fn put(args: &mut Value, key: &str, value: Option<impl serde::Serialize>) {
    if let Some(value) = value {
        args[key] = serde_json::to_value(value).unwrap();
    }
}
fn common(cli: &Cli, management: bool) -> Value {
    let mut args = json!({"timeout_ms":cli.timeout_ms.unwrap_or(5000)});
    put(&mut args, "snapshot_id", cli.snapshot_id.as_ref());
    if !management {
        put(&mut args, "semantic_epoch", cli.semantic_epoch);
        if cli.require_fresh {
            args["require_fresh"] = json!(true);
        }
        put(&mut args, "fresh_wait_ms", cli.fresh_wait_ms);
        put(&mut args, "limit", cli.limit);
        put(&mut args, "cursor", cli.cursor.as_ref());
        if !cli.source.is_empty() {
            args["source"] = json!(cli.source);
        }
        if !cli.certainty.is_empty() {
            args["certainty"] = json!(cli.certainty);
        }
        if !cli.freshness.is_empty() {
            args["freshness"] = json!(cli.freshness);
        }
    }
    args
}

#[derive(Subcommand)]
enum SnapshotCommand {
    /// Physical disk usage by object kind and metadata.
    Usage,
    /// Re-encode legacy objects atomically, retaining their identities.
    Compact,
    Pin {
        snapshot: String,
        #[arg(long)]
        epoch: Option<u64>,
    },
    Unpin {
        snapshot: String,
        #[arg(long)]
        epoch: Option<u64>,
    },
    Gc {
        #[arg(long, default_value_t = 10)]
        keep_last: usize,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        max_bytes: Option<u64>,
    },
}

#[derive(Subcommand)]
enum GuardCommand {
    Before {
        #[arg(long = "file")]
        files: Vec<String>,
        #[arg(long = "symbol")]
        symbols: Vec<String>,
        #[arg(long)]
        depth: Option<u64>,
        #[arg(long)]
        max_nodes: Option<u64>,
    },
    After {
        #[arg(long)]
        diff: Option<String>,
        #[arg(long)]
        baseline: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ReviewCommand {
    Create {
        #[arg(long, default_value = "WORKTREE")]
        diff: String,
        #[arg(long, default_value = "变更审查")]
        title: String,
        #[arg(long)]
        open: bool,
    },
    Open {
        review_id: String,
    },
    Decide {
        review_id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        item_id: Option<String>,
        #[arg(long, value_parser = ["approve", "reject", "comment", "accept", "dismiss"])]
        conclusion: String,
        #[arg(long)]
        reviewer: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    Wait {
        review_id: String,
        #[arg(long, default_value_t = 0)]
        after_revision: u64,
        #[arg(long, default_value_t = 30000, value_parser = clap::value_parser!(u64).range(1..=60000))]
        wait_ms: u64,
    },
    Save {
        #[arg(long)]
        input: PathBuf,
        #[arg(long,value_parser=["approve","reject","comment"])]
        conclusion: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    Get {
        review_id: String,
    },
}

async fn run(cli: &Cli) -> Result<Option<Value>> {
    if (cli.risk_inputs.is_some() || cli.base_risk_inputs.is_some())
        && !matches!(
            cli.command,
            Command::Impact { .. }
                | Command::Verify { .. }
                | Command::Tests { .. }
                | Command::Guard { .. }
                | Command::ArchCheck { .. }
                | Command::ContractImpact { .. }
        )
    {
        bail!("invalid_argument: risk inputs require a diff report or preflight command");
    }
    if let Command::Hook { host } = &cli.command {
        use std::io::Read;
        if cli
            .format
            .is_some_and(|format| !matches!(format, Format::Json))
        {
            bail!("invalid_argument: hook output must be JSON");
        }
        let payload: Value = serde_json::from_reader(std::io::stdin().lock().take(1_048_576))
            .context("invalid_argument: invalid hook JSON")?;
        return Ok(Some(hooks::run(
            &payload,
            host,
            cli.workspace.as_deref(),
            cli.config.as_deref(),
        )?));
    }
    if cli.all_pages
        && !matches!(
            cli.command,
            Command::Verify { .. }
                | Command::Tests { .. }
                | Command::Impact { .. }
                | Command::ArchCheck { .. }
                | Command::Guard { .. }
                | Command::Map { .. }
                | Command::Search { .. }
                | Command::Calls { .. }
                | Command::Path { .. }
                | Command::Refs { .. }
                | Command::Impls { .. }
                | Command::ContractImpact { .. }
        )
    {
        bail!("invalid_argument: all-pages requires a paginated fact or verification query");
    }
    if let Command::Report { input } = &cli.command {
        let mut report = reports::load(input)?;
        if matches!(cli.format, Some(Format::Sarif)) {
            reports::set_source_root(
                &mut report,
                cli.workspace
                    .as_deref()
                    .unwrap_or(std::path::Path::new(".")),
            )?;
        }
        return Ok(Some(report));
    }
    if matches!(cli.format, Some(Format::Sarif))
        && !matches!(cli.command, Command::Verify { .. } | Command::Report { .. })
    {
        bail!("invalid_argument: SARIF is supported for verify and saved verification reports");
    }
    if let Command::Prototype { command } = &cli.command {
        let root = cli.workspace.clone().unwrap_or_else(|| ".".into());
        return Ok(Some(match command {
            Prototype::Scan => prototype::scan(&root)?,
            Prototype::Check {
                edits,
                rust_analyzer,
                accept_build_execution,
            } => {
                if !accept_build_execution {
                    bail!(
                        "trust_required: L3 may execute Cargo scripts and macros; pass --accept-build-execution after reviewing the workspace"
                    );
                }
                prototype::check(
                    &root,
                    edits.as_deref(),
                    rust_analyzer,
                    cli.timeout_ms.unwrap_or(30000),
                )?
            }
        }));
    }
    if matches!(cli.command, Command::Capabilities) {
        return Ok(Some(query::capabilities()));
    }
    if let Command::BenchmarkRecovery {
        index,
        workspace_id,
    } = &cli.command
    {
        return Ok(Some(benchmark::recover(index, workspace_id)?));
    }
    if let Command::BenchmarkRun { input } = &cli.command {
        return Ok(Some(benchmark::run_one(semweft_bench::load_input(input)?)?));
    }
    if let Command::BenchmarkStorage {
        revisions,
        runs,
        input,
        save_input,
        baseline,
    } = &cli.command
    {
        let mut input = if let Some(path) = input {
            semweft_bench::load_input(path)?
        } else {
            benchmark::capture(
                config::resolve(cli.workspace.as_deref(), cli.config.as_deref())?,
                revisions.unwrap_or(10),
            )?
        };
        if let Some(revisions) = revisions {
            input.revisions = *revisions;
        }
        return Ok(Some(benchmark::run(
            input,
            *runs,
            save_input.as_deref(),
            baseline.as_deref(),
        )?));
    }
    let resolved = config::resolve(cli.workspace.as_deref(), cli.config.as_deref())?;
    match &cli.command {
        Command::RiskInputs {
            coverage,
            source_manifest,
            history_commits,
        } => {
            let coverage = coverage.as_deref().zip(source_manifest.as_deref());
            let artifact = semweft_query::risk_inputs::collect(
                &resolved,
                coverage,
                *history_commits,
                std::time::Instant::now()
                    + std::time::Duration::from_millis(cli.timeout_ms.unwrap_or(60000)),
            )?;
            return Ok(Some(serde_json::to_value(artifact)?));
        }
        Command::Review {
            command: ReviewCommand::Create { diff, title, open },
        } => {
            let report = daemon::call(
                &resolved,
                "agent_verify",
                json!({"diff":git::diff_selectors(&resolved.root, diff)?,"limit":1000,"depth":5,"max_nodes":5000,"timeout_ms":cli.timeout_ms.unwrap_or(60000)}),
            )?;
            if report.get("error").is_some() {
                return Ok(Some(report));
            }
            let report = reports::collect(&resolved, "agent_verify", report)?;
            let request = daemon::call(
                &resolved,
                "__review_request",
                json!({"action":"create","report":report,"title":title}),
            )?;
            if *open && let Some(id) = request["review_id"].as_str() {
                desktop::open_review(&resolved, id)?;
            }
            return Ok(Some(request));
        }
        Command::Review {
            command: ReviewCommand::Open { review_id },
        } => {
            let request = daemon::call(
                &resolved,
                "get_review_result",
                json!({"review_id":review_id}),
            )?;
            if request.get("error").is_none() {
                desktop::open_review(&resolved, review_id)?;
            }
            return Ok(Some(request));
        }
        Command::Review {
            command:
                ReviewCommand::Decide {
                    review_id,
                    revision,
                    item_id,
                    conclusion,
                    reviewer,
                    note,
                },
        } => {
            return Ok(Some(daemon::call(
                &resolved,
                "__review_request",
                json!({"action":"decide","review_id":review_id,"revision":revision,"item_id":item_id,"conclusion":conclusion,"reviewer":reviewer,"note":note}),
            )?));
        }
        Command::Review {
            command:
                ReviewCommand::Wait {
                    review_id,
                    after_revision,
                    wait_ms,
                },
        } => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(*wait_ms);
            loop {
                let mut value = daemon::call(
                    &resolved,
                    "get_review_result",
                    json!({"review_id":review_id,"timeout_ms":60000}),
                )?;
                let changed = value["review"]["revision"]
                    .as_u64()
                    .is_some_and(|revision| revision > *after_revision)
                    || value["review"]["state"] == "superseded"
                    || value.get("error").is_some();
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if changed || remaining.is_zero() {
                    value["wait"] = json!({"changed":changed,"after_revision":after_revision});
                    return Ok(Some(value));
                }
                tokio::time::sleep(remaining.min(std::time::Duration::from_millis(250))).await;
            }
        }
        Command::Review {
            command:
                ReviewCommand::Save {
                    input,
                    conclusion,
                    note,
                },
        } => {
            return Ok(Some(semweft_query::reviews::save(
                &resolved,
                reports::load(input)?,
                conclusion,
                note,
            )?));
        }
        Command::Daemon => {
            daemon::serve(resolved)?;
            return Ok(None);
        }
        Command::Desktop => return Ok(Some(desktop::bootstrap(&resolved)?)),
        Command::Snapshots { command } => {
            let args = match command {
                SnapshotCommand::Compact => json!({"action":"compact"}),
                SnapshotCommand::Pin { snapshot, epoch } => {
                    json!({"action":"pin","snapshot_id":snapshot,"semantic_epoch":epoch})
                }
                SnapshotCommand::Unpin { snapshot, epoch } => {
                    json!({"action":"unpin","snapshot_id":snapshot,"semantic_epoch":epoch})
                }
                SnapshotCommand::Usage => json!({"action":"usage"}),
                SnapshotCommand::Gc {
                    keep_last,
                    dry_run,
                    max_bytes,
                } => {
                    json!({"action":"gc","keep_last":keep_last,"dry_run":dry_run,"max_bytes":max_bytes})
                }
            };
            return Ok(Some(daemon::call(&resolved, "__snapshots", args)?));
        }
        Command::Stop => return Ok(Some(daemon::stop(&resolved)?)),
        Command::Mcp => {
            mcp::serve(resolved).await?;
            return Ok(None);
        }
        Command::Init {
            accept_build_execution,
        } => {
            let mut value = resolved.init()?;
            if *accept_build_execution {
                value["trust"] = resolved.trust(false)?["trust"].clone();
            }
            return Ok(Some(value));
        }
        Command::Trust {
            accept_build_execution,
            revoke,
        } => {
            if !accept_build_execution && !revoke {
                bail!(
                    "trust_required: specify --accept-build-execution to authorize Cargo scripts and macros, or --revoke"
                );
            }
            let result = resolved.trust(*revoke)?;
            if resolved.cache.join("daemon.lock").exists() {
                let updated = daemon::call(&resolved, "__trust_changed", json!({}))?;
                if updated.get("error").is_some() {
                    return Ok(Some(updated));
                }
            }
            return Ok(Some(result));
        }
        Command::Index {
            syntax_only,
            watch,
            scip,
            scip_manifest,
            export_manifest,
        } => {
            let lease = if *watch {
                Some(daemon::lease(&resolved)?)
            } else {
                None
            };
            let result = daemon::call(
                &resolved,
                "index",
                json!({"syntax_only":syntax_only,"timeout_ms":cli.timeout_ms.unwrap_or(60000),
                    "scip":scip.as_ref().map(|p|p.canonicalize()).transpose()?.map(|p|p.to_string_lossy().into_owned()),
                    "scip_manifest":scip_manifest.as_ref().map(|p|p.canonicalize()).transpose()?.map(|p|p.to_string_lossy().into_owned()),
                    "export_manifest":export_manifest.is_some()}),
            )?;
            if let Some(path) = export_manifest
                && let Some(manifest) = result.get("source_manifest")
            {
                std::fs::write(path, serde_json::to_vec_pretty(manifest)?)?;
            }
            if result.get("error").is_some() || !*watch {
                return Ok(Some(result));
            }
            eprintln!(
                "Index ready; watching {}. Ctrl-C releases this client.",
                resolved.root.display()
            );
            let format = cli.format.unwrap_or_else(|| {
                if std::io::stdout().is_terminal() {
                    Format::Text
                } else {
                    Format::Json
                }
            });
            println!(
                "{}",
                render::render(&result, format, cli.max_chars, cli.gaps == "all")?
            );
            let _lease = lease;
            tokio::signal::ctrl_c().await?;
            std::process::exit(130);
        }
        _ => {}
    }
    let mut args = common(cli, matches!(cli.command, Command::Status));
    let tool = match &cli.command {
        Command::ContractImpact {
            diff,
            depth,
            max_nodes,
        } => {
            if let Some(diff) = diff {
                args["diff"] = git::diff_selectors(&resolved.root, diff)?;
            }
            put(&mut args, "depth", *depth);
            put(&mut args, "max_nodes", *max_nodes);
            "contract_impact"
        }
        Command::Review {
            command: ReviewCommand::Get { review_id },
        } => {
            args["review_id"] = json!(review_id);
            "get_review_result"
        }
        Command::Guard { command } => match command {
            GuardCommand::Before {
                files,
                symbols,
                depth,
                max_nodes,
            } => {
                if !files.is_empty() {
                    args["files"] = json!(files);
                }
                if !symbols.is_empty() {
                    args["symbols"] = json!(symbols);
                }
                put(&mut args, "depth", *depth);
                put(&mut args, "max_nodes", *max_nodes);
                "agent_preflight"
            }
            GuardCommand::After { diff, baseline } => {
                if let Some(diff) = diff {
                    args["diff"] = git::diff_selectors(&resolved.root, diff)?;
                }
                if let Some(path) = baseline {
                    args["baseline"] = serde_json::from_slice(&std::fs::read(path)?)?;
                }
                "agent_verify"
            }
        },
        Command::ArchCheck { diff, baseline, .. } => {
            if let Some(diff) = diff {
                args["diff"] = git::diff_selectors(&resolved.root, diff)?;
            }
            if let Some(path) = baseline {
                args["baseline"] = serde_json::from_slice(&std::fs::read(path)?)?;
            }
            "arch_check"
        }
        Command::Status => "index_status",
        Command::Doctor => "repo_map",
        Command::Map {
            cycles,
            path,
            depth,
            max_symbols_per_module,
            dependencies,
        } => {
            put(&mut args, "path_prefix", path.as_ref());
            put(&mut args, "depth", *depth);
            put(&mut args, "max_symbols_per_module", *max_symbols_per_module);
            put(&mut args, "dependencies", dependencies.as_ref());
            if *cycles {
                args["cycles"] = json!(true);
            }
            if cli
                .format
                .is_some_and(|f| matches!(f, Format::Text | Format::Markdown))
                || (cli.format.is_none() && std::io::stdout().is_terminal())
            {
                put(
                    &mut args,
                    "max_symbols_per_module",
                    Some(max_symbols_per_module.unwrap_or(0)),
                );
            }
            "repo_map"
        }
        Command::Search {
            query,
            kind,
            path,
            match_mode,
            visibility,
            api_scope,
        } => {
            put(&mut args, "query", query.as_ref());
            if query.is_none() && (visibility.is_some() || api_scope.is_some()) {
                args["query"] = json!("*");
            }
            put(&mut args, "kind", kind.as_ref());
            put(&mut args, "path_prefix", path.as_ref());
            put(&mut args, "match", match_mode.as_ref());
            put(&mut args, "visibility", visibility.as_ref());
            put(&mut args, "api_scope", api_scope.as_ref());
            "search_symbols"
        }
        Command::Symbol {
            symbol,
            include_body,
            no_docs,
        } => {
            put(&mut args, "symbol", symbol.as_ref());
            if *include_body {
                args["include_body"] = json!(true);
            }
            if *no_docs {
                args["include_docs"] = json!(false);
            }
            "get_symbol"
        }
        Command::SymbolAt {
            location,
            file_hash,
        } => {
            if let Some(location) = location {
                let parts = location.rsplitn(3, ':').collect::<Vec<_>>();
                if parts.len() != 3 {
                    bail!("invalid_argument: location must be file:line:column");
                }
                args["file"] = json!(parts[2]);
                args["line"] = json!(
                    parts[1]
                        .parse::<u64>()
                        .map_err(|_| anyhow::anyhow!("invalid_argument: invalid line"))?
                );
                args["column"] = json!(
                    parts[0]
                        .parse::<u64>()
                        .map_err(|_| anyhow::anyhow!("invalid_argument: invalid column"))?
                );
            }
            put(&mut args, "file_hash", file_hash.as_ref());
            "symbol_at"
        }
        Command::Refs {
            symbol,
            path,
            kinds,
            include_stale,
            stale_from,
            stale_semantic_epoch,
        } => {
            put(&mut args, "symbol", symbol.as_ref());
            put(&mut args, "path_prefix", path.as_ref());
            if !kinds.is_empty() {
                args["kinds"] = json!(kinds);
            }
            if *include_stale {
                args["include_stale"] = json!(true);
            }
            if let Some(id) = stale_from {
                let epoch = stale_semantic_epoch
                    .context("invalid_argument: --stale-from requires --stale-semantic-epoch")?;
                args["stale_from"] = json!({"snapshot_id":id,"semantic_epoch":epoch});
            } else if stale_semantic_epoch.is_some() {
                bail!("invalid_argument: --stale-semantic-epoch requires --stale-from");
            }
            "find_references"
        }
        Command::Impls { symbol, path } => {
            put(&mut args, "symbol", symbol.as_ref());
            put(&mut args, "path_prefix", path.as_ref());
            "find_implementations"
        }
        Command::Calls {
            symbol,
            path,
            direction,
            depth,
            max_nodes,
        } => {
            put(&mut args, "symbol", symbol.as_ref());
            put(&mut args, "path_prefix", path.as_ref());
            put(&mut args, "direction", direction.as_ref());
            put(&mut args, "depth", *depth);
            put(&mut args, "max_nodes", *max_nodes);
            "call_graph"
        }
        Command::Path {
            from,
            to,
            scope,
            path,
            max_depth,
            max_paths,
            max_nodes,
        } => {
            put(&mut args, "from", from.as_ref());
            put(&mut args, "to", to.as_ref());
            put(&mut args, "scope", scope.as_ref());
            put(&mut args, "path_prefix", path.as_ref());
            put(&mut args, "max_depth", *max_depth);
            put(&mut args, "max_paths", *max_paths);
            put(&mut args, "max_nodes", *max_nodes);
            "find_paths"
        }
        Command::Impact {
            symbol,
            path,
            diff,
            depth,
            max_nodes,
        } => {
            put(&mut args, "symbol", symbol.as_ref());
            put(&mut args, "path_prefix", path.as_ref());
            if let Some(diff) = diff {
                args["diff"] = git::diff_selectors(&resolved.root, diff)?;
            }
            put(&mut args, "depth", *depth);
            put(&mut args, "max_nodes", *max_nodes);
            "impact_of"
        }
        Command::Verify {
            diff,
            depth,
            max_nodes,
        } => {
            if let Some(diff) = diff {
                args["diff"] = git::diff_selectors(&resolved.root, diff)?;
            }
            put(&mut args, "depth", *depth);
            put(&mut args, "max_nodes", *max_nodes);
            "verify_change"
        }
        Command::Tests {
            symbol,
            path,
            diff,
            depth,
            max_nodes,
            ..
        } => {
            put(&mut args, "symbol", symbol.as_ref());
            put(&mut args, "path_prefix", path.as_ref());
            if let Some(diff) = diff {
                args["diff"] = git::diff_selectors(&resolved.root, diff)?;
            }
            put(&mut args, "depth", *depth);
            put(&mut args, "max_nodes", *max_nodes);
            "suggest_tests"
        }
        Command::Context {
            symbol,
            task,
            budget,
            tokenizer,
            include,
        } => {
            put(&mut args, "symbol", symbol.as_ref());
            put(&mut args, "task", task.as_ref());
            put(&mut args, "budget", *budget);
            if cli.cursor.is_none() || tokenizer.is_some() {
                put(
                    &mut args,
                    "tokenizer",
                    Some(
                        tokenizer
                            .as_deref()
                            .unwrap_or(&resolved.config.context.tokenizer),
                    ),
                );
            }
            if !include.is_empty() {
                args["include"] = json!(include);
            }
            "context_pack"
        }
        _ => unreachable!(),
    };
    for (side, path) in [("head", &cli.risk_inputs), ("base", &cli.base_risk_inputs)] {
        if let Some(path) = path {
            let artifact = semweft_query::risk_inputs::Artifact::load(path)?;
            if args.get("risk_inputs").is_none() {
                args["risk_inputs"] = json!({});
            }
            args["risk_inputs"][side] = serde_json::to_value(artifact)?;
        }
    }
    query::validate(tool, &args)?;
    let mut response = daemon::call(&resolved, tool, args)?;
    if response.get("error").is_none()
        && (cli.all_pages
            || matches!(cli.format, Some(Format::Sarif))
            || matches!(
                &cli.command,
                Command::ArchCheck {
                    write_baseline: Some(_),
                    ..
                }
            ))
    {
        response = reports::collect(&resolved, tool, response)?;
    }
    if let Command::ArchCheck {
        write_baseline: Some(path),
        ..
    } = &cli.command
        && response.get("error").is_none()
    {
        let baseline = semweft_query::architecture::baseline(&response)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec_pretty(&baseline)?)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
    }
    if matches!(cli.format, Some(Format::Sarif)) && response.get("error").is_none() {
        reports::set_source_root(&mut response, &resolved.root)?;
    }
    if matches!(cli.command, Command::Doctor) && response.get("error").is_none() {
        response["health"] = daemon::call(
            &resolved,
            "index_status",
            json!({"snapshot_id":response["snapshot_id"]}),
        )?;
    }
    Ok(Some(response))
}
fn exit_code(value: &Value, verification: bool) -> i32 {
    if let Some(code) = value["error"]["code"].as_str() {
        return match code {
            "trust_required" => 4,
            "cancelled" => 130,
            "invalid_argument" | "symbol_not_found" | "ambiguous_symbol" | "file_not_found"
            | "content_mismatch" | "budget_too_small" | "snapshot_expired" | "cursor_expired"
            | "config_conflict" | "review_not_found" => 2,
            _ => 5,
        };
    }
    if verification {
        return match value["verification"]["status"].as_str() {
            Some("no_violations_found") => 0,
            Some("violations_found") => 1,
            Some("insufficient_evidence") => 3,
            _ => 5,
        };
    }
    0
}
use anyhow::Context;
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let format = cli.format.unwrap_or_else(|| {
        if matches!(cli.command, Command::Hook { .. }) {
            return Format::Json;
        }
        if std::io::stdout().is_terminal() {
            Format::Text
        } else {
            Format::Json
        }
    });
    let value = match run(&cli).await {
        Ok(None) => return,
        Ok(Some(mut value)) => {
            if value["matches_cold"] == false {
                value["error"] = json!({"code":"index_refresh_failed","message":"incremental and cold views differ"});
            }
            value
        }
        Err(error) => query::error_value(&error),
    };
    let mut exit = exit_code(
        &value,
        matches!(
            cli.command,
            Command::Verify { .. }
                | Command::ArchCheck { .. }
                | Command::Guard { .. }
                | Command::ContractImpact { .. }
        ) || (matches!(cli.command, Command::Report { .. }) && value.get("verification").is_some()),
    );
    if matches!(cli.command, Command::Daemon | Command::Mcp) && exit != 0 {
        eprintln!("{value}");
        std::process::exit(exit);
    }
    match render::render(&value, format, cli.max_chars, cli.gaps == "all") {
        Ok(text) => println!("{text}"),
        Err(error) => {
            println!("{}", query::error_value(&error));
            exit = 2;
        }
    }
    if exit != 0 {
        std::process::exit(exit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_parse_symbol_pairs_filters_limits_and_cursor_only_requests() {
        let cli = Cli::try_parse_from([
            "semweft",
            "path",
            "entry",
            "stock",
            "--path",
            "src",
            "--max-depth",
            "4",
            "--max-paths",
            "20",
            "--max-nodes",
            "30",
            "--source",
            "L1",
            "--certainty",
            "candidate",
        ])
        .unwrap();
        assert!(
            matches!(&cli.command, Command::Path { from:Some(from),to:Some(to),path:Some(path),max_depth:Some(4),max_paths:Some(20),max_nodes:Some(30),.. } if from == "entry" && to == "stock" && path == "src")
        );
        assert_eq!(cli.source, ["L1"]);
        let cli = Cli::try_parse_from(["semweft", "path", "--cursor", "next"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Path {
                from: None,
                to: None,
                ..
            }
        ));
        assert_eq!(
            common(&cli, false),
            json!({"timeout_ms":5000,"cursor":"next"})
        );
    }

    #[test]
    fn verification_parses_diff_limits_and_cursor_requests() {
        let cli = Cli::try_parse_from([
            "semweft",
            "verify",
            "--diff",
            "WORKTREE",
            "--depth",
            "2",
            "--max-nodes",
            "10",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Command::Verify { diff:Some(diff), depth:Some(2), max_nodes:Some(10) } if diff == "WORKTREE")
        );
        let cli = Cli::try_parse_from(["semweft", "verify", "--cursor", "page"]).unwrap();
        assert!(matches!(cli.command, Command::Verify { diff: None, .. }));
        assert_eq!(
            common(&cli, false),
            json!({"timeout_ms":5000,"cursor":"page"})
        );
    }

    #[test]
    fn verification_exit_codes_use_the_full_verdict_and_preserve_errors() {
        for (status, expected) in [
            ("no_violations_found", 0),
            ("violations_found", 1),
            ("insufficient_evidence", 3),
        ] {
            let value = json!({"verification":{"status":status},"findings":[]});
            assert_eq!(exit_code(&value, true), expected);
            assert_eq!(exit_code(&value, false), 0);
        }
        for (code, expected) in [
            ("invalid_argument", 2),
            ("trust_required", 4),
            ("timeout", 5),
            ("cancelled", 130),
        ] {
            assert_eq!(exit_code(&json!({"error":{"code":code}}), true), expected);
        }
        assert_eq!(exit_code(&json!({}), true), 5);
    }

    #[test]
    fn impls_parses_path_filters_and_common_arguments() {
        let cli = Cli::try_parse_from([
            "semweft",
            "impls",
            "Deduct::name",
            "--path",
            "src/production.rs",
            "--source",
            "L1,L3",
            "--certainty",
            "candidate",
            "--freshness",
            "current",
            "--snapshot-id",
            "snap_impls",
            "--semantic-epoch",
            "3",
            "--require-fresh",
            "--fresh-wait-ms",
            "200",
            "--timeout-ms",
            "1000",
            "--limit",
            "1",
            "--format",
            "json",
        ])
        .unwrap();
        assert!(matches!(
            &cli.command,
            Command::Impls { symbol: Some(symbol), path: Some(path) }
                if symbol == "Deduct::name" && path == "src/production.rs"
        ));
        assert_eq!(
            common(&cli, false),
            json!({"snapshot_id":"snap_impls","semantic_epoch":3,"require_fresh":true,
                "fresh_wait_ms":200,"timeout_ms":1000,"limit":1,"source":["L1","L3"],
                "certainty":["candidate"],"freshness":["current"]})
        );
        assert!(matches!(cli.format, Some(Format::Json)));
    }

    #[test]
    fn impls_cursor_parses_without_a_symbol_or_filter_defaults() {
        let cli = Cli::try_parse_from(["semweft", "impls", "--cursor", "impls_page"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Impls {
                symbol: None,
                path: None
            }
        ));
        assert_eq!(
            common(&cli, false),
            json!({"timeout_ms":5000,"cursor":"impls_page"})
        );
    }

    #[test]
    fn budget_parsing_accepts_k_suffixes_and_rejects_invalid_values() {
        assert_eq!(parse_budget("4096").unwrap(), 4096);
        assert_eq!(parse_budget("8k").unwrap(), 8000);
        assert_eq!(parse_budget("10K").unwrap(), 10_000);
        for invalid in [
            "",
            "0",
            "0k",
            "k",
            "-1",
            "abc",
            "1.5k",
            "18446744073709551616",
        ] {
            assert!(parse_budget(invalid).is_err(), "{invalid}");
        }
        assert!(
            parse_budget("18446744073709551615k").is_err(),
            "suffix multiplication must not overflow"
        );
    }

    #[test]
    fn ci_report_commands_parse_all_pages_sarif_and_offline_input() {
        let cli = Cli::try_parse_from([
            "semweft",
            "verify",
            "--diff",
            "WORKTREE",
            "--all-pages",
            "--format",
            "sarif",
        ])
        .unwrap();
        assert!(cli.all_pages);
        assert!(matches!(cli.format, Some(Format::Sarif)));
        assert!(
            Cli::try_parse_from([
                "semweft",
                "verify",
                "--diff",
                "WORKTREE",
                "--cursor",
                "next",
                "--all-pages",
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from([
            "semweft",
            "report",
            "--input",
            "verify.json",
            "--format",
            "markdown",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Command::Report { input } if input.as_path() == std::path::Path::new("verify.json"))
        );
    }
}
