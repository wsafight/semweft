use crate::{engine::StoredView, query};
use anyhow::{Context, Result};
use semweft_core::{SourceFile, Symbol};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};
use tree_sitter::Node;

const MAX_ENTRIES: usize = 10_000;

#[derive(Clone, PartialEq, Eq)]
struct Binding {
    symbol: String,
    scope: Option<String>,
    public: bool,
    exportable: bool,
    conditional: bool,
    evidence: Vec<Value>,
}

struct Import {
    scope: String,
    root: String,
    path: Vec<String>,
    alias: String,
    public: bool,
    conditional: bool,
    evidence: Value,
}

struct Inherent {
    scope: String,
    root: String,
    target: Vec<String>,
    members: Vec<(String, Binding)>,
}

#[derive(Default)]
struct Model {
    scopes: BTreeMap<String, BTreeMap<String, Vec<Binding>>>,
    roots: Vec<(String, String)>,
    imports: Vec<Import>,
    inherent: Vec<Inherent>,
    visited: BTreeSet<(String, String)>,
    gaps: BTreeSet<String>,
    entries: usize,
}

#[derive(Default)]
pub(crate) struct Inventory {
    pub by_symbol: BTreeMap<String, Vec<Value>>,
    pub exports: BTreeMap<(String, String), Value>,
    pub gaps: BTreeSet<String>,
}

fn text<'a>(node: Node<'_>, file: &'a SourceFile) -> &'a str {
    &file.content[node.byte_range()]
}

fn attributes(node: Node<'_>, file: &SourceFile) -> Vec<String> {
    let mut attrs = vec![];
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        if sibling.kind() == "attribute_item" {
            attrs.push(
                text(sibling, file)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect(),
            );
        } else if !sibling.kind().ends_with("comment") {
            break;
        }
        previous = sibling.prev_named_sibling();
    }
    attrs
}

fn source_evidence(path: &str, file: &SourceFile, node: Node<'_>, kind: &str) -> Value {
    json!({"kind":kind,"file":path,"file_hash":file.file_hash,
        "line":node.start_position().row+1,"column":node.start_position().column+1,
        "attributes":attributes(node,file)})
}

