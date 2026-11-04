use crate::{daemon, reports};
use anyhow::{Context, Result, bail};
use semweft_core::identity;
use semweft_query::config;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

fn files(input: &Value) -> Vec<String> {
    let mut files = BTreeSet::new();
    for key in ["file_path", "path"] {
        if let Some(path) = input[key].as_str() {
            files.insert(path.to_owned());
        }
    }
    let patch = input.as_str().or_else(|| {
        ["patch", "patch_text", "input"]
            .iter()
            .find_map(|key| input[key].as_str())
            .or_else(|| {
                input["command"]
                    .as_str()
                    .filter(|command| command.trim_start().starts_with("*** Begin Patch\n"))
            })
    });
    if let Some(patch) = patch {
        for line in patch.lines() {
            for prefix in [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ] {
                if let Some(path) = line.strip_prefix(prefix) {
                    files.insert(path.trim().to_owned());
                }
            }
        }
    }
    files.into_iter().collect()
}

/// Returns `None` for paths outside the workspace: they are not guard-protected, so they
/// are reported but never blocked. Symbolic links that lead out of the workspace still fail.
fn relative(root: &Path, cwd: &Path, path: &str) -> Result<Option<String>> {
    let requested = cwd.join(path);
    let mut normalized = std::path::PathBuf::new();
    for component in requested.components() {
        match component {
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    bail!("invalid_argument: hook path escapes filesystem root");
                }
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    // Hosts may pass paths through system aliases such as macOS `/tmp` -> `/private/tmp`.
    // Resolve the alias of the deepest existing ancestor first, so only links inside the
    // workspace are treated as edit-scope escapes.
    if !normalized.starts_with(root) {
        let mut tail = Vec::new();
        let mut head = normalized.as_path();
        while let Some(parent) = head.parent() {
            if let Ok(actual) = head.canonicalize() {
                if actual.starts_with(root) {
                    normalized = tail.iter().rev().fold(actual, |acc, part| acc.join(part));
                }
                break;
            }
            if let Some(name) = head.file_name() {
                tail.push(name.to_owned());
            }
            head = parent;
        }
    }
    if !normalized.starts_with(root) {
        return Ok(None);
    }
    for ancestor in normalized.ancestors() {
        if std::fs::symlink_metadata(ancestor)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!("invalid_argument: symbolic-link edit paths are outside guard capture scope");
        }
        if let Ok(actual) = ancestor.canonicalize()
            && !actual.starts_with(root)
        {
            bail!("invalid_argument: hook path or symbolic link escapes workspace");
        }
        if ancestor == root {
            break;
        }
    }
    let path = normalized
        .strip_prefix(root)
        .context("invalid_argument: hook path escapes workspace")?
        .to_str()
        .context("invalid_argument: hook path is not UTF-8")?
        .replace('\\', "/");
    semweft_protocol::safe_relative(&path)?;
    Ok(Some(path))
}

