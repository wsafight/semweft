//! Go remains an opt-in experimental frontend with full-project SCIP refresh.
use crate::{config::Resolved, workspace::Capture};
use anyhow::{Context, Result, bail};
use semweft_core::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub fn is_project(root: &Path) -> bool {
    root.join("go.mod").is_file() || root.join("go.work").is_file()
}
pub fn enabled(root: &Path) -> bool {
    cfg!(feature = "go-experimental") && !root.join("Cargo.toml").is_file() && is_project(root)
}

pub fn is_build_input(path: &str) -> bool {
    matches!(
        path.rsplit('.').next(),
        Some("go" | "s" | "S" | "h" | "c" | "cc" | "cpp" | "m" | "mm" | "f" | "f90")
    )
}

pub(crate) fn build_environment(resolved: &Resolved) -> Result<BTreeMap<String, Option<String>>> {
    let mut environment = BTreeMap::new();
    for name in [
        "GOFLAGS",
        "GOOS",
        "GOARCH",
        "CGO_ENABLED",
        "GOWORK",
        "GOTOOLCHAIN",
        "GOENV",
        "GOPATH",
        "GOROOT",
        "GOEXPERIMENT",
    ] {
        environment.insert(
            name.into(),
            std::env::var_os(name)
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("invalid_argument: {name} must be UTF-8"))
                })
                .transpose()?,
        );
    }
    if let Some(tags) = &resolved.config.go.tags {
        let tags: std::collections::BTreeSet<_> = tags.iter().collect();
        environment.insert(
            "GOFLAGS".into(),
            Some(if tags.is_empty() {
                String::new()
            } else {
                format!(
                    "-tags={}",
                    tags.into_iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            }),
        );
    }
    Ok(environment)
}

/// Tokenize go.mod/go.work directives, retaining quoted paths and comments.
pub fn words(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut token = String::new();
    let mut quote = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                if let Some(next) = chars.next() {
                    token.push(next);
                }
            } else {
                token.push(c);
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'/') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            if !token.is_empty() {
                out.push(std::mem::take(&mut token));
            }
            continue;
        }
        if matches!(c, '"' | '`') {
            quote = Some(c);
        } else if c.is_whitespace() || matches!(c, '(' | ')') {
            if !token.is_empty() {
                out.push(std::mem::take(&mut token));
            }
            if matches!(c, '(' | ')') {
                out.push(c.to_string());
            }
        } else {
            token.push(c);
        }
    }
    if !token.is_empty() {
        out.push(token);
    }
    out
}
pub fn module(text: &str) -> Option<String> {
    let parts = words(text);
    parts
        .windows(2)
        .find(|w| w[0] == "module")
        .map(|w| w[1].clone())
}
pub fn uses(text: &str) -> Vec<String> {
    let parts = words(text);
    let mut paths = vec![];
    let mut i = 0;
    while i < parts.len() {
        if parts[i] == "use" {
            i += 1;
            if parts.get(i).is_some_and(|s| s == "(") {
                i += 1;
                while i < parts.len() && parts[i] != ")" {
                    paths.push(parts[i].clone());
                    i += 1;
                }
            } else if let Some(p) = parts.get(i) {
                paths.push(p.clone());
            }
        }
        i += 1;
    }
    paths
}

