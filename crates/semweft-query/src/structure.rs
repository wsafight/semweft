//! Structural summaries use the same immutable facts as navigation queries.
use crate::{
    engine::{self, StoredView},
    query,
};
use anyhow::Result;
use semweft_core::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

fn included(path: &str, args: &Value) -> bool {
    args["path_prefix"].as_str().is_none_or(|p| {
        p == "." || path == p || path.starts_with(&format!("{}/", p.trim_end_matches('/')))
    })
}
fn normalized(path: &str) -> String {
    let mut parts = vec![];
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}
pub fn package<'a>(stored: &'a StoredView, file: &str) -> Option<(&'a str, &'a str)> {
    stored
        .namespaces
        .iter()
        .filter(|(dir, _)| dir.is_empty() || file.starts_with(&format!("{dir}/")))
        .max_by_key(|(dir, _)| dir.len())
        .map(|(dir, name)| (dir.as_str(), name.as_str()))
}

pub fn repo_map(
    result: &mut Value,
    stored: &StoredView,
    args: &Value,
    deadline: Instant,
) -> Result<()> {
    query::check_deadline(deadline)?;
    let snapshot = &stored.view.snapshot;
    let files = &snapshot.manifest.files;
    let depth = args["depth"].as_u64().unwrap_or(2) as usize;
    let max = match args["max_symbols_per_module"].as_u64().unwrap_or(20) {
        0 => usize::MAX,
        n => n as usize,
    };
    let mut modules = vec![];
    let mut omitted_modules = vec![];
    let mut entries = vec![];
    let mut dependencies = vec![];
    // Keep complete records even when a later phase runs out of time.
    let computed = (|| -> Result<()> {
        let mut symbols_by_file = BTreeMap::<&str, Vec<&Symbol>>::new();
        for symbol in &snapshot.syntax.symbols {
            query::check_deadline(deadline)?;
            symbols_by_file
                .entry(&symbol.file)
                .or_default()
                .push(symbol);
        }
        for path in files
            .keys()
            .filter(|p| p.ends_with(".rs") || p.ends_with(".go") || crate::web::is_source(p))
        {
            query::check_deadline(deadline)?;
            if !included(path, args) {
                continue;
            }
            let (dir, krate) = package(stored, path).unwrap_or(("", "workspace"));
            let local = path
                .strip_prefix(&format!("{dir}/"))
                .unwrap_or(path)
                .strip_prefix("src/")
                .unwrap_or_else(|| path.strip_prefix(&format!("{dir}/")).unwrap_or(path));
            let module_depth = local.split('/').count().saturating_sub(1)
                + usize::from(!matches!(local, "lib.rs" | "main.rs" | "mod.rs"));
            if module_depth > depth {
                omitted_modules.push(path.clone());
                continue;
            }
            let mut symbols = BTreeMap::new();
            for &s in symbols_by_file.get(path.as_str()).into_iter().flatten() {
                query::check_deadline(deadline)?;
                let public = snapshot.syntax.declarations.get(&s.symbol_id).map_or_else(
                    || s.signature.starts_with("pub "),
                    |info| info.visibility == "public",
                );
                symbols.insert(
                    (!public, query::public_name(s), s.name_span, &s.symbol_id),
                    s,
                );
            }
            let count = symbols.len();
            modules.push(json!({"path":path,"crate":krate,"depth":module_depth,"symbols":[],
                "symbol_count":count,"omitted_symbols":count,"ranking_basis":"public declarations first, then qualified name and source position"}));
            let module = modules.last_mut().unwrap();
            for s in symbols.into_values().take(max) {
                query::check_deadline(deadline)?;
                let selected = module["symbols"].as_array_mut().unwrap();
                selected.push(query::symbol_value(s, stored, false, true));
                module["omitted_symbols"] = json!(count - selected.len());
            }
            query::check_deadline(deadline)?;
        }
        let manifests = files
            .iter()
            .filter(|(p, _)| p.ends_with("Cargo.toml"))
            .map(|(path, file)| {
                query::check_deadline(deadline)?;
                Ok(toml::from_str::<toml::Value>(&file.content)
                    .ok()
                    .map(|m| (path, m)))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let root = manifests
            .iter()
            .find(|(p, _)| p.as_str() == "Cargo.toml")
            .map(|(_, m)| m);
        let packages = manifests
            .iter()
            .filter_map(|(p, m)| {
                m.get("package")
                    .and_then(|p| p.get("name"))
                    .and_then(toml::Value::as_str)
                    .map(|n| (p.as_str(), n))
            })
            .collect::<BTreeMap<_, _>>();
        let mode = args["dependencies"]
            .as_str()
            .unwrap_or(if args["cycles"] == true {
                "module"
            } else {
                "crate"
            });
        let mut extra_gaps = vec![];
        for (path, manifest) in &manifests {
            query::check_deadline(deadline)?;
            let Some(name) = packages.get(path.as_str()) else {
                continue;
            };
            let dir = path.strip_suffix("Cargo.toml").unwrap();
            let mut targets = BTreeMap::<(String, &str), String>::new();
            if let Some(lib) = manifest.get("lib") {
                targets.insert(
                    (
                        lib.get("name")
                            .and_then(toml::Value::as_str)
                            .unwrap_or(name)
                            .into(),
                        "lib",
                    ),
                    lib.get("path")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("src/lib.rs")
                        .into(),
                );
            } else if files.contains_key(&format!("{dir}src/lib.rs"))
                && manifest["package"]
                    .get("autolib")
                    .and_then(toml::Value::as_bool)
                    != Some(false)
            {
                targets.insert((name.to_string(), "lib"), "src/lib.rs".into());
            }
            if manifest["package"]
                .get("autobins")
                .and_then(toml::Value::as_bool)
                != Some(false)
            {
                if files.contains_key(&format!("{dir}src/main.rs")) {
                    targets.insert((name.to_string(), "bin"), "src/main.rs".into());
                }
                for path in files.keys() {
                    query::check_deadline(deadline)?;
                    let Some(relative) = path.strip_prefix(&format!("{dir}src/bin/")) else {
                        continue;
                    };
                    let target = if !relative.contains('/') {
                        relative.strip_suffix(".rs")
                    } else {
                        relative
                            .strip_suffix("/main.rs")
                            .filter(|p| !p.contains('/'))
                    };
                    if let Some(target) = target {
                        targets.insert((target.into(), "bin"), format!("src/bin/{relative}"));
                    }
                }
            }
            for bin in manifest
                .get("bin")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
            {
                let target = bin
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(name);
                let relative = bin
                    .get("path")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| targets.get(&(target.into(), "bin")).cloned())
                    .unwrap_or_else(|| {
                        if target == *name {
                            "src/main.rs".into()
                        } else {
                            format!("src/bin/{target}.rs")
                        }
                    });
                targets.insert((target.into(), "bin"), relative);
            }
            for ((target, kind), relative) in targets {
                query::check_deadline(deadline)?;
                let file = normalized(&format!("{dir}{relative}"));
                if !included(&file, args) {
                    continue;
                }
                let captured = files.contains_key(&file);
                let mut candidates = vec![];
                for &s in symbols_by_file.get(file.as_str()).into_iter().flatten() {
                    query::check_deadline(deadline)?;
                    if kind == "bin"
                        && s.kind == "function"
                        && s.qualified_name == format!("{file}::main")
                    {
                        candidates.push(s);
                    }
                }
                let main = if candidates.len() == 1 {
                    Some(candidates[0])
                } else {
                    None
                };
                entries.push(json!({"crate":name,"target":target,"kind":kind,"file":file,"main_symbol_id":main.map(|s|&s.symbol_id),"source":"L1","certainty":if captured {"definite"} else {"candidate"},"source_captured":captured,"basis":"Cargo manifest and conventional target paths"}));
                if !captured {
                    extra_gaps.push(json!({"kind":"target_source_not_captured","file":file,"reason":"Declared target path is missing or excluded"}));
                }
            }
            if mode != "crate" || !included(path, args) {
                continue;
            }
            let mut tables = vec![("all".to_owned(), manifest)];
            if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
                tables.extend(
                    targets
                        .iter()
                        .map(|(condition, table)| (condition.clone(), table)),
                );
            }
            for (condition, table) in tables {
                for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    for (alias, declared) in table
                        .get(section)
                        .and_then(toml::Value::as_table)
                        .into_iter()
                        .flatten()
                    {
                        query::check_deadline(deadline)?;
                        let inherited =
                            declared.get("workspace").and_then(toml::Value::as_bool) == Some(true);
                        let value = if inherited {
                            root.and_then(|m| m.get("workspace"))
                                .and_then(|w| w.get("dependencies"))
                                .and_then(|d| d.get(alias))
                                .unwrap_or(declared)
                        } else {
                            declared
                        };
                        let package = value
                            .get("package")
                            .and_then(toml::Value::as_str)
                            .unwrap_or(alias);
                        let local = value.get("path").and_then(toml::Value::as_str).map(|p| {
                            normalized(&format!(
                                "{}{p}/Cargo.toml",
                                if inherited { "" } else { dir }
                            ))
                        });
                        let target = local
                            .as_deref()
                            .and_then(|p| packages.get(p))
                            .copied()
                            .or_else(|| {
                                let relative = value.get("path")?.as_str()?;
                                let metadata: Value = serde_json::from_str(
                                    &files.get(".semweft-inputs/cargo-metadata.json")?.content,
                                )
                                .ok()?;
                                let root =
                                    std::path::Path::new(metadata["workspace_root"].as_str()?);
                                let owner = crate::dependencies::source_path(
                                    root,
                                    &snapshot.manifest,
                                    if inherited { "Cargo.toml" } else { path },
                                );
                                let desired = normalized(
                                    &owner
                                        .parent()?
                                        .join(relative)
                                        .join("Cargo.toml")
                                        .to_string_lossy()
                                        .replace('\\', "/"),
                                );
                                packages
                                    .iter()
                                    .find(|(p, _)| {
                                        normalized(
                                            &crate::dependencies::source_path(
                                                root,
                                                &snapshot.manifest,
                                                p,
                                            )
                                            .to_string_lossy()
                                            .replace('\\', "/"),
                                        ) == desired
                                    })
                                    .map(|(_, name)| *name)
                            });
                        dependencies.push(json!({"from":name,"to":target.unwrap_or("external dependencies"),"dependency":package,"alias":alias,"kind":section,"condition":condition,"workspace_inherited":inherited,"source":"L1","certainty":"candidate","basis":"declared Cargo dependency; features and target activation are not evaluated"}));
                    }
                }
            }
        }
        let relations = engine::relations_with_deadline(stored, deadline)?;
        if mode == "module" {
            let symbols = snapshot
                .syntax
                .symbols
                .iter()
                .map(|s| (s.symbol_id.as_str(), s))
                .collect::<BTreeMap<_, _>>();
            let mut grouped = BTreeMap::<(String, String), Vec<&Relation>>::new();
            for r in &relations {
                query::check_deadline(deadline)?;
                if let Some(target) = symbols.get(r.to_symbol.as_str())
                    && r.file != target.file
                    && included(&r.file, args)
                {
                    grouped
                        .entry((r.file.clone(), target.file.clone()))
                        .or_default()
                        .push(r);
                }
            }
            for ((from, to), edges) in grouped {
                query::check_deadline(deadline)?;
                let definite = edges
                    .iter()
                    .any(|r| r.certainty == Certainty::Definite && r.freshness == "current");
                let sources = edges
                    .iter()
                    .flat_map(|r| r.evidence.iter().map(|e| e.source.clone()))
                    .collect::<BTreeSet<_>>();
                dependencies.push(json!({"from":from,"to":to,"source":if sources.contains("L3") {"L3"} else {"L1"},"sources":sources,"certainty":if definite {"definite"} else {"candidate"},"basis":"indexed symbol relations","relation_ids":edges.iter().map(|r|&r.relation_id).collect::<Vec<_>>()}));
            }
            for s in snapshot.syntax.symbols.iter().filter(|s| {
                s.kind == "module" && s.signature.ends_with(';') && included(&s.file, args)
            }) {
                query::check_deadline(deadline)?;
                let local = s
                    .qualified_name
                    .strip_prefix(&format!("{}::", s.file))
                    .unwrap_or(&s.name)
                    .replace("::", "/");
                let directory = s.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let stem = s.file.rsplit('/').next().unwrap().trim_end_matches(".rs");
                let parent = if matches!(stem, "lib" | "main" | "mod") {
                    directory.to_owned()
                } else {
                    format!("{directory}/{stem}")
                };
                for target in [
                    format!("{parent}/{local}.rs"),
                    format!("{parent}/{local}/mod.rs"),
                ] {
                    if files.contains_key(&target) {
                        dependencies.push(json!({"from":s.file,"to":target,"source":"L1","certainty":"candidate","basis":"conventional Rust module declaration; cfg and path attributes are not evaluated"}));
                    }
                }
            }
        }
        crate::go::augment_map(
            stored,
            args,
            &mut entries,
            &mut dependencies,
            mode,
            deadline,
        )?;
        crate::web::augment_map(
            stored,
            args,
            &mut entries,
            &mut dependencies,
            &mut extra_gaps,
            mode,
            deadline,
        )?;
        if mode == "service" {
            let (mapped, summary) = crate::services::dependencies(stored, args, deadline)?;
            dependencies = mapped;
            result["service_mapping"] = summary;
        }
        let mut incoming = BTreeMap::<&str, BTreeSet<&str>>::new();
        for r in &relations {
            query::check_deadline(deadline)?;
            if r.from_symbol != r.to_symbol {
                incoming
                    .entry(&r.to_symbol)
                    .or_default()
                    .insert(&r.from_symbol);
            }
        }
        let mut core = BTreeMap::new();
        let mut core_count: usize = 0;
        for s in &snapshot.syntax.symbols {
            query::check_deadline(deadline)?;
            if !included(&s.file, args) || query::test_scope(s, stored) == "test" {
                continue;
            }
            core_count += 1;
            core.insert(
                (
                    std::cmp::Reverse(incoming.get(s.symbol_id.as_str()).map_or(0, BTreeSet::len)),
                    query::public_name(s),
                    s.file.clone(),
                    s.name_span,
                    &s.symbol_id,
                ),
                s,
            );
            if core.len() > 20 {
                core.pop_last();
            }
        }
        result["omitted_core_symbols"] = json!(core_count.saturating_sub(20));
        result["core_symbols"] = json!([]);
        for s in core.into_values() {
            query::check_deadline(deadline)?;
            let mut value = query::symbol_value(s, stored, false, false);
            value["incoming_callers"] =
                json!(incoming.get(s.symbol_id.as_str()).map_or(0, BTreeSet::len));
            value["ranking_basis"] = json!(
                "distinct indexed incoming symbols, then qualified name; includes syntax candidates"
            );
            result["core_symbols"].as_array_mut().unwrap().push(value);
        }
        result["dependency_kind"] = json!(mode);
        result["omitted_modules"] = json!(omitted_modules);
        if !omitted_modules.is_empty() {
            result["output"] = json!({"truncated":true,"truncation_reason":"depth_limit","continuation":{"kind":"requery","suggested":{"depth":(depth+1).min(10)}}});
        }
        result["completeness"]["known_gaps"]
            .as_array_mut()
            .unwrap()
            .extend(extra_gaps);
        result["evidence_summary"] = query::summary(&relations, "complete");
        result["analysis_health"] = health(stored, &relations, deadline)?;
        query::check_deadline(deadline)?;
        Ok(())
    })();
    result["modules"] = Value::Array(modules);
    result["entries"] = Value::Array(entries);
    result["dependencies"] = Value::Array(dependencies);
    computed
}

