//! TypeScript/JavaScript syntax candidates. No lexical or type binding is inferred.
use semweft_core::*;
use std::collections::BTreeMap;
use tree_sitter::{Node, Parser, Tree};

pub const PARSER_VERSION: &str =
    "tree-sitter-typescript@0.23;tree-sitter-javascript@0.25;extractor@0.1";
pub fn is_source(path: &str) -> bool {
    [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"]
        .iter()
        .any(|ext| path.ends_with(ext))
}
pub fn language(path: &str) -> tree_sitter::Language {
    if path.ends_with(".tsx") {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else if [".ts", ".mts", ".cts"]
        .iter()
        .any(|ext| path.ends_with(ext))
    {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    } else {
        tree_sitter_javascript::LANGUAGE.into()
    }
}
pub struct WebParser {
    parser: Parser,
    trees: BTreeMap<String, (String, Tree)>,
    file_facts: BTreeMap<String, (String, SyntaxFacts)>,
    pub files_extracted: usize,
}
impl WebParser {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            parser: Parser::new(),
            trees: BTreeMap::new(),
            file_facts: BTreeMap::new(),
            files_extracted: 0,
        })
    }
    pub fn analyze(&mut self, manifest: &Manifest) -> Result<SyntaxFacts, String> {
        self.files_extracted = 0;
        self.file_facts
            .retain(|path, _| manifest.files.contains_key(path));
        let mut facts = SyntaxFacts::default();
        self.trees.retain(|p, _| manifest.files.contains_key(p));
        for (path, file) in manifest.files.iter().filter(|(p, _)| is_source(p)) {
            if let Some((hash, cached)) = self.file_facts.get(path)
                && hash == &file.file_hash
                && digest(&file.content) == file.file_hash
            {
                facts.extend(cached);
                continue;
            }
            self.files_extracted += 1;
            let mut file_facts = SyntaxFacts::default();
            self.parser
                .set_language(&language(path))
                .map_err(|e| e.to_string())?;
            let tree = match self
                .trees
                .get(path)
                .filter(|(hash, _)| hash == &file.file_hash)
            {
                Some((_, tree)) => tree.clone(),
                None => self
                    .parser
                    .parse(&file.content, None)
                    .ok_or("parse_cancelled")?,
            };
            extract(tree.root_node(), path, file, &[], None, &mut file_facts);
            file_facts
                .skeletons
                .insert(path.clone(), file.file_hash.clone());
            facts.extend(&file_facts);
            self.file_facts
                .insert(path.clone(), (file.file_hash.clone(), file_facts));
            self.trees
                .insert(path.clone(), (file.file_hash.clone(), tree));
        }
        facts.symbols.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));
        facts
            .calls
            .sort_by(|a, b| a.callsite_id.cmp(&b.callsite_id));
        facts.diagnostics.sort();
        facts.diagnostics.dedup();
        Ok(facts)
    }
}
fn span(n: Node<'_>) -> Span {
    Span {
        start: Position {
            line: n.start_position().row as u32 + 1,
            column: n.start_position().column as u32 + 1,
        },
        end: Position {
            line: n.end_position().row as u32 + 1,
            column: n.end_position().column as u32 + 1,
        },
    }
}
fn text<'a>(n: Node<'_>, file: &'a SourceFile) -> &'a str {
    &file.content[n.byte_range()]
}
fn extract(
    n: Node<'_>,
    path: &str,
    file: &SourceFile,
    scope: &[String],
    owner: Option<&str>,
    facts: &mut SyntaxFacts,
) {
    if n.is_error() || n.is_missing() {
        facts.diagnostics.push(Diagnostic {
            code: "parse_error".into(),
            file: Some(path.into()),
            message: format!(
                "{} at {}:{}",
                n.kind(),
                n.start_position().row + 1,
                n.start_position().column + 1
            ),
        });
    }
    if matches!(n.kind(), "import_statement" | "export_statement")
        && let Some(source) = n.child_by_field_name("source")
    {
        facts
            .imports
            .entry(path.into())
            .or_default()
            .push(text(source, file).trim_matches(['\'', '"']).into());
    }
    let value = n.child_by_field_name("value");
    let kind = match n.kind() {
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            Some("function")
        }
        "method_definition" | "method_signature" | "abstract_method_signature" => Some("function"),
        "class_declaration" | "abstract_class_declaration" => Some("struct"),
        "interface_declaration" => Some("trait"),
        "type_alias_declaration" => Some("type"),
        "enum_declaration" => Some("enum"),
        "public_field_definition" | "field_definition" | "property_signature" => Some("field"),
        "variable_declarator"
            if value.is_some_and(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "generator_function"
                )
            }) =>
        {
            Some("function")
        }
        "variable_declarator" if owner.is_none() => Some("static"),
        _ => None,
    };
    let mut nested = scope.to_vec();
    let mut caller = owner.map(str::to_owned);
    if let (Some(kind), Some(name)) = (kind, n.child_by_field_name("name")) {
        // Destructuring does not declare a single symbol with the pattern as its name.
        if matches!(
            name.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "private_property_identifier"
        ) {
            let name_text = text(name, file).to_owned();
            nested.push(name_text.clone());
            let qualified = format!("{path}::{}", nested.join("::"));
            let id = identity("sym", &[&qualified, kind, &name.start_byte().to_string()]);
            let body = n
                .child_by_field_name("body")
                .or_else(|| value.and_then(|v| v.child_by_field_name("body")));
            let signature = file.content
                [n.start_byte()..body.map_or(n.end_byte(), |b| b.start_byte())]
                .trim()
                .to_owned();
            facts.symbols.push(Symbol {
                symbol_id: id.clone(),
                name: name_text,
                qualified_name: qualified,
                kind: kind.into(),
                file: path.into(),
                file_hash: file.file_hash.clone(),
                span: span(n),
                name_span: span(name),
                signature,
            });
            let declaration = n
                .parent()
                .filter(|p| matches!(p.kind(), "lexical_declaration" | "variable_declaration"))
                .unwrap_or(n);
            let exported = declaration
                .parent()
                .is_some_and(|p| p.kind() == "export_statement");
            let documented = if exported {
                declaration.parent().unwrap()
            } else {
                declaration
            };
            let mut comments = vec![];
            let mut previous = documented.prev_named_sibling();
            while let Some(comment) = previous.filter(|p| p.kind() == "comment") {
                comments.push(
                    text(comment, file)
                        .trim_start_matches('/')
                        .trim_start_matches('*')
                        .trim_end_matches("*/")
                        .trim()
                        .to_owned(),
                );
                previous = comment.prev_named_sibling();
            }
            comments.reverse();
            facts.declarations.insert(
                id.clone(),
                DeclarationInfo {
                    docs: comments.join("\n"),
                    visibility: if exported { "public" } else { "private" }.into(),
                    test_only: path
                        .split('/')
                        .any(|p| matches!(p, "test" | "tests" | "__tests__"))
                        || [".test.", ".spec."].iter().any(|s| path.contains(s)),
                    attributes: vec![],
                },
            );
            caller = Some(id);
        }
    }
    if matches!(
        n.kind(),
        "arrow_function" | "function_expression" | "generator_function"
    ) && !n
        .parent()
        .is_some_and(|p| p.kind() == "variable_declarator")
    {
        caller = None;
        facts.diagnostics.push(Diagnostic {
            code: "closure_owner_not_modeled".into(),
            file: Some(path.into()),
            message: format!("anonymous callback at line {}", n.start_position().row + 1),
        });
    }
    if matches!(n.kind(), "call_expression" | "new_expression")
        && let Some(function) = n
            .child_by_field_name("function")
            .or_else(|| n.child_by_field_name("constructor"))
    {
        let (target, kind) = match function.kind() {
            "identifier" => (function, CallKind::Direct),
            "member_expression" => (
                function.child_by_field_name("property").unwrap_or(function),
                CallKind::Method,
            ),
            _ => (function, CallKind::Indirect),
        };
        let pos = span(target);
        facts.calls.push(CallSite {
            callsite_id: identity(
                "call",
                &[
                    path,
                    &format!(
                        "{}:{}-{}:{}",
                        pos.start.line, pos.start.column, pos.end.line, pos.end.column
                    ),
                ],
            ),
            caller_id: caller.clone(),
            file: path.into(),
            file_hash: file.file_hash.clone(),
            span: pos,
            expression: text(function, file).into(),
            kind,
        });
    }
    if matches!(
        n.kind(),
        "identifier" | "type_identifier" | "property_identifier" | "shorthand_property_identifier"
    ) && !facts
        .symbols
        .iter()
        .any(|s| s.file == path && s.name_span == span(n))
    {
        let mut kind = if n.kind() == "type_identifier" {
            "type"
        } else {
            "read"
        };
        let mut ancestor = n.parent();
        while let Some(p) = ancestor {
            if p.kind() == "import_statement" {
                kind = "import";
                break;
            }
            if matches!(
                p.kind(),
                "assignment_expression" | "augmented_assignment_expression"
            ) && p
                .child_by_field_name("left")
                .is_some_and(|l| l.byte_range().contains(&n.start_byte()))
            {
                kind = "write";
                break;
            }
            if p.kind() == "update_expression" {
                kind = "write";
                break;
            }
            if p.kind() == "statement_block" {
                break;
            }
            ancestor = p.parent();
        }
        if facts
            .calls
            .iter()
            .any(|c| c.file == path && c.span == span(n))
        {
            kind = "call";
        }
        facts.occurrences.push(Occurrence {
            name: text(n, file).into(),
            file: path.into(),
            span: span(n),
            kind: kind.into(),
            enclosing_symbol_id: caller.clone(),
        });
    }
    let mut cursor = n.walk();
    for child in n.named_children(&mut cursor) {
        extract(child, path, file, &nested, caller.as_deref(), facts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captures_tsx_js_and_typescript_candidates_without_running_the_project() {
        let manifest = Manifest {
            workspace_id: "web".into(),
            files: BTreeMap::from([
                (
                    "src/store.ts".into(),
                    SourceFile::new(
                        "import {send} from './api';\n/** reserve */\nexport const reserve = (n: number): number => { send(n); return n; };\nexport interface Stock { count: number; get(): number }\nexport class Shop { buy() { reserve(1); } }\n",
                    ),
                ),
                (
                    "src/View.tsx".into(),
                    SourceFile::new(
                        "export function View() { return <button onClick={() => reserve(1)}>Buy</button>; }",
                    ),
                ),
                (
                    "tests/api.test.mjs".into(),
                    SourceFile::new("export function send(n) { return n; }"),
                ),
            ]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        };
        let facts = WebParser::new().unwrap().analyze(&manifest).unwrap();
        assert!(
            !facts.diagnostics.iter().any(|d| d.code == "parse_error"),
            "{:?}",
            facts.diagnostics
        );
        let reserve = facts.symbols.iter().find(|s| s.name == "reserve").unwrap();
        assert_eq!(facts.declarations[&reserve.symbol_id].visibility, "public");
        assert!(facts.calls.iter().any(|c| c.caller_id.as_ref() == Some(&reserve.symbol_id) && c.expression == "send"));
        assert_eq!(facts.imports["src/store.ts"], ["./api"]);
        assert!(
            facts
                .symbols
                .iter()
                .any(|s| s.qualified_name.ends_with("Shop::buy"))
        );
        assert!(
            facts
                .diagnostics
                .iter()
                .any(|d| d.code == "closure_owner_not_modeled")
        );
        let test = facts.symbols.iter().find(|s| s.name == "send").unwrap();
        assert!(facts.declarations[&test.symbol_id].test_only);
    }
}

#[cfg(test)]
mod fact_cache_tests {
    use super::*;
    #[test]
    fn cached_file_facts_equal_cold_rebuild_after_edits_and_removal() {
        let source = "/** Docs. */\nexport function a() { b(); }\nfunction b() {}\n";
        let mut manifest = Manifest {
            workspace_id: "facts-cache".into(),
            files: BTreeMap::from([
                ("a.ts".into(), SourceFile::new(source)),
                ("b.ts".into(), SourceFile::new(source)),
            ]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        };
        let mut parser = WebParser::new().unwrap();
        let first = parser.analyze(&manifest).unwrap();
        assert_eq!(parser.files_extracted, 2);
        assert_eq!(parser.analyze(&manifest).unwrap(), first);
        assert_eq!(parser.files_extracted, 0);
        manifest.files.insert(
            "a.ts".into(),
            SourceFile::new(source.replace("b()", "changed()")),
        );
        assert_eq!(
            parser.analyze(&manifest).unwrap(),
            WebParser::new().unwrap().analyze(&manifest).unwrap()
        );
        assert_eq!(parser.files_extracted, 1);
        let moved = manifest.files.remove("b.ts").unwrap();
        manifest.files.insert("renamed.ts".into(), moved);
        assert_eq!(
            parser.analyze(&manifest).unwrap(),
            WebParser::new().unwrap().analyze(&manifest).unwrap()
        );
        assert_eq!(parser.files_extracted, 1);
        manifest.files.remove("a.ts");
        assert_eq!(
            parser.analyze(&manifest).unwrap(),
            WebParser::new().unwrap().analyze(&manifest).unwrap()
        );
        assert_eq!(parser.files_extracted, 0);
    }
}
