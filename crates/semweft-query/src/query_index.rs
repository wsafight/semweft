//! Per-query relation index.
//!
//! Syntax (L1) name matching pairs every same-named occurrence with every
//! same-named symbol, which is several times the number of occurrences. Keeping
//! those edges resident costs far more than the lookups that need them, so this
//! index stores only the small published/reference layers plus compact name,
//! caller and location tables, and derives the edges incident to one symbol or
//! location when asked. Derived edges are built by the same function as the full
//! derivation, then overlaid with any explicit relation that has the same id in
//! the same order the full merge uses, so results are identical.
use crate::engine::{L1_DERIVATION, l1_applies, l1_caller, l1_relation};
use semweft_core::*;
use semweft_store::{Direction, IndexedView};
use std::{collections::BTreeMap, sync::Arc};

pub struct QueryIndex {
    explicit: IndexedView,
    indexer: Arc<str>,
    derivation: Arc<str>,
    file_hashes: BTreeMap<String, Arc<str>>,
    symbols_by_name: BTreeMap<String, Vec<usize>>,
    names_by_symbol_id: BTreeMap<String, Vec<String>>,
    occurrences_by_name: BTreeMap<String, Vec<usize>>,
    /// Occurrences keyed by the symbol that encloses them.
    enclosed: BTreeMap<String, Vec<usize>>,
    /// Top-level occurrences keyed by file; their caller is the `file:` node.
    top_level: BTreeMap<String, Vec<usize>>,
    occurrences_by_location: BTreeMap<String, BTreeMap<Span, Vec<usize>>>,
}

/// Look the key up first so it is cloned only the first time it is seen.
fn push(map: &mut BTreeMap<String, Vec<usize>>, key: &str, value: usize) {
    match map.get_mut(key) {
        Some(values) => values.push(value),
        None => {
            map.insert(key.to_owned(), vec![value]);
        }
    }
}

/// Layer `explicit` over `derived` exactly as the full merge folds a later
/// layer onto an earlier one.
fn overlay(derived: &mut Relation, explicit: &Relation) {
    derived.evidence.extend(explicit.evidence.iter().cloned());
    derived.certainty = explicit.certainty.clone();
    derived.freshness = explicit.freshness.clone();
    derived.origin = explicit.origin.clone();
}

impl QueryIndex {
    /// `explicit` is the already folded published and reference relations.
    pub(crate) fn new(view: Arc<AnalysisView>, explicit: Vec<Relation>, indexer: &str) -> Self {
        let mut symbols_by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut names_by_symbol_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (i, symbol) in view.snapshot.syntax.symbols.iter().enumerate() {
            push(&mut symbols_by_name, &symbol.name, i);
            let names = names_by_symbol_id
                .entry(symbol.symbol_id.clone())
                .or_default();
            if !names.contains(&symbol.name) {
                names.push(symbol.name.clone());
            }
        }
        let mut occurrences_by_name = BTreeMap::new();
        let mut enclosed = BTreeMap::new();
        let mut top_level = BTreeMap::new();
        let mut occurrences_by_location: BTreeMap<String, BTreeMap<Span, Vec<usize>>> =
            BTreeMap::new();
        let mut file_hashes: BTreeMap<String, Arc<str>> = BTreeMap::new();
        for (i, occurrence) in view.snapshot.syntax.occurrences.iter().enumerate() {
            push(&mut occurrences_by_name, &occurrence.name, i);
            match &occurrence.enclosing_symbol_id {
                Some(symbol) => push(&mut enclosed, symbol, i),
                None => push(&mut top_level, &occurrence.file, i),
            }
            if !occurrences_by_location.contains_key(occurrence.file.as_str()) {
                occurrences_by_location.insert(occurrence.file.clone(), BTreeMap::new());
            }
            occurrences_by_location
                .get_mut(occurrence.file.as_str())
                .unwrap()
                .entry(occurrence.span)
                .or_default()
                .push(i);
            if !file_hashes.contains_key(occurrence.file.as_str()) {
                let hash = view
                    .snapshot
                    .manifest
                    .files
                    .get(&occurrence.file)
                    .map_or("", |file| file.file_hash.as_str());
                file_hashes.insert(occurrence.file.clone(), hash.into());
            }
        }
        Self {
            explicit: IndexedView::new(view, explicit),
            indexer: indexer.into(),
            derivation: L1_DERIVATION.into(),
            file_hashes,
            symbols_by_name,
            names_by_symbol_id,
            occurrences_by_name,
            enclosed,
            top_level,
            occurrences_by_location,
        }
    }

