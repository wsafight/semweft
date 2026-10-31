//! Rust syntax extraction. Name bindings are delegated to a semantic backend.
use semweft_core::*;
use std::collections::BTreeMap;
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

pub const PARSER_VERSION: &str = "tree-sitter-rust@0.24;extractor@0.2-impls";

pub struct RustParser {
    parser: Parser,
    trees: BTreeMap<String, (String, Tree)>,
    file_facts: BTreeMap<String, (String, SyntaxFacts)>,
    pub files_extracted: usize,
}

impl RustParser {
    pub fn new() -> Result<Self, String> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|e| e.to_string())?;
        Ok(Self {
            parser,
            trees: BTreeMap::new(),
            file_facts: BTreeMap::new(),
            files_extracted: 0,
        })
    }

    pub fn analyze(&mut self, manifest: &Manifest) -> Result<SyntaxFacts, String> {
        self.files_extracted = 0;
        self.file_facts
            .retain(|path, _| manifest.files.contains_key(path));
        self.trees
            .retain(|path, _| manifest.files.contains_key(path));
        let mut facts = SyntaxFacts::default();
        for (path, file) in &manifest.files {
            if !path.ends_with(".rs") {
                continue;
            }
            if let Some((hash, cached)) = self.file_facts.get(path)
                && hash == &file.file_hash
                && self
                    .trees
                    .get(path)
                    .is_some_and(|(text, _)| text == &file.content)
            {
                facts.extend(cached);
                continue;
            }
            self.files_extracted += 1;
            let mut file_facts = SyntaxFacts::default();
            let old = self.trees.get_mut(path);
            let tree = if let Some((text, tree)) = old {
                if text == &file.content {
                    tree.clone()
                } else {
                    tree.edit(&edit_between(text, &file.content));
                    self.parser
                        .parse(&file.content, Some(tree))
                        .ok_or("parse_cancelled")?
                }
            } else {
                self.parser
                    .parse(&file.content, None)
                    .ok_or("parse_cancelled")?
            };
            extract(
                tree.root_node(),
                path,
                file,
                &[],
                None,
                false,
                &mut file_facts,
            );
            file_facts
                .skeletons
                .insert(path.clone(), skeleton(tree.root_node(), file));
            facts.extend(&file_facts);
            self.file_facts
                .insert(path.clone(), (file.file_hash.clone(), file_facts));
            self.trees
                .insert(path.clone(), (file.content.clone(), tree));
        }
        facts.symbols.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));
        facts
            .calls
            .sort_by(|a, b| a.callsite_id.cmp(&b.callsite_id));
        facts.diagnostics.sort();
        facts.diagnostics.dedup();
        facts.impls.sort_by(|a, b| {
            (&a.file, a.span.start, &a.impl_id).cmp(&(&b.file, b.span.start, &b.impl_id))
        });
        Ok(facts)
    }
}

fn position(point: Point) -> Position {
    Position {
        line: point.row as u32 + 1,
        column: point.column as u32 + 1,
    }
}
fn span(node: Node<'_>) -> Span {
    Span {
        start: position(node.start_position()),
        end: position(node.end_position()),
    }
}
fn source<'a>(node: Node<'_>, file: &'a SourceFile) -> &'a str {
    &file.content[node.byte_range()]
}

