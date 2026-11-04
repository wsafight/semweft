use crate::sync::MutexExt;
use crate::{
    engine::StoredView,
    query::{self, QueryService},
};
use anyhow::{Context, Result, bail};
use semweft_core::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) fn command(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(root).args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "diff.external=",
    ]);
    cmd
}
pub(crate) fn head_revision(root: &Path) -> String {
    command(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unavailable".into())
}
fn run(root: &Path, args: &[&str]) -> Result<String> {
    let output = command(root)
        .args(args)
        .output()
        .context("invalid_argument: Git unavailable")?;
    if !output.status.success() {
        bail!(
            "invalid_argument: Git selector failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}
fn revision(root: &Path, name: &str) -> Result<String> {
    if name.is_empty() || name == "WORKTREE" {
        bail!("invalid_argument: invalid revision selector");
    }
    run(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{name}^{{commit}}"),
        ],
    )
}
pub fn diff_selectors(root: &Path, spec: &str) -> Result<Value> {
    let (base, head) = if spec == "WORKTREE" {
        (revision(root, "HEAD")?, json!({"kind":"workspace"}))
    } else if let Some((a, b)) = spec.split_once("...") {
        if a.is_empty() || b.is_empty() || b.contains("..") {
            bail!("invalid_argument: diff range needs two revisions");
        }
        let a = revision(root, a)?;
        let b = revision(root, b)?;
        let bases = run(root, &["merge-base", "--all", &a, &b])?;
        if bases.lines().count() != 1 {
            bail!("invalid_argument: no unique merge base");
        }
        (bases, json!({"kind":"revision","revision":b}))
    } else if let Some((a, b)) = spec.split_once("..") {
        if a.is_empty() || b.is_empty() || b.contains("..") {
            bail!("invalid_argument: diff range needs two revisions");
        }
        (
            revision(root, a)?,
            json!({"kind":"revision","revision":revision(root,b)?}),
        )
    } else {
        (revision(root, spec)?, json!({"kind":"workspace"}))
    };
    Ok(json!({"base":{"kind":"revision","revision":base},"head":head}))
}

fn capture_revision(service: &QueryService, sha: &str, deadline: Instant) -> Result<StoredView> {
    let resolved = service.engine.state.locked().resolved.clone();
    let root = &resolved.root;
    let prefix = run(root, &["rev-parse", "--show-prefix"])?;
    let output = command(root)
        .args(["ls-tree", "--full-tree", "-r", "-z", sha])
        .output()?;
    if !output.status.success() {
        bail!("invalid_argument: cannot read revision tree");
    }
    let exclude = crate::config::exclusions(&resolved.config.workspace.exclude)?;
    let protected = resolved.config.guard.path_globs()?;
    let historical_config = run(
        &resolved.root,
        &["show", &format!("{sha}:{prefix}semweft.toml")],
    )
    .ok();
    let historical_config = historical_config
        .as_deref()
        .map(toml::from_str::<crate::config::Config>);
    // An unparsable historical config must not silently shrink guard coverage: record it.
    let historical_config_unparsed = matches!(historical_config, Some(Err(_)));
    let historical_protected = historical_config
        .and_then(Result::ok)
        .map(|config| config.guard.path_globs())
        .transpose()?
        .unwrap_or_else(globset::GlobSet::empty);
    let mut objects = vec![];
    for entry in output.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = std::str::from_utf8(entry).context("invalid_argument: non-UTF-8 Git path")?;
        let (header, path) = text
            .split_once('\t')
            .context("internal_error: malformed Git tree")?;
        let fields = header.split_whitespace().collect::<Vec<_>>();
        let Some(path) = path.strip_prefix(&prefix) else {
            continue;
        };
        if fields.len() != 3
            || fields[0] == "120000"
            || fields[1] != "blob"
            || exclude.is_match(path)
            || path.starts_with(".git/")
            || path.starts_with(".semweft/")
            || (crate::web::excluded(path)
                && (crate::web::is_source(path)
                    || (cfg!(feature = "web-experimental")
                        && matches!(
                            path.rsplit('/').next(),
                            Some(
                                "package.json"
                                    | "package-lock.json"
                                    | "pnpm-lock.yaml"
                                    | "yarn.lock"
                                    | "tsconfig.json"
                                    | "jsconfig.json"
                            )
                        ))))
        {
            continue;
        }
        if crate::web::is_source(path)
            || path.ends_with(".proto")
            || protected.is_match(path)
            || historical_protected.is_match(path)
            || (cfg!(feature = "web-experimental")
                && path.rsplit('/').next().is_some_and(|name| {
                    (name.starts_with("tsconfig") || name.starts_with("jsconfig"))
                        && name.ends_with(".json")
                }))
            || (cfg!(feature = "web-experimental")
                && matches!(
                    path.rsplit('/').next(),
                    Some(
                        "package.json"
                            | "package-lock.json"
                            | "pnpm-lock.yaml"
                            | "yarn.lock"
                            | "tsconfig.json"
                            | "jsconfig.json"
                    )
                ))
            || path.ends_with(".rs")
            || (cfg!(feature = "go-experimental")
                && (crate::go::is_build_input(path)
                    || matches!(
                        path.rsplit('/').next(),
                        Some("go.mod" | "go.sum" | "go.work" | "go.work.sum")
                    )))
            || path.ends_with("Cargo.toml")
            || path.ends_with("Cargo.lock")
            || path.ends_with(".gitignore")
            || path.ends_with("rust-toolchain.toml")
            || path.ends_with("rust-toolchain")
            || path.ends_with("semweft.toml")
            || path == ".cargo/config.toml"
        {
            objects.push((path.to_owned(), fields[2].to_owned()));
        }
    }
    let mut child = command(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut files = BTreeMap::new();
    let result = (|| -> Result<()> {
        for (path, oid) in &objects {
            query::check_cancelled()?;
            if Instant::now() >= deadline {
                bail!("timeout: revision capture exceeded query deadline");
            }
            writeln!(input, "{oid}")?;
            input.flush()?;
            let mut header = String::new();
            output.read_line(&mut header)?;
            let len = header
                .split_whitespace()
                .nth(2)
                .context("internal_error: malformed object header")?
                .parse::<usize>()?;
            let mut bytes = vec![0; len];
            output.read_exact(&mut bytes)?;
            let mut newline = [0];
            output.read_exact(&mut newline)?;
            files.insert(
                path.clone(),
                SourceFile::new(
                    String::from_utf8(bytes)
                        .with_context(|| format!("invalid_argument: non-UTF-8 input {path}"))?,
                ),
            );
        }
        Ok(())
    })();
    drop(input);
    let _ = child.kill();
    let _ = child.wait();
    result?;
    let root_manifest: toml::Value = toml::from_str(
        files
            .get("Cargo.toml")
            .map(|f| f.content.as_str())
            .unwrap_or(""),
    )?;
    let mut member_paths = vec![];
    if root_manifest.get("package").is_some() {
        member_paths.push(String::new());
    }
    let web = !files.contains_key("Cargo.toml")
        && !files.contains_key("go.mod")
        && !files.contains_key("go.work")
        && files.contains_key("package.json")
        && cfg!(feature = "web-experimental");
    if web {
        member_paths = files
            .keys()
            .filter_map(|p| {
                p.strip_suffix("package.json")
                    .map(|p| p.trim_end_matches('/').to_owned())
            })
            .collect();
    }
    let go = !files.contains_key("Cargo.toml")
        && (files.contains_key("go.mod") || files.contains_key("go.work"))
        && cfg!(feature = "go-experimental");
    if go {
        member_paths = if let Some(work) = files.get("go.work") {
            crate::go::uses(&work.content)
                .iter()
                .filter_map(|p| {
                    let p = p.trim_start_matches("./").trim_end_matches('/');
                    if p.is_empty() || p == "." {
                        Some(String::new())
                    } else if crate::workspace::safe_relative(p).is_ok() {
                        Some(p.to_owned())
                    } else {
                        None
                    }
                })
                .collect()
        } else if files.contains_key("go.mod") {
            vec![String::new()]
        } else {
            bail!("invalid_argument: revision has no supported workspace manifest");
        };
    }
    if let Some(patterns) = root_manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
    {
        let patterns = patterns
            .iter()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let globs = crate::config::exclusions(&patterns)?;
        for path in files.keys().filter(|p| p.ends_with("/Cargo.toml")) {
            let dir = path.strip_suffix("/Cargo.toml").unwrap();
            if globs.is_match(dir) {
                member_paths.push(dir.into());
            }
        }
    }
    let excluded_members = root_manifest
        .get("workspace")
        .and_then(|w| w.get("exclude"))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let excluded_members = crate::config::exclusions(&excluded_members)?;
    member_paths.retain(|p| !excluded_members.is_match(p));
    let mut ignored = ignore::gitignore::GitignoreBuilder::new(root);
    if resolved.config.workspace.respect_gitignore {
        for (path, file) in &files {
            if path.ends_with(".gitignore") {
                for line in file.content.lines() {
                    ignored.add_line(Some(root.join(path)), line)?;
                }
            }
        }
    }
    let ignored = ignored.build()?;
    let package_dirs = files
        .keys()
        .filter_map(|p| {
            p.strip_suffix("Cargo.toml")
                .or_else(|| p.strip_suffix("go.mod"))
                .or_else(|| p.strip_suffix("package.json"))
                .map(|d| d.trim_end_matches('/').to_owned())
        })
        .collect::<Vec<_>>();
    files.retain(|path, _| {
        if ignored
            .matched_path_or_any_parents(root.join(path), false)
            .is_ignore()
        {
            return false;
        }
        if path.ends_with(".gitignore") {
            return false;
        }
        let owner = package_dirs
            .iter()
            .filter(|d| d.is_empty() || path.starts_with(&format!("{d}/")))
            .max_by_key(|d| d.len());
        crate::web::is_source(path)
            || path.ends_with(".proto")
            || protected.is_match(path)
            || historical_protected.is_match(path)
            || owner.is_some_and(|d| member_paths.contains(d))
            || (!path.contains('/') && !path.ends_with(".rs") && !path.ends_with(".go"))
            || path == ".cargo/config.toml"
    });
    let mut manifest=Manifest {workspace_id:resolved.workspace_id.clone(),files,binary_files:BTreeMap::new(),analysis_versions:BTreeMap::from([("syntax".into(),crate::frontend::version()),("semantic".into(),"not_enabled".into()),("trust".into(),"false".into()),("analysis_config".into(),resolved.analysis_hash()),("revision".into(),sha.into()),("engine".into(),env!("CARGO_PKG_VERSION").into())]),scope:"Git revision workspace members; reuse matching captured L3 under the current analysis/build environment when available, otherwise syntax candidates; external consumers and macro expansion excluded".into()};
    if historical_config_unparsed {
        manifest
            .analysis_versions
            .insert("historical_guard_config".into(), "unparsed".into());
    }
    if go {
        manifest.analysis_versions.insert(
            "go_build_environment".into(),
            serde_json::to_string(&crate::go::build_environment(&resolved)?)?,
        );
    }
    service.engine.publish_revision(manifest)
}
fn select(
    service: &QueryService,
    selector: &Value,
    base: bool,
    deadline: Instant,
) -> Result<StoredView> {
    let keys = selector
        .as_object()
        .context("invalid_argument: selector must be an object")?;
    match selector["kind"].as_str() {
        Some("revision") if keys.len() == 2 && keys.contains_key("revision") => {
            let root = service.engine.state.locked().resolved.root.clone();
            let sha = revision(&root, selector["revision"].as_str().unwrap())?;
            capture_revision(service, &sha, deadline)
        }
        Some("snapshot")
            if keys.contains_key("snapshot_id")
                && keys.len() <= 4
                && !keys.contains_key("revision") =>
        {
            let stored = service.engine.historical(
                selector["snapshot_id"].as_str().unwrap(),
                selector["semantic_epoch"].as_u64(),
            )?;
            if let Some(commit) = selector["commit_context"].as_str() {
                let root = service.engine.state.locked().resolved.root.clone();
                let commit = revision(&root, commit)?;
                let context = capture_revision(service, &commit, deadline)?;
                if context.view.snapshot.manifest.files != stored.view.snapshot.manifest.files
                    || context.view.snapshot.manifest.binary_files
                        != stored.view.snapshot.manifest.binary_files
                {
                    bail!(
                        "content_mismatch: commit context does not match the selected snapshot inputs"
                    );
                }
            }
            Ok(stored)
        }
        Some("workspace") if !base && keys.len() == 1 => {
            service.engine.scan(false)?;
            Ok(service.engine.bind(&json!({}), deadline)?.0)
        }
        _ => bail!("invalid_argument: invalid diff selector; base cannot be workspace"),
    }
}

#[cfg(test)]
fn tokens(text: &str) -> String {
    language_tokens(text, "source.rs")
}
fn language_tokens(text: &str, path: &str) -> String {
    // Tree-sitter excludes comments and whitespace, without changing string literals.
    let mut parser = tree_sitter::Parser::new();
    let language = tree_sitter_rust::LANGUAGE.into();
    #[cfg(feature = "go-experimental")]
    let language = if path.ends_with(".go") {
        semweft_lang_go::language()
    } else {
        language
    };
    #[cfg(not(feature = "go-experimental"))]
    let _ = path;
    #[cfg(feature = "web-experimental")]
    let language = if crate::web::is_source(path) {
        semweft_lang_web::language(path)
    } else {
        language
    };
    parser.set_language(&language).unwrap();
    let Some(tree) = parser.parse(text, None) else {
        return text.into();
    };
    fn walk(node: tree_sitter::Node<'_>, text: &str, out: &mut Vec<String>) {
        if matches!(node.kind(), "line_comment" | "block_comment" | "comment") {
            return;
        }
        if node.child_count() == 0 {
            out.push(format!("{}:{}", node.kind(), &text[node.byte_range()]));
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, text, out);
        }
    }
    let mut out = vec![];
    walk(tree.root_node(), text, &mut out);
    out.join("\0")
}
fn text<'a>(symbol: &Symbol, stored: &'a StoredView) -> &'a str {
    let source = &stored.view.snapshot.manifest.files[&symbol.file].content;
    let a = byte_offset(source, symbol.span.start).unwrap();
    let b = byte_offset(source, symbol.span.end).unwrap();
    &source[a..b]
}

