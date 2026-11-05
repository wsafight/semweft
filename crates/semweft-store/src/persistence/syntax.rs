use super::*;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SyntaxOrder {
    symbols: Vec<usize>,
    calls: Vec<usize>,
    occurrences: Vec<usize>,
    diagnostics: Vec<usize>,
}

/// Content-addressed per-file facts, with a compact ordering vector to preserve
/// arbitrary frontend ordering without putting global offsets into each shard.
pub(super) fn split(facts: &SyntaxFacts) -> (BTreeMap<String, SyntaxFacts>, SyntaxOrder) {
    let mut shards: BTreeMap<String, SyntaxFacts> = BTreeMap::new();
    for symbol in &facts.symbols {
        shards
            .entry(symbol.file.clone())
            .or_default()
            .symbols
            .push(symbol.clone());
    }
    for call in &facts.calls {
        shards
            .entry(call.file.clone())
            .or_default()
            .calls
            .push(call.clone());
    }
    for occurrence in &facts.occurrences {
        shards
            .entry(occurrence.file.clone())
            .or_default()
            .occurrences
            .push(occurrence.clone());
    }
    for diagnostic in &facts.diagnostics {
        shards
            .entry(diagnostic.file.clone().unwrap_or_default())
            .or_default()
            .diagnostics
            .push(diagnostic.clone());
    }
    let owners: BTreeMap<_, _> = facts
        .symbols
        .iter()
        .map(|s| (&s.symbol_id, &s.file))
        .collect();
    for (id, declaration) in &facts.declarations {
        let file = owners
            .get(id)
            .map(|file| (*file).clone())
            .unwrap_or_default();
        shards
            .entry(file)
            .or_default()
            .declarations
            .insert(id.clone(), declaration.clone());
    }
    for (file, skeleton) in &facts.skeletons {
        shards
            .entry(file.clone())
            .or_default()
            .skeletons
            .insert(file.clone(), skeleton.clone());
    }
    for block in &facts.impls {
        shards
            .entry(block.file.clone())
            .or_default()
            .impls
            .push(block.clone());
    }
    for (file, imports) in &facts.imports {
        shards
            .entry(file.clone())
            .or_default()
            .imports
            .insert(file.clone(), imports.clone());
    }
    let indices: BTreeMap<_, _> = shards
        .keys()
        .enumerate()
        .map(|(i, file)| (file.as_str(), i))
        .collect();
    let order = SyntaxOrder {
        symbols: facts
            .symbols
            .iter()
            .map(|s| indices[s.file.as_str()])
            .collect(),
        calls: facts
            .calls
            .iter()
            .map(|s| indices[s.file.as_str()])
            .collect(),
        occurrences: facts
            .occurrences
            .iter()
            .map(|s| indices[s.file.as_str()])
            .collect(),
        diagnostics: facts
            .diagnostics
            .iter()
            .map(|s| indices[s.file.as_deref().unwrap_or("")])
            .collect(),
    };
    (shards, order)
}

fn ordered<T>(groups: Vec<Vec<T>>, order: Vec<usize>) -> Result<Vec<T>> {
    let mut groups: Vec<_> = groups.into_iter().map(Vec::into_iter).collect();
    let mut result = Vec::with_capacity(order.len());
    for index in order {
        result.push(
            groups
                .get_mut(index)
                .and_then(Iterator::next)
                .context("index_store_error: invalid syntax ordering")?,
        );
    }
    ensure!(
        groups.iter_mut().all(|group| group.next().is_none()),
        "index_store_error: incomplete syntax ordering"
    );
    Ok(result)
}

