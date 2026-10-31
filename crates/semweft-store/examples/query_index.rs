//! Synthetic lookup measurement, not a semantic-quality or end-to-end benchmark.
use semweft_core::*;
use semweft_store::{Direction, IndexedView};
use std::{collections::BTreeMap, hint::black_box, sync::Arc, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let count: usize = args
        .next()
        .map(|n| n.parse())
        .transpose()?
        .unwrap_or(20_000);
    let queries: usize = args.next().map(|n| n.parse()).transpose()?.unwrap_or(1_000);
    assert!(count > 0 && queries > 0);
    let file = SourceFile::new("x\n");
    let span = Span {
        start: Position { line: 1, column: 1 },
        end: Position { line: 1, column: 2 },
    };
    let symbols = (0..count)
        .map(|i| Symbol {
            symbol_id: format!("s{i:08}"),
            name: format!("f{i}"),
            qualified_name: format!("f{i}"),
            kind: "function".into(),
            file: "synthetic.rs".into(),
            file_hash: file.file_hash.clone(),
            span,
            name_span: span,
            signature: format!("fn f{i}()"),
        })
        .collect::<Vec<_>>();
    let relations = (0..count)
        .map(|i| Relation {
            relation_id: format!("r{i:08}"),
            kind: "call".into(),
            from_symbol: symbols[i].symbol_id.clone(),
            to_symbol: symbols[(i + 1) % count].symbol_id.clone(),
            file: "synthetic.rs".into(),
            span,
            certainty: Certainty::Candidate,
            freshness: "current".into(),
            runtime: "unavailable".into(),
            evidence: vec![],
            origin: None,
        })
        .collect::<Vec<_>>();
    let manifest = Manifest {
        workspace_id: "synthetic".into(),
        files: BTreeMap::from([("synthetic.rs".into(), file)]),
        binary_files: BTreeMap::new(),
        analysis_versions: BTreeMap::new(),
        scope: "synthetic lookup load".into(),
    };
    let view = Arc::new(AnalysisView {
        snapshot: Arc::new(Snapshot {
            snapshot_id: "synthetic".into(),
            syntax_epoch: 1,
            input_hash: manifest.input_hash(),
            manifest,
            syntax: SyntaxFacts {
                symbols,
                ..SyntaxFacts::default()
            },
        }),
        epochs: Epochs {
            syntax_epoch: 1,
            semantic_epoch: 0,
        },
        relations: vec![],
        pending_units: vec![],
        known_gaps: vec![],
        resolutions: vec![],
    });
    let targets = (0..queries)
        .map(|i| format!("s{:08}", (i * 7919) % count))
        .collect::<Vec<_>>();
    let started = Instant::now();
    let mut scanned = 0;
    for target in &targets {
        let target = black_box(target);
        scanned += relations.iter().filter(|r| &r.to_symbol == target).count();
        scanned += usize::from(
            view.snapshot
                .syntax
                .symbols
                .iter()
                .any(|s| &s.symbol_id == target),
        );
    }
    black_box(scanned);
    let scan_us = started.elapsed().as_micros();
    let started = Instant::now();
    let index = IndexedView::new(view, relations);
    let build_us = started.elapsed().as_micros();
    let started = Instant::now();
    let mut indexed = 0;
    for target in &targets {
        let target = black_box(target);
        indexed += index.adjacent(target, Direction::Incoming).count();
        indexed += usize::from(index.symbol(target).is_some());
    }
    black_box(indexed);
    let indexed_us = started.elapsed().as_micros();
    assert_eq!(scanned, indexed);
    println!("symbols={count} relations={count} queries={queries} checksum={indexed}");
    println!("build_us={build_us} full_scan_us={scan_us} indexed_us={indexed_us}");
    Ok(())
}
