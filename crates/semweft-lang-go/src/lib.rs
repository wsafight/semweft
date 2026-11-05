//! Experimental Go syntax facts; semantic binding comes from SCIP.
use semweft_core::*;
use std::collections::BTreeMap;
use tree_sitter::{Node, Parser, Tree};

pub const PARSER_VERSION: &str = "tree-sitter-go@0.25;extractor@0.1-experimental";
pub fn language() -> tree_sitter::Language {
    tree_sitter_go::LANGUAGE.into()
}
pub struct GoParser {
    parser: Parser,
    trees: BTreeMap<String, (String, Tree)>,
    file_facts: BTreeMap<String, (String, SyntaxFacts)>,
    pub files_extracted: usize,
}
impl GoParser {
    pub fn new() -> Result<Self, String> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
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
        let mut facts = SyntaxFacts::default();
        self.trees.retain(|p, _| manifest.files.contains_key(p));
        for (path, file) in &manifest.files {
            if !path.ends_with(".go") {
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
            let tree = if let Some((old, tree)) =
                self.trees.get(path).filter(|(old, _)| old == &file.content)
            {
                let _ = old;
                tree.clone()
            } else {
                self.parser
                    .parse(&file.content, None)
                    .ok_or("parse_cancelled")?
            };
            extract(tree.root_node(), path, file, &[], None, &mut file_facts);
            // Go edits conservatively rebuild the project, including bodies.
            file_facts
                .skeletons
                .insert(path.clone(), file.file_hash.clone());
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
        Ok(facts)
    }
}
fn span(node: Node<'_>) -> Span {
    Span {
        start: Position {
            line: node.start_position().row as u32 + 1,
            column: node.start_position().column as u32 + 1,
        },
        end: Position {
            line: node.end_position().row as u32 + 1,
            column: node.end_position().column as u32 + 1,
        },
    }
}
fn text<'a>(node: Node<'_>, file: &'a SourceFile) -> &'a str {
    &file.content[node.byte_range()]
}
fn extract(
    node: Node<'_>,
    path: &str,
    file: &SourceFile,
    scope: &[String],
    owner: Option<&str>,
    facts: &mut SyntaxFacts,
) {
    if node.kind() == "import_spec"
        && let Some(import) = node.child_by_field_name("path")
    {
        facts
            .imports
            .entry(path.into())
            .or_default()
            .push(text(import, file).trim_matches(['"', '`']).into());
    }
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
    let kind = match node.kind() {
        "function_declaration" | "method_declaration" => Some("function"),
        "method_elem" => Some("trait_method"),
        "type_spec" | "type_alias" => {
            Some(match node.child_by_field_name("type").map(|n| n.kind()) {
                Some("struct_type") => "struct",
                Some("interface_type") => "trait",
                _ => "type",
            })
        }
        "field_declaration" => Some("field"),
        "const_spec" => Some("const"),
        "var_spec" if owner.is_none() => Some("static"),
        _ => None,
    };
    let mut nested = scope.to_vec();
    let mut caller = owner.map(str::to_owned);
    if let (Some(kind), Some(name)) = (kind, node.child_by_field_name("name")) {
        if node.kind() == "method_declaration"
            && let Some(receiver) = node.child_by_field_name("receiver")
        {
            let source = text(receiver, file).trim_matches(['(', ')', ' ']);
            nested.push(
                source
                    .split_whitespace()
                    .last()
                    .unwrap_or(source)
                    .trim_start_matches('*')
                    .into(),
            );
        }
        let name_text = text(name, file).to_owned();
        nested.push(name_text.clone());
        let qualified = format!("{path}::{}", nested.join("::"));
        let id = identity("sym", &[&qualified, kind, &name.start_byte().to_string()]);
        let body = node.child_by_field_name("body").or_else(|| {
            node.child_by_field_name("type")
                .filter(|n| matches!(n.kind(), "struct_type" | "interface_type"))
        });
        let signature = file.content
            [node.start_byte()..body.map_or(node.end_byte(), |b| b.start_byte())]
            .trim()
            .to_owned();
        facts.symbols.push(Symbol {
            symbol_id: id.clone(),
            name: name_text.clone(),
            qualified_name: qualified,
            kind: kind.into(),
            file: path.into(),
            file_hash: file.file_hash.clone(),
            span: span(node),
            name_span: span(name),
            signature,
        });
        let mut comments = vec![];
        let documented = node
            .parent()
            .filter(|p| {
                matches!(
                    p.kind(),
                    "type_declaration" | "const_declaration" | "var_declaration"
                )
            })
            .filter(|_| node.prev_named_sibling().is_none())
            .unwrap_or(node);
        let mut previous = documented.prev_named_sibling();
        while let Some(comment) = previous.filter(|n| n.kind() == "comment") {
            comments.push(
                text(comment, file)
                    .trim_start_matches("//")
                    .trim_start_matches("/*")
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
                visibility: if name_text.chars().next().is_some_and(char::is_uppercase) {
                    "public"
                } else {
                    "private"
                }
                .into(),
                test_only: path.ends_with("_test.go"),
                attributes: vec![],
            },
        );
        caller = Some(id);
    }
    if node.kind() == "func_literal" {
        caller = None;
        facts.diagnostics.push(Diagnostic {
            code: "closure_owner_not_modeled".into(),
            file: Some(path.into()),
            message: format!("function literal at {}", node.start_position().row + 1),
        });
    }
    if node.kind() == "call_expression"
        && let Some(function) = node.child_by_field_name("function")
    {
        let (target, kind) = match function.kind() {
            "identifier" => (function, CallKind::Direct),
            "selector_expression" => (
                function.child_by_field_name("field").unwrap_or(function),
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
        node.kind(),
        "identifier" | "field_identifier" | "type_identifier" | "package_identifier"
    ) && !facts
        .symbols
        .iter()
        .any(|s| s.file == path && s.name_span == span(node))
    {
        let mut kind = if node.kind() == "type_identifier" {
            "type"
        } else {
            "read"
        };
        let mut ancestor = node.parent();
        while let Some(parent) = ancestor {
            if parent.kind() == "import_spec" {
                kind = "import";
                break;
            }
            if matches!(
                parent.kind(),
                "assignment_statement" | "short_var_declaration"
            ) && parent
                .child_by_field_name("left")
                .is_some_and(|left| left.byte_range().contains(&node.start_byte()))
            {
                kind = "write";
                break;
            }
            if parent.kind() == "block" {
                break;
            }
            ancestor = parent.parent();
        }
        if facts
            .calls
            .iter()
            .any(|c| c.file == path && c.span == span(node))
        {
            kind = "call";
        }
        facts.occurrences.push(Occurrence {
            name: text(node, file).into(),
            file: path.into(),
            span: span(node),
            kind: kind.into(),
            enclosing_symbol_id: caller.clone(),
        });
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        extract(child, path, file, &nested, caller.as_deref(), facts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_exported_symbols_methods_tests_and_calls_without_resolving_bindings() {
        let manifest = Manifest {
            workspace_id: "ws".into(),
            files: BTreeMap::from([(
                "shop_test.go".into(),
                SourceFile::new(
                    "package shop\n// Stock docs.\ntype Stock struct { Value int }\nfunc (s *Stock) Count() int { return s.Value }\nfunc caller() { target(); s.Count() }\nfunc target() {}\n",
                ),
            )]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        };
        let facts = GoParser::new().unwrap().analyze(&manifest).unwrap();
        assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
        assert_eq!(facts.calls.len(), 2);
        assert!(
            facts
                .symbols
                .iter()
                .any(|s| s.qualified_name.ends_with("Stock::Count"))
        );
        assert!(facts.declarations.values().all(|d| d.test_only));
        assert!(
            facts
                .declarations
                .values()
                .any(|d| d.docs == "Stock docs." && d.visibility == "public")
        );
        assert!(
            facts
                .symbols
                .iter()
                .any(|s| s.kind == "field" && s.name == "Value")
        );
    }
}

#[cfg(test)]
mod fact_cache_tests {
    use super::*;
    #[test]
    fn cached_file_facts_equal_cold_rebuild_after_edits_and_removal() {
        let source = "package sample\n// Docs.\nfunc a() { b() }\nfunc b() {}\n";
        let mut manifest = Manifest {
            workspace_id: "facts-cache".into(),
            files: BTreeMap::from([
                ("a.go".into(), SourceFile::new(source)),
                ("b.go".into(), SourceFile::new(source)),
            ]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        };
        let mut parser = GoParser::new().unwrap();
        let first = parser.analyze(&manifest).unwrap();
        assert_eq!(parser.files_extracted, 2);
        assert_eq!(parser.analyze(&manifest).unwrap(), first);
        assert_eq!(parser.files_extracted, 0);
        manifest.files.insert(
            "a.go".into(),
            SourceFile::new(source.replace("b()", "changed()")),
        );
        assert_eq!(
            parser.analyze(&manifest).unwrap(),
            GoParser::new().unwrap().analyze(&manifest).unwrap()
        );
        assert_eq!(parser.files_extracted, 1);
        let moved = manifest.files.remove("b.go").unwrap();
        manifest.files.insert("renamed.go".into(), moved);
        assert_eq!(
            parser.analyze(&manifest).unwrap(),
            GoParser::new().unwrap().analyze(&manifest).unwrap()
        );
        assert_eq!(parser.files_extracted, 1);
        manifest.files.remove("a.go");
        assert_eq!(
            parser.analyze(&manifest).unwrap(),
            GoParser::new().unwrap().analyze(&manifest).unwrap()
        );
        assert_eq!(parser.files_extracted, 0);
    }
}

#[cfg(test)]
mod extraction_tests {
    use super::*;

    fn manifest(files: &[(&str, &str)]) -> Manifest {
        Manifest {
            workspace_id: "go-extraction".into(),
            files: files
                .iter()
                .map(|(path, content)| ((*path).into(), SourceFile::new(*content)))
                .collect(),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        }
    }

    fn analyze(files: &[(&str, &str)]) -> SyntaxFacts {
        GoParser::new().unwrap().analyze(&manifest(files)).unwrap()
    }

    fn symbol<'a>(facts: &'a SyntaxFacts, name: &str) -> &'a Symbol {
        facts
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing symbol {name}: {:#?}", facts.symbols))
    }

    #[test]
    fn imports_cover_grouped_and_aliased_specs() {
        let facts = analyze(&[(
            "main.go",
            "package main\nimport (\n\t\"fmt\"\n\talias \"example.com/pkg\"\n)\nfunc main() { fmt.Println() }\n",
        )]);
        let imports = &facts.imports["main.go"];
        assert!(imports.contains(&"fmt".to_string()), "{imports:?}");
        assert!(
            imports.contains(&"example.com/pkg".to_string()),
            "{imports:?}"
        );
        assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
    }

    #[test]
    fn declarations_map_go_constructs_to_stable_kinds() {
        let facts = analyze(&[(
            "kinds.go",
            "package kinds\n// Reader docs.\ntype Reader interface { Read() error }\ntype Alias = int\ntype Stock struct { Value int }\nconst Pi = 3\nvar Global int\nfunc (s *Stock) Count() int { return s.Value }\nfunc helper() {}\n",
        )]);
        assert_eq!(symbol(&facts, "Reader").kind, "trait");
        assert_eq!(symbol(&facts, "Read").kind, "trait_method");
        assert_eq!(symbol(&facts, "Alias").kind, "type");
        assert_eq!(symbol(&facts, "Stock").kind, "struct");
        assert_eq!(symbol(&facts, "Value").kind, "field");
        assert_eq!(symbol(&facts, "Pi").kind, "const");
        assert_eq!(symbol(&facts, "Global").kind, "static");
        assert_eq!(symbol(&facts, "Count").kind, "function");
        assert_eq!(symbol(&facts, "helper").kind, "function");
        assert_eq!(
            symbol(&facts, "Count").qualified_name,
            "kinds.go::Stock::Count"
        );
        assert_eq!(
            facts.declarations[&symbol(&facts, "Reader").symbol_id].docs,
            "Reader docs."
        );
    }

    #[test]
    fn visibility_and_test_scope_follow_names_and_paths() {
        let facts = analyze(&[
            (
                "pkg/main.go",
                "package pkg\nfunc Exported() {}\nfunc internal() {}\n",
            ),
            ("pkg/main_test.go", "package pkg\nfunc TestThing() {}\n"),
        ]);
        let exported = symbol(&facts, "Exported");
        let internal = symbol(&facts, "internal");
        let test = symbol(&facts, "TestThing");
        assert_eq!(facts.declarations[&exported.symbol_id].visibility, "public");
        assert_eq!(
            facts.declarations[&internal.symbol_id].visibility,
            "private"
        );
        assert!(!facts.declarations[&exported.symbol_id].test_only);
        assert!(facts.declarations[&test.symbol_id].test_only);
    }

    #[test]
    fn call_sites_keep_direct_method_and_indirect_kinds() {
        let facts = analyze(&[(
            "calls.go",
            "package calls\ntype Stock struct{}\nfunc (s Stock) Count() int { return 0 }\nfunc target() {}\nfunc indirect() func() { return target }\nfunc caller() { target(); var s Stock; s.Count(); indirect()(); }\n",
        )]);
        let direct = facts
            .calls
            .iter()
            .find(|call| call.expression == "target" && call.caller_id.is_some())
            .unwrap();
        assert_eq!(direct.kind, CallKind::Direct);
        let method = facts
            .calls
            .iter()
            .find(|call| call.expression == "s.Count")
            .unwrap();
        assert_eq!(method.kind, CallKind::Method);
        let indirect = facts
            .calls
            .iter()
            .find(|call| call.kind == CallKind::Indirect)
            .unwrap();
        assert_eq!(indirect.expression, "indirect()");
    }

    #[test]
    fn function_literals_record_a_gap_and_clear_the_caller() {
        let facts = analyze(&[(
            "closures.go",
            "package closures\nfunc target() {}\nfunc caller() { f := func() { target() }; f() }\n",
        )]);
        assert!(
            facts
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "closure_owner_not_modeled"),
            "{:?}",
            facts.diagnostics
        );
        let inner = facts
            .calls
            .iter()
            .find(|call| call.expression == "target")
            .unwrap();
        assert_eq!(inner.caller_id, None, "closures have no modeled owner");
        let outer = facts
            .calls
            .iter()
            .find(|call| call.expression == "f")
            .unwrap();
        assert!(outer.caller_id.is_some());
    }

    #[test]
    fn occurrences_distinguish_reads_writes_calls_and_types() {
        let facts = analyze(&[(
            "occurrences.go",
            "package occurrences\nimport \"fmt\"\ntype T struct{}\nfunc f(x int) {\n\ty := x\n\ty = 2\n\tfmt.Println(y)\n}\n",
        )]);
        let kinds = |name: &str| {
            facts
                .occurrences
                .iter()
                .filter(|occurrence| occurrence.name == name)
                .map(|occurrence| occurrence.kind.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds("y").iter().filter(|kind| **kind == "write").count(),
            2
        );
        assert!(kinds("Println").contains(&"call"), "{:?}", kinds("Println"));
        assert!(
            facts
                .occurrences
                .iter()
                .any(|occurrence| occurrence.name == "int" && occurrence.kind == "type"),
            "{:?}",
            facts.occurrences
        );
        let caller = symbol(&facts, "f").symbol_id.clone();
        assert!(
            facts
                .occurrences
                .iter()
                .filter(|occurrence| occurrence.enclosing_symbol_id.is_some())
                .all(|occurrence| occurrence.enclosing_symbol_id.as_deref() == Some(&caller))
        );
    }

    #[test]
    fn parse_errors_are_reported_with_the_go_file() {
        let facts = analyze(&[("broken.go", "package broken\nfunc f( {\n")]);
        let parse_errors = facts
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "parse_error")
            .collect::<Vec<_>>();
        assert!(!parse_errors.is_empty(), "{:?}", facts.diagnostics);
        assert!(
            parse_errors
                .iter()
                .all(|diagnostic| diagnostic.file.as_deref() == Some("broken.go"))
        );
    }

    #[test]
    fn analysis_is_deterministic_across_file_insertion_orders() {
        let forward = analyze(&[
            ("b.go", "package p\nfunc B() { a() }\n"),
            ("a.go", "package p\nfunc a() {}\n"),
        ]);
        let reverse = analyze(&[
            ("a.go", "package p\nfunc a() {}\n"),
            ("b.go", "package p\nfunc B() { a() }\n"),
        ]);
        assert_eq!(forward.symbols, reverse.symbols);
        assert_eq!(forward.calls, reverse.calls);
        assert_eq!(forward.occurrences, reverse.occurrences);
        assert_eq!(forward.diagnostics, reverse.diagnostics);
        assert!(
            forward
                .symbols
                .windows(2)
                .all(|pair| pair[0].symbol_id <= pair[1].symbol_id)
        );
        assert!(
            forward
                .calls
                .windows(2)
                .all(|pair| pair[0].callsite_id <= pair[1].callsite_id)
        );
    }

    #[test]
    fn non_go_files_are_ignored_without_extraction_work() {
        let mut parser = GoParser::new().unwrap();
        let facts = parser
            .analyze(&manifest(&[("lib.rs", "pub fn entry() {}\n")]))
            .unwrap();
        assert_eq!(parser.files_extracted, 0);
        assert!(facts.symbols.is_empty());
        assert!(facts.calls.is_empty());
        assert!(facts.diagnostics.is_empty());
    }
}
