use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use semweft_core::*;
use std::{collections::BTreeMap, fs, path::Path};

pub struct Capture {
    pub manifest: Manifest,
    pub bytes: BTreeMap<String, Vec<u8>>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Paths that carry indexable source in any supported frontend.
pub fn is_source(path: &str) -> bool {
    path.ends_with(".rs") || path.ends_with(".go") || crate::web::is_source(path)
}

pub fn capture(root: &Path, workspace_id: &str, backend: &str) -> Result<Capture> {
    let mut files = BTreeMap::new();
    let mut binary_files = BTreeMap::new();
    let mut bytes = BTreeMap::new();
    let mut diagnostics = vec![];
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | ".semweft")
            )
        })
        .build();
    for entry in walker {
        let entry = entry.context("index_store_error: walking workspace")?;
        if entry.path() == root {
            continue;
        }
        let path = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("invalid_argument: non-UTF-8 path")?
            .replace('\\', "/");
        let ty = entry
            .file_type()
            .context("index_store_error: missing file type")?;
        if ty.is_symlink() {
            diagnostics.push(Diagnostic {
                code: "symlink_not_followed".into(),
                file: Some(path),
                message: "Prototype does not ingest symbolic links".into(),
            });
            continue;
        }
        if !ty.is_file() {
            continue;
        }
        let content =
            fs::read(entry.path()).with_context(|| format!("index_store_error: reading {path}"))?;
        match String::from_utf8(content.clone()) {
            Ok(text) => {
                files.insert(path.clone(), SourceFile::new(text));
            }
            Err(_) if path.ends_with(".rs") => {
                bail!("invalid_argument: Rust source is not UTF-8: {path}")
            }
            Err(_) => {
                binary_files.insert(path.clone(), digest(&content));
            }
        }
        bytes.insert(path, content);
    }
    let manifest = Manifest {
        workspace_id: workspace_id.into(), files, binary_files,
        analysis_versions: BTreeMap::from([
            ("syntax".into(), semweft_lang_rust::PARSER_VERSION.into()),
            ("semantic".into(), backend.into()),
            ("prototype".into(), env!("CARGO_PKG_VERSION").into()),
        ]),
        scope: "Prototype filesystem root; gitignore respected; .git/target/.semweft and symlinks excluded; external dependencies, macros, cfg evaluation and generated outputs not modeled".into(),
    };
    Ok(Capture {
        manifest,
        bytes,
        diagnostics,
    })
}

