//! Immutable query indexes over published facts. Language-specific relation
//! derivation and evidence merging belong to the caller; disk persistence is
//! independent of these rebuildable, in-memory indexes.
pub mod codec;
mod persistence;
pub use persistence::{SnapshotStore, atomic_write, private_dir};
pub use semweft_storage::{
    DiskUsage, GcStats, ObjectUsage, Publication, PublicationRef, PublicationStore, RecoveredStore,
    StorageMetrics, ViewInfo, ViewKey, WriteStats,
};

use semweft_core::{AnalysisView, Occurrence, Relation, Span, Symbol};
use std::{collections::BTreeMap, sync::Arc};

/// Allocation-free serialized payload size, used as a conservative cache weight
/// estimate with an explicit multiplier; this is not a process RSS measurement.
pub fn payload_bytes<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    if serde_json::to_writer(&mut counter, value).is_err() {
        return usize::MAX;
    }
    counter.0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
    Both,
}

/// Facts and their offsets are owned together so an index cannot accidentally
/// be read against another snapshot or semantic epoch. Inputs are published,
/// validated facts with unique symbol and relation IDs. External endpoints and
/// historical coordinates are retained without inventing local declarations.
pub struct IndexedView {
    view: Arc<AnalysisView>,
    relations: Vec<Relation>,
    symbols_by_id: BTreeMap<String, usize>,
    symbols_by_file: BTreeMap<String, Vec<usize>>,
    occurrences_by_file: BTreeMap<String, Vec<usize>>,
    incoming: BTreeMap<String, Vec<usize>>,
    outgoing: BTreeMap<String, Vec<usize>>,
    locations: BTreeMap<String, BTreeMap<Span, Vec<usize>>>,
}

impl IndexedView {
    pub fn estimated_heap_bytes(&self) -> usize {
        let offsets = |map: &BTreeMap<String, Vec<usize>>| {
            map.iter()
                .map(|(key, offsets)| {
                    key.capacity() + offsets.capacity() * std::mem::size_of::<usize>() + 96
                })
                .sum::<usize>()
        };
        payload_bytes(&self.relations)
            .saturating_mul(2)
            .saturating_add(offsets(&self.symbols_by_file))
            .saturating_add(offsets(&self.occurrences_by_file))
            .saturating_add(offsets(&self.incoming))
            .saturating_add(offsets(&self.outgoing))
            .saturating_add(
                self.symbols_by_id
                    .keys()
                    .map(|key| key.capacity() + 96)
                    .sum::<usize>(),
            )
            .saturating_add(
                self.locations
                    .iter()
                    .map(|(file, spans)| {
                        file.capacity()
                            + 96
                            + spans
                                .values()
                                .map(|offsets| {
                                    96 + offsets.capacity() * std::mem::size_of::<usize>()
                                })
                                .sum::<usize>()
                    })
                    .sum::<usize>(),
            )
    }
    pub fn new(view: Arc<AnalysisView>, mut relations: Vec<Relation>) -> Self {
        relations.sort_by(|a, b| a.relation_id.cmp(&b.relation_id));
        let mut index = Self {
            view,
            relations,
            symbols_by_id: BTreeMap::new(),
            symbols_by_file: BTreeMap::new(),
            occurrences_by_file: BTreeMap::new(),
            incoming: BTreeMap::new(),
            outgoing: BTreeMap::new(),
            locations: BTreeMap::new(),
        };
        for (i, symbol) in index.view.snapshot.syntax.symbols.iter().enumerate() {
            index
                .symbols_by_id
                .entry(symbol.symbol_id.clone())
                .or_insert(i);
            index
                .symbols_by_file
                .entry(symbol.file.clone())
                .or_default()
                .push(i);
        }
        // Most relations share a handful of symbols and files, so look the key up
        // first and clone it only when it is new.
        fn push<K: Ord + ToOwned<Owned = String> + ?Sized>(
            map: &mut BTreeMap<String, Vec<usize>>,
            key: &K,
            i: usize,
        ) where
            String: std::borrow::Borrow<K>,
        {
            match map.get_mut(key) {
                Some(offsets) => offsets.push(i),
                None => {
                    map.insert(key.to_owned(), vec![i]);
                }
            }
        }
        for (i, relation) in index.relations.iter().enumerate() {
            push(&mut index.incoming, relation.to_symbol.as_str(), i);
            push(&mut index.outgoing, relation.from_symbol.as_str(), i);
            // An origin-bearing relation has coordinates in its origin snapshot,
            // so it cannot resolve a position in the current captured source.
            if relation.origin.is_none() {
                if !index.locations.contains_key(relation.file.as_str()) {
                    index
                        .locations
                        .insert(relation.file.clone(), BTreeMap::new());
                }
                index
                    .locations
                    .get_mut(relation.file.as_str())
                    .unwrap()
                    .entry(relation.span)
                    .or_default()
                    .push(i);
            }
        }
        for (i, occurrence) in index.view.snapshot.syntax.occurrences.iter().enumerate() {
            index
                .occurrences_by_file
                .entry(occurrence.file.clone())
                .or_default()
                .push(i);
        }
        index
    }

