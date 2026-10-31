use semweft_core::*;
use serde_json::json;

fn block(file: &str) -> ImplBlock {
    ImplBlock {
        impl_id: format!("impl:{file}"),
        file: file.into(),
        file_hash: "hash".into(),
        span: Span {
            start: Position { line: 1, column: 1 },
            end: Position { line: 3, column: 2 },
        },
        trait_path: "module::Trait".into(),
        trait_span: Span {
            start: Position {
                line: 1,
                column: 14,
            },
            end: Position {
                line: 1,
                column: 19,
            },
        },
        self_type: "Container<T>".into(),
        generics: vec!["T".into()],
        negative: true,
        unsafe_impl: true,
        test_only: true,
        methods: vec!["method".into()],
    }
}

#[test]
fn syntax_facts_without_impls_deserialize_as_empty() {
    let legacy = json!({
        "symbols": [],
        "calls": [],
        "diagnostics": [],
        "skeletons": {}
    });
    let facts: SyntaxFacts = serde_json::from_value(legacy).unwrap();
    assert_eq!(facts, SyntaxFacts::default());
    let with_impls = SyntaxFacts {
        impls: vec![block("a.rs")],
        ..Default::default()
    };
    assert_eq!(
        serde_json::from_slice::<SyntaxFacts>(&serde_json::to_vec(&with_impls).unwrap()).unwrap(),
        with_impls
    );
}

#[test]
fn syntax_facts_extend_keeps_impls_from_each_file() {
    let mut facts = SyntaxFacts {
        impls: vec![block("a.rs")],
        ..Default::default()
    };
    let other = SyntaxFacts {
        impls: vec![block("b.rs"), block("c.rs")],
        ..Default::default()
    };
    facts.extend(&other);
    assert_eq!(facts.impls, [block("a.rs"), block("b.rs"), block("c.rs")]);
    assert_eq!(other.impls, [block("b.rs"), block("c.rs")]);
    facts.extend(&SyntaxFacts::default());
    assert_eq!(facts.impls.len(), 3);
}