pub fn capture_config(resolved: &crate::config::Resolved, backend: &str) -> Result<Capture> {
    capture_config_cached(resolved, backend, None)
}
pub(crate) fn capture_config_cached(
    resolved: &crate::config::Resolved,
    backend: &str,
    mut cache: Option<&mut crate::capture_cache::CaptureCache>,
) -> Result<Capture> {
    if crate::web::enabled(&resolved.root) {
        return crate::web::capture(resolved, backend);
    }
    if crate::go::enabled(&resolved.root) {
        return crate::go::capture(resolved, backend);
    }
    let root = &resolved.root;
    let members = crate::config::members(root)?;
    let exclude = crate::config::exclusions(&resolved.config.workspace.exclude)?;
    let protected = resolved.config.guard.path_globs()?;
    let index_dir = resolved.index_dir.clone();
    let cache_dir = resolved.cache.clone();
    let filter_root = root.clone();
    let mut files = BTreeMap::new();
    let mut diagnostics = vec![];
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .git_ignore(resolved.config.workspace.respect_gitignore)
        .parents(resolved.config.workspace.respect_gitignore)
        .filter_entry(move |entry| {
            if matches!(entry.file_name().to_str(), Some(".git" | ".semweft"))
                || entry.path().starts_with(&index_dir)
                || entry.path().starts_with(&cache_dir)
            {
                return false;
            }
            let relative = entry
                .path()
                .strip_prefix(&filter_root)
                .unwrap_or(entry.path());
            !exclude.is_match(relative) && !exclude.is_match(relative.join("__contents__"))
        })
        .build();
    for entry in walker {
        let entry = entry.context("index_store_error: walking workspace")?;
        let relative = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("invalid_argument: non-UTF-8 path")?
            .replace('\\', "/");
        if entry.file_type().is_some_and(|t| t.is_symlink()) {
            diagnostics.push(Diagnostic {
                code: "symlink_not_followed".into(),
                file: Some(relative),
                message: "Symbolic links are outside the indexed source scope".into(),
            });
            continue;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let owned = members
            .iter()
            .any(|p| p.is_empty() || relative.starts_with(&format!("{p}/")));
        let filename = entry.file_name().to_string_lossy();
        let configuration = matches!(
            filename.as_ref(),
            "Cargo.toml" | "Cargo.lock" | "semweft.toml" | "rust-toolchain" | "rust-toolchain.toml"
        ) || relative == ".cargo/config.toml"
            || relative == ".cargo/config";
        if (!owned || !relative.ends_with(".rs"))
            && !configuration
            && !protected.is_match(&relative)
            && !relative.ends_with(".proto")
        {
            continue;
        }
        let file = if let Some(cache) = cache.as_deref_mut() {
            cache.read(entry.path())?
        } else {
            SourceFile::new(
                fs::read_to_string(entry.path())
                    .with_context(|| format!("invalid_argument: input is not UTF-8: {relative}"))?,
            )
        };
        files.insert(relative, file);
    }
    let package_dirs = files
        .keys()
        .filter_map(|p| {
            p.strip_suffix("Cargo.toml")
                .map(|d| d.trim_end_matches('/').to_owned())
        })
        .collect::<Vec<_>>();
    files.retain(|path, _| {
        let owner = package_dirs
            .iter()
            .filter(|d| d.is_empty() || path.starts_with(&format!("{d}/")))
            .max_by_key(|d| d.len());
        let member = owner.is_some_and(|d| members.contains(d));
        let root_config = !path.contains('/') && !path.ends_with(".rs");
        member
            || root_config
            || protected.is_match(path)
            || path.ends_with(".proto")
            || path == ".cargo/config.toml"
            || path == ".cargo/config"
    });
    crate::web::collect(resolved, &mut files, &mut diagnostics)?;
    crate::dependencies::capture(resolved, &mut files, &mut diagnostics)?;
    let trusted = resolved.trusted()?;
    let revision = crate::git::head_revision(root);
    let manifest = Manifest {
        workspace_id: resolved.workspace_id.clone(),
        files,
        binary_files: BTreeMap::new(),
        analysis_versions: BTreeMap::from([
            ("syntax".into(), crate::frontend::version()),
            ("semantic".into(), backend.into()),
            ("analysis_config".into(), resolved.analysis_hash()),
            (
                "architecture_policy".into(),
                serde_json::to_string(&resolved.config.architecture)?,
            ),
            (
                "guard_policy".into(),
                serde_json::to_string(&resolved.config.guard)?,
            ),
            (
                "contracts_policy".into(),
                serde_json::to_string(&resolved.config.contracts)?,
            ),
            ("trust".into(), trusted.to_string()),
            (
                "service_mapping".into(),
                serde_json::to_string(&resolved.config.services)?,
            ),
            ("repository_revision".into(), revision),
            ("engine".into(), env!("CARGO_PKG_VERSION").into()),
        ]),
        scope: format!(
            "Cargo workspace members and resolved dependency sources up to {} hops; configured exclusions and gitignore; external consumers, macro expansion and generated-source mappings excluded",
            resolved.config.index.dependency_depth
        ),
    };
    Ok(Capture {
        manifest,
        bytes: BTreeMap::new(),
        diagnostics,
    })
}

pub use semweft_protocol::safe_relative;

impl Capture {
    pub fn materialize(&self, root: &Path) -> Result<()> {
        for (path, bytes) in &self.bytes {
            let destination = root.join(safe_relative(path)?);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::write(destination, bytes)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_untracked_inputs_and_binary_hashes_but_respects_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(dir.path().join("ignored.rs"), "broken").unwrap();
        fs::write(dir.path().join("new.rs"), "fn new() {} ").unwrap();
        fs::write(dir.path().join("asset.bin"), [255, 0]).unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/out.rs"), "generated").unwrap();
        let snapshot = capture(dir.path(), "test", "none").unwrap();
        assert!(snapshot.manifest.files.contains_key("new.rs"));
        assert!(!snapshot.manifest.files.contains_key("ignored.rs"));
        assert!(!snapshot.manifest.files.contains_key("target/out.rs"));
        assert_eq!(
            snapshot.manifest.binary_files["asset.bin"],
            digest([255, 0])
        );
        let copy = tempfile::tempdir().unwrap();
        snapshot.materialize(copy.path()).unwrap();
        assert_eq!(fs::read(copy.path().join("asset.bin")).unwrap(), [255, 0]);
    }

    #[test]
    fn edits_cannot_escape_scratch_workspace() {
        for path in ["../x.rs", "/tmp/x.rs", "a/../x.rs", "./x.rs", ""] {
            assert!(safe_relative(path).is_err(), "{path}");
        }
        assert!(safe_relative("src/lib.rs").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn capture_reports_symlinks_and_rejects_non_utf8_rust_sources() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("real.rs"), "fn real() {}").unwrap();
        symlink(dir.path().join("real.rs"), dir.path().join("link.rs")).unwrap();
        let snapshot = capture(dir.path(), "test", "none").unwrap();
        assert!(snapshot.manifest.files.contains_key("real.rs"));
        assert!(!snapshot.manifest.files.contains_key("link.rs"));
        assert!(
            snapshot
                .diagnostics
                .iter()
                .any(|d| d.code == "symlink_not_followed")
        );

        fs::write(dir.path().join("bad.rs"), [0xff, 0xfe]).unwrap();
        assert!(capture(dir.path(), "test", "none").is_err());
    }

    #[test]
    fn configured_capture_keeps_member_sources_and_build_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        fs::create_dir_all(root.join("crates/a/src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/*\"]\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/a/Cargo.toml"),
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        fs::write(root.join("crates/a/src/lib.rs"), "pub fn alpha() {}\n").unwrap();
        fs::create_dir_all(root.join("outside/src")).unwrap();
        fs::write(
            root.join("outside/Cargo.toml"),
            "[package]\nname=\"outside\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        fs::write(root.join("outside/src/lib.rs"), "pub fn ghost() {}\n").unwrap();

        let resolved = crate::config::Resolved {
            root: root.clone(),
            config_path: None,
            config: crate::config::Config::default(),
            workspace_id: "ws".into(),
            cache: temp.path().join("cache"),
            index_dir: temp.path().join("index"),
            config_dir: temp.path().join("config"),
        };
        let captured = capture_config(&resolved, "backend@1").unwrap();
        assert!(captured.manifest.files.contains_key("Cargo.toml"));
        assert!(captured.manifest.files.contains_key("crates/a/Cargo.toml"));
        assert!(captured.manifest.files.contains_key("crates/a/src/lib.rs"));
        assert!(!captured.manifest.files.contains_key("outside/src/lib.rs"));
        assert_eq!(captured.manifest.workspace_id, "ws");
        assert_eq!(captured.manifest.analysis_versions["semantic"], "backend@1");
        assert!(captured.manifest.analysis_versions.contains_key("trust"));
    }
}