pub(super) fn join(shards: Vec<SyntaxFacts>, order: SyntaxOrder) -> Result<SyntaxFacts> {
    let mut facts = SyntaxFacts::default();
    let (mut symbols, mut calls, mut occurrences, mut diagnostics) =
        (vec![], vec![], vec![], vec![]);
    for shard in shards {
        symbols.push(shard.symbols);
        calls.push(shard.calls);
        occurrences.push(shard.occurrences);
        diagnostics.push(shard.diagnostics);
        facts.declarations.extend(shard.declarations);
        facts.skeletons.extend(shard.skeletons);
        facts.imports.extend(shard.imports);
        facts.impls.extend(shard.impls);
    }
    facts.impls.sort_by(|a, b| {
        (&a.file, a.span.start, &a.impl_id).cmp(&(&b.file, b.span.start, &b.impl_id))
    });
    facts.symbols = ordered(symbols, order.symbols)?;
    facts.calls = ordered(calls, order.calls)?;
    facts.occurrences = ordered(occurrences, order.occurrences)?;
    facts.diagnostics = ordered(diagnostics, order.diagnostics)?;
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use semweft_core::{CallSite, DeclarationInfo, ImplBlock, Occurrence, Position, Span, Symbol};

    fn span(line: u32, column: u32) -> Span {
        Span {
            start: Position { line, column },
            end: Position {
                line,
                column: column + 1,
            },
        }
    }

    fn symbol(id: &str, file: &str, name: &str) -> Symbol {
        Symbol {
            symbol_id: id.into(),
            name: name.into(),
            qualified_name: format!("{file}::{name}"),
            kind: "function".into(),
            file: file.into(),
            file_hash: format!("hash-{file}"),
            span: span(1, 1),
            name_span: span(1, 5),
            signature: format!("fn {name}()"),
        }
    }

    fn facts() -> SyntaxFacts {
        SyntaxFacts {
            symbols: vec![
                symbol("sym_a1", "a.rs", "alpha"),
                symbol("sym_b1", "b.rs", "beta"),
                symbol("sym_a2", "a.rs", "gamma"),
            ],
            calls: vec![
                CallSite {
                    callsite_id: "call_a".into(),
                    caller_id: Some("sym_a1".into()),
                    file: "a.rs".into(),
                    file_hash: "hash-a.rs".into(),
                    span: span(2, 3),
                    expression: "delta".into(),
                    kind: semweft_core::CallKind::Direct,
                },
                CallSite {
                    callsite_id: "call_b".into(),
                    caller_id: None,
                    file: "b.rs".into(),
                    file_hash: "hash-b.rs".into(),
                    span: span(4, 1),
                    expression: "beta".into(),
                    kind: semweft_core::CallKind::Indirect,
                },
            ],
            diagnostics: vec![
                Diagnostic {
                    code: "parse_error".into(),
                    file: Some("b.rs".into()),
                    message: "bad".into(),
                },
                Diagnostic {
                    code: "workspace".into(),
                    file: None,
                    message: "global".into(),
                },
            ],
            skeletons: BTreeMap::from([
                ("a.rs".into(), "skeleton-a".into()),
                ("b.rs".into(), "skeleton-b".into()),
            ]),
            occurrences: vec![
                Occurrence {
                    name: "delta".into(),
                    file: "a.rs".into(),
                    span: span(2, 3),
                    kind: "call".into(),
                    enclosing_symbol_id: Some("sym_a1".into()),
                },
                Occurrence {
                    name: "beta".into(),
                    file: "b.rs".into(),
                    span: span(4, 1),
                    kind: "read".into(),
                    enclosing_symbol_id: None,
                },
            ],
            declarations: BTreeMap::from([
                (
                    "sym_a1".into(),
                    DeclarationInfo {
                        docs: "alpha docs".into(),
                        visibility: "public".into(),
                        test_only: false,
                        attributes: vec!["inline".into()],
                    },
                ),
                (
                    "orphan".into(),
                    DeclarationInfo {
                        docs: "no symbol".into(),
                        visibility: "private".into(),
                        test_only: true,
                        attributes: vec![],
                    },
                ),
            ]),
            imports: BTreeMap::from([(
                "a.rs".into(),
                vec!["std::fmt".into(), "crate::beta".into()],
            )]),
            impls: vec![ImplBlock {
                impl_id: "impl_a".into(),
                file: "a.rs".into(),
                file_hash: "hash-a.rs".into(),
                span: span(10, 1),
                trait_path: "Display".into(),
                trait_span: span(10, 7),
                self_type: "Alpha".into(),
                generics: vec!["T".into()],
                negative: false,
                unsafe_impl: true,
                test_only: false,
                methods: vec!["sym_a1".into()],
            }],
        }
    }

    #[test]
    fn split_groups_every_fact_by_owning_file_and_preserves_order() {
        let facts = facts();
        let (shards, order) = split(&facts);
        assert_eq!(
            shards.keys().cloned().collect::<Vec<String>>(),
            vec!["", "a.rs", "b.rs"],
            "ownerless diagnostics get the empty-name shard"
        );
        assert_eq!(shards["a.rs"].symbols.len(), 2);
        assert_eq!(shards["b.rs"].symbols.len(), 1);
        assert_eq!(shards["a.rs"].calls.len(), 1);
        assert_eq!(shards["b.rs"].calls.len(), 1);
        assert_eq!(shards["a.rs"].occurrences.len(), 1);
        assert_eq!(shards["b.rs"].occurrences.len(), 1);
        assert_eq!(shards["b.rs"].diagnostics.len(), 1);
        assert_eq!(shards[""].diagnostics.len(), 1);
        assert_eq!(shards["a.rs"].declarations.len(), 1);
        assert_eq!(shards[""].declarations.len(), 1, "orphan declarations");
        assert_eq!(shards["a.rs"].imports["a.rs"].len(), 2);
        assert_eq!(shards["a.rs"].skeletons["a.rs"], "skeleton-a");
        assert_eq!(shards["a.rs"].impls.len(), 1);

        // Order vectors are shard indexes in the sorted BTreeMap key order.
        assert_eq!(order.symbols, vec![1, 2, 1]);
        assert_eq!(order.calls, vec![1, 2]);
        assert_eq!(order.occurrences, vec![1, 2]);
        assert_eq!(order.diagnostics, vec![2, 0]);
    }

    #[test]
    fn join_reconstructs_the_original_facts() {
        let facts = facts();
        let (shards, order) = split(&facts);
        let rebuilt = join(shards.into_values().collect(), order).unwrap();
        assert_eq!(rebuilt.symbols, facts.symbols);
        assert_eq!(rebuilt.calls, facts.calls);
        assert_eq!(rebuilt.occurrences, facts.occurrences);
        assert_eq!(rebuilt.diagnostics, facts.diagnostics);
        assert_eq!(rebuilt.declarations, facts.declarations);
        assert_eq!(rebuilt.skeletons, facts.skeletons);
        assert_eq!(rebuilt.imports, facts.imports);
        assert_eq!(rebuilt.impls, facts.impls);
    }

    #[test]
    fn join_sorts_impl_blocks_by_file_position_and_identity() {
        let mut facts = facts();
        facts.impls.push(ImplBlock {
            impl_id: "impl_b".into(),
            file: "a.rs".into(),
            file_hash: "hash-a.rs".into(),
            span: span(9, 1),
            trait_path: "Debug".into(),
            trait_span: span(9, 7),
            self_type: "Alpha".into(),
            generics: vec![],
            negative: false,
            unsafe_impl: false,
            test_only: false,
            methods: vec![],
        });
        facts.impls.push(ImplBlock {
            impl_id: "impl_c".into(),
            file: "b.rs".into(),
            file_hash: "hash-b.rs".into(),
            span: span(1, 1),
            trait_path: "Clone".into(),
            trait_span: span(1, 7),
            self_type: "Beta".into(),
            generics: vec![],
            negative: false,
            unsafe_impl: false,
            test_only: false,
            methods: vec![],
        });
        let (shards, order) = split(&facts);
        let rebuilt = join(shards.into_values().collect(), order).unwrap();
        let ids = rebuilt
            .impls
            .iter()
            .map(|block| block.impl_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["impl_b", "impl_a", "impl_c"]);
    }

    #[test]
    fn join_rejects_out_of_range_duplicate_and_incomplete_orders() {
        let facts = facts();
        let (shards, mut order) = split(&facts);
        let template = shards.into_values().collect::<Vec<_>>();

        order.symbols[0] = 99;
        let error = join(template.clone(), order).err().unwrap();
        assert!(
            format!("{error:#}").contains("invalid syntax ordering"),
            "{error:#}"
        );

        let (_, mut order) = split(&facts);
        order.calls = vec![1, 1];
        let error = join(template.clone(), order).err().unwrap();
        assert!(
            format!("{error:#}").contains("invalid syntax ordering"),
            "{error:#}"
        );

        let (_, mut order) = split(&facts);
        order.occurrences.pop();
        let error = join(template.clone(), order).err().unwrap();
        assert!(
            format!("{error:#}").contains("incomplete syntax ordering"),
            "{error:#}"
        );

        let (_, mut order) = split(&facts);
        order.diagnostics.push(0);
        let error = join(template, order).err().unwrap();
        assert!(
            format!("{error:#}").contains("invalid syntax ordering"),
            "{error:#}"
        );
    }

    #[test]
    fn empty_facts_split_into_no_shards_and_join_back() {
        let (shards, order) = split(&SyntaxFacts::default());
        assert!(shards.is_empty());
        assert!(order.symbols.is_empty());
        assert!(order.calls.is_empty());
        assert!(order.occurrences.is_empty());
        assert!(order.diagnostics.is_empty());
        assert_eq!(join(vec![], order).unwrap(), SyntaxFacts::default());
    }
}
