use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use semweft_core::{digest, identity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const DEFAULT_CONFIG: &str = "schema_version = \"0.1\"\n\n[workspace]\nroot = \".\"\nexclude = [\"target/**\", \".git/**\", \".semweft/**\"]\nrespect_gitignore = true\n\n[index]\ndependency_depth = 0\n\n[context]\ntokenizer = \"estimated\"\n\n[daemon]\nidle_timeout_ms = 60000\n";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: String,
    #[serde(default)]
    pub workspace: Workspace,
    #[serde(default)]
    pub index: Index,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub daemon: Daemon,
    #[serde(default)]
    pub architecture: crate::architecture::Policy,
    #[serde(default)]
    pub guard: crate::guard::Policy,
    #[serde(default)]
    pub contracts: crate::contracts::Config,
    #[serde(default)]
    pub services: Vec<crate::services::Service>,
    #[serde(default)]
    pub go: GoBuild,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GoBuild {
    pub tags: Option<Vec<String>>,
}

impl GoBuild {
    pub fn validate(&self) -> Result<()> {
        if let Some(tags) = &self.tags
            && (tags.len() > 256
                || tags.iter().any(|tag| {
                    tag.is_empty()
                        || tag.len() > 128
                        || !tag.chars().all(|character| {
                            character.is_ascii_alphanumeric() || matches!(character, '_' | '.')
                        })
                }))
        {
            bail!(
                "invalid_argument: go.tags must contain at most 256 nonempty alphanumeric, underscore or dot tags (128 bytes each)"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workspace {
    pub root: String,
    pub exclude: Vec<String>,
    pub respect_gitignore: bool,
}
impl Default for Workspace {
    fn default() -> Self {
        Self {
            root: ".".into(),
            exclude: vec!["target/**".into(), ".git/**".into(), ".semweft/**".into()],
            respect_gitignore: true,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Index {
    pub directory: Option<String>,
    pub dependency_depth: u32,
    pub query_cache_views: usize,
    pub query_cache_bytes: usize,
    pub retain_snapshots: Option<usize>,
    pub max_disk_bytes: Option<u64>,
    pub cursor_cache_count: usize,
    pub cursor_cache_bytes: usize,
}
impl Default for Index {
    fn default() -> Self {
        Self {
            directory: None,
            dependency_depth: 0,
            query_cache_views: 8,
            query_cache_bytes: 64 * 1024 * 1024,
            retain_snapshots: None,
            max_disk_bytes: None,
            cursor_cache_count: 256,
            cursor_cache_bytes: 64 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub tokenizer: String,
}
impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            tokenizer: "estimated".into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Daemon {
    pub idle_timeout_ms: u64,
}
impl Default for Daemon {
    fn default() -> Self {
        Self {
            idle_timeout_ms: 60_000,
        }
    }
}
impl Default for Config {
    fn default() -> Self {
        toml::from_str(DEFAULT_CONFIG).unwrap()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resolved {
    pub root: PathBuf,
    pub config_path: Option<PathBuf>,
    pub config: Config,
    pub workspace_id: String,
    pub cache: PathBuf,
    pub index_dir: PathBuf,
    pub config_dir: PathBuf,
}

fn platform_dir(cache: bool) -> Result<PathBuf> {
    let override_name = if cache {
        "SEMWEFT_CACHE_DIR"
    } else {
        "SEMWEFT_CONFIG_DIR"
    };
    if let Some(path) = std::env::var_os(override_name) {
        return Ok(PathBuf::from(path));
    }
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os(if cache { "LOCALAPPDATA" } else { "APPDATA" })
            .map(|p| {
                PathBuf::from(p)
                    .join("semweft")
                    .join(if cache { "cache" } else { "config" })
            })
            .context("index_store_error: user directory unavailable");
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var_os("HOME").context("index_store_error: HOME unavailable")?;
        #[cfg(target_os = "macos")]
        {
            Ok(PathBuf::from(home).join(if cache {
                "Library/Caches/semweft"
            } else {
                "Library/Application Support/semweft"
            }))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(std::env::var_os(if cache {
                "XDG_CACHE_HOME"
            } else {
                "XDG_CONFIG_HOME"
            })
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(if cache { ".cache" } else { ".config" }))
            .join("semweft"))
        }
    }
}

pub use semweft_store::{atomic_write, private_dir};

pub fn cargo_root(start: &Path) -> PathBuf {
    let nearest_cargo = start.ancestors().find(|p| p.join("Cargo.toml").is_file());
    if let Some(web) = crate::web::project_root(start)
        && nearest_cargo.is_none_or(|cargo| !cargo.starts_with(&web))
        && crate::go::project_root(start).is_none_or(|go| !go.starts_with(&web))
    {
        return web;
    }
    if let Some(root) = crate::go::project_root(start)
        && nearest_cargo.is_none_or(|cargo| !cargo.starts_with(&root))
    {
        return root;
    }
    let mut nearest = None;
    for path in start.ancestors() {
        let manifest = path.join("Cargo.toml");
        if let Ok(text) = fs::read_to_string(&manifest) {
            nearest.get_or_insert_with(|| path.to_path_buf());
            if let Ok(value) = toml::from_str::<toml::Value>(&text) {
                if value.get("workspace").is_some() {
                    let member = start
                        .strip_prefix(path)
                        .ok()
                        .map(|p| p.to_string_lossy().replace('\\', "/"));
                    if start == path
                        || members(path).is_ok_and(|roots| {
                            roots.iter().any(|r| {
                                !r.is_empty()
                                    && member
                                        .as_ref()
                                        .is_some_and(|m| m == r || m.starts_with(&format!("{r}/")))
                            })
                        })
                    {
                        return path.into();
                    }
                }
                if let Some(explicit) = value
                    .get("package")
                    .and_then(|p| p.get("workspace"))
                    .and_then(toml::Value::as_str)
                    && let Ok(root) = path.join(explicit).canonicalize()
                {
                    return root;
                }
            }
        }
    }
    nearest.unwrap_or_else(|| start.into())
}

pub fn resolve(workspace: Option<&Path>, config_path: Option<&Path>) -> Result<Resolved> {
    let cwd = std::env::current_dir()?;
    let start = workspace
        .unwrap_or(&cwd)
        .canonicalize()
        .context("invalid_argument: workspace does not exist")?;
    let config_path = match config_path {
        Some(p) => Some(
            p.canonicalize()
                .context("invalid_argument: config does not exist")?,
        ),
        None => start
            .ancestors()
            .map(|p| p.join("semweft.toml"))
            .find(|p| p.is_file()),
    };
    let config: Config = match &config_path {
        Some(path) => toml::from_str(&fs::read_to_string(path)?)
            .context("invalid_argument: invalid semweft.toml")?,
        None => Config::default(),
    };
    if config.schema_version != "0.1" || config.daemon.idle_timeout_ms == 0 {
        bail!("invalid_argument: unsupported schema or zero idle timeout");
    }
    exclusions(&config.workspace.exclude)?;
    config.architecture.validate()?;
    config.guard.validate()?;
    config.contracts.validate()?;
    crate::services::validate(&config.services)?;
    config.go.validate()?;
    let root = if let Some(path) = &config_path {
        path.parent()
            .unwrap()
            .join(&config.workspace.root)
            .canonicalize()
            .context("invalid_argument: configured root does not exist")?
    } else {
        cargo_root(&start)
    };
    if workspace.is_some() && config_path.is_some() && cargo_root(&start) != root {
        bail!("invalid_argument: --workspace and --config resolve to different roots");
    }
    let workspace_id = identity(
        "ws",
        &[root
            .to_str()
            .context("invalid_argument: root is not UTF-8")?],
    );
    if crate::go::is_project(&root) && !root.join("Cargo.toml").is_file() {
        if !cfg!(feature = "go-experimental") {
            bail!("capability_unsupported: build with --features go-experimental to index Go");
        }
        if config.index.dependency_depth != 0 {
            bail!("capability_unsupported: Go experimental supports dependency_depth=0");
        }
    }
    if crate::web::is_project(&root)
        && !root.join("Cargo.toml").is_file()
        && !crate::go::is_project(&root)
    {
        if !cfg!(feature = "web-experimental") {
            bail!("capability_unsupported: build with --features web-experimental to index TS/JS");
        }
        if config.index.dependency_depth != 0 {
            bail!("capability_unsupported: TS/JS supports dependency_depth=0");
        }
    }
    let cache = platform_dir(true)?.join("workspaces").join(&workspace_id);
    let index_dir = config
        .index
        .directory
        .as_ref()
        .map(|p| root.join(p))
        .unwrap_or_else(|| cache.join("index"));
    let config_dir = platform_dir(false)?;
    Ok(Resolved {
        root,
        config_path,
        config,
        workspace_id,
        cache,
        index_dir,
        config_dir,
    })
}

pub fn exclusions(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).context("invalid_argument: invalid exclude glob")?);
    }
    Ok(builder.build()?)
}

/// Cargo member discovery without running Cargo, build scripts or macros.
pub fn members(root: &Path) -> Result<Vec<String>> {
    let manifest: toml::Value = toml::from_str(
        &fs::read_to_string(root.join("Cargo.toml"))
            .context("invalid_argument: Cargo.toml required")?,
    )
    .context("invalid_argument: invalid Cargo.toml")?;
    let mut paths = vec![];
    if manifest.get("package").is_some() {
        paths.push(String::new());
    }
    if let Some(workspace) = manifest.get("workspace") {
        let empty = vec![];
        let patterns = workspace
            .get("members")
            .and_then(toml::Value::as_array)
            .unwrap_or(&empty)
            .iter()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let exclude = workspace
            .get("exclude")
            .and_then(toml::Value::as_array)
            .unwrap_or(&empty)
            .iter()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let included = exclusions(&patterns)?;
        let excluded = exclusions(&exclude)?;
        for entry in ignore::WalkBuilder::new(root)
            // Cargo ownership comes from manifests, independently of source filters.
            .standard_filters(false)
            .follow_links(false)
            .filter_entry(|e| {
                !matches!(e.file_name().to_str(), Some("target" | ".git" | ".semweft"))
            })
            .build()
        {
            let entry = entry?;
            if entry.file_name() != "Cargo.toml" {
                continue;
            }
            let member = entry
                .path()
                .parent()
                .unwrap()
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            if included.is_match(&member) && !excluded.is_match(&member) {
                paths.push(member);
            }
        }
        for pattern in patterns.iter().filter(|p| !p.contains(['*', '?', '['])) {
            crate::workspace::safe_relative(pattern)?;
            if !root.join(pattern).join("Cargo.toml").is_file() && !excluded.is_match(pattern) {
                bail!("invalid_argument: missing Cargo member {pattern}");
            }
        }
    }
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        bail!("invalid_argument: no Cargo members found");
    }
    Ok(paths)
}

impl Resolved {
    pub fn analysis_hash(&self) -> String {
        digest(serde_json::to_vec(&json!({"workspace":self.config.workspace,"dependency_depth":self.config.index.dependency_depth,"tokenizer":self.config.context.tokenizer,"go":self.config.go})).unwrap())
    }
    fn trust_identity(&self) -> Result<String> {
        // Git identity is independent of HEAD; changing remotes invalidates trust.
        let output = std::process::Command::new("git")
            .arg("-c")
            .arg("core.fsmonitor=false")
            .args(["config", "--local", "--get-regexp", "remote\\..*\\.url"])
            .current_dir(&self.root)
            .output();
        let remotes = output
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
            .unwrap_or_default();
        Ok(identity(
            "repo",
            &[self.root.to_string_lossy().as_ref(), &digest(remotes)],
        ))
    }
    pub fn trusted(&self) -> Result<bool> {
        let path = self.config_dir.join("trust.json");
        if !path.exists() {
            return Ok(false);
        }
        let records: Value = serde_json::from_slice(&fs::read(path)?)
            .context("index_store_error: invalid trust records")?;
        Ok(records
            .get(self.root.to_string_lossy().as_ref())
            .and_then(Value::as_str)
            .is_some_and(|id| self.trust_identity().is_ok_and(|current| current == id)))
    }
    pub fn trust(&self, revoke: bool) -> Result<Value> {
        private_dir(&self.config_dir)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.config_dir.join("trust.lock"))?;
        lock.lock()?;
        let path = self.config_dir.join("trust.json");
        let mut records: serde_json::Map<String, Value> = if path.exists() {
            serde_json::from_slice(&fs::read(&path)?)
                .context("index_store_error: invalid trust records")?
        } else {
            Default::default()
        };
        if revoke {
            records.remove(self.root.to_string_lossy().as_ref());
        } else {
            records.insert(
                self.root.to_string_lossy().into_owned(),
                json!(self.trust_identity()?),
            );
        }
        atomic_write(&path, &serde_json::to_vec_pretty(&records)?)?;
        Ok(
            json!({"schema_version":"0.1","workspace_id":self.workspace_id,"trust":{"state":if revoke {"untrusted"} else {"trusted"},"requires_build":true,"requires_trust":true}}),
        )
    }
    pub fn init(&self) -> Result<Value> {
        let path = self.root.join("semweft.toml");
        let created = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(DEFAULT_CONFIG.as_bytes())?;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e.into()),
        };
        Ok(
            json!({"schema_version":"0.1","workspace_id":self.workspace_id,"config":path,"created":created,"build_system":if self.root.join("Cargo.toml").exists() {"cargo"} else if crate::go::enabled(&self.root) {"go"} else if crate::web::enabled(&self.root) {"node"} else {"unknown"},"trust":{"state":if self.trusted()? {"trusted"} else {"untrusted"},"hint":"semweft trust --accept-build-execution"}}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_configuration_matches_the_documented_defaults() {
        let config = Config::default();
        assert_eq!(config.schema_version, "0.1");
        assert_eq!(
            config.workspace.exclude,
            vec!["target/**", ".git/**", ".semweft/**"]
        );
        assert!(config.workspace.respect_gitignore);
        assert_eq!(config.index.dependency_depth, 0);
        assert_eq!(config.context.tokenizer, "estimated");
        assert_eq!(config.daemon.idle_timeout_ms, 60_000);
        assert!(config.go.tags.is_none());
    }

    #[test]
    fn go_tags_reject_flags_and_preserve_explicit_empty_selection() {
        let selected: Config = toml::from_str("schema_version='0.1'\n[go]\ntags=[]\n").unwrap();
        assert_eq!(selected.go.tags, Some(vec![]));
        selected.go.validate().unwrap();
        for tag in ["", "-mod=mod", "tag,other", "tag other", "a/b", "标签"] {
            assert!(
                GoBuild {
                    tags: Some(vec![tag.into()])
                }
                .validate()
                .is_err(),
                "{tag}"
            );
        }
        GoBuild {
            tags: Some(vec!["go1.22".into(), "custom_tag".into()]),
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn exclusions_compile_globs_and_reject_invalid_patterns() {
        let set = exclusions(&["target/**".into(), "generated/**".into()]).unwrap();
        assert!(set.is_match("target/debug/out"));
        assert!(set.is_match("generated/x.rs"));
        assert!(!set.is_match("src/lib.rs"));
        assert!(exclusions(&["[".into()]).is_err());
    }

    #[test]
    fn cargo_roots_follow_workspace_members_and_package_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/*\"]\nresolver=\"2\"\n",
        )
        .unwrap();
        for name in ["a", "b"] {
            let dir = root.join("crates").join(name);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname=\"{name}\"\nversion=\"0.1.0\"\n"),
            )
            .unwrap();
        }
        assert_eq!(cargo_root(&root), root);
        assert_eq!(cargo_root(&root.join("crates/a")), root);
        assert_eq!(
            members(&root).unwrap(),
            vec!["crates/a".to_string(), "crates/b".to_string()]
        );

        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(
            nested.join("Cargo.toml"),
            "[package]\nname=\"nested\"\nversion=\"0.1.0\"\nworkspace=\"..\"\n",
        )
        .unwrap();
        assert_eq!(cargo_root(&nested), root);
    }

    #[test]
    fn members_support_single_packages_and_report_missing_directories() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("package");
        fs::create_dir(&package).unwrap();
        fs::write(
            package.join("Cargo.toml"),
            "[package]\nname=\"solo\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(members(&package).unwrap(), vec!["".to_string()]);

        let broken = temp.path().join("broken");
        fs::create_dir(&broken).unwrap();
        fs::write(
            broken.join("Cargo.toml"),
            "[workspace]\nmembers=[\"missing\"]\n",
        )
        .unwrap();
        assert!(
            members(&broken)
                .unwrap_err()
                .to_string()
                .contains("missing Cargo member")
        );
    }

    #[test]
    fn atomic_writes_replace_files_under_private_directories() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store").join("views.json");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_directories_reject_permissive_or_symlinked_paths() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let fresh = temp.path().join("fresh");
        private_dir(&fresh).unwrap();
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let open = temp.path().join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            private_dir(&open)
                .unwrap_err()
                .to_string()
                .contains("dedicated private storage directory")
        );

        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(private_dir(&link).is_err());
    }
}
