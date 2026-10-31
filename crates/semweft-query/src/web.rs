//! Read-only TS/JS package capture; no package-manager or build execution.
use crate::{config::Resolved, workspace::Capture};
use anyhow::{Context, Result};
use semweft_core::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub fn is_source(path: &str) -> bool {
    cfg!(feature = "web-experimental")
        && [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"]
            .iter()
            .any(|ext| path.ends_with(ext))
}
pub fn excluded(path: &str) -> bool {
    path.split('/').any(|p| {
        matches!(
            p,
            "node_modules" | "target" | "dist" | "build" | "coverage" | ".next" | ".nuxt" | ".yarn"
        )
    })
}
pub fn is_project(root: &Path) -> bool {
    root.join("package.json").is_file()
}
pub fn enabled(root: &Path) -> bool {
    cfg!(feature = "web-experimental")
        && is_project(root)
        && !root.join("Cargo.toml").is_file()
        && !crate::go::is_project(root)
}
pub fn project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|p| is_project(p))
        .map(Path::to_path_buf)
}
pub fn collect(
    resolved: &Resolved,
    files: &mut BTreeMap<String, SourceFile>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<()> {
    if !cfg!(feature = "web-experimental") {
        return Ok(());
    }
    let exclude = crate::config::exclusions(&resolved.config.workspace.exclude)?;
    for entry in ignore::WalkBuilder::new(&resolved.root)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .git_ignore(resolved.config.workspace.respect_gitignore)
        .parents(resolved.config.workspace.respect_gitignore)
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(
                    ".git"
                        | ".semweft"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | "coverage"
                        | ".next"
                        | ".nuxt"
                        | ".yarn"
                )
            )
        })
        .build()
    {
        let entry = entry?;
        let path = entry
            .path()
            .strip_prefix(&resolved.root)?
            .to_str()
            .context("invalid_argument: path is not UTF-8")?
            .replace('\\', "/");
        let excluded = path
            .split('/')
            .scan(String::new(), |prefix, segment| {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(segment);
                Some(prefix.clone())
            })
            .any(|p| exclude.is_match(p));
        if excluded
            || entry.path().starts_with(&resolved.cache)
            || entry.path().starts_with(&resolved.index_dir)
        {
            continue;
        }
        if entry.file_type().is_some_and(|t| t.is_symlink()) {
            diagnostics.push(Diagnostic {
                code: "symlink_not_followed".into(),
                file: Some(path),
                message: "Symbolic links are outside the package tree".into(),
            });
            continue;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if is_source(&path)
            || matches!(
                entry.file_name().to_str(),
                Some(
                    "package.json"
                        | "package-lock.json"
                        | "pnpm-lock.yaml"
                        | "yarn.lock"
                        | "tsconfig.json"
                        | "jsconfig.json"
                        | "semweft.toml"
                )
            )
        {
            let content = fs::read_to_string(entry.path())
                .with_context(|| format!("invalid_argument: web input is not UTF-8: {path}"))?;
            if entry.file_name() == "package.json" {
                serde_json::from_str::<Value>(&content)
                    .context("invalid_argument: invalid package.json")?;
            }
            files.insert(path, SourceFile::new(content));
        }
    }
    if files.keys().any(|p| is_source(p)) {
        diagnostics.push(Diagnostic { code: "web_syntax_scope".into(), file: None, message: "TS/JS syntax candidates: lexical binding, tsconfig aliases, dynamic imports, CommonJS exports, anonymous callback ownership and external consumers are not resolved. Resident language server is not enabled.".into() });
    }
    Ok(())
}
pub fn capture(resolved: &Resolved, backend: &str) -> Result<Capture> {
    let mut files = BTreeMap::new();
    let mut diagnostics = vec![];
    collect(resolved, &mut files, &mut diagnostics)?;
    Ok(Capture { manifest: Manifest { workspace_id: resolved.workspace_id.clone(), files, binary_files: BTreeMap::new(), analysis_versions: BTreeMap::from([
        ("syntax".into(), crate::frontend::version()), ("semantic".into(), backend.into()), ("analysis_config".into(), resolved.analysis_hash()), ("trust".into(), resolved.trusted()?.to_string()), ("engine".into(), env!("CARGO_PKG_VERSION").into()),
    ]), scope: "TS/JS package tree, gitignore and exclusions respected; generated output, node_modules, symlinks and dependency sources excluded; syntax candidates only".into() }, bytes: BTreeMap::new(), diagnostics })
}
pub fn augment_map(
    stored: &crate::engine::StoredView,
    args: &Value,
    entries: &mut Vec<Value>,
    dependencies: &mut Vec<Value>,
    mode: &str,
    deadline: std::time::Instant,
) -> Result<()> {
    let files = &stored.view.snapshot.manifest.files;
    for (path, source) in files.iter().filter(|(p, _)| p.ends_with("package.json")) {
        crate::query::check_deadline(deadline)?;
        let Ok(package) = serde_json::from_str::<Value>(&source.content) else {
            continue;
        };
        let name = package["name"].as_str().unwrap_or("package");
        let dir = path.trim_end_matches("package.json");
        for field in ["main", "module", "types"] {
            if let Some(file) = package[field].as_str() {
                entries.push(json!({"crate":name,"target":field,"kind":"package_entry","file":format!("{dir}{}",file.trim_start_matches("./")),"source":"L1","certainty":"candidate","basis":"package.json declaration"}));
            }
        }
        if mode == "crate" {
            for field in [
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies",
            ] {
                for (dependency, version) in package[field].as_object().into_iter().flatten() {
                    crate::query::check_deadline(deadline)?;
                    dependencies.push(json!({"from":name,"to":dependency,"version":version,"source":"L1","certainty":"candidate","basis":format!("package.json {field}")}));
                }
            }
        }
    }
    if mode != "module" {
        return Ok(());
    }
    for (path, imports) in &stored.view.snapshot.syntax.imports {
        crate::query::check_deadline(deadline)?;
        if !is_source(path)
            || args["path_prefix"].as_str().is_some_and(|p| {
                p != "." && path != p && !path.starts_with(&format!("{}/", p.trim_end_matches('/')))
            })
        {
            continue;
        }
        for import in imports.iter().filter(|p| p.starts_with('.')) {
            crate::query::check_deadline(deadline)?;
            let mut parts: Vec<_> = path.split('/').collect();
            parts.pop();
            for part in import.split('/') {
                match part {
                    "." => {}
                    ".." => {
                        parts.pop();
                    }
                    p => parts.push(p),
                }
            }
            let base = parts.join("/");
            let mut candidates = vec![base.clone()];
            let stem = base
                .strip_suffix(".js")
                .or_else(|| base.strip_suffix(".mjs"))
                .unwrap_or(&base);
            for ext in ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"] {
                candidates.push(format!("{stem}.{ext}"));
                candidates.push(format!("{base}/index.{ext}"));
            }
            if let Some(target) = candidates.iter().find(|p| files.contains_key(*p)) {
                dependencies.push(json!({"from":path,"to":target,"source":"L1","certainty":"candidate","basis":"relative ES import/export to captured file; aliases and conditional exports not evaluated"}));
            }
        }
    }
    Ok(())
}