const RECEIPT_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Removes receipts whose PostToolUse never arrived, e.g. denied or interrupted edits.
fn prune_receipts(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let expired = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > RECEIPT_TTL);
        if expired {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn response(event: &str, block: bool, message: String) -> Value {
    if event == "PreToolUse" {
        let mut output = json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":message}});
        if block {
            output["hookSpecificOutput"]["permissionDecision"] = json!("deny");
            output["hookSpecificOutput"]["permissionDecisionReason"] =
                output["hookSpecificOutput"]["additionalContext"].clone();
        }
        output
    } else {
        let mut output = json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":message}});
        if block {
            output["decision"] = json!("block");
            output["reason"] = output["hookSpecificOutput"]["additionalContext"].clone();
        }
        output
    }
}

fn analyze(
    payload: &Value,
    host: &str,
    workspace: Option<&Path>,
    config_path: Option<&Path>,
) -> Result<(bool, String)> {
    let cwd = Path::new(
        payload["cwd"]
            .as_str()
            .context("invalid_argument: hook input requires cwd")?,
    )
    .canonicalize()?;
    let resolved = config::resolve(workspace.or(Some(&cwd)), config_path)?;
    let event = payload["hook_event_name"].as_str().unwrap_or("");
    let session = payload["session_id"]
        .as_str()
        .context("invalid_argument: hook input requires session_id")?;
    let tool_id = payload["tool_use_id"]
        .as_str()
        .context("invalid_argument: hook input requires tool_use_id")?;
    let directory = resolved.index_dir.join("hook-receipts");
    let receipt = directory.join(format!(
        "{}.json",
        identity("hook", &[host, session, tool_id])
    ));
    if matches!(
        payload["tool_name"].as_str(),
        Some("Read" | "Grep" | "Glob" | "LS" | "NotebookRead")
    ) {
        return Ok((false, "Semweft: read-only tool; guard not applied.".into()));
    }
    let input = &payload["tool_input"];
    let requested = files(input);
    let paths = requested
        .iter()
        .map(|path| relative(&resolved.root, &cwd, path))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if paths.is_empty() && !requested.is_empty() {
        return Ok((
            false,
            "Semweft: edit paths are outside the workspace and not protected by guard.".into(),
        ));
    }
    if paths.is_empty() {
        let edit = matches!(
            payload["tool_name"].as_str(),
            Some("Edit" | "Write" | "apply_patch")
        );
        return Ok((edit, "Semweft: unsupported tool input; no edit paths were inferred. Shell, network and indirect writes are not protected by this adapter. Use guard before/after explicitly.".into()));
    }
    let report = if event == "PreToolUse" {
        let report = daemon::call(&resolved, "agent_preflight", json!({"files":paths}))?;
        if report.get("error").is_some() {
            bail!("invalid_argument: guard query failed: {}", report["error"]);
        }
        config::private_dir(&directory)?;
        prune_receipts(&directory);
        config::atomic_write(
            &receipt,
            &serde_json::to_vec(
                &json!({"snapshot_id":report["snapshot_id"],"semantic_epoch":report["epochs"]["semantic"],"files":paths}),
            )?,
        )?;
        reports::collect(&resolved, "agent_preflight", report)?
    } else {
        let before: Value = serde_json::from_slice(&std::fs::read(&receipt).context(
            "invalid_argument: no matching pre-edit receipt; cannot establish an edit diff",
        )?)?;
        let report = daemon::call(
            &resolved,
            "agent_verify",
            json!({"diff":{"base":{"kind":"snapshot","snapshot_id":before["snapshot_id"],"semantic_epoch":before["semantic_epoch"]},"head":{"kind":"workspace"}}}),
        )?;
        if report.get("error").is_some() {
            bail!(
                "invalid_argument: post-edit guard query failed: {}",
                report["error"]
            );
        }
        let _ = std::fs::remove_file(&receipt);
        reports::collect(&resolved, "agent_verify", report)?
    };
    let blocked = report["guard"]["decision"] == "block";
    Ok((
        blocked,
        format!(
            "Semweft {}: {}. Snapshot {}. Findings: {}. No compiler/tests executed. {}",
            event,
            report["guard"]["decision"].as_str().unwrap_or("review"),
            report["snapshot_id"].as_str().unwrap_or("unknown"),
            report["verification"]["finding_count"],
            report["findings"]
                .as_array()
                .into_iter()
                .flatten()
                .take(8)
                .filter_map(|finding| finding["message"].as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ),
    ))
}

pub fn run(
    payload: &Value,
    host: &str,
    workspace: Option<&Path>,
    config_path: Option<&Path>,
) -> Result<Value> {
    let event = payload["hook_event_name"].as_str().unwrap_or("");
    if !matches!(event, "PreToolUse" | "PostToolUse") {
        bail!("invalid_argument: hook_event_name must be PreToolUse or PostToolUse");
    }
    // Failing closed is the default. `SEMWEFT_HOOK_ON_ERROR=warn` lets infrastructure
    // errors (daemon down, bad config) warn instead of blocking every edit; a guard
    // decision of `block` still blocks.
    let fail_open = std::env::var("SEMWEFT_HOOK_ON_ERROR").is_ok_and(|mode| mode == "warn");
    let (block, message) = analyze(payload, host, workspace, config_path).unwrap_or_else(|error| {
        if fail_open {
            (
                false,
                format!("Semweft guard unavailable, edit not checked: {error:#}"),
            )
        } else {
            (true, format!("Semweft guard failed closed: {error:#}"))
        }
    });
    Ok(response(event, block, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patches_include_moves_and_never_execute_shell_input() {
        let paths = files(
            &json!({"input":"*** Begin Patch\n*** Update File: src/a.rs\n*** Move to: src/b.rs\n*** Delete File: src/c.rs\n*** End Patch"}),
        );
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert!(files(&json!({"command":"rm -rf /tmp/something"})).is_empty());
        assert_eq!(
            files(
                &json!({"command":"*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** End Patch"})
            ),
            vec!["src/a.rs"]
        );
        assert!(
            files(&json!({"command":"echo '\n*** Update File: src/not-a-patch.rs\n'"})).is_empty()
        );
        assert_eq!(
            response("PreToolUse", true, "blocked".into())["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        assert_eq!(
            response("PostToolUse", true, "blocked".into())["decision"],
            "block"
        );
    }
    #[test]
    fn paths_cannot_escape_through_parent_or_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert_eq!(
            relative(&root, &root, "src/new.rs").unwrap().as_deref(),
            Some("src/new.rs")
        );
        assert_eq!(relative(&root, &root, "../outside").unwrap(), None);
        assert_eq!(relative(&root, &root, "/etc/hosts").unwrap(), None);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp", root.join("link")).unwrap();
            assert!(relative(&root, &root, "link/new.rs").is_err());
        }
    }
    #[test]
    fn expired_receipts_are_pruned_and_fresh_ones_kept() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("old.json");
        let fresh = temp.path().join("fresh.json");
        std::fs::write(&old, b"{}").unwrap();
        std::fs::write(&fresh, b"{}").unwrap();
        let aged = std::time::SystemTime::now() - RECEIPT_TTL - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(aged)
            .unwrap();
        prune_receipts(temp.path());
        assert!(!old.exists());
        assert!(fresh.exists());
    }
    #[cfg(unix)]
    #[test]
    fn aliased_absolute_paths_inside_the_workspace_are_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().canonicalize().unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let root = real.join("proj");
        std::fs::create_dir(&root).unwrap();
        let through_alias = alias.join("proj/src/new.rs");
        assert_eq!(
            relative(&root, &root, through_alias.to_str().unwrap())
                .unwrap()
                .as_deref(),
            Some("src/new.rs")
        );
    }
}