pub fn augment_map(
    stored: &crate::engine::StoredView,
    args: &serde_json::Value,
    entries: &mut Vec<serde_json::Value>,
    dependencies: &mut Vec<serde_json::Value>,
    mode: &str,
    deadline: std::time::Instant,
) -> Result<()> {
    use serde_json::json;
    let mut packages = BTreeMap::<String, (String, Vec<String>)>::new();
    for path in stored
        .view
        .snapshot
        .manifest
        .files
        .keys()
        .filter(|p| p.ends_with(".go"))
    {
        crate::query::check_deadline(deadline)?;
        let directory = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let (root, module) = crate::structure::package(stored, path).unwrap_or(("", "workspace"));
        let local = directory
            .strip_prefix(root)
            .unwrap_or(directory)
            .trim_start_matches('/');
        let import = if local.is_empty() {
            module.to_owned()
        } else {
            format!("{module}/{local}")
        };
        packages
            .entry(import)
            .or_insert_with(|| (directory.into(), vec![]))
            .1
            .push(path.clone());
    }
    let included = |p: &str| {
        args["path_prefix"].as_str().is_none_or(|prefix| {
            prefix == "."
                || p == prefix
                || p.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
        })
    };
    for (import, (dir, files)) in &packages {
        crate::query::check_deadline(deadline)?;
        if !files.iter().any(|p| included(p)) {
            continue;
        }
        let main = stored.view.snapshot.syntax.symbols.iter().find(|s| {
            files.contains(&s.file)
                && s.name == "main"
                && words(&stored.view.snapshot.manifest.files[&s.file].content)
                    .windows(2)
                    .any(|w| w[0] == "package" && w[1] == "main")
        });
        entries.push(json!({"crate":import,"target":import,"kind":if main.is_some(){"bin"}else{"lib"},"file":main.map(|s|s.file.as_str()).unwrap_or(&files[0]),"main_symbol_id":main.map(|s|&s.symbol_id),"source":"L1","certainty":"candidate","basis":"Go package declarations; build tags are not evaluated"}));
        for file in files {
            crate::query::check_deadline(deadline)?;
            for dependency in stored
                .view
                .snapshot
                .syntax
                .imports
                .get(file)
                .into_iter()
                .flatten()
            {
                crate::query::check_deadline(deadline)?;
                if mode == "crate" {
                    dependencies.push(json!({"from":import,"to":if packages.contains_key(dependency){dependency.as_str()}else{"external dependencies"},"dependency":dependency,"source":"L1","certainty":"candidate","basis":"Go import declaration; build tags are not evaluated"}));
                } else if mode == "module"
                    && let Some((_, targets)) = packages.get(dependency)
                {
                    for target in targets {
                        crate::query::check_deadline(deadline)?;
                        dependencies.push(json!({"from":file,"to":target,"source":"L1","certainty":"candidate","basis":"Go package import to captured package files"}));
                    }
                }
            }
        }
        let _ = dir;
    }
    Ok(())
}
pub fn project_root(start: &Path) -> Option<PathBuf> {
    let nearest = start.ancestors().find(|p| p.join("go.mod").is_file());
    for root in start.ancestors().filter(|p| p.join("go.work").is_file()) {
        if start == root
            || nearest.is_some_and(|member| {
                fs::read_to_string(root.join("go.work"))
                    .ok()
                    .is_some_and(|text| {
                        uses(&text)
                            .iter()
                            .any(|p| root.join(p).canonicalize().is_ok_and(|p| p == member))
                    })
            })
        {
            return Some(root.into());
        }
    }
    nearest.map(Path::to_path_buf)
}
pub fn capture(resolved: &Resolved, backend: &str) -> Result<Capture> {
    if !cfg!(feature = "go-experimental") {
        bail!("capability_unsupported: enable the go-experimental Cargo feature");
    }
    let root = &resolved.root;
    let mut files = BTreeMap::new();
    let mut diagnostics = vec![];
    let mut maps = BTreeMap::new();
    let work = fs::read_to_string(root.join("go.work")).ok();
    let paths = work
        .as_deref()
        .map(uses)
        .unwrap_or_else(|| vec![".".into()]);
    let exclude = crate::config::exclusions(&resolved.config.workspace.exclude)?;
    for path in paths {
        let member = root
            .join(&path)
            .canonicalize()
            .with_context(|| format!("invalid_argument: missing Go workspace member {path}"))?;
        if !member.join("go.mod").is_file() {
            bail!("invalid_argument: Go member has no go.mod: {path}");
        }
        let prefix = member
            .strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| {
                format!(
                    "modules/{}",
                    &digest(member.to_string_lossy().as_bytes())[..16]
                )
            });
        if !member.starts_with(root) {
            maps.insert(prefix.clone(), member.to_string_lossy().into_owned());
        }
        let mut member_files = BTreeMap::new();
        let mut nested = vec![];
        for entry in ignore::WalkBuilder::new(&member)
            .hidden(false)
            .follow_links(false)
            .git_ignore(resolved.config.workspace.respect_gitignore)
            .require_git(false)
            .filter_entry(|e| {
                !matches!(e.file_name().to_str(), Some(".git" | ".semweft" | "vendor"))
            })
            .build()
        {
            let entry = entry?;
            if entry.file_type().is_some_and(|t| t.is_symlink()) {
                diagnostics.push(Diagnostic {
                    code: "symlink_not_followed".into(),
                    file: Some(entry.path().to_string_lossy().into_owned()),
                    message: "Symbolic links are outside the indexed source scope".into(),
                });
                continue;
            }
            if entry.path().starts_with(&resolved.cache)
                || entry.path().starts_with(&resolved.index_dir)
                || !entry.file_type().is_some_and(|t| t.is_file())
            {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&member)?
                .to_string_lossy()
                .replace('\\', "/");
            let key = if prefix.is_empty() {
                relative.clone()
            } else {
                format!("{prefix}/{relative}")
            };
            if exclude.is_match(&key) || exclude.is_match(&relative) {
                continue;
            }
            if relative.ends_with("/go.mod") {
                nested.push(relative.trim_end_matches("/go.mod").to_owned());
            }
            if !(is_build_input(&relative)
                || relative.ends_with(".proto")
                || matches!(relative.as_str(), "go.mod" | "go.sum")
                || resolved.config.guard.path_globs()?.is_match(&key))
            {
                continue;
            }
            member_files.insert(
                relative,
                SourceFile::new(
                    fs::read_to_string(entry.path()).with_context(|| {
                        format!("invalid_argument: Go input is not UTF-8: {key}")
                    })?,
                ),
            );
        }
        for (path, file) in member_files {
            if !nested.iter().any(|n| path.starts_with(&format!("{n}/"))) {
                files.insert(
                    if prefix.is_empty() {
                        path
                    } else {
                        format!("{prefix}/{path}")
                    },
                    file,
                );
            }
        }
    }
    crate::web::collect(resolved, &mut files, &mut diagnostics)?;
    for path in ["go.work", "go.work.sum", "semweft.toml"] {
        if let Ok(text) = fs::read_to_string(root.join(path)) {
            files.insert(path.into(), SourceFile::new(text));
        }
    }
    if !maps.is_empty() {
        files.insert(
            crate::dependencies::MAP.into(),
            SourceFile::new(serde_json::to_string(&maps)?),
        );
    }
    diagnostics.push(Diagnostic{code:"go_experimental_scope".into(),file:None,message:"Go experimental: producer evaluates declared build tags; inactive callsites stay unresolved candidates. Generated code, interface dispatch completeness, external go env files/dependencies and external consumers are not fully modeled; project refresh uses scip-go".into()});
    Ok(Capture{manifest:Manifest{workspace_id:resolved.workspace_id.clone(),files,binary_files:BTreeMap::new(),analysis_versions:BTreeMap::from([
        ("go_build_environment".into(),serde_json::to_string(&build_environment(resolved)?)?),
        ("service_mapping".into(),serde_json::to_string(&resolved.config.services)?),
        ("syntax".into(),crate::frontend::version()),("semantic".into(),backend.into()),("analysis_config".into(),resolved.analysis_hash()),("architecture_policy".into(),serde_json::to_string(&resolved.config.architecture)?),("guard_policy".into(),serde_json::to_string(&resolved.config.guard)?),("contracts_policy".into(),serde_json::to_string(&resolved.config.contracts)?),("repository_revision".into(),crate::git::head_revision(root)),("trust".into(),resolved.trusted()?.to_string()),("engine".into(),env!("CARGO_PKG_VERSION").into())]),scope:"Experimental Go module/workspace members; dependency source depth 0; declared build tags evaluated by producer; generated code, exhaustive dispatch and external consumers excluded".into()},bytes:BTreeMap::new(),diagnostics})
}