fn rename_fingerprint(symbol: &Symbol, stored: &StoredView) -> String {
    let source = &stored.view.snapshot.manifest.files[&symbol.file].content;
    let start = byte_offset(source, symbol.span.start).unwrap();
    let name_start = byte_offset(source, symbol.name_span.start).unwrap() - start;
    let name_end = byte_offset(source, symbol.name_span.end).unwrap() - start;
    let mut normalized = text(symbol, stored).to_owned();
    // Mask only the declaration token. Replacing name strings in the token
    // stream also masks references and prefixes of unrelated identifiers.
    normalized.replace_range(name_start..name_end, "__semweft_declaration__");
    digest(language_tokens(&normalized, &symbol.file))
}

fn check_diff_budget(deadline: Instant) -> Result<()> {
    query::check_cancelled()?;
    if Instant::now() >= deadline {
        bail!("timeout: symbol diff exceeded query deadline");
    }
    Ok(())
}

fn match_symbols(
    base: &StoredView,
    head: &StoredView,
    deadline: Instant,
) -> Result<BTreeMap<usize, (usize, &'static str)>> {
    let old = &base.view.snapshot.syntax.symbols;
    let new = &head.view.snapshot.syntax.symbols;
    let group_names = |symbols: &[Symbol]| {
        let mut groups = BTreeMap::<(String, String), Vec<usize>>::new();
        for (i, symbol) in symbols.iter().enumerate() {
            groups
                .entry((symbol.qualified_name.clone(), symbol.kind.clone()))
                .or_default()
                .push(i);
        }
        groups
    };
    let old_names = group_names(old);
    let new_names = group_names(new);
    let mut matches = BTreeMap::new();
    let mut used = BTreeSet::new();
    // Reserve all exact matches before considering moves or renames. Both
    // sides must be unique, including declarations behind different cfgs.
    for (key, before) in old_names {
        check_diff_budget(deadline)?;
        let Some(after) = new_names.get(&key) else {
            continue;
        };
        // Identical declarations in unchanged files need no inference,
        // even when cfg alternatives share a qualified name.
        let unchanged = before.len() == after.len()
            && before.iter().zip(after).all(|(&a, &b)| old[a] == new[b]);
        if unchanged || (before.len() == 1 && after.len() == 1) {
            for (&a, &b) in before.iter().zip(after) {
                matches.insert(a, (b, "exact"));
                used.insert(b);
            }
        }
    }

    // Group each unmatched declaration once. A fingerprint must identify
    // exactly one declaration on each side; iteration order cannot select a
    // winner among several indistinguishable candidates.
    let mut old_fingerprints = BTreeMap::<(String, String), Vec<usize>>::new();
    let mut new_fingerprints = BTreeMap::<(String, String), Vec<usize>>::new();
    for (i, symbol) in old.iter().enumerate() {
        check_diff_budget(deadline)?;
        if !matches.contains_key(&i) {
            old_fingerprints
                .entry((symbol.kind.clone(), rename_fingerprint(symbol, base)))
                .or_default()
                .push(i);
        }
    }
    for (i, symbol) in new.iter().enumerate() {
        check_diff_budget(deadline)?;
        if !used.contains(&i) {
            new_fingerprints
                .entry((symbol.kind.clone(), rename_fingerprint(symbol, head)))
                .or_default()
                .push(i);
        }
    }
    for (key, before) in old_fingerprints {
        check_diff_budget(deadline)?;
        if let Some(after) = new_fingerprints.get(&key)
            && before.len() == 1
            && after.len() == 1
        {
            matches.insert(before[0], (after[0], "inferred"));
        }
    }
    Ok(matches)
}