fn extract(
    node: Node<'_>,
    path: &str,
    file: &SourceFile,
    scope: &[String],
    caller: Option<&str>,
    in_trait: bool,
    facts: &mut SyntaxFacts,
) {
    if node.is_error() || node.is_missing() {
        facts.diagnostics.push(Diagnostic {
            code: "parse_error".into(),
            file: Some(path.into()),
            message: format!(
                "{} at {}:{}",
                node.kind(),
                node.start_position().row + 1,
                node.start_position().column + 1
            ),
        });
    }
    if node.kind() == "macro_invocation" {
        facts.diagnostics.push(Diagnostic {
            code: "macro_not_expanded".into(),
            file: Some(path.into()),
            message: source(node, file).into(),
        });
        return;
    }
    let mut nested_scope = scope.to_vec();
    let mut nested_caller = caller.map(str::to_owned);
    let nested_trait = node.kind() == "trait_item" || (in_trait && node.kind() != "impl_item");
    let trait_impl = (node.kind() == "impl_item")
        .then(|| node.child_by_field_name("trait"))
        .flatten()
        .map(|trait_node| (trait_node, facts.symbols.len()));
    if node.kind() == "impl_item" {
        let ty = node
            .child_by_field_name("type")
            .map(|n| source(n, file))
            .unwrap_or("?");
        let name = node
            .child_by_field_name("trait")
            .map(|n| format!("<{} for {ty}>", source(n, file)))
            .unwrap_or_else(|| ty.into());
        nested_scope.push(name);
    }
    let kind = match node.kind() {
        "function_item" | "function_signature_item" => {
            Some(if in_trait { "trait_method" } else { "function" })
        }
        "struct_item" => Some("struct"),
        "enum_item" => Some("enum"),
        "trait_item" => Some("trait"),
        "mod_item" => Some("module"),
        "type_item" => Some("type"),
        "const_item" => Some("const"),
        "static_item" => Some("static"),
        "field_declaration" => Some("field"),
        "enum_variant" => Some("variant"),
        _ => None,
    };
    if let (Some(kind), Some(name_node)) = (kind, node.child_by_field_name("name")) {
        let name = source(name_node, file).to_string();
        nested_scope.push(name.clone());
        let qualified_name = format!("{path}::{}", nested_scope.join("::"));
        let symbol_id = identity(
            "sym",
            &[&qualified_name, kind, &name_node.start_byte().to_string()],
        );
        let signature_end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |body| body.start_byte());
        facts.symbols.push(Symbol {
            symbol_id: symbol_id.clone(),
            name,
            qualified_name,
            kind: kind.into(),
            file: path.into(),
            file_hash: file.file_hash.clone(),
            span: span(node),
            name_span: span(name_node),
            signature: file.content[node.start_byte()..signature_end].trim().into(),
        });
        facts
            .declarations
            .insert(symbol_id.clone(), declaration_info(node, path, file));
        // Types, fields and initializers also own references used by impact analysis.
        nested_caller = Some(symbol_id);
    }
    if node.kind() == "closure_expression" {
        nested_caller = None;
        facts.diagnostics.push(Diagnostic {
            code: "closure_owner_not_modeled".into(),
            file: Some(path.into()),
            message: format!("closure at line {}", node.start_position().row + 1),
        });
    }
    if node.kind() == "call_expression"
        && let Some(function) = node.child_by_field_name("function")
    {
        let (target_node, kind) = match function.kind() {
            "identifier" | "scoped_identifier" => (last_name(function), CallKind::Direct),
            "generic_function" => {
                let inner = function.child_by_field_name("function").unwrap_or(function);
                match inner.kind() {
                    "identifier" | "scoped_identifier" => (last_name(inner), CallKind::Direct),
                    "field_expression" => (
                        inner.child_by_field_name("field").unwrap_or(inner),
                        CallKind::Method,
                    ),
                    _ => (inner, CallKind::Indirect),
                }
            }
            "field_expression" => (
                function.child_by_field_name("field").unwrap_or(function),
                CallKind::Method,
            ),
            _ => (function, CallKind::Indirect),
        };
        let call_span = span(target_node);
        facts.calls.push(CallSite {
            callsite_id: identity("call", &[path, &serde_span(call_span)]),
            caller_id: nested_caller.clone(),
            file: path.into(),
            file_hash: file.file_hash.clone(),
            span: call_span,
            expression: source(function, file).into(),
            kind,
        });
    }
    if matches!(
        node.kind(),
        "identifier" | "type_identifier" | "field_identifier"
    ) {
        let declaration = facts
            .symbols
            .iter()
            .any(|s| s.file == path && s.name_span == span(node));
        if !declaration {
            let mut ancestor = node.parent();
            let mut kind = if node.kind() == "type_identifier" {
                "type"
            } else {
                "read"
            };
            while let Some(parent) = ancestor {
                if parent.kind() == "use_declaration" {
                    kind = "import";
                    break;
                }
                if matches!(
                    parent.kind(),
                    "assignment_expression" | "compound_assignment_expr"
                ) && parent
                    .child_by_field_name("left")
                    .is_some_and(|left| left.byte_range().contains(&node.start_byte()))
                {
                    kind = "write";
                    break;
                }
                if matches!(parent.kind(), "function_item" | "block") {
                    break;
                }
                ancestor = parent.parent();
            }
            if facts
                .calls
                .iter()
                .any(|call| call.file == path && call.span == span(node))
            {
                kind = "call";
            }
            facts.occurrences.push(Occurrence {
                name: source(node, file).into(),
                file: path.into(),
                span: span(node),
                kind: kind.into(),
                enclosing_symbol_id: nested_caller.clone(),
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        extract(
            child,
            path,
            file,
            &nested_scope,
            nested_caller.as_deref(),
            nested_trait,
            facts,
        );
    }
    if let Some((trait_node, first_symbol)) = trait_impl {
        let block = impl_block(node, trait_node, path, file, first_symbol, facts);
        facts.impls.push(block);
    }
}

/// Record `impl Trait for Type` with the methods its body declares directly.
fn impl_block(
    node: Node<'_>,
    trait_node: Node<'_>,
    path: &str,
    file: &SourceFile,
    first_symbol: usize,
    facts: &SyntaxFacts,
) -> ImplBlock {
    // `Trait<T>` keeps the written path; `a::Trait` binds at its final segment.
    let trait_path_node = if trait_node.kind() == "generic_type" {
        trait_node.child_by_field_name("type").unwrap_or(trait_node)
    } else {
        trait_node
    };
    let trait_name_node = last_name(trait_path_node);
    let self_type = node
        .child_by_field_name("type")
        .map(|n| source(n, file).to_owned())
        .unwrap_or_default();
    let generics =
        node.child_by_field_name("type_parameters")
            .map_or_else(Vec::new, |parameters| {
                let mut cursor = parameters.walk();
                parameters
                    .named_children(&mut cursor)
                    .filter(|parameter| parameter.kind() == "type_parameter")
                    .filter_map(|parameter| parameter.child_by_field_name("name"))
                    .map(|name| source(name, file).to_owned())
                    .collect()
            });
    let mut cursor = node.walk();
    let modifiers: Vec<_> = node
        .children(&mut cursor)
        .take_while(|child| child.id() != trait_node.id())
        .map(|child| child.kind())
        .collect();
    let methods = node
        .child_by_field_name("body")
        .map_or_else(Vec::new, |body| {
            let mut cursor = body.walk();
            body.named_children(&mut cursor)
                .filter(|child| child.kind() == "function_item")
                .filter_map(|method| method.child_by_field_name("name"))
                .filter_map(|name| {
                    facts.symbols[first_symbol..]
                        .iter()
                        .find(|symbol| symbol.kind == "function" && symbol.name_span == span(name))
                })
                .map(|symbol| symbol.symbol_id.clone())
                .collect()
        });
    let trait_path = source(trait_path_node, file).to_owned();
    ImplBlock {
        impl_id: identity(
            "impl",
            &[
                path,
                &trait_path,
                &self_type,
                &node.start_byte().to_string(),
            ],
        ),
        file: path.into(),
        file_hash: file.file_hash.clone(),
        span: span(node),
        trait_path,
        trait_span: span(trait_name_node),
        self_type,
        generics,
        negative: modifiers.contains(&"!"),
        unsafe_impl: modifiers.contains(&"unsafe"),
        test_only: declaration_info(node, path, file).test_only,
        methods,
    }
}

fn declaration_info(node: Node<'_>, path: &str, file: &SourceFile) -> DeclarationInfo {
    let mut docs = vec![];
    let mut attributes = vec![];
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        let text = source(previous, file).trim();
        if let Some(line) = text
            .strip_prefix("///")
            .filter(|line| !line.starts_with('/'))
        {
            docs.push(line.trim().to_owned());
        } else if let Some(block) = text.strip_prefix("/**").and_then(|s| s.strip_suffix("*/")) {
            docs.push(
                block
                    .lines()
                    .map(|line| line.trim().trim_start_matches('*').trim())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        } else if previous.kind() == "attribute_item" {
            if !text.starts_with("#[doc") {
                attributes.push(text.into());
            }
            if let Some(value) = text
                .strip_prefix("#[doc")
                .and_then(|s| s.trim_start().strip_prefix('='))
                .and_then(|s| s.trim().strip_suffix(']'))
                .and_then(|s| serde_json::from_str::<String>(s.trim()).ok())
            {
                docs.push(value);
            }
        } else if !previous.kind().ends_with("comment") {
            break;
        }
        sibling = previous.prev_named_sibling();
    }
    docs.reverse();
    let mut ancestor = Some(node);
    let mut test_only = path.starts_with("tests/") || path.contains("/tests/");
    while let Some(item) = ancestor {
        let mut sibling = item.prev_named_sibling();
        while let Some(attr) =
            sibling.filter(|n| n.kind() == "attribute_item" || n.kind().ends_with("comment"))
        {
            if attr.kind() == "attribute_item" {
                let normalized = source(attr, file)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>();
                test_only |= normalized == "#[test]"
                    || normalized == "#[cfg(test)]"
                    || normalized.starts_with("#[cfg(all(test,");
            }
            sibling = attr.prev_named_sibling();
        }
        ancestor = item.parent();
    }
    let mut cursor = node.walk();
    let visibility = node
        .named_children(&mut cursor)
        .find(|n| n.kind() == "visibility_modifier")
        .map(|n| {
            if source(n, file) == "pub" {
                "public"
            } else {
                "restricted"
            }
        })
        .unwrap_or("private");
    attributes.reverse();
    DeclarationInfo {
        docs: docs.join("\n"),
        visibility: visibility.into(),
        test_only,
        attributes,
    }
}

/// Attributes known not to rewrite a function body into other items. Any
/// other attribute (for example a procedural macro) keeps the body in the
/// skeleton, so editing it forces a wider refresh.
const BODY_NEUTRAL_ATTRIBUTES: &[&str] = &[
    "allow",
    "cfg",
    "cold",
    "deny",
    "deprecated",
    "doc",
    "expect",
    "forbid",
    "inline",
    "must_use",
    "test",
    "track_caller",
    "warn",
];

fn skeleton(root: Node<'_>, file: &SourceFile) -> String {
    let mut bodies = vec![];
    collect_bodies(root, file, &mut bodies);
    let mut text = String::with_capacity(file.content.len());
    let mut at = 0;
    for (start, end) in bodies {
        text.push_str(&file.content[at..start]);
        text.push_str("{}");
        at = end;
    }
    text.push_str(&file.content[at..]);
    digest(text)
}

fn collect_bodies(node: Node<'_>, file: &SourceFile, bodies: &mut Vec<(usize, usize)>) {
    if node.kind() == "function_item"
        && let Some(body) = node.child_by_field_name("body")
        && body_neutral_attributes(node, file)
    {
        // Nested items inside a body are local to it, so the outermost body wins.
        bodies.push((body.start_byte(), body.end_byte()));
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_bodies(child, file, bodies);
    }
}

fn body_neutral_attributes(function: Node<'_>, file: &SourceFile) -> bool {
    let mut sibling = function.prev_named_sibling();
    while let Some(node) =
        sibling.filter(|n| n.kind() == "attribute_item" || n.kind().ends_with("comment"))
    {
        if node.kind() == "attribute_item" {
            let text = source(node, file).trim_start_matches("#[").trim_start();
            let name: String = text
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !BODY_NEUTRAL_ATTRIBUTES.contains(&name.as_str()) {
                return false;
            }
        }
        sibling = node.prev_named_sibling();
    }
    true
}

fn last_name(node: Node<'_>) -> Node<'_> {
    node.child_by_field_name("name").unwrap_or(node)
}

fn serde_span(span: Span) -> String {
    format!(
        "{}:{}-{}:{}",
        span.start.line, span.start.column, span.end.line, span.end.column
    )
}

fn point_at(text: &str, offset: usize) -> Point {
    let prefix = &text[..offset];
    Point {
        row: prefix.bytes().filter(|&b| b == b'\n').count(),
        column: prefix.rfind('\n').map_or(offset, |n| offset - n - 1),
    }
}

/// A minimal contiguous UTF-8-safe edit, suitable for tree-sitter tree reuse.
fn edit_between(old: &str, new: &str) -> InputEdit {
    let mut start = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let mut suffix = old.as_bytes()[start..]
        .iter()
        .rev()
        .zip(new.as_bytes()[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;
    InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_at(old, start),
        old_end_position: point_at(old, old_end),
        new_end_position: point_at(new, new_end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(text: &str) -> Manifest {
        Manifest {
            workspace_id: "test".into(),
            files: BTreeMap::from([("src/lib.rs".into(), SourceFile::new(text))]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "fixture".into(),
        }
    }

    #[test]
    fn extracts_calls_without_inventing_name_bindings() {
        let facts = RustParser::new().unwrap().analyze(&manifest("fn target() {} fn caller() { target(); foo::target::<u8>(); x.run(); x.run::<u8>(); (factory())(); }" )).unwrap();
        assert_eq!(facts.symbols.len(), 2);
        assert_eq!(facts.calls.len(), 6);
        assert_eq!(
            facts
                .calls
                .iter()
                .filter(|c| c.kind == CallKind::Method)
                .count(),
            2
        );
        assert_eq!(
            facts
                .calls
                .iter()
                .filter(|c| c.kind == CallKind::Indirect)
                .count(),
            1
        );
        assert!(facts.calls.iter().all(|c| c.caller_id.is_some()));
    }

    #[test]
    fn trait_and_closure_and_macro_boundaries_are_visible() {
        let facts = RustParser::new().unwrap().analyze(&manifest("trait T { fn run(&self); } impl T for X { fn run(&self) { let f = || target(); println!(\"hi\"); } }" )).unwrap();
        assert!(facts.symbols.iter().any(|s| s.kind == "trait_method"));
        assert!(facts.calls[0].caller_id.is_none());
        assert!(
            facts
                .diagnostics
                .iter()
                .any(|d| d.code == "macro_not_expanded")
        );
        assert!(
            facts
                .diagnostics
                .iter()
                .any(|d| d.code == "closure_owner_not_modeled")
        );
    }

    #[test]
    fn skeleton_ignores_body_edits_but_not_signatures_or_macro_bodies() {
        let hash = |text: &str| {
            RustParser::new()
                .unwrap()
                .analyze(&manifest(text))
                .unwrap()
                .skeletons["src/lib.rs"]
                .clone()
        };
        let base = hash("fn a() { b(); }\nfn b() {}");
        assert_eq!(
            base,
            hash("fn a() {\n    b();\n    fn local() {}\n}\nfn b() { 1; }")
        );
        assert_ne!(base, hash("fn a(x: u8) { b(); }\nfn b() {}"));
        assert_ne!(base, hash("fn a() { b(); }\nfn b() {}\nfn c() {}"));
        let attributed = "#[inline]\n#[my_macro]\nfn a() { b(); }";
        assert_ne!(
            hash(attributed),
            hash("#[inline]\n#[my_macro]\nfn a() { c(); }")
        );
        assert_eq!(
            hash("#[inline]\nfn a() { b(); }"),
            hash("#[inline]\nfn a() { c(); }")
        );
    }

    #[test]
    fn incremental_parse_matches_cold_parse_through_unicode_edits() {
        let mut parser = RustParser::new().unwrap();
        for text in [
            "fn café() {}",
            "// 😀\nfn café() {} fn x(){ café(); }",
            "fn cafe() { }",
            "",
            "fn broken( { target();",
        ] {
            let input = manifest(text);
            assert_eq!(
                parser.analyze(&input).unwrap(),
                RustParser::new().unwrap().analyze(&input).unwrap()
            );
        }
    }

    const DECLARATIONS: &str = "use crate::thing;\n\npub struct S { field: u8 }\n\npub enum E { V }\n\npub trait T { fn m(&self); }\n\npub mod m {\n    pub const C: u8 = 1;\n    pub static X: u8 = 2;\n    pub type Y = u8;\n}\n\nimpl T for S { fn m(&self) { self.field; } }\n\nfn caller() { let mut x = 0; x = 1; x += 2; thing(); }\n";

    fn span_text(text: &str, span: Span) -> &str {
        let start = semweft_core::byte_offset(text, span.start).unwrap();
        let end = semweft_core::byte_offset(text, span.end).unwrap();
        &text[start..end]
    }

    #[test]
    fn declarations_cover_every_modeled_item_kind_and_scope() {
        let facts = RustParser::new()
            .unwrap()
            .analyze(&manifest(DECLARATIONS))
            .unwrap();
        for kind in [
            "function",
            "struct",
            "field",
            "enum",
            "variant",
            "trait",
            "trait_method",
            "module",
            "const",
            "static",
            "type",
        ] {
            assert!(
                facts.symbols.iter().any(|s| s.kind == kind),
                "missing {kind}: {:?}",
                facts
                    .symbols
                    .iter()
                    .map(|s| (&s.name, &s.kind))
                    .collect::<Vec<_>>()
            );
        }
        let method = |qualified: &str| {
            facts
                .symbols
                .iter()
                .find(|s| s.qualified_name == qualified)
                .unwrap_or_else(|| panic!("missing {qualified}"))
        };
        assert_eq!(method("src/lib.rs::T::m").kind, "trait_method");
        assert_eq!(method("src/lib.rs::<T for S>::m").kind, "function");
        assert_eq!(method("src/lib.rs::<T for S>::m").signature, "fn m(&self)");
        assert_eq!(method("src/lib.rs::m::C").kind, "const");
        assert_eq!(method("src/lib.rs::m::X").kind, "static");
        assert_eq!(method("src/lib.rs::m::Y").kind, "type");
        assert_eq!(method("src/lib.rs::S::field").kind, "field");
        assert_eq!(method("src/lib.rs::E::V").kind, "variant");
        assert_eq!(method("src/lib.rs::caller").signature, "fn caller()");
        assert_eq!(method("src/lib.rs::S").signature, "pub struct S");
        for symbol in &facts.symbols {
            assert_eq!(
                span_text(DECLARATIONS, symbol.name_span),
                symbol.name,
                "{}",
                symbol.qualified_name
            );
            assert!(
                symbol.span.contains(symbol.name_span.start),
                "{} is not inside its declaration",
                symbol.qualified_name
            );
        }
    }

    #[test]
    fn occurrences_distinguish_imports_writes_calls_and_declarations() {
        let facts = RustParser::new()
            .unwrap()
            .analyze(&manifest(DECLARATIONS))
            .unwrap();
        let occurrence = |name: &str, kind: &str| {
            facts
                .occurrences
                .iter()
                .any(|o| o.name == name && o.kind == kind)
        };
        assert!(occurrence("thing", "import"), "use declaration");
        assert!(occurrence("thing", "call"), "call expression");
        assert!(occurrence("x", "write"), "assignment target");
        assert!(occurrence("field", "read"), "field read");
        assert!(occurrence("S", "type"), "type identifier");
        assert!(
            facts.occurrences.iter().all(|o| !facts
                .symbols
                .iter()
                .any(|s| s.file == o.file && s.name_span == o.span)),
            "declaration name spans must not also be occurrences"
        );
        let call = facts
            .occurrences
            .iter()
            .find(|o| o.name == "thing" && o.kind == "call")
            .unwrap();
        let caller = facts.symbols.iter().find(|s| s.name == "caller").unwrap();
        assert_eq!(
            call.enclosing_symbol_id.as_deref(),
            Some(caller.symbol_id.as_str())
        );
    }

    #[test]
    fn call_targets_and_indirect_expressions_are_recorded_precisely() {
        let text = "fn target() {}\nstruct X;\nfn run() { foo::target::<u8>(); x.run::<u8>(); (factory())(); }";
        let facts = RustParser::new().unwrap().analyze(&manifest(text)).unwrap();
        let direct = facts
            .calls
            .iter()
            .find(|c| c.expression == "foo::target::<u8>" && c.kind == CallKind::Direct)
            .unwrap();
        assert_eq!(span_text(text, direct.span), "target");
        let run_id = &facts
            .symbols
            .iter()
            .find(|s| s.name == "run")
            .unwrap()
            .symbol_id;
        assert_eq!(direct.caller_id.as_deref(), Some(run_id.as_str()));
        let method = facts
            .calls
            .iter()
            .find(|c| c.kind == CallKind::Method)
            .unwrap();
        assert_eq!(span_text(text, method.span), "run");
        let indirect = facts
            .calls
            .iter()
            .find(|c| c.kind == CallKind::Indirect)
            .unwrap();
        assert_eq!(indirect.expression, "(factory())");
    }

    #[test]
    fn call_owners_are_cleared_for_closures_and_restored_for_functions() {
        let text = "fn outer() { let f = || target(); target(); }";
        let facts = RustParser::new().unwrap().analyze(&manifest(text)).unwrap();
        let calls = &facts.calls;
        assert!(!calls.is_empty());
        // Calls inside the closure have no modeled owner; calls after it do.
        assert!(calls.iter().any(|c| c.caller_id.is_some()));
        assert!(calls.iter().any(|c| c.caller_id.is_none()));
        assert!(
            facts
                .diagnostics
                .iter()
                .any(|d| d.code == "closure_owner_not_modeled")
        );
    }

    #[test]
    fn removed_files_disappear_from_facts_and_skeletons() {
        let mut parser = RustParser::new().unwrap();
        let both = Manifest {
            workspace_id: "test".into(),
            files: BTreeMap::from([
                ("src/a.rs".into(), SourceFile::new("fn a() {}")),
                ("src/b.rs".into(), SourceFile::new("fn b() {}")),
            ]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "fixture".into(),
        };
        let facts = parser.analyze(&both).unwrap();
        assert_eq!(facts.symbols.len(), 2);
        assert_eq!(facts.skeletons.len(), 2);
        let mut only_a = both.clone();
        only_a.files.remove("src/b.rs");
        let facts = parser.analyze(&only_a).unwrap();
        assert_eq!(facts.symbols.len(), 1);
        assert_eq!(facts.symbols[0].file, "src/a.rs");
        assert_eq!(
            facts.skeletons.keys().cloned().collect::<Vec<_>>(),
            vec!["src/a.rs"]
        );
    }

    #[test]
    fn parse_errors_and_macro_boundaries_become_diagnostics() {
        let facts = RustParser::new()
            .unwrap()
            .analyze(&manifest(
                "fn broken( { target();\nfn ok() { println!(\"hi\"); }",
            ))
            .unwrap();
        assert!(facts.diagnostics.iter().any(|d| d.code == "parse_error"));
        assert!(
            facts
                .diagnostics
                .iter()
                .any(|d| d.code == "macro_not_expanded")
        );
        assert!(facts.diagnostics.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn trait_impl_blocks_record_paths_generics_methods_and_scope() {
        let text = "trait T { fn m(&self); fn d(&self) {} }\nstruct S;\nimpl S { fn own(&self) {} }\nimpl T for S { fn m(&self) { fn nested() {} } }\nimpl<'a, U: Clone> a::b::G<u8> for Vec<U> {}\nunsafe impl Send for S {}\nimpl !Sync for S {}\n#[cfg(test)]\nmod tests { impl super::T for super::S { fn m(&self) {} } }\n";
        let facts = RustParser::new().unwrap().analyze(&manifest(text)).unwrap();
        let paths: Vec<_> = facts.impls.iter().map(|b| b.trait_path.as_str()).collect();
        assert_eq!(paths, ["T", "a::b::G", "Send", "Sync", "super::T"]);
        let plain = &facts.impls[0];
        assert_eq!(plain.self_type, "S");
        assert_eq!(span_text(text, plain.trait_span), "T");
        assert_eq!(
            plain.methods.len(),
            1,
            "nested functions are not impl methods"
        );
        let method = facts
            .symbols
            .iter()
            .find(|symbol| symbol.symbol_id == plain.methods[0])
            .unwrap();
        assert_eq!(method.qualified_name, "src/lib.rs::<T for S>::m");
        assert!(!plain.test_only && !plain.negative && !plain.unsafe_impl);
        let generic = &facts.impls[1];
        assert_eq!(span_text(text, generic.trait_span), "G");
        assert_eq!(generic.generics, ["U"]);
        assert_eq!(generic.self_type, "Vec<U>");
        assert!(facts.impls[2].unsafe_impl);
        assert!(facts.impls[3].negative);
        assert!(facts.impls[4].test_only);
        assert_eq!(span_text(text, facts.impls[4].trait_span), "T");
    }

    #[test]
    fn trait_impls_keep_empty_marker_negative_and_unsafe_blocks() {
        let text = "impl Marker for S {}\nimpl S {}\nunsafe /* before impl */ impl /* header */ Send for S {}\nimpl ! /* negative */ Sync for S {}\nunsafe impl ! /* negative */ UnwindSafe for S {}\n";
        let input = manifest(text);
        let facts = RustParser::new().unwrap().analyze(&input).unwrap();
        assert!(facts.symbols.is_empty());
        assert!(facts.diagnostics.is_empty());
        assert_eq!(facts.impls.len(), 4);
        for block in &facts.impls {
            assert!(block.methods.is_empty());
            assert_eq!(block.file_hash, input.files["src/lib.rs"].file_hash);
            assert_eq!(span_text(text, block.trait_span), block.trait_path);
            assert!(span_text(text, block.span).ends_with("{}"));
        }
        assert!(!facts.impls[0].negative && !facts.impls[0].unsafe_impl);
        assert!(!facts.impls[1].negative && facts.impls[1].unsafe_impl);
        assert!(facts.impls[2].negative && !facts.impls[2].unsafe_impl);
        assert!(facts.impls[3].negative && facts.impls[3].unsafe_impl);
        RefreshCoordinator::default()
            .publish_inputs(input, facts)
            .unwrap();
    }

    #[test]
    fn trait_impls_preserve_generic_paths_self_types_and_only_direct_methods() {
        let text = r#"
impl<'a, T: Clone, U, const N: usize> crate::api::Trait<'a, Vec<U>, N>
    for &'a mut Container<T, U, N> where U: Copy
{
    type Item = U;
    const SIZE: usize = { fn helper() {} 0 };
    fn first(&self) {
        fn nested() {}
        struct Local;
        impl Other for Local { fn local(&self) {} }
    }
    fn second(&mut self) {}
}
"#;
        let input = manifest(text);
        let facts = RustParser::new().unwrap().analyze(&input).unwrap();
        assert!(facts.diagnostics.is_empty());
        assert_eq!(facts.impls.len(), 2);
        let outer = &facts.impls[0];
        assert_eq!(outer.trait_path, "crate::api::Trait");
        assert_eq!(span_text(text, outer.trait_span), "Trait");
        assert_eq!(outer.self_type, "&'a mut Container<T, U, N>");
        assert_eq!(outer.generics, ["T", "U"]);
        let names = |block: &ImplBlock| {
            block
                .methods
                .iter()
                .map(|id| {
                    facts
                        .symbols
                        .iter()
                        .find(|symbol| &symbol.symbol_id == id)
                        .unwrap()
                        .name
                        .as_str()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(names(outer), ["first", "second"]);
        assert_eq!(facts.impls[1].trait_path, "Other");
        assert_eq!(names(&facts.impls[1]), ["local"]);
        RefreshCoordinator::default()
            .publish_inputs(input, facts)
            .unwrap();
    }

    #[test]
    fn trait_impls_inherit_test_scope_from_attributes_ancestors_and_paths() {
        let text = r#"
impl Production for S {}
#[cfg(test)]
impl Direct for S {}
#[cfg(all(test, feature = "checks"))]
mod checks { impl super::Nested for super::S {} }
#[test]
fn case() { impl Local for S {} }
#[cfg(not(test))]
impl NonTest for S {}
"#;
        let mut input = manifest(text);
        let facts = RustParser::new().unwrap().analyze(&input).unwrap();
        assert!(facts.diagnostics.is_empty());
        let scopes: Vec<_> = facts.impls.iter().map(|block| block.test_only).collect();
        assert_eq!(scopes, [false, true, true, true, false]);
        input.files = BTreeMap::from([("tests/marker.rs".into(), SourceFile::new(text))]);
        let facts = RustParser::new().unwrap().analyze(&input).unwrap();
        assert!(facts.impls.iter().all(|block| block.test_only));
        RefreshCoordinator::default()
            .publish_inputs(input, facts)
            .unwrap();
    }

    #[test]
    fn trait_impl_methods_inside_default_methods_are_functions() {
        let text =
            "trait Host { fn factory() { struct S; impl Inner for S { fn run(&self) {} } } }";
        let input = manifest(text);
        let facts = RustParser::new().unwrap().analyze(&input).unwrap();
        assert!(facts.diagnostics.is_empty());
        assert_eq!(facts.impls.len(), 1);
        let block = &facts.impls[0];
        assert_eq!(block.methods.len(), 1);
        let method = facts
            .symbols
            .iter()
            .find(|symbol| symbol.symbol_id == block.methods[0])
            .unwrap();
        assert_eq!(method.kind, "function");
        assert_eq!(method.name, "run");
        RefreshCoordinator::default()
            .publish_inputs(input, facts)
            .unwrap();
    }

    #[test]
    fn skeleton_handles_comments_neutral_attributes_and_nested_items() {
        let hash = |text: &str| {
            RustParser::new()
                .unwrap()
                .analyze(&manifest(text))
                .unwrap()
                .skeletons["src/lib.rs"]
                .clone()
        };
        // Documentation comments do not hide neutral attributes.
        assert_eq!(
            hash("// doc\n#[inline]\nfn a() { b(); }"),
            hash("// doc\n#[inline]\nfn a() { c(); }")
        );
        // Comments outside bodies remain part of the structural skeleton.
        assert_ne!(
            hash("// doc\nfn a() { b(); }"),
            hash("// other\nfn a() { b(); }")
        );
        // Nested items live inside the outer body and are elided with it.
        assert_eq!(
            hash("fn a() { fn local() {} struct S; }"),
            hash("fn a() {}")
        );
        // A non-neutral attribute keeps the body, so edits widen the refresh.
        assert_ne!(
            hash("#[my_macro]\nfn a() { b(); }"),
            hash("#[my_macro]\nfn a() { c(); }")
        );
        // A macro body inside a neutral function is still body content.
        assert_eq!(hash("fn a() { m!(1); }"), hash("fn a() { m!(2, 3); }"));
    }
}

#[cfg(test)]
mod fact_cache_tests {
    use super::*;
    #[test]
    fn cached_file_facts_equal_cold_rebuild_after_edits_and_removal() {
        let source = "/// Docs.\npub fn a() { b(); }\npub fn b() {}\ntrait T { fn m(&self); }\nstruct S;\nimpl T for S { fn m(&self) { b(); } }\nimpl Marker for S {}\n";
        let mut manifest = Manifest {
            workspace_id: "facts-cache".into(),
            files: BTreeMap::from([
                ("a.rs".into(), SourceFile::new(source)),
                ("b.rs".into(), SourceFile::new(source)),
            ]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        };
        let mut parser = RustParser::new().unwrap();
        let first = parser.analyze(&manifest).unwrap();
        assert_eq!(parser.files_extracted, 2);
        assert_eq!(first.impls.len(), 4);
        assert_eq!(parser.analyze(&manifest).unwrap(), first);
        assert_eq!(parser.files_extracted, 0);
        manifest.files.insert(
            "a.rs".into(),
            SourceFile::new(source.replace("b()", "changed()")),
        );
        let changed = parser.analyze(&manifest).unwrap();
        assert_eq!(
            changed,
            RustParser::new().unwrap().analyze(&manifest).unwrap()
        );
        assert_eq!(
            changed
                .impls
                .iter()
                .filter(|block| block.file == "b.rs")
                .collect::<Vec<_>>(),
            first
                .impls
                .iter()
                .filter(|block| block.file == "b.rs")
                .collect::<Vec<_>>()
        );
        assert_eq!(parser.files_extracted, 1);
        let moved = manifest.files.remove("b.rs").unwrap();
        manifest.files.insert("renamed.rs".into(), moved);
        assert_eq!(
            parser.analyze(&manifest).unwrap(),
            RustParser::new().unwrap().analyze(&manifest).unwrap()
        );
        assert_eq!(parser.files_extracted, 1);
        manifest.files.remove("a.rs");
        assert_eq!(
            parser.analyze(&manifest).unwrap(),
            RustParser::new().unwrap().analyze(&manifest).unwrap()
        );
        assert_eq!(parser.files_extracted, 0);
        assert_eq!(parser.analyze(&manifest).unwrap().impls.len(), 2);
    }
}