    /// Build the relations of one occurrence, optionally only those ending at
    /// `only_target`, keyed by id so repeats collapse.
    fn derive(
        &self,
        occurrence: usize,
        only_target: Option<&str>,
        out: &mut BTreeMap<String, Relation>,
    ) {
        let syntax = &self.explicit.view().snapshot.syntax;
        let occurrence = &syntax.occurrences[occurrence];
        let Some(targets) = self.symbols_by_name.get(occurrence.name.as_str()) else {
            return;
        };
        let caller = l1_caller(occurrence);
        let span = serde_json::to_string(&occurrence.span).unwrap();
        let file_hash = &self.file_hashes[occurrence.file.as_str()];
        for &target in targets {
            let symbol = &syntax.symbols[target];
            if only_target.is_some_and(|id| symbol.symbol_id != id)
                || !l1_applies(occurrence, symbol)
            {
                continue;
            }
            let relation = l1_relation(
                occurrence,
                &caller,
                &span,
                symbol,
                [&self.indexer, &self.derivation, file_hash],
            );
            out.insert(relation.relation_id.clone(), relation);
        }
    }

    pub fn view(&self) -> &Arc<AnalysisView> {
        self.explicit.view()
    }

    pub fn symbol(&self, id: &str) -> Option<&Symbol> {
        self.explicit.symbol(id)
    }