fn health(stored: &StoredView, relations: &[Relation], deadline: Instant) -> Result<Value> {
    let facts = &stored.view.snapshot.syntax;
    let parsed = stored
        .view
        .snapshot
        .manifest
        .files
        .keys()
        .filter(|p| p.ends_with(".rs") || p.ends_with(".go") || crate::web::is_source(p))
        .count();
    let failed = facts
        .diagnostics
        .iter()
        .filter(|d| d.code == "parse_error")
        .filter_map(|d| d.file.clone())
        .collect::<BTreeSet<_>>();
    let mut resolved = BTreeSet::new();
    for r in relations {
        query::check_deadline(deadline)?;
        if r.freshness == "current" && r.evidence.iter().any(|e| e.source == "L3") {
            resolved.insert((&r.file, r.span));
        }
    }
    let mut unresolved = vec![];
    for o in &facts.occurrences {
        query::check_deadline(deadline)?;
        if !resolved.contains(&(&o.file, o.span)) {
            unresolved.push(o);
        }
    }
    // Full diagnostic detail lives here; query responses carry only counted samples.
    let diagnostics_all = facts.diagnostics.iter().chain(stored.reference_gaps.iter());
    let mut diagnostic_counts = BTreeMap::<&str, usize>::new();
    for d in diagnostics_all.clone() {
        *diagnostic_counts.entry(d.code.as_str()).or_default() += 1;
    }
    let diagnostic_total = diagnostic_counts.values().sum::<usize>();
    let diagnostics = diagnostics_all
        .take(1000)
        .map(|d| json!({"kind":d.code,"file":d.file,"reason":d.message}))
        .collect::<Vec<_>>();
    let omitted_diagnostics = diagnostic_total.saturating_sub(diagnostics.len());
    Ok(
        json!({"source_files":parsed,"parse_failed_files":failed,"syntax_symbols":facts.symbols.len(),"syntax_calls":facts.calls.len(),
        "unresolved_occurrence_count":unresolved.len(),"unresolved_occurrences":unresolved.iter().take(1000).map(|o|json!({"name":o.name,"kind":o.kind,"file":o.file,"span":o.span,"reason":"No current semantic binding was captured for this occurrence; local names and external symbols may be outside the model"})).collect::<Vec<_>>(),
        "omitted_unresolved_occurrences":unresolved.len().saturating_sub(1000),"evidence_summary":query::summary(relations,"complete"),"diagnostic_counts":diagnostic_counts,"diagnostics":diagnostics,"omitted_diagnostics":omitted_diagnostics}),
    )
}
