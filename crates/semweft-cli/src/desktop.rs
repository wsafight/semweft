use crate::{config::Resolved, daemon, git, query::QueryService};
use anyhow::{Context, Result, ensure};
use semweft_protocol::{DAEMON_PROTOCOL, DesktopEndpoint};
use serde_json::{Value, json};

pub fn bootstrap(resolved: &Resolved) -> Result<Value> {
    daemon::ensure(resolved)?;
    #[cfg(not(windows))]
    let endpoint = daemon::endpoint(resolved).to_string_lossy().into_owned();
    #[cfg(windows)]
    let endpoint = format!(
        "semweft-{}-{}",
        semweft_core::digest(std::env::var("USERNAME").unwrap_or_default()),
        resolved.workspace_id
    );
    Ok(serde_json::to_value(DesktopEndpoint {
        protocol: DAEMON_PROTOCOL.into(),
        workspace: resolved.root.clone(),
        workspace_id: resolved.workspace_id.clone(),
        config_hash: resolved.analysis_hash(),
        endpoint,
    })?)
}

pub fn context(resolved: &Resolved) -> Result<Value> {
    let selected = git::diff_selectors(&resolved.root, "WORKTREE");
    let mut result = match selected {
        Ok(diff) => json!({"diff":diff,"churn":{}}),
        Err(error) => json!({"diff":null,"churn":{},"git_error":format!("{error:#}")}),
    };
    let history = git_output(
        &resolved.root,
        &[
            "log",
            "-n",
            "200",
            "--since=90 days ago",
            "--format=",
            "--numstat",
            "--relative",
            "-z",
            "--no-renames",
            "--",
            ".",
        ],
    );
    match history {
        Ok(history) => result["churn"] = churn(&history),
        Err(error) => result["git_error"] = json!(format!("{error:#}")),
    }
    result["branch"] = git_output(&resolved.root, &["branch", "--show-current"])
        .ok()
        .filter(|name| !name.trim().is_empty())
        .map(|name| json!(name.trim()))
        .unwrap_or(Value::Null);
    Ok(result)
}

fn git_output(root: &std::path::Path, args: &[&str]) -> Result<String> {
    use std::io::{Read, Seek};
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = std::process::Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Git history timed out after 20 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    stdout.rewind()?;
    stderr.rewind()?;
    let mut errors = String::new();
    stderr.read_to_string(&mut errors)?;
    ensure!(status.success(), "Git: {}", errors);
    let mut output = String::new();
    stdout.read_to_string(&mut output)?;
    Ok(output)
}

fn churn(history: &str) -> Value {
    let mut result = serde_json::Map::<String, Value>::new();
    for record in history.split('\0') {
        let fields = record
            .trim_start_matches('\n')
            .splitn(3, '\t')
            .collect::<Vec<_>>();
        if fields.len() != 3 {
            continue;
        }
        let (Ok(added), Ok(deleted)) = (fields[0].parse::<u64>(), fields[1].parse::<u64>()) else {
            continue;
        };
        let value = result
            .entry(fields[2])
            .or_insert_with(|| json!({"added":0,"deleted":0,"touches":0}));
        for (field, increment) in [("added", added), ("deleted", deleted), ("touches", 1)] {
            value[field] = json!(value[field].as_u64().unwrap_or(0).saturating_add(increment));
        }
    }
    Value::Object(result)
}