fn joined(base: &str, path: &str) -> Option<String> {
    if path.starts_with('/') || path.contains(['\\', ':']) {
        return None;
    }
    let mut parts = base
        .split('/')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn path_parts(path: &str) -> Vec<String> {
    path.split("::")
        .filter(|part| !part.trim().is_empty())
        .map(|part| part.trim().trim_start_matches("r#").into())
        .collect()
}

fn uses(
    node: Node<'_>,
    file: &SourceFile,
    prefix: &[String],
    out: &mut Vec<(Vec<String>, String)>,
) {
    match node.kind() {
        "scoped_use_list" => {
            let mut prefix = prefix.to_vec();
            if let Some(path) = node.child_by_field_name("path") {
                prefix.extend(path_parts(text(path, file)));
            }
            if let Some(list) = node.child_by_field_name("list") {
                uses(list, file, &prefix, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                uses(child, file, prefix, out);
            }
        }
        "use_as_clause" => {
            if let (Some(path), Some(alias)) = (
                node.child_by_field_name("path"),
                node.child_by_field_name("alias"),
            ) {
                let mut target = prefix.to_vec();
                target.extend(path_parts(text(path, file)));
                if target.last().is_some_and(|s| s == "self") {
                    target.pop();
                }
                out.push((target, text(alias, file).trim_start_matches("r#").into()));
            }
        }
        _ => {
            let mut target = prefix.to_vec();
            target.extend(path_parts(text(node, file)));
            if target.last().is_some_and(|s| s == "self") {
                target.pop();
            }
            if let Some(alias) = target.last().cloned() {
                out.push((target, alias));
            }
        }
    }
}

impl Model {
    fn insert(&mut self, scope: &str, name: &str, binding: Binding) -> bool {
        let bindings = self
            .scopes
            .entry(scope.into())
            .or_default()
            .entry(name.into())
            .or_default();
        if bindings.iter().any(|old| {
            old.symbol == binding.symbol
                && old.scope == binding.scope
                && old.public == binding.public
                && old.conditional == binding.conditional
        }) {
            return false;
        }
        if self.entries >= MAX_ENTRIES {
            self.gaps.insert("binding_budget_exceeded".into());
            return false;
        }
        bindings.push(binding);
        self.entries += 1;
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_file(
        &mut self,
        stored: &StoredView,
        path: &str,
        scope: &str,
        root: &str,
        module_dir: &str,
        conditional: bool,
        deadline: Instant,
    ) -> Result<()> {
        query::check_deadline(deadline)?;
        if !self.visited.insert((path.into(), scope.into())) {
            return Ok(());
        }
        if self.visited.len() > MAX_ENTRIES || scope.matches("::").count() > 64 {
            self.gaps.insert("module_budget_exceeded".into());
            return Ok(());
        }
        let Some(file) = stored.view.snapshot.manifest.files.get(path) else {
            self.gaps.insert(format!("module_not_captured:{path}"));
            return Ok(());
        };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
        let tree = parser
            .parse(&file.content, None)
            .context("timeout: API parse cancelled")?;
        if tree.root_node().has_error() {
            self.gaps.insert(format!("parse_error:{path}"));
        }
        self.children(
            stored,
            path,
            file,
            tree.root_node(),
            scope,
            root,
            module_dir,
            conditional,
            false,
            deadline,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn children(
        &mut self,
        stored: &StoredView,
        path: &str,
        file: &SourceFile,
        container: Node<'_>,
        scope: &str,
        root: &str,
        module_dir: &str,
        inherited_conditional: bool,
        implicit_public: bool,
        deadline: Instant,
    ) -> Result<()> {
        if scope.matches("::").count() > 64 {
            self.gaps.insert("module_depth_exceeded".into());
            return Ok(());
        }
        let mut cursor = container.walk();
        for node in container.named_children(&mut cursor) {
            query::check_deadline(deadline)?;
            let attrs = attributes(node, file);
            if attrs.iter().any(|a| a == "#[cfg(test)]" || a == "#[test]") {
                continue;
            }
            let conditional = inherited_conditional
                || attrs
                    .iter()
                    .any(|a| a.starts_with("#[cfg(") || a.starts_with("#[cfg_attr("));
            if node.kind() == "macro_invocation" {
                self.gaps.insert(format!(
                    "macro_not_expanded:{path}:{}",
                    node.start_position().row + 1
                ));
            }
            let mut visibility_cursor = node.walk();
            let public = implicit_public
                || node.named_children(&mut visibility_cursor).any(|child| {
                    child.kind() == "visibility_modifier" && text(child, file) == "pub"
                });
            if node.kind() == "use_declaration" {
                if let Some(argument) = node.child_by_field_name("argument") {
                    let mut imports = vec![];
                    uses(argument, file, &[], &mut imports);
                    for (target, alias) in imports {
                        if self.imports.len() >= MAX_ENTRIES {
                            self.gaps.insert("import_budget_exceeded".into());
                            break;
                        }
                        self.imports.push(Import {
                            scope: scope.into(),
                            root: root.into(),
                            path: target,
                            alias,
                            public,
                            conditional,
                            evidence: source_evidence(path, file, node, "reexport"),
                        });
                    }
                }
                continue;
            }
            if node.kind() == "impl_item" {
                if node.child_by_field_name("trait").is_none()
                    && let (Some(target), Some(body)) = (
                        node.child_by_field_name("type"),
                        node.child_by_field_name("body"),
                    )
                {
                    let target = if target.kind() == "generic_type" {
                        target.child_by_field_name("type").unwrap_or(target)
                    } else {
                        target
                    };
                    if matches!(target.kind(), "type_identifier" | "scoped_type_identifier") {
                        let temporary = format!("{scope}::$impl:{}:{}", path, node.start_byte());
                        self.children(
                            stored,
                            path,
                            file,
                            body,
                            &temporary,
                            root,
                            module_dir,
                            conditional,
                            false,
                            deadline,
                        )?;
                        let members = self
                            .scopes
                            .remove(&temporary)
                            .unwrap_or_default()
                            .into_iter()
                            .flat_map(|(name, bindings)| {
                                bindings
                                    .into_iter()
                                    .map(move |binding| (name.clone(), binding))
                            })
                            .collect();
                        self.inherent.push(Inherent {
                            scope: scope.into(),
                            root: root.into(),
                            target: path_parts(text(target, file)),
                            members,
                        });
                    } else {
                        self.gaps.insert(format!(
                            "impl_type_not_resolved:{path}:{}",
                            node.start_position().row + 1
                        ));
                    }
                }
                continue;
            }
            let Some(name_node) = node.child_by_field_name("name") else {
                continue;
            };
            let Some(symbol) = stored
                .index()
                .symbols_in_file(path)
                .into_iter()
                .find(|symbol| {
                    symbol.name_span.start.line as usize == name_node.start_position().row + 1
                        && symbol.name_span.start.column as usize
                            == name_node.start_position().column + 1
                })
            else {
                continue;
            };
            if stored
                .view
                .snapshot
                .syntax
                .declarations
                .get(&symbol.symbol_id)
                .is_some_and(|info| info.test_only)
            {
                continue;
            }
            let name = symbol.name.trim_start_matches("r#");
            let child_scope = format!("{scope}::{name}");
            let container_item = matches!(
                node.kind(),
                "mod_item" | "struct_item" | "enum_item" | "trait_item"
            );
            self.insert(
                scope,
                name,
                Binding {
                    symbol: symbol.symbol_id.clone(),
                    scope: container_item.then(|| child_scope.clone()),
                    public,
                    exportable: public,
                    conditional,
                    evidence: vec![source_evidence(path, file, node, "declaration")],
                },
            );
            match node.kind() {
                "mod_item" => {
                    let child_dir = joined(module_dir, name).unwrap();
                    if let Some(body) = node.child_by_field_name("body") {
                        self.children(
                            stored,
                            path,
                            file,
                            body,
                            &child_scope,
                            root,
                            &child_dir,
                            conditional,
                            false,
                            deadline,
                        )?;
                    } else {
                        let explicit = attrs.iter().find_map(|attr| {
                            attr.strip_prefix("#[path=")
                                .and_then(|attr| attr.strip_suffix(']'))
                        });
                        let target = if let Some(literal) = explicit {
                            let mut ancestor = node.parent();
                            let mut inline = false;
                            while let Some(parent) = ancestor {
                                inline |= parent.kind() == "mod_item";
                                ancestor = parent.parent();
                            }
                            let parent = if inline {
                                module_dir
                            } else {
                                path.rsplit_once('/').map_or("", |(directory, _)| directory)
                            };
                            serde_json::from_str::<String>(literal)
                                .ok()
                                .and_then(|relative| joined(parent, &relative))
                        } else {
                            let matches =
                                [format!("{child_dir}.rs"), format!("{child_dir}/mod.rs")]
                                    .into_iter()
                                    .filter(|path| {
                                        stored.view.snapshot.manifest.files.contains_key(path)
                                    })
                                    .collect::<Vec<_>>();
                            if matches.len() == 1 {
                                matches.into_iter().next()
                            } else {
                                None
                            }
                        };
                        if let Some(target) = target {
                            let target_dir = if explicit.is_some() {
                                target
                                    .rsplit_once('/')
                                    .map_or("", |(dir, _)| dir)
                                    .to_string()
                            } else {
                                child_dir
                            };
                            self.parse_file(
                                stored,
                                &target,
                                &child_scope,
                                root,
                                &target_dir,
                                conditional,
                                deadline,
                            )?;
                        } else {
                            self.gaps
                                .insert(format!("module_path_unresolved:{path}:{name}"));
                        }
                    }
                }
                "trait_item" | "enum_item" | "struct_item" => {
                    if let Some(body) = node.child_by_field_name("body") {
                        self.children(
                            stored,
                            path,
                            file,
                            body,
                            &child_scope,
                            root,
                            module_dir,
                            conditional,
                            node.kind() != "struct_item",
                            deadline,
                        )?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn resolve(&self, scope: &str, root: &str, path: &[String]) -> Vec<Binding> {
        let mut current = scope.to_string();
        let mut offset = 0;
        if path.first().is_some_and(|part| part == "crate") {
            current = root.into();
            offset += 1;
        } else if path.first().is_some_and(|part| part == "self") {
            offset += 1;
        }
        while path.get(offset).is_some_and(|part| part == "super") {
            if current == root {
                return vec![];
            }
            let Some((parent, _)) = current.rsplit_once("::") else {
                return vec![];
            };
            current = parent.into();
            offset += 1;
        }
        let mut scopes = vec![current];
        let mut bindings = vec![];
        for part in &path[offset..] {
            bindings = scopes
                .iter()
                .filter_map(|scope| self.scopes.get(scope)?.get(part))
                .flatten()
                .cloned()
                .collect();
            scopes = bindings
                .iter()
                .filter_map(|binding| binding.scope.clone())
                .collect();
        }
        bindings
    }

    fn resolve_imports(&mut self, deadline: Instant) -> Result<()> {
        let imports = std::mem::take(&mut self.imports);
        let mut resolved = BTreeSet::new();
        for _ in 0..64 {
            let mut changed = false;
            for (index, import) in imports.iter().enumerate() {
                query::check_deadline(deadline)?;
                let wildcard = import.alias == "*";
                let path = if wildcard {
                    &import.path[..import.path.len() - 1]
                } else {
                    &import.path[..]
                };
                let targets = self.resolve(&import.scope, &import.root, path);
                let mut members = vec![];
                if wildcard {
                    for target in targets {
                        if let Some(scope) = target.scope.and_then(|scope| self.scopes.get(&scope))
                        {
                            members.extend(scope.iter().flat_map(|(name, bindings)| {
                                bindings
                                    .iter()
                                    .filter(|binding| binding.public)
                                    .map(move |binding| (name.clone(), binding.clone()))
                            }));
                        }
                    }
                } else {
                    members.extend(
                        targets
                            .into_iter()
                            .map(|target| (import.alias.clone(), target)),
                    );
                }
                for (name, mut binding) in members {
                    if import.public && !binding.exportable {
                        continue;
                    }
                    resolved.insert(index);
                    binding.public = import.public;
                    binding.conditional |= import.conditional;
                    if binding.evidence.len() < 64 {
                        binding.evidence.push(import.evidence.clone());
                    }
                    changed |= self.insert(&import.scope, &name, binding);
                }
            }
            if !changed {
                break;
            }
        }
        for (index, import) in imports.iter().enumerate() {
            if import.public && !resolved.contains(&index) {
                self.gaps.insert(format!(
                    "reexport_unresolved:{}:{}",
                    import.scope,
                    import.path.join("::")
                ));
            }
        }
        for implementation in std::mem::take(&mut self.inherent) {
            let targets = self.resolve(
                &implementation.scope,
                &implementation.root,
                &implementation.target,
            );
            let scopes = targets
                .iter()
                .filter_map(|target| target.scope.clone())
                .collect::<BTreeSet<_>>();
            if scopes.len() != 1 {
                self.gaps.insert(format!(
                    "impl_target_unresolved:{}:{}",
                    implementation.scope,
                    implementation.target.join("::")
                ));
                continue;
            }
            for (name, binding) in implementation.members {
                self.insert(scopes.first().unwrap(), &name, binding);
            }
        }
        Ok(())
    }
}

pub(crate) fn build(stored: &StoredView, deadline: Instant) -> Result<Inventory> {
    let mut model = Model::default();
    let files = &stored.view.snapshot.manifest.files;
    for (path, file) in files
        .iter()
        .filter(|(path, _)| path.ends_with("Cargo.toml"))
    {
        query::check_deadline(deadline)?;
        let Ok(manifest) = toml::from_str::<toml::Value>(&file.content) else {
            continue;
        };
        let Some(package) = manifest.get("package") else {
            continue;
        };
        let Some(name) = manifest
            .get("lib")
            .and_then(|lib| lib.get("name"))
            .or_else(|| package.get("name"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        let directory = path
            .strip_suffix("Cargo.toml")
            .unwrap()
            .trim_end_matches('/');
        let explicit = manifest
            .get("lib")
            .and_then(|lib| lib.get("path"))
            .and_then(toml::Value::as_str);
        if explicit.is_none()
            && package.get("autolib").and_then(toml::Value::as_bool) == Some(false)
        {
            continue;
        }
        let Some(target) = joined(directory, explicit.unwrap_or("src/lib.rs")) else {
            model
                .gaps
                .insert(format!("library_target_outside_capture:{path}"));
            continue;
        };
        if !files.contains_key(&target) {
            if explicit.is_some() {
                model
                    .gaps
                    .insert(format!("library_target_not_captured:{target}"));
            }
            continue;
        }
        let root = format!("{path}::$crate");
        model.roots.push((root.clone(), name.replace('-', "_")));
        let module_dir = target
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory);
        model.parse_file(stored, &target, &root, &root, module_dir, false, deadline)?;
    }
    model.resolve_imports(deadline)?;
    let mut output = Inventory {
        gaps: model.gaps.clone(),
        ..Default::default()
    };
    let mut pending = model
        .roots
        .iter()
        .map(|(scope, path)| {
            (
                scope.clone(),
                path.clone(),
                BTreeSet::<String>::new(),
                false,
                Vec::<Value>::new(),
            )
        })
        .collect::<Vec<_>>();
    let mut traversed = 0;
    'exports: while let Some((scope, path, visited, conditional, evidence)) = pending.pop() {
        query::check_deadline(deadline)?;
        traversed += 1;
        if traversed > MAX_ENTRIES || output.exports.len() >= MAX_ENTRIES {
            output.gaps.insert("export_budget_exceeded".into());
            break;
        }
        for (name, bindings) in model.scopes.get(&scope).into_iter().flatten() {
            for binding in bindings.iter().filter(|binding| binding.public) {
                query::check_deadline(deadline)?;
                if output.exports.len() >= MAX_ENTRIES {
                    output.gaps.insert("export_budget_exceeded".into());
                    break 'exports;
                }
                if visited.contains(&binding.symbol) {
                    output
                        .gaps
                        .insert(format!("recursive_export:{path}::{name}"));
                    continue;
                }
                let Some(symbol) = stored.index().symbol(&binding.symbol) else {
                    continue;
                };
                let export_path = format!("{path}::{name}");
                let conditional = conditional || binding.conditional;
                let mut chain = evidence.clone();
                chain.extend(binding.evidence.clone());
                let conditions = chain
                    .iter()
                    .flat_map(|evidence| evidence["attributes"].as_array().into_iter().flatten())
                    .filter_map(Value::as_str)
                    .filter(|attr| attr.starts_with("#[cfg(") || attr.starts_with("#[cfg_attr("))
                    .collect::<BTreeSet<_>>();
                let export = json!({"path":export_path,"symbol_id":symbol.symbol_id,"certainty":"candidate",
                    "conditional":conditional,"conditions":conditions,"evidence":chain,"declaration":query::symbol_value(symbol,stored,false,false)});
                output.exports.insert(
                    (export_path.clone(), binding.symbol.clone()),
                    export.clone(),
                );
                output
                    .by_symbol
                    .entry(binding.symbol.clone())
                    .or_default()
                    .push(export);
                if let Some(scope) = &binding.scope {
                    let mut visited = visited.clone();
                    visited.insert(binding.symbol.clone());
                    pending.push((scope.clone(), export_path, visited, conditional, chain));
                }
            }
        }
    }
    for exports in output.by_symbol.values_mut() {
        exports.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        exports.dedup_by(|a, b| a["path"] == b["path"]);
    }
    Ok(output)
}

impl Inventory {
    pub fn estimated_heap_bytes(&self) -> usize {
        let exports = self.exports.values().collect::<Vec<_>>();
        semweft_store::payload_bytes(&(&self.by_symbol, &exports, &self.gaps)).saturating_mul(2)
    }

    pub fn exposure(&self, symbol: &Symbol) -> Value {
        json!({"external_reachability":if self.by_symbol.contains_key(&symbol.symbol_id) {"candidate"} else {"not_observed"},
            "paths":self.by_symbol.get(&symbol.symbol_id).cloned().unwrap_or_default(),
            "basis":"captured_rust_library_module_and_reexport_graph","semantic_validation":false})
    }

    pub fn summary(&self, filtered: usize) -> Value {
        json!({"declaration_count":filtered,"candidate_declarations":self.by_symbol.len(),"export_path_count":self.exports.len(),
            "basis":"captured_rust_library_module_and_reexport_graph","external_reachability":"candidate",
            "reexports_expanded":true,"includes_test_declarations":false,"known_gaps":self.gaps,
            "semantic_validation":false,"languages":["rust"],
            "limitations":["Conditional compilation is reported, not evaluated", "Macro/generated items and external crates are not expanded", "Type aliases and trait implementations do not synthesize inherited members", "Candidates require compiler validation; absence is not a proof of private API"]})
    }
}

fn comparison(export: &Value) -> Value {
    let declaration = &export["declaration"];
    json!({"declaration":declaration["syntax_qualified_name"],"kind":declaration["kind"],
        "signature":declaration["signature"],"conditions":export["conditions"]})
}

pub(crate) fn diff(
    result: &mut Value,
    base: &StoredView,
    head: &StoredView,
    deadline: Instant,
) -> Result<()> {
    let before = base.api_surface(deadline)?;
    let after = head.api_surface(deadline)?;
    for change in result["changes"].as_array_mut().into_iter().flatten() {
        for (side, inventory, stored) in [("base", &before, base), ("head", &after, head)] {
            if let Some(symbol) = change[format!("{side}_symbol")]["symbol_id"]
                .as_str()
                .and_then(|id| stored.index().symbol(id))
            {
                change["public_api_review"][format!("{side}_exposure")] =
                    inventory.exposure(symbol);
            }
        }
    }
    let group = |inventory: &Inventory| {
        let mut paths = BTreeMap::<String, Vec<Value>>::new();
        for ((path, _), export) in &inventory.exports {
            paths.entry(path.clone()).or_default().push(export.clone());
        }
        for exports in paths.values_mut() {
            exports.sort_by_key(|export| comparison(export).to_string());
        }
        paths
    };
    let before_paths = group(before);
    let after_paths = group(after);
    let mut changes = vec![];
    for path in before_paths
        .keys()
        .chain(after_paths.keys())
        .collect::<BTreeSet<_>>()
    {
        query::check_deadline(deadline)?;
        let before = before_paths.get(path).cloned().unwrap_or_default();
        let after = after_paths.get(path).cloned().unwrap_or_default();
        if before.iter().map(comparison).collect::<Vec<_>>()
            == after.iter().map(comparison).collect::<Vec<_>>()
        {
            continue;
        }
        let kind = if before.is_empty() {
            "added"
        } else if after.is_empty() {
            "removed"
        } else {
            "changed"
        };
        changes.push(json!({"path":path,"kind":kind,"base_exports":before,"head_exports":after,"certainty":"candidate","compatibility":"not_evaluated"}));
    }
    result["api_surface_summary"] = json!({"base":before.summary(before.by_symbol.len()),"head":after.summary(after.by_symbol.len()),
        "change_count":changes.len(),"compatibility":"not_evaluated","semantic_validation":false});
    result["api_changes"] = json!(changes);
    Ok(())
}
