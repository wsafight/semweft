//! TypeScript/JavaScript syntax candidates. No lexical or type binding is inferred.
use semweft_core::*;
use std::collections::BTreeMap;
use tree_sitter::{Node, Parser, Tree};

pub const PARSER_VERSION: &str =
    "tree-sitter-typescript@0.23;tree-sitter-javascript@0.25;extractor@0.3-anonymous-owners";
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
    if n.kind() == "call_expression"
        && let Some(function) = n.child_by_field_name("function")
        && matches!(text(function, file), "import" | "require")
    {
        let argument = n
            .child_by_field_name("arguments")
            .and_then(|arguments| arguments.named_child(0));
        if let Some(argument) = argument.filter(|argument| argument.kind() == "string") {
            facts
                .imports
                .entry(path.into())
                .or_default()
                .push(text(argument, file).trim_matches(['\'', '"']).into());
        } else {
            facts.diagnostics.push(Diagnostic {code:"dynamic_module_specifier".into(),file:Some(path.into()),
                message:format!("Module specifier at line {} is not a string literal; runtime resolution is unknown",n.start_position().row + 1)});
        }
    }
    let value = n.child_by_field_name("value");
    let anonymous_callable = matches!(
        n.kind(),
        "arrow_function" | "function_expression" | "generator_function"
    ) && !n
        .parent()
        .is_some_and(|parent| parent.kind() == "variable_declarator");
    let default_export = anonymous_callable
        .then(|| n.parent())
        .flatten()
        .filter(|parent| parent.kind() == "export_statement");
    let synthetic_name = if let Some(export) = default_export {
        (0..export.child_count())
            .filter_map(|index| export.child(index))
            .find(|child| child.kind() == "default")
    } else if anonymous_callable {
        (0..n.child_count())
            .filter_map(|index| n.child(index))
            .find(|child| matches!(child.kind(), "function" | "=>"))
    } else {
        None
    };
    let kind = match n.kind() {
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            Some("function")
        }
        "method_definition" | "method_signature" | "abstract_method_signature" => Some("function"),
        "arrow_function" | "function_expression" | "generator_function" if anonymous_callable => {
            Some("function")
        }
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
    if let (Some(kind), Some(name)) = (kind, n.child_by_field_name("name").or(synthetic_name)) {
        // Destructuring does not declare a single symbol with the pattern as its name.
        if synthetic_name.is_some()
            || matches!(
                name.kind(),
                "identifier"
                    | "type_identifier"
                    | "property_identifier"
                    | "private_property_identifier"
            )
        {
            let name_text = if default_export.is_some() {
                "default".into()
            } else if anonymous_callable && n.child_by_field_name("name").is_none() {
                format!(
                    "<anonymous@{}:{}>",
                    n.start_position().row + 1,
                    n.start_position().column + 1
                )
            } else {
                text(name, file).to_owned()
            };
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
                span: span(default_export.unwrap_or(n)),
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
    if anonymous_callable && default_export.is_none() {
        facts.diagnostics.push(Diagnostic {
            code: "callback_invocation_not_modeled".into(),
            file: Some(path.into()),
            message: format!("Function body has its own declaration at line {}; callback invocation and dynamic flow are not inferred", n.start_position().row + 1),
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
    fn anonymous_exports_and_callbacks_keep_separate_function_owners() {
        let source = "/** parse arguments */\nexport default function (args) {\n  direct(args);\n  args.forEach(value => inner(value));\n  args.map(function (value) { return mapped(value); });\n  return args;\n}\nfunction direct(value) { return value; }\nfunction inner(value) { return value; }\nfunction mapped(value) { return value; }\n";
        let manifest = Manifest {
            workspace_id: "anonymous-owners".into(),
            files: BTreeMap::from([("src/index.js".into(), SourceFile::new(source))]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        };
        let facts = WebParser::new().unwrap().analyze(&manifest).unwrap();
        let exported = facts
            .symbols
            .iter()
            .find(|symbol| symbol.name == "default")
            .unwrap();
        assert_eq!(facts.declarations[&exported.symbol_id].visibility, "public");
        assert_eq!(
            facts.declarations[&exported.symbol_id].docs,
            "parse arguments"
        );
        assert!(exported.span.contains(exported.name_span.start));
        let direct = facts
            .calls
            .iter()
            .find(|call| call.expression == "direct")
            .unwrap();
        assert_eq!(
            direct.caller_id.as_deref(),
            Some(exported.symbol_id.as_str())
        );
        let mut owners = std::collections::BTreeSet::new();
        for expression in ["inner", "mapped"] {
            let call = facts
                .calls
                .iter()
                .find(|call| call.expression == expression)
                .unwrap();
            let owner = facts
                .symbols
                .iter()
                .find(|symbol| Some(&symbol.symbol_id) == call.caller_id.as_ref())
                .unwrap();
            assert!(owner.name.starts_with("<anonymous@"));
            assert!(owner.span.contains(call.span.start));
            assert!(owner.span.contains(owner.name_span.start));
            assert_ne!(owner.symbol_id, exported.symbol_id);
            owners.insert(owner.symbol_id.as_str());
        }
        assert_eq!(owners.len(), 2);
        assert_eq!(
            facts
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "callback_invocation_not_modeled")
                .count(),
            2
        );
        let mut coordinator = RefreshCoordinator::default();
        coordinator.publish_inputs(manifest, facts).unwrap();
    }

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
                .any(|d| d.code == "callback_invocation_not_modeled")
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

#[cfg(test)]
mod extraction_tests {
    use super::*;

    fn manifest(files: &[(&str, &str)]) -> Manifest {
        Manifest {
            workspace_id: "web-extraction".into(),
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
        WebParser::new().unwrap().analyze(&manifest(files)).unwrap()
    }

    fn symbol<'a>(facts: &'a SyntaxFacts, name: &str) -> &'a Symbol {
        facts
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing symbol {name}: {:#?}", facts.symbols))
    }

    #[test]
    fn source_extensions_and_grammar_selection_cover_the_web_set() {
        for path in [
            "a.ts", "a.tsx", "a.js", "a.jsx", "a.mjs", "a.cjs", "a.mts", "a.cts",
        ] {
            assert!(is_source(path), "{path} is a web source");
        }
        for path in ["a.json", "a.rs", "a.go", "a.css", "a"] {
            assert!(!is_source(path), "{path} is not a web source");
        }
        assert_eq!(
            language("view.tsx"),
            tree_sitter_typescript::LANGUAGE_TSX.into()
        );
        for path in ["a.ts", "a.mts", "a.cts"] {
            assert_eq!(
                language(path),
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            );
        }
        for path in ["a.js", "a.jsx", "a.mjs", "a.cjs"] {
            assert_eq!(language(path), tree_sitter_javascript::LANGUAGE.into());
        }
    }

    #[test]
    fn imports_cover_static_exports_require_and_dynamic_import() {
        let facts = analyze(&[(
            "src/index.js",
            "import value from './value';\nexport { x } from './x';\nconst c = require('./cjs');\nconst d = import('./dynamic');\n",
        )]);
        let imports = &facts.imports["src/index.js"];
        for expected in ["./value", "./x", "./cjs", "./dynamic"] {
            assert!(
                imports.contains(&expected.to_string()),
                "missing {expected}: {imports:?}"
            );
        }
        assert!(
            !facts
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "dynamic_module_specifier"),
            "{:?}",
            facts.diagnostics
        );
    }

    #[test]
    fn non_literal_dynamic_imports_record_a_gap() {
        let facts = analyze(&[(
            "src/dynamic.js",
            "export function load(name) { return import(name); }\n",
        )]);
        assert!(
            facts
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "dynamic_module_specifier"),
            "{:?}",
            facts.diagnostics
        );
        assert!(!facts.imports.contains_key("src/dynamic.js"));
    }

    #[test]
    fn declarations_map_web_constructs_to_stable_kinds() {
        let facts = analyze(&[(
            "src/kinds.ts",
            "export function send(n: number) { return n; }\nfunction helper() {}\nexport class Shop { price = 1; buy() { return this.price; } }\nexport interface Store { value: number; }\nexport type ID = string;\nexport enum Kind { A }\nconst value = 1;\nconst arrow = (x: number) => x;\n",
        )]);
        assert_eq!(symbol(&facts, "send").kind, "function");
        assert_eq!(symbol(&facts, "helper").kind, "function");
        assert_eq!(symbol(&facts, "Shop").kind, "struct");
        assert_eq!(symbol(&facts, "price").kind, "field");
        assert_eq!(symbol(&facts, "buy").kind, "function");
        assert_eq!(symbol(&facts, "Store").kind, "trait");
        assert!(
            facts
                .symbols
                .iter()
                .any(|symbol| symbol.name == "value" && symbol.kind == "field"),
            "interface properties are fields: {:?}",
            facts.symbols
        );
        assert_eq!(symbol(&facts, "ID").kind, "type");
        assert_eq!(symbol(&facts, "Kind").kind, "enum");
        assert_eq!(
            facts
                .symbols
                .iter()
                .find(|symbol| symbol.kind == "static" && symbol.name == "value")
                .map(|symbol| symbol.name.as_str()),
            Some("value")
        );
        assert_eq!(symbol(&facts, "arrow").kind, "function");
        assert_eq!(
            facts.declarations[&symbol(&facts, "Shop").symbol_id].visibility,
            "public"
        );
        assert_eq!(
            facts.declarations[&symbol(&facts, "helper").symbol_id].visibility,
            "private"
        );
    }

    #[test]
    fn test_scope_follows_directory_and_filename_markers() {
        let facts = analyze(&[
            ("test/api.test.ts", "export function first() {}\n"),
            ("src/app.spec.js", "export function second() {}\n"),
            ("src/app.ts", "export function third() {}\n"),
            ("__tests__/helper.js", "export function fourth() {}\n"),
        ]);
        for (name, expected) in [
            ("first", true),
            ("second", true),
            ("third", false),
            ("fourth", true),
        ] {
            assert_eq!(
                facts.declarations[&symbol(&facts, name).symbol_id].test_only,
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn jsdoc_is_detached_from_documented_declarations() {
        let facts = analyze(&[(
            "src/docs.ts",
            "export function undocumented() {}\n/**\n * Send docs.\n * Second line.\n */\nexport function send() {}\n",
        )]);
        let docs = &facts.declarations[&symbol(&facts, "send").symbol_id].docs;
        assert!(docs.contains("Send docs."), "{docs:?}");
        assert!(docs.contains("Second line."), "{docs:?}");
        assert_eq!(
            facts.declarations[&symbol(&facts, "undocumented").symbol_id].docs,
            ""
        );
    }

    #[test]
    fn call_sites_keep_direct_method_and_constructor_kinds() {
        let facts = analyze(&[(
            "src/calls.js",
            "function target() {}\nclass A { method() {} }\nfunction caller() { target(); new A().method(); new A(); }\n",
        )]);
        let target = facts
            .calls
            .iter()
            .find(|call| call.expression == "target")
            .unwrap();
        assert_eq!(target.kind, CallKind::Direct);
        assert!(target.caller_id.is_some());
        let method = facts
            .calls
            .iter()
            .find(|call| call.expression.ends_with(".method"))
            .unwrap();
        assert_eq!(method.kind, CallKind::Method);
        assert!(
            facts
                .calls
                .iter()
                .any(|call| call.expression == "A" && call.kind == CallKind::Direct),
            "constructors are calls: {:?}",
            facts.calls
        );
    }

    #[test]
    fn callbacks_owned_by_expressions_record_a_gap() {
        let facts = analyze(&[(
            "src/callbacks.js",
            "function outer() { const f = (x) => x; items.forEach((x) => inner(x)); }\n",
        )]);
        assert!(
            facts
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "callback_invocation_not_modeled"),
            "{:?}",
            facts.diagnostics
        );
    }

    #[test]
    fn occurrences_distinguish_reads_writes_calls_and_types() {
        let facts = analyze(&[(
            "src/occurrences.ts",
            "type Meter = number;\nlet count: Meter = 0;\nfunction target(): void {}\nfunction run() { count = 1; target(); }\n",
        )]);
        let kinds = |name: &str| {
            facts
                .occurrences
                .iter()
                .filter(|occurrence| occurrence.name == name)
                .map(|occurrence| occurrence.kind.as_str())
                .collect::<Vec<_>>()
        };
        assert!(kinds("count").contains(&"write"), "{:?}", kinds("count"));
        assert!(kinds("target").contains(&"call"), "{:?}", kinds("target"));
        assert!(kinds("Meter").contains(&"type"), "{:?}", kinds("Meter"));
    }

    #[test]
    fn tsx_jsx_is_parsed_with_the_tsx_grammar() {
        let facts = analyze(&[(
            "src/view.tsx",
            "export function View({ value }: { value: number }) {\n  return <div className=\"box\">{value}</div>;\n}\n",
        )]);
        assert!(
            !facts
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "parse_error"),
            "{:?}",
            facts.diagnostics
        );
        assert_eq!(symbol(&facts, "View").kind, "function");
    }

    #[test]
    fn parse_errors_are_reported_with_the_web_file() {
        let facts = analyze(&[("src/broken.ts", "export function broken( {")]);
        let parse_errors = facts
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "parse_error")
            .collect::<Vec<_>>();
        assert!(!parse_errors.is_empty(), "{:?}", facts.diagnostics);
        assert!(
            parse_errors
                .iter()
                .all(|diagnostic| diagnostic.file.as_deref() == Some("src/broken.ts"))
        );
    }

    #[test]
    fn analysis_is_deterministic_across_file_insertion_orders() {
        let forward = analyze(&[
            ("src/b.js", "export function b() { a(); }\n"),
            ("src/a.js", "export function a() {}\n"),
        ]);
        let reverse = analyze(&[
            ("src/a.js", "export function a() {}\n"),
            ("src/b.js", "export function b() { a(); }\n"),
        ]);
        assert_eq!(forward.symbols, reverse.symbols);
        assert_eq!(forward.calls, reverse.calls);
        assert_eq!(forward.occurrences, reverse.occurrences);
        assert_eq!(forward.diagnostics, reverse.diagnostics);
    }

    #[test]
    fn non_web_files_are_ignored_without_extraction_work() {
        let mut parser = WebParser::new().unwrap();
        let facts = parser
            .analyze(&manifest(&[("src/lib.rs", "pub fn entry() {}\n")]))
            .unwrap();
        assert_eq!(parser.files_extracted, 0);
        assert!(facts.symbols.is_empty());
        assert!(facts.imports.is_empty());
    }
}