pub fn sources(service: &QueryService, args: &Value) -> Result<Value> {
    let snapshot = args["snapshot_id"]
        .as_str()
        .context("invalid_argument: source snapshot required")?;
    let epoch = args["semantic_epoch"]
        .as_u64()
        .context("invalid_argument: source semantic epoch required")?;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().unwrap_or(32) as usize;
    ensure!(
        (1..=128).contains(&limit),
        "invalid_argument: source limit must be 1..128"
    );
    let stored = service.engine.historical(snapshot, Some(epoch))?;
    let files = stored
        .view
        .snapshot
        .manifest
        .files
        .iter()
        .filter(|(path, _)| {
            path.ends_with(".rs") || path.ends_with(".go") || semweft_query::web::is_source(path)
        })
        .collect::<Vec<_>>();
    ensure!(
        offset <= files.len(),
        "invalid_argument: source offset out of range"
    );
    let end = offset.saturating_add(limit).min(files.len());
    let items = files[offset..end]
        .iter()
        .map(|(path, file)| {
            (
                path.as_str(),
                json!({"content":file.content,"file_hash":file.file_hash}),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    Ok(
        json!({"snapshot_id":snapshot,"epochs":{"semantic":epoch},"files":items,
        "next_offset":(end < files.len()).then_some(end),"total_files":files.len()}),
    )
}

pub fn review(resolved: &Resolved, args: &Value) -> Result<Value> {
    semweft_query::reviews::save(
        resolved,
        args["report"].clone(),
        args["conclusion"]
            .as_str()
            .context("invalid_argument: review conclusion required")?,
        args["note"].as_str().unwrap_or(""),
    )
}

pub fn review_request(
    service: &QueryService,
    resolved: &Resolved,
    args: &Value,
    _deadline: std::time::Instant,
) -> Result<Value> {
    match args["action"].as_str() {
        Some("create") => {
            for side in ["base", "head"] {
                if let Some(snapshot) = args["report"]["snapshots"][side]["snapshot_id"].as_str() {
                    service.engine.snapshots(&json!({"action":"pin","snapshot_id":snapshot,"semantic_epoch":args["report"]["snapshots"][side]["epochs"]["semantic"]}))?;
                }
            }
            semweft_query::reviews::create(
                resolved,
                args["report"].clone(),
                args["title"].as_str().unwrap_or("变更审查"),
            )
        }
        Some("decide") => semweft_query::reviews::decide(resolved, &service.engine, args),
        _ => anyhow::bail!("invalid_argument: unknown review action"),
    }
}

pub fn open_review(resolved: &Resolved, id: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let executable = std::env::current_exe()?;
        let bundled = executable
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::parent);
        let application = std::env::var_os("SEMWEFT_DESKTOP_APP")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                bundled
                    .filter(|path| path.extension().is_some_and(|extension| extension == "app"))
                    .map(std::path::Path::to_path_buf)
            })
            .unwrap_or_else(|| {
                resolved
                    .root
                    .join("target/desktop-preview/Semweft Preview.app")
            });
        ensure!(
            application.is_dir(),
            "invalid_argument: desktop app not found; set SEMWEFT_DESKTOP_APP to its .app path"
        );
        let status = std::process::Command::new("open")
            .args(["-n", "-a"])
            .arg(application)
            .arg("--args")
            .arg("--workspace")
            .arg(&resolved.root)
            .args(["--review", id])
            .status()?;
        ensure!(
            status.success(),
            "indexer_unavailable: cannot open desktop review"
        );
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (resolved, id);
        anyhow::bail!("capability_unsupported: desktop review currently requires macOS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_pages_are_bound_to_historical_contents_and_epoch() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join("src")).unwrap();
        std::fs::write(
            temporary.path().join("Cargo.toml"),
            "[package]\nname='desktop-sources'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("src/lib.rs"),
            "pub mod other;\npub fn original() {}\n",
        )
        .unwrap();
        std::fs::write(temporary.path().join("src/other.rs"), "pub fn other() {}\n").unwrap();
        let config = temporary.path().join("semweft.toml");
        std::fs::write(
            &config,
            "schema_version='0.1'\n[index]\ndirectory='.semweft/index'\n",
        )
        .unwrap();
        let resolved = crate::config::resolve(Some(temporary.path()), Some(&config)).unwrap();
        let engine = semweft_query::Engine::new(resolved).unwrap();
        let service = QueryService::new(engine.clone());
        let indexed = engine
            .index(
                &json!({"syntax_only":true}),
                std::time::Instant::now() + std::time::Duration::from_secs(10),
            )
            .unwrap();
        let mut args = json!({"snapshot_id":indexed["snapshot_id"],"semantic_epoch":indexed["epochs"]["semantic"],"limit":1});
        let first = sources(&service, &args).unwrap();
        assert_eq!(first["total_files"], 2);
        assert_eq!(first["next_offset"], 1);
        std::fs::write(temporary.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        let repeated = sources(&service, &args).unwrap();
        assert_eq!(first, repeated);
        assert!(
            first["files"]["src/lib.rs"]["content"]
                .as_str()
                .unwrap()
                .contains("original")
        );
        args["offset"] = json!(1);
        let second = sources(&service, &args).unwrap();
        assert!(second["files"].get("src/other.rs").is_some());
        assert!(second["next_offset"].is_null());
        args["offset"] = json!(3);
        assert!(sources(&service, &args).is_err());
        args["offset"] = json!(0);
        args["semantic_epoch"] = json!(999);
        assert!(sources(&service, &args).is_err());
        engine.shutdown();
    }

    #[test]
    fn git_numstat_preserves_paths_and_skips_binary_files() {
        let result = super::churn(concat!(
            "\n3\t1\tsrc/a.rs\0\n2\t0\tsrc/a.rs\0-\t-\timage.png\0",
            "1\t2\ta\tb.rs\0"
        ));
        assert_eq!(
            result["src/a.rs"],
            serde_json::json!({"added":5,"deleted":1,"touches":2})
        );
        assert!(result.get("image.png").is_none());
        assert_eq!(result["a\tb.rs"]["deleted"], 2);
    }
}
