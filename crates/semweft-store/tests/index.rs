use semweft_core::*;
use semweft_store::{Direction, IndexedView};
use std::{collections::BTreeMap, sync::Arc};

fn span() -> Span {
    Span {
        start: Position { line: 1, column: 1 },
        end: Position { line: 1, column: 2 },
    }
}

fn view(id: &str, semantic: u64) -> Arc<AnalysisView> {
    let manifest = Manifest {
        workspace_id: "workspace".into(),
        files: ["a.rs", "b.rs"]
            .into_iter()
            .map(|file| (file.into(), SourceFile::new("x\n")))
            .collect(),
        binary_files: BTreeMap::new(),
        analysis_versions: BTreeMap::new(),
        scope: "index fixture".into(),
    };
    let symbols = [("b", "b.rs"), ("a", "a.rs"), ("nested", "a.rs")]
        .into_iter()
        .map(|(name, file)| Symbol {
            symbol_id: name.into(),
            name: name.into(),
            qualified_name: name.into(),
            kind: "function".into(),
            file: file.into(),
            file_hash: manifest.files[file].file_hash.clone(),
            span: span(),
            name_span: span(),
            signature: format!("fn {name}()"),
        })
        .collect();
    Arc::new(AnalysisView {
        snapshot: Arc::new(Snapshot {
            snapshot_id: id.into(),
            syntax_epoch: 10,
            input_hash: manifest.input_hash(),
            manifest,
            syntax: SyntaxFacts {
                symbols,
                occurrences: vec![Occurrence {
                    name: "a".into(),
                    file: "a.rs".into(),
                    span: span(),
                    kind: "call".into(),
                    enclosing_symbol_id: None,
                }],
                ..SyntaxFacts::default()
            },
        }),
        epochs: Epochs {
            syntax_epoch: 10,
            semantic_epoch: semantic,
        },
        relations: vec![],
        pending_units: vec![],
        known_gaps: vec![],
        resolutions: vec![],
    })
}

fn relation(id: &str, from: &str, to: &str) -> Relation {
    Relation {
        relation_id: id.into(),
        kind: "call".into(),
        from_symbol: from.into(),
        to_symbol: to.into(),
        file: "a.rs".into(),
        span: span(),
        certainty: Certainty::Candidate,
        freshness: "current".into(),
        runtime: "unavailable".into(),
        evidence: vec![],
        origin: None,
    }
}

fn relations() -> Vec<Relation> {
    vec![
        relation("r3", "b", "a"),
        relation("r1", "a", "a"),
        relation("r4", "a", "b"),
        relation("r2", "a", "b"),
        relation("r5", "file:a.rs", "external"),
    ]
}

#[test]
fn adjacency_matches_full_scans_in_every_direction_and_preserves_external_endpoints() {
    let index = IndexedView::new(view("s1", 0), relations());
    for node in ["a", "b", "nested", "file:a.rs", "external", "missing"] {
        for direction in [Direction::Incoming, Direction::Outgoing, Direction::Both] {
            let expected = index
                .relations()
                .iter()
                .filter(|r| match direction {
                    Direction::Incoming => r.to_symbol == node,
                    Direction::Outgoing => r.from_symbol == node,
                    Direction::Both => r.to_symbol == node || r.from_symbol == node,
                })
                .map(|r| &r.relation_id)
                .collect::<Vec<_>>();
            let actual = index
                .adjacent(node, direction)
                .map(|r| &r.relation_id)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{node} {direction:?}");
        }
    }
    assert!(index.symbol("external").is_none());
    assert!(index.relation("missing").is_none());
    assert_eq!(index.relation("r5").unwrap().to_symbol, "external");
}

#[test]
fn file_indexes_keep_published_order_and_old_coordinates_cannot_resolve_current_source() {
    let mut edges = relations();
    edges[0].origin = Some(RelationOrigin {
        snapshot_id: "old".into(),
        epochs: Epochs {
            syntax_epoch: 1,
            semantic_epoch: 1,
        },
        file_hash: "old hash".into(),
        reason: "pending refresh".into(),
    });
    edges[0].freshness = "pending".into();
    let index = IndexedView::new(view("s1", 0), edges);
    assert_eq!(
        index
            .symbols_in_file("a.rs")
            .map(|s| s.symbol_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "nested"]
    );
    assert_eq!(index.occurrences_in_file("a.rs").count(), 1);
    assert_eq!(index.occurrences_in_file("b.rs").count(), 0);
    assert_eq!(index.symbols_in_file("missing").count(), 0);
    let current = index
        .relations_at("a.rs", span())
        .map(|r| r.relation_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(current, ["r1", "r2", "r4", "r5"]);
    assert!(
        index
            .adjacent("a", Direction::Incoming)
            .any(|r| r.relation_id == "r3")
    );
    assert_eq!(index.relations_at("missing", span()).count(), 0);
}

#[test]
fn relation_input_order_does_not_change_indexes() {
    let a = IndexedView::new(view("s1", 0), relations());
    let b = IndexedView::new(view("s1", 0), relations().into_iter().rev().collect());
    assert_eq!(a.relations(), b.relations());
    for node in ["a", "b", "external"] {
        assert_eq!(
            a.adjacent(node, Direction::Both).collect::<Vec<_>>(),
            b.adjacent(node, Direction::Both).collect::<Vec<_>>()
        );
    }
}

#[test]
fn indexes_are_bound_to_their_owned_snapshot_and_semantic_epoch() {
    let pending = IndexedView::new(view("s1", 0), relations());
    let complete = IndexedView::new(view("s1", 10), vec![]);
    let edited = IndexedView::new(view("s2", 0), vec![relation("edited", "a", "external")]);
    assert_eq!(pending.view().snapshot.snapshot_id, "s1");
    assert_eq!(complete.view().epochs.semantic_epoch, 10);
    assert_eq!(edited.view().snapshot.snapshot_id, "s2");
    assert_eq!(pending.adjacent("a", Direction::Both).count(), 4);
    assert_eq!(complete.adjacent("a", Direction::Both).count(), 0);
    assert_eq!(edited.adjacent("a", Direction::Both).count(), 1);
}
