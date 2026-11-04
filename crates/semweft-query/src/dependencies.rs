//! Captured dependency sources retain virtual paths and an explicit disk map.
use crate::config::Resolved;
use crate::sync::MutexExt;
use anyhow::{Context, Result};
use semweft_core::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

pub const MAP: &str = ".semweft-inputs/dependencies.json";

struct MetadataCache {
    inputs: String,
    manifests: BTreeMap<PathBuf, String>,
    value: Value,
}

fn metadata(resolved: &Resolved, files: &BTreeMap<String, SourceFile>) -> Result<Value> {
    static CACHE: OnceLock<Mutex<BTreeMap<PathBuf, MetadataCache>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let inputs = digest(serde_json::to_vec(
        &files
            .iter()
            .filter(|(p, _)| !p.ends_with(".rs"))
            .map(|(p, f)| (p, &f.file_hash))
            .collect::<BTreeMap<_, _>>(),
    )?);
    {
        let cached = cache.locked();
        if let Some(entry) = cached.get(&resolved.root)
            && entry.inputs == inputs
            && entry
                .manifests
                .iter()
                .all(|(p, h)| fs::read(p).is_ok_and(|b| digest(b) == *h))
        {
            return Ok(entry.value.clone());
        }
    }
    let mut command = Command::new("cargo");
    command.current_dir(&resolved.root).args([
        "metadata",
        "--offline",
        "--locked",
        "--format-version",
        "1",
    ]);
    let output = crate::process::output(&mut command, Instant::now() + Duration::from_secs(10))?;
    if !output.status.success() {
        anyhow::bail!(
            "Cargo offline dependency resolution failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let manifests = value["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["manifest_path"].as_str())
        .map(|p| Ok((PathBuf::from(p), digest(fs::read(p)?))))
        .collect::<Result<BTreeMap<_, _>>>()?;
    cache.locked().insert(
        resolved.root.clone(),
        MetadataCache {
            inputs,
            manifests,
            value: value.clone(),
        },
    );
    Ok(value)
}

pub fn source_path(root: &Path, manifest: &Manifest, file: &str) -> PathBuf {
    manifest
        .files
        .get(MAP)
        .and_then(|f| serde_json::from_str::<BTreeMap<String, String>>(&f.content).ok())
        .and_then(|map| {
            map.into_iter()
                .filter_map(|(prefix, dir)| {
                    file.strip_prefix(&format!("{prefix}/"))
                        .map(|p| PathBuf::from(dir).join(p))
                })
                .next()
        })
        .unwrap_or_else(|| root.join(file))
}

pub fn relative_path(root: &Path, manifest: &Manifest, path: &Path) -> Result<String> {
    if let Ok(relative) = path.strip_prefix(root) {
        let relative = relative.to_string_lossy().replace('\\', "/");
        if manifest.files.contains_key(&relative) {
            return Ok(relative);
        }
    }
    if let Some(file) = manifest.files.get(MAP) {
        let map: BTreeMap<String, String> = serde_json::from_str(&file.content)?;
        for (prefix, dir) in map {
            if let Ok(relative) = path.strip_prefix(dir) {
                let key = format!("{prefix}/{}", relative.to_string_lossy().replace('\\', "/"));
                if manifest.files.contains_key(&key) {
                    return Ok(key);
                }
            }
        }
    }
    anyhow::bail!("location is outside captured source scope")
}

pub fn capture(
    resolved: &Resolved,
    files: &mut BTreeMap<String, SourceFile>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<()> {
    let depth = resolved.config.index.dependency_depth;
    if depth == 0 {
        return Ok(());
    }
    if !resolved.trusted()? {
        diagnostics.push(Diagnostic { code:"dependency_metadata_blocked".into(),file:None,
            message:"Dependency resolution may execute Cargo-configured tools; authorize the repository before capturing dependency sources".into() });
        return Ok(());
    }
    let metadata = match metadata(resolved, files) {
        Ok(metadata) => metadata,
        Err(error) => {
            crate::query::check_cancelled()?;
            if Instant::now()
                >= crate::query::bound_deadline(Instant::now() + Duration::from_secs(10))
            {
                return Err(error);
            }
            diagnostics.push(Diagnostic {
                code: "dependency_metadata_unavailable".into(),
                file: None,
                message: error.to_string(),
            });
            return Ok(());
        }
    };
    let members = metadata["workspace_members"]
        .as_array()
        .context("invalid Cargo metadata")?;
    let packages = metadata["packages"]
        .as_array()
        .context("missing Cargo packages")?;
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .context("missing Cargo resolve graph")?;
    let mut seen = members
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut queue = seen
        .iter()
        .cloned()
        .map(|id| (id, 0))
        .collect::<VecDeque<_>>();
    let mut selected = BTreeSet::new();
    while let Some((id, distance)) = queue.pop_front() {
        if distance >= depth {
            continue;
        }
        if let Some(node) = nodes.iter().find(|n| n["id"] == id) {
            for dependency in node["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if seen.insert(dependency.to_owned()) {
                    selected.insert(dependency.to_owned());
                    queue.push_back((dependency.to_owned(), distance + 1));
                }
            }
        }
    }
    let exclude = crate::config::exclusions(&resolved.config.workspace.exclude)?;
    let mut map = BTreeMap::new();
    for id in selected {
        crate::query::check_cancelled()?;
        let package = packages
            .iter()
            .find(|p| p["id"] == id)
            .context("missing resolved package")?;
        let manifest = Path::new(
            package["manifest_path"]
                .as_str()
                .context("missing dependency manifest")?,
        );
        let root = manifest.parent().context("missing package root")?;
        let prefix = format!(
            "dependencies/{}-{}-{}",
            package["name"].as_str().unwrap_or("crate"),
            package["version"].as_str().unwrap_or("unknown"),
            &digest(&id)[..12]
        );
        map.insert(prefix.clone(), root.to_string_lossy().into_owned());
        for entry in ignore::WalkBuilder::new(root)
            .hidden(false)
            .follow_links(false)
            .git_ignore(resolved.config.workspace.respect_gitignore)
            .require_git(false)
            .filter_entry(|e| {
                !matches!(e.file_name().to_str(), Some(".git" | "target" | ".semweft"))
            })
            .build()
        {
            let entry = entry?;
            if entry.file_type().is_some_and(|t| t.is_symlink()) {
                diagnostics.push(Diagnostic {
                    code: "symlink_not_followed".into(),
                    file: Some(format!(
                        "{prefix}/{}",
                        entry.path().strip_prefix(root)?.to_string_lossy()
                    )),
                    message: "Dependency source symbolic links are outside the indexed scope"
                        .into(),
                });
                continue;
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            if exclude.is_match(&relative)
                || exclude.is_match(format!("{prefix}/{relative}"))
                || !(relative.ends_with(".rs")
                    || relative == "Cargo.toml"
                    || relative == "Cargo.lock")
            {
                continue;
            }
            let key = format!("{prefix}/{relative}");
            match fs::read_to_string(entry.path()) {
                Ok(text) => {
                    files.insert(key, SourceFile::new(text));
                }
                Err(error) => diagnostics.push(Diagnostic {
                    code: "dependency_source_unavailable".into(),
                    file: Some(key),
                    message: error.to_string(),
                }),
            }
        }
    }
    files.insert(MAP.into(), SourceFile::new(serde_json::to_string(&map)?));
    files.insert(".semweft-inputs/cargo-metadata.json".into(),SourceFile::new(serde_json::to_string(&json!({"packages":metadata["packages"],"resolve":metadata["resolve"],"workspace_members":members,"workspace_root":metadata["workspace_root"]}))?));
    Ok(())
}