    pub fn view(&self) -> &Arc<AnalysisView> {
        &self.view
    }

    pub fn symbol(&self, id: &str) -> Option<&Symbol> {
        self.symbols_by_id
            .get(id)
            .map(|&i| &self.view.snapshot.syntax.symbols[i])
    }

    /// Preserve the published symbol order, including ambiguous declarations.
    pub fn symbols_in_file<'a>(&'a self, file: &str) -> impl Iterator<Item = &'a Symbol> + use<'a> {
        self.symbols_by_file
            .get(file)
            .into_iter()
            .flatten()
            .map(|&i| &self.view.snapshot.syntax.symbols[i])
    }

    pub fn relations(&self) -> &[Relation] {
        &self.relations
    }

    pub fn occurrences_in_file<'a>(
        &'a self,
        file: &str,
    ) -> impl Iterator<Item = &'a Occurrence> + use<'a> {
        self.occurrences_by_file
            .get(file)
            .into_iter()
            .flatten()
            .map(|&i| &self.view.snapshot.syntax.occurrences[i])
    }

    pub fn relation(&self, id: &str) -> Option<&Relation> {
        self.relations
            .binary_search_by(|r| r.relation_id.as_str().cmp(id))
            .ok()
            .map(|i| &self.relations[i])
    }

    /// Incident edges in stable relation-ID order. Both merges the two sorted
    /// offset lists without allocation and yields self-loops exactly once.
    pub fn adjacent<'a>(
        &'a self,
        id: &str,
        direction: Direction,
    ) -> impl Iterator<Item = &'a Relation> + use<'a> {
        let mut incoming = self
            .incoming
            .get(id)
            .filter(|_| direction != Direction::Outgoing)
            .into_iter()
            .flatten()
            .peekable();
        let mut outgoing = self
            .outgoing
            .get(id)
            .filter(|_| direction != Direction::Incoming)
            .into_iter()
            .flatten()
            .peekable();
        std::iter::from_fn(move || {
            let offset = match (incoming.peek(), outgoing.peek()) {
                (Some(a), Some(b)) => match a.cmp(b) {
                    std::cmp::Ordering::Less => incoming.next(),
                    std::cmp::Ordering::Greater => outgoing.next(),
                    std::cmp::Ordering::Equal => {
                        outgoing.next();
                        incoming.next()
                    }
                },
                (Some(_), None) => incoming.next(),
                (None, Some(_)) => outgoing.next(),
                (None, None) => None,
            }?;
            Some(&self.relations[*offset])
        })
    }

    /// Current-source coordinates only; old origin spans remain on graph edges.
    pub fn relations_at<'a>(
        &'a self,
        file: &str,
        span: Span,
    ) -> impl Iterator<Item = &'a Relation> + use<'a> {
        self.locations
            .get(file)
            .and_then(|spans| spans.get(&span))
            .into_iter()
            .flatten()
            .map(|&i| &self.relations[i])
    }
}
