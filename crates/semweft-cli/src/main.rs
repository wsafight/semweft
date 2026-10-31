use semweft_query::{config, dependencies, engine, frontend, git, lsp, query, web, workspace};
mod benchmark;
mod daemon;
mod mcp;
mod prototype;
mod render;

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
        #[arg(long, alias = "path-prefix")]
        path: Option<String>,
        #[arg(long)]
        depth: Option<u64>,
        #[arg(long)]
        max_symbols_per_module: Option<u64>,
        #[arg(long,value_parser=["crate","module","none"])]
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

async fn run(cli: &Cli) -> Result<Option<Value>> {
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
        Command::Daemon => {
            daemon::serve(resolved)?;
            return Ok(None);
        }
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
        Command::Status => "index_status",
        Command::Doctor => "repo_map",
        Command::Map {
            path,
            depth,
            max_symbols_per_module,
            dependencies,
        } => {
            put(&mut args, "path_prefix", path.as_ref());
            put(&mut args, "depth", *depth);
            put(&mut args, "max_symbols_per_module", *max_symbols_per_module);
            put(&mut args, "dependencies", dependencies.as_ref());
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
        } => {
            put(&mut args, "query", query.as_ref());
            put(&mut args, "kind", kind.as_ref());
            put(&mut args, "path_prefix", path.as_ref());
            put(&mut args, "match", match_mode.as_ref());
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
    query::validate(tool, &args)?;
    let mut response = daemon::call(&resolved, tool, args)?;
    if matches!(cli.command, Command::Doctor) && response.get("error").is_none() {
        response["health"] = daemon::call(
            &resolved,
            "index_status",
            json!({"snapshot_id":response["snapshot_id"]}),
        )?;
    }
    Ok(Some(response))
}
use anyhow::Context;
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let format = cli.format.unwrap_or_else(|| {
        if std::io::stdout().is_terminal() {
            Format::Text
        } else {
            Format::Json
        }
    });
    let (value, exit) = match run(&cli).await {
        Ok(None) => return,
        Ok(Some(mut value)) => {
            if value["matches_cold"] == false {
                value["error"] = json!({"code":"index_refresh_failed","message":"incremental and cold views differ"});
            }
            (value, 0)
        }
        Err(error) => (query::error_value(&error), 0),
    };
    let mut exit = exit;
    if let Some(code) = value["error"]["code"].as_str() {
        exit = match code {
            "trust_required" => 4,
            "cancelled" => 130,
            "invalid_argument" | "symbol_not_found" | "ambiguous_symbol" | "file_not_found"
            | "content_mismatch" | "budget_too_small" | "snapshot_expired" | "cursor_expired"
            | "config_conflict" => 2,
            _ => 5,
        };
    }
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
    use super::parse_budget;

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
}