pub fn impact(service: &QueryService, args: &Value, deadline: Instant) -> Result<Value> {
    // Resolve immutable revisions before any expensive analysis; workspace head is captured once.
    let mut selectors = args["diff"].clone();
    for side in ["base", "head"] {
        let selector = &mut selectors[side];
        let keys = selector.as_object().unwrap();
        match selector["kind"].as_str() {
            Some("revision") if keys.len() == 2 && keys.contains_key("revision") => {
                let root = service.engine.state.locked().resolved.root.clone();
                selector["revision"] =
                    json!(revision(&root, selector["revision"].as_str().unwrap())?);
            }
            Some("snapshot")
                if keys.contains_key("snapshot_id")
                    && keys.len() <= 4
                    && !keys.contains_key("revision") =>
            {
                service.engine.historical(
                    selector["snapshot_id"].as_str().unwrap(),
                    selector["semantic_epoch"].as_u64(),
                )?;
                if let Some(commit) = selector["commit_context"].as_str() {
                    let root = service.engine.state.locked().resolved.root.clone();
                    selector["commit_context"] = json!(revision(&root, commit)?);
                }
            }
            Some("workspace") if side == "head" && keys.len() == 1 => {}
            _ => bail!("invalid_argument: invalid diff selector"),
        }
    }
    // Freeze mutable workspace input before reading an immutable revision tree.
    let head_input = select(service, &selectors["head"], false, deadline)?;
    let base_input = select(service, &selectors["base"], true, deadline)?;
    let wait_budget = args["fresh_wait_ms"]
        .as_u64()
        .unwrap_or(2000.min(args["timeout_ms"].as_u64().unwrap_or(5000) / 2));
    let wait_deadline = deadline.min(Instant::now() + Duration::from_millis(wait_budget));
    let bind_side = |stored: StoredView, side: &str| -> Result<(StoredView, Value)> {
        let mut wait_args = args.clone();
        wait_args.as_object_mut().unwrap().remove("diff");
        wait_args["snapshot_id"] = json!(stored.view.snapshot.snapshot_id);
        wait_args["fresh_wait_ms"] = json!(
            wait_deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64
        );
        if let Some(epoch) = selectors[side]["semantic_epoch"].as_u64() {
            wait_args["semantic_epoch"] = json!(epoch);
        }
        service.engine.bind(&wait_args, deadline)
    };
    let (head, fresh) = bind_side(head_input, "head")?;
    let (base, base_fresh) = bind_side(base_input, "base")?;
    let mut result = query::envelope(&head, fresh.clone());
    result["snapshots"] = json!({"base":{"snapshot_id":base.view.snapshot.snapshot_id,"epochs":{"syntax":base.view.epochs.syntax_epoch,"semantic":base.view.epochs.semantic_epoch},"fresh_requirement":base_fresh},"head":{"snapshot_id":head.view.snapshot.snapshot_id,"epochs":{"syntax":head.view.epochs.syntax_epoch,"semantic":head.view.epochs.semantic_epoch},"fresh_requirement":fresh}});
    for (side, stored) in [("base", &base), ("head", &head)] {
        let analysis_commit = stored
            .view
            .snapshot
            .manifest
            .analysis_versions
            .get("revision")
            .or_else(|| {
                stored
                    .view
                    .snapshot
                    .manifest
                    .analysis_versions
                    .get("repository_revision")
            })
            .filter(|commit| {
                matches!(commit.len(), 40 | 64)
                    && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        result["snapshots"][side]["analysis_commit"] = json!(analysis_commit);
        result["snapshots"][side]["semantic_reused_from"] = json!(
            stored
                .view
                .snapshot
                .manifest
                .analysis_versions
                .get("semantic_reused_from")
        );
        result["snapshots"][side]["commit"] = json!(
            selectors[side]["revision"]
                .as_str()
                .or_else(|| selectors[side]["commit_context"].as_str())
                .map(str::to_owned)
                .or_else(|| analysis_commit.cloned())
                .filter(|commit| matches!(commit.len(), 40 | 64)
                    && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
        );
    }
    let mut file_changes = vec![];
    for path in base
        .view
        .snapshot
        .manifest
        .files
        .keys()
        .chain(head.view.snapshot.manifest.files.keys())
        .collect::<BTreeSet<_>>()
    {
        check_diff_budget(deadline)?;
        let before = base
            .view
            .snapshot
            .manifest
            .files
            .get(path)
            .map(|file| &file.file_hash);
        let after = head
            .view
            .snapshot
            .manifest
            .files
            .get(path)
            .map(|file| &file.file_hash);
        if before != after {
            file_changes.push(json!({"path":path,"base_hash":before,"head_hash":after}));
        }
    }
    result["diff_identity"] = json!({"digest":identity("diff", &[&serde_json::to_string(&file_changes)?]),"files":file_changes,
        "basis":"captured file content hashes; excluded and uncaptured inputs are not included"});
    result["fresh_requirement"]["satisfied"] =
        json!(fresh["satisfied"] == true && base_fresh["satisfied"] == true);
    result["fresh_requirement"]["waited_ms"] = json!(
        fresh["waited_ms"].as_u64().unwrap_or(0) + base_fresh["waited_ms"].as_u64().unwrap_or(0)
    );
    result["fresh_requirement"]["reason"] =
        ["refresh_failed", "superseded", "version_pinned", "timeout"]
            .into_iter()
            .find(|reason| fresh["reason"] == *reason || base_fresh["reason"] == *reason)
            .map_or(Value::Null, |reason| json!(reason));
    result["completeness"]["known_gaps"]
        .as_array_mut()
        .unwrap()
        .extend(
            base.view
                .known_gaps
                .iter()
                .chain(base.reference_gaps.iter())
                .map(|d| json!({"kind":d.code,"side":"base","reason":d.message})),
        );
    let mut changes = vec![];
    let mut formatting = vec![];
    let mut used = BTreeSet::new();
    let old = &base.view.snapshot.syntax.symbols;
    let new = &head.view.snapshot.syntax.symbols;
    let matches = match_symbols(&base, &head, deadline)?;
    let canonical = matches
        .iter()
        .map(|(&before, &(after, _))| (new[after].symbol_id.clone(), old[before].symbol_id.clone()))
        .collect::<BTreeMap<_, _>>();
    for (i, before) in old.iter().enumerate() {
        check_diff_budget(deadline)?;
        if let Some(&(after, matching)) = matches.get(&i) {
            let after = &new[after];
            used.insert(after.symbol_id.clone());
            let kind = if matching == "inferred" {
                if before.name != after.name {
                    "renamed"
                } else {
                    "moved"
                }
            } else if language_tokens(&before.signature, &before.file)
                != language_tokens(&after.signature, &after.file)
                || base
                    .view
                    .snapshot
                    .syntax
                    .declarations
                    .get(&before.symbol_id)
                    .map(|i| {
                        i.attributes
                            .iter()
                            .map(|s| language_tokens(s, &before.file))
                            .collect::<Vec<_>>()
                    })
                    != head
                        .view
                        .snapshot
                        .syntax
                        .declarations
                        .get(&after.symbol_id)
                        .map(|i| {
                            i.attributes
                                .iter()
                                .map(|s| language_tokens(s, &after.file))
                                .collect::<Vec<_>>()
                        })
            {
                "signature_changed"
            } else if language_tokens(text(before, &base), &before.file)
                != language_tokens(text(after, &head), &after.file)
            {
                "modified"
            } else {
                if before.file_hash != after.file_hash {
                    formatting.push(json!({"base_symbol":before.symbol_id,"head_symbol":after.symbol_id,"kind":"unchanged_symbol_or_comment_format_only"}));
                }
                continue;
            };
            changes.push(json!({"kind":kind,"matching":matching,"matching_basis":if matching=="exact" {"syntax qualified name and kind unique on both sides"} else {"syntax token fingerprint unique among unmatched declarations on both sides; only the declaration name is masked"},"base_symbol":query::symbol_value(before,&base,false,false),"head_symbol":query::symbol_value(after,&head,false,false)}));
        } else {
            changes.push(json!({"kind":"deleted","matching":"unmatched","base_symbol":query::symbol_value(before,&base,false,false),"head_symbol":null}));
        }
    }
    for after in new {
        check_diff_budget(deadline)?;
        if !used.contains(&after.symbol_id) {
            changes.push(json!({"kind":"added","matching":"unmatched","base_symbol":null,"head_symbol":query::symbol_value(after,&head,false,false)}));
        }
    }
    let old_names = changes
        .iter()
        .filter(|c| matches!(c["kind"].as_str(), Some("deleted" | "renamed" | "moved")))
        .filter_map(|c| c["base_symbol"]["name"].as_str())
        .collect::<BTreeSet<_>>();
    let mut residual = vec![];
    for occurrence in &head.view.snapshot.syntax.occurrences {
        check_diff_budget(deadline)?;
        if old_names.contains(occurrence.name.as_str()) {
            residual.push(json!({"name":occurrence.name,"kind":occurrence.kind,"side":"head","location":{"file":occurrence.file,"line":occurrence.span.start.line,"column":occurrence.span.start.column,"file_hash":head.view.snapshot.manifest.files[&occurrence.file].file_hash,"coordinate_snapshot_id":head.view.snapshot.snapshot_id},"source":"L1","certainty":"candidate","reason":"old target name remains after deletion, rename or move; lexical binding and migration status are unresolved"}));
        }
    }
    let mut edges = BTreeMap::new();
    let mut raw = BTreeMap::new();
    let mut frontier = BTreeSet::new();
    let mut reason = None;
    let mut direct = BTreeSet::new();
    let mut callers = BTreeSet::new();
    let max_nodes = args["max_nodes"].as_u64().unwrap_or(1000) as usize;
    let mut nodes = BTreeSet::new();
    'changes: for change in &changes {
        for (side, stored, key) in [
            ("base", &base, "base_symbol"),
            ("head", &head, "head_symbol"),
        ] {
            let Some(id) = change[key]["symbol_id"].as_str() else {
                continue;
            };
            if !nodes.contains(&(side, id.to_owned())) && nodes.len() >= max_nodes {
                reason = query::truncation_reason(reason, Some("node_limit"));
                frontier.insert(id.to_owned());
                break 'changes;
            }
            nodes.insert((side, id.to_owned()));
            let graph = query::traverse(stored, id, args, true, deadline);
            reason = query::truncation_reason(reason, graph.reason);
            frontier.extend(graph.frontier);
            for (relation, depth, path) in graph.edges {
                let from = (side, relation.from_symbol.clone());
                let to = (side, relation.to_symbol.clone());
                let additional = usize::from(!nodes.contains(&from))
                    + usize::from(to != from && !nodes.contains(&to));
                if nodes.len() + additional > max_nodes {
                    reason = query::truncation_reason(reason, Some("node_limit"));
                    frontier.insert(id.to_owned());
                    break 'changes;
                }
                nodes.insert(from);
                nodes.insert(to);
                let mut value = query::relation_value(&relation, stored);
                value["side"] = json!(side);
                value["depth"] = json!(depth);
                value["path"] = json!(path);
                let caller = if side == "head" {
                    canonical
                        .get(&relation.from_symbol)
                        .cloned()
                        .unwrap_or_else(|| format!("head:{}", relation.from_symbol))
                } else {
                    relation.from_symbol.clone()
                };
                if relation.kind == "call" && depth == 1 && relation.from_symbol != id {
                    direct.insert(caller.clone());
                }
                if relation.kind == "call" && relation.from_symbol != id {
                    callers.insert(caller);
                }
                let key = (side, relation.relation_id.clone());
                if edges
                    .get(&key)
                    .is_none_or(|old: &Value| old["depth"].as_u64().unwrap_or(u64::MAX) > depth)
                {
                    edges.insert(key.clone(), value);
                    raw.insert(key, relation);
                }
            }
        }
    }
    result["changes"] = json!(changes);
    result["non_impact_changes"] = json!(formatting);
    result["residual_references"] = json!(residual);
    result["relation_changes"] =
        relation_changes(&base, &head, &canonical, &changes, args, deadline)?;
    if result["relation_changes"]["basis"] == "partial" {
        reason = query::truncation_reason(reason, Some("timeout"));
    }
    let config_changes = base
        .view
        .snapshot
        .manifest
        .files
        .iter()
        .filter(|(p, _)| !p.ends_with(".rs") && !p.ends_with(".go") && !crate::web::is_source(p))
        .filter(|(p, f)| {
            head.view
                .snapshot
                .manifest
                .files
                .get(*p)
                .is_none_or(|h| h.file_hash != f.file_hash)
        })
        .map(|(p, _)| p.clone())
        .chain(
            head.view
                .snapshot
                .manifest
                .files
                .keys()
                .filter(|p| {
                    !p.ends_with(".rs")
                        && !p.ends_with(".go")
                        && !crate::web::is_source(p)
                        && !base.view.snapshot.manifest.files.contains_key(*p)
                })
                .cloned(),
        )
        .collect::<BTreeSet<_>>();
    result["configuration_changes"] = json!(config_changes);
    if !config_changes.is_empty() {
        result["completeness"]["known_gaps"].as_array_mut().unwrap().push(json!({"kind":"configuration_change_requires_review","files":config_changes,"reason":"Build configuration changes can affect unchanged declarations; symbol diff impact is not exhaustive"}));
    }
    result["relations"] = json!(edges.into_values().collect::<Vec<_>>());
    result["frontier"] = json!(frontier);
    let basis = match reason {
        Some("timeout" | "node_limit") => "partial",
        Some(_) => "within_limits",
        None => "complete",
    };
    result["evidence_summary"] = query::summary(&raw.into_values().collect::<Vec<_>>(), basis);
    result["summary"] = json!({"changed_symbols":result["changes"].as_array().unwrap().len(),"direct_callers":direct.len(),"transitive_callers":callers.len(),"services":null,"tests_to_run":null,"risk":null});
    if let Some(reason) = reason {
        result["output"] = json!({"truncated":true,"truncation_reason":reason,"remaining_relations":null,"continuation":{"kind":"requery","frontier":frontier,"suggested":{"depth":(args["depth"].as_u64().unwrap_or(3)+1).min(10)}}});
    }
    crate::impact_report::enrich(&mut result, &base, &head, args, deadline)?;
    if result["output"]["truncated"] == true {
        result["output"]["continuation"]["kind"] = json!("requery");
        result["output"]["continuation"]["suggested"] = query::fixed_requery(&result, args);
    }
    Ok(result)
}

fn relation_changes(
    base: &StoredView,
    head: &StoredView,
    canonical: &BTreeMap<String, String>,
    changes: &[Value],
    args: &Value,
    deadline: Instant,
) -> Result<Value> {
    let changed = changes
        .iter()
        .flat_map(|c| {
            [
                c["base_symbol"]["symbol_id"].as_str(),
                c["head_symbol"]["symbol_id"].as_str(),
            ]
        })
        .flatten()
        .collect::<BTreeSet<_>>();
    let mut sides = vec![];
    for (side, stored) in [("base", base), ("head", head)] {
        let mut grouped = BTreeMap::<(String, String, String), Vec<Relation>>::new();
        for relation in query::filtered(crate::engine::relations(stored), args) {
            query::check_cancelled()?;
            if Instant::now() >= deadline {
                return Ok(
                    json!({"basis":"partial","reason":"timeout","added_count":null,"removed_count":null,"unchanged_count":null,"added":[],"removed":[]}),
                );
            }
            if !changed.contains(relation.from_symbol.as_str())
                && !changed.contains(relation.to_symbol.as_str())
            {
                continue;
            }
            let identity = |id: &str| {
                if side == "head" {
                    canonical
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| format!("head:{id}"))
                } else {
                    id.to_owned()
                }
            };
            grouped
                .entry((
                    relation.kind.clone(),
                    identity(&relation.from_symbol),
                    identity(&relation.to_symbol),
                ))
                .or_default()
                .push(relation);
        }
        sides.push(grouped);
    }
    let keys = sides
        .iter()
        .flat_map(|side| side.keys().cloned())
        .collect::<BTreeSet<_>>();
    let mut added = vec![];
    let mut removed = vec![];
    let mut unchanged = 0;
    for key in keys {
        let old = sides[0].get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let new = sides[1].get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let same = old.len().min(new.len());
        unchanged += same;
        removed.extend(old[same..].iter().map(|relation| {
            let mut value = query::relation_value(relation, base);
            value["side"] = json!("base");
            value
        }));
        added.extend(new[same..].iter().map(|relation| {
            let mut value = query::relation_value(relation, head);
            value["side"] = json!("head");
            value
        }));
    }
    Ok(
        json!({"basis":"complete","matching_basis":"multiset of kind and matched endpoint identities; locations may shift; evidence and binding gaps remain those of each side","added_count":added.len(),"removed_count":removed.len(),"unchanged_count":unchanged,"added":added,"removed":removed}),
    )
}

#[cfg(test)]
mod tests {
    use super::tokens;

    #[test]
    fn token_fingerprints_ignore_comments_and_layout_but_not_contents() {
        assert_eq!(
            tokens("fn a() { /* note */ x + 1; }"),
            tokens("fn  a(){\n x+1;\n// note\n}")
        );
        // Comments inside string literals are content, not trivia.
        assert_ne!(tokens("fn a() { \"/*keep*/\" }"), tokens("fn a() { \"\" }"));
        assert_ne!(tokens("fn a() { x + 1 }"), tokens("fn a() { x + 2 }"));
        assert_ne!(tokens("fn a() {}"), tokens("fn b() {}"));
    }
}