    pub fn symbols_in_file<'a>(&'a self, file: &str) -> impl Iterator<Item = &'a Symbol> + use<'a> {
        self.explicit.symbols_in_file(file)
    }

    pub fn occurrences_in_file<'a>(
        &'a self,
        file: &str,
    ) -> impl Iterator<Item = &'a Occurrence> + use<'a> {
        self.explicit.occurrences_in_file(file)
    }

    /// Relations incident to `id` in relation-id order; a self-loop appears once.
    pub fn adjacent(&self, id: &str, direction: Direction) -> Vec<Relation> {
        let mut edges = BTreeMap::new();
        if direction != Direction::Incoming {
            for &o in self.enclosed.get(id).into_iter().flatten() {
                self.derive(o, None, &mut edges);
            }
            if let Some(file) = id.strip_prefix("file:") {
                for &o in self.top_level.get(file).into_iter().flatten() {
                    self.derive(o, None, &mut edges);
                }
            }
        }
        if direction != Direction::Outgoing {
            for name in self.names_by_symbol_id.get(id).into_iter().flatten() {
                for &o in self
                    .occurrences_by_name
                    .get(name.as_str())
                    .into_iter()
                    .flatten()
                {
                    self.derive(o, Some(id), &mut edges);
                }
            }
        }
        for explicit in self.explicit.adjacent(id, direction) {
            match edges.get_mut(&explicit.relation_id) {
                Some(derived) => overlay(derived, explicit),
                None => {
                    edges.insert(explicit.relation_id.clone(), explicit.clone());
                }
            }
        }
        edges.into_values().collect()
    }

    /// Relations anchored at a current-source location. A relation whose final
    /// origin is an older snapshot has no current coordinates and is excluded.
    pub fn relations_at(&self, file: &str, span: Span) -> Vec<Relation> {
        let mut edges = BTreeMap::new();
        for &o in self
            .occurrences_by_location
            .get(file)
            .and_then(|spans| spans.get(&span))
            .into_iter()
            .flatten()
        {
            self.derive(o, None, &mut edges);
        }
        for derived in edges.values_mut() {
            if let Some(explicit) = self.explicit.relation(&derived.relation_id) {
                overlay(derived, explicit);
            }
        }
        edges.retain(|_, relation| relation.origin.is_none());
        for explicit in self.explicit.relations_at(file, span) {
            edges
                .entry(explicit.relation_id.clone())
                .or_insert_with(|| explicit.clone());
        }
        edges.into_values().collect()
    }

    pub fn estimated_heap_bytes(&self) -> usize {
        fn keyed<V>(map: &BTreeMap<String, Vec<V>>) -> usize {
            map.iter()
                .map(|(key, values)| {
                    key.capacity() + values.capacity() * std::mem::size_of::<V>() + 96
                })
                .sum()
        }
        let locations: usize = self
            .occurrences_by_location
            .iter()
            .map(|(file, spans)| {
                file.capacity()
                    + spans
                        .values()
                        .map(|offsets| offsets.capacity() * std::mem::size_of::<usize>() + 120)
                        .sum::<usize>()
            })
            .sum();
        let names: usize = self
            .names_by_symbol_id
            .iter()
            .map(|(id, names)| {
                id.capacity() + names.iter().map(|n| n.capacity() + 24).sum::<usize>() + 96
            })
            .sum();
        self.explicit.estimated_heap_bytes()
            + keyed(&self.symbols_by_name)
            + keyed(&self.occurrences_by_name)
            + keyed(&self.enclosed)
            + keyed(&self.top_level)
            + locations
            + names
            + self
                .file_hashes
                .iter()
                .map(|(file, hash)| file.capacity() + hash.len() + 64)
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::{RelationDeriver, StoredView},
        frontend::Frontends,
    };

    fn layered() -> StoredView {
        let mut manifest = Manifest {
            workspace_id: "query-index".into(),
            files: BTreeMap::new(),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "fixture".into(),
        };
        // `t` is declared twice (ambiguous), called from several functions and
        // from top-level code, and `S` is used as a type.
        manifest.files.insert(
            "a.rs".into(),
            SourceFile::new(
                "pub struct S;\npub fn t() {}\npub fn x() { t(); t(); }\npub fn y(_: S) { t(); x(); }\npub const C: u32 = { t(); 1 };\n",
            ),
        );
        manifest.files.insert(
            "b.rs".into(),
            SourceFile::new("pub fn t() {}\npub fn z() { t(); crate::x(); }\n"),
        );
        let syntax = Frontends::new().unwrap().analyze(&manifest).unwrap();
        let mut view = RefreshCoordinator::default()
            .publish_inputs(manifest, syntax)
            .unwrap();
        let derived = RelationDeriver::default().derive(&view);
        assert!(derived.len() >= 8, "fixture too small: {}", derived.len());

        let evidence = |tag: &str, certainty: Certainty| Evidence {
            evidence_id: format!("ev-{tag}"),
            source: tag.into(),
            certainty,
            indexer: "test".into(),
            derivation: tag.into(),
            file_hash: "h".into(),
        };
        let origin = RelationOrigin {
            snapshot_id: "older".into(),
            epochs: Epochs {
                syntax_epoch: 1,
                semantic_epoch: 1,
            },
            file_hash: "old".into(),
            reason: "historical".into(),
        };
        // Published layer repeats a derived id.
        let mut published = derived[0].clone();
        published.evidence = vec![evidence("published", Certainty::Candidate)];
        published.freshness = "pending".into();
        Arc::make_mut(&mut view).relations.push(published);
        // Reference layer: overlap that upgrades certainty, an overlap that moves
        // to an older origin, and explicit-only relations at an existing location
        // with and without an origin.
        let mut upgraded = derived[0].clone();
        upgraded.evidence = vec![evidence("L3", Certainty::Definite)];
        upgraded.certainty = Certainty::Definite;
        let mut historical = derived[1].clone();
        historical.origin = Some(origin.clone());
        let mut only_current = derived[2].clone();
        only_current.relation_id = "rel_only_current".into();
        only_current.evidence = vec![evidence("L3", Certainty::Definite)];
        let mut only_historical = derived[2].clone();
        only_historical.relation_id = "rel_only_historical".into();
        only_historical.origin = Some(origin);
        StoredView::new(
            view,
            vec![upgraded, historical, only_current, only_historical],
            vec![],
        )
    }

    #[test]
    fn point_lookups_match_the_fully_merged_index() {
        let stored = layered();
        let full = stored.full_index();
        let lazy = stored.index();
        let syntax = &stored.view.snapshot.syntax;

        let mut ids: Vec<String> = syntax.symbols.iter().map(|s| s.symbol_id.clone()).collect();
        ids.extend(
            stored
                .view
                .snapshot
                .manifest
                .files
                .keys()
                .map(|f| format!("file:{f}")),
        );
        ids.push("sym_missing".into());
        let mut compared = 0;
        for id in &ids {
            for direction in [Direction::Incoming, Direction::Outgoing, Direction::Both] {
                let expected: Vec<Relation> = full.adjacent(id, direction).cloned().collect();
                assert_eq!(lazy.adjacent(id, direction), expected, "{id} {direction:?}");
                compared += expected.len();
            }
        }
        assert!(compared >= 30, "adjacency comparison too thin: {compared}");

        let mut spans: Vec<(String, Span)> = syntax
            .occurrences
            .iter()
            .map(|o| (o.file.clone(), o.span))
            .collect();
        spans.extend(full.relations().iter().map(|r| (r.file.clone(), r.span)));
        let nowhere = Span {
            start: Position {
                line: 9999,
                column: 1,
            },
            end: Position {
                line: 9999,
                column: 2,
            },
        };
        spans.push(("a.rs".into(), nowhere));
        spans.push(("missing.rs".into(), nowhere));
        for (file, span) in &spans {
            let expected: Vec<Relation> = full.relations_at(file, *span).cloned().collect();
            assert_eq!(lazy.relations_at(file, *span), expected, "{file} {span:?}");
        }
    }

    #[test]
    fn layers_fold_in_order_and_origins_leave_current_coordinates() {
        let stored = layered();
        let lazy = stored.index();
        let full = stored.full_index();
        // The published and reference layers both landed on the first relation:
        // evidence accumulates in layer order and the last certainty wins.
        let first = full
            .relations()
            .iter()
            .find(|r| r.evidence.iter().any(|e| e.source == "published"))
            .unwrap();
        let sources: Vec<&str> = first.evidence.iter().map(|e| e.source.as_str()).collect();
        assert_eq!(sources, ["L1", "published", "L3"]);
        assert_eq!(first.certainty, Certainty::Definite);
        let seen = lazy.adjacent(&first.to_symbol, Direction::Incoming);
        assert_eq!(
            seen.iter().find(|r| r.relation_id == first.relation_id),
            Some(first)
        );
        // Origin-bearing relations are still adjacent but have no current location.
        let located = |id: &str| {
            full.relations()
                .iter()
                .filter(|r| r.relation_id == id)
                .flat_map(|r| lazy.relations_at(&r.file, r.span))
                .any(|r| r.relation_id == id)
        };
        assert!(located("rel_only_current"));
        assert!(!located("rel_only_historical"));
        let historical = full
            .relations()
            .iter()
            .find(|r| r.origin.is_some() && r.relation_id != "rel_only_historical")
            .unwrap();
        assert!(!located(&historical.relation_id));
        assert!(
            lazy.adjacent(&historical.to_symbol, Direction::Incoming)
                .iter()
                .any(|r| r.relation_id == historical.relation_id)
        );
    }
}
