use crate::{engine::StoredView, query};
use anyhow::Result;
use semweft_core::{Certainty, Epochs, Evidence, Relation, RelationOrigin, Span};
use semweft_store::codec::{self, Codec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, ops::Range, sync::Arc};

#[derive(Default)]
struct Dictionary<'a> {
    ids: HashMap<&'a str, u32>,
    values: Vec<&'a str>,
}

impl<'a> Dictionary<'a> {
    fn intern(&mut self, value: &'a str) -> u32 {
        let next = self.values.len() as u32;
        *self.ids.entry(value).or_insert_with(|| {
            self.values.push(value);
            next
        })
    }
}

#[derive(Serialize, Deserialize)]
struct PackedEvidence {
    strings: [u32; 5],
    certainty: Certainty,
}

#[derive(Serialize, Deserialize)]
struct PackedOrigin {
    strings: [u32; 3],
    epochs: Epochs,
}

#[derive(Serialize, Deserialize)]
struct PackedEdge {
    strings: [u32; 7],
    span: Span,
    certainty: Certainty,
    evidence: Vec<PackedEvidence>,
    origin: Option<PackedOrigin>,
    depth: u64,
    path: Vec<u32>,
}

impl PackedEdge {
    fn new<'a>(edge: &'a (Relation, u64, Vec<String>), dictionary: &mut Dictionary<'a>) -> Self {
        let (relation, depth, path) = edge;
        Self {
            strings: [
                &relation.relation_id,
                &relation.kind,
                &relation.from_symbol,
                &relation.to_symbol,
                &relation.file,
                &relation.freshness,
                &relation.runtime,
            ]
            .map(|value| dictionary.intern(value)),
            span: relation.span,
            certainty: relation.certainty.clone(),
            evidence: relation
                .evidence
                .iter()
                .map(|evidence| PackedEvidence {
                    strings: [
                        evidence.evidence_id.as_str(),
                        evidence.source.as_str(),
                        &evidence.indexer,
                        &evidence.derivation,
                        &evidence.file_hash,
                    ]
                    .map(|value| dictionary.intern(value)),
                    certainty: evidence.certainty.clone(),
                })
                .collect(),
            origin: relation.origin.as_ref().map(|origin| PackedOrigin {
                strings: [&origin.snapshot_id, &origin.file_hash, &origin.reason]
                    .map(|value| dictionary.intern(value)),
                epochs: origin.epochs.clone(),
            }),
            depth: *depth,
            path: path.iter().map(|id| dictionary.intern(id)).collect(),
        }
    }

    fn relation(&self, strings: &[Arc<str>]) -> Relation {
        let text = |index: u32| strings[index as usize].to_string();
        Relation {
            relation_id: text(self.strings[0]),
            kind: text(self.strings[1]),
            from_symbol: text(self.strings[2]),
            to_symbol: text(self.strings[3]),
            file: text(self.strings[4]),
            freshness: text(self.strings[5]),
            runtime: text(self.strings[6]),
            span: self.span,
            certainty: self.certainty.clone(),
            evidence: self
                .evidence
                .iter()
                .map(|evidence| Evidence {
                    evidence_id: text(evidence.strings[0]),
                    source: text(evidence.strings[1]),
                    indexer: strings[evidence.strings[2] as usize].clone(),
                    derivation: strings[evidence.strings[3] as usize].clone(),
                    file_hash: strings[evidence.strings[4] as usize].clone(),
                    certainty: evidence.certainty.clone(),
                })
                .collect(),
            origin: self.origin.as_ref().map(|origin| RelationOrigin {
                snapshot_id: text(origin.strings[0]),
                file_hash: text(origin.strings[1]),
                reason: text(origin.strings[2]),
                epochs: origin.epochs.clone(),
            }),
        }
    }
}

pub(crate) struct GraphRelations {
    blocks: Vec<Vec<u8>>,
    length: usize,
    symbols: HashMap<String, (String, &'static str)>,
    file_hashes: HashMap<String, String>,
    snapshot_id: String,
}

impl GraphRelations {
    const BLOCK_SIZE: usize = 256;

    pub(crate) fn new(
        edges: Vec<(Relation, u64, Vec<String>)>,
        stored: &StoredView,
    ) -> Result<Self> {
        let _timing = crate::timing::Timing::new("graph_pack");
        let mut symbols: HashMap<String, (String, &'static str)> = HashMap::new();
        let mut file_hashes: HashMap<String, String> = HashMap::new();
        let index = stored.index();
        for (relation, _, _) in &edges {
            for symbol_id in [&relation.from_symbol, &relation.to_symbol] {
                if !symbols.contains_key(symbol_id.as_str())
                    && let Some(symbol) = index.symbol(symbol_id)
                {
                    symbols.insert(
                        symbol_id.clone(),
                        (
                            query::public_name(symbol),
                            query::test_scope(symbol, stored),
                        ),
                    );
                }
            }
            if !file_hashes.contains_key(relation.file.as_str())
                && let Some(file) = stored.view.snapshot.manifest.files.get(&relation.file)
            {
                file_hashes.insert(relation.file.clone(), file.file_hash.clone());
            }
        }
        let blocks = edges
            .chunks(Self::BLOCK_SIZE)
            .map(|block| {
                query::check_cancelled()?;
                let mut dictionary = Dictionary::default();
                let packed = block
                    .iter()
                    .map(|edge| PackedEdge::new(edge, &mut dictionary))
                    .collect::<Vec<_>>();
                Ok(codec::encode(
                    &serde_json::to_vec(&(dictionary.values, packed))?,
                    Codec::Lz4,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            blocks,
            length: edges.len(),
            symbols,
            file_hashes,
            snapshot_id: stored.view.snapshot.snapshot_id.clone(),
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.length
    }

    pub(crate) fn values(&self, range: Range<usize>) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(range.len());
        let mut position = range.start;
        while position < range.end {
            let block_index = position / Self::BLOCK_SIZE;
            let decoded = codec::decode(&self.blocks[block_index])?;
            let (strings, block): (Vec<Arc<str>>, Vec<PackedEdge>) =
                serde_json::from_slice(&decoded)?;
            let begin = position % Self::BLOCK_SIZE;
            let finish = block.len().min(range.end - block_index * Self::BLOCK_SIZE);
            for edge in &block[begin..finish] {
                let relation = edge.relation(&strings);
                let from = self.symbols.get(&relation.from_symbol);
                let to = self.symbols.get(&relation.to_symbol);
                let mut value = query::relation_value_with_context(
                    &relation,
                    from.map(|(name, _)| name.as_str())
                        .unwrap_or(&relation.from_symbol),
                    to.map(|(name, _)| name.as_str()),
                    from.map(|(_, scope)| *scope).unwrap_or("production"),
                    self.file_hashes.get(&relation.file).map(String::as_str),
                    &self.snapshot_id,
                );
                value["depth"] = serde_json::json!(edge.depth);
                value["path"] = serde_json::json!(
                    edge.path
                        .iter()
                        .map(|&id| &strings[id as usize])
                        .collect::<Vec<_>>()
                );
                values.push(value);
            }
            position += finish - begin;
        }
        Ok(values)
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        let strings =
            |values: &[&String]| values.iter().map(|value| value.capacity()).sum::<usize>();
        let mut bytes = std::mem::size_of::<Self>()
            + self.snapshot_id.capacity()
            + self.blocks.capacity() * std::mem::size_of::<Vec<u8>>()
            + self.blocks.iter().map(Vec::capacity).sum::<usize>();
        for (symbol_id, (name, scope)) in &self.symbols {
            bytes += strings(&[symbol_id, name])
                + std::mem::size_of::<(String, (String, &str))>()
                + scope.len()
                + 4 * std::mem::size_of::<usize>();
        }
        for (file, hash) in &self.file_hashes {
            bytes += strings(&[file, hash])
                + std::mem::size_of::<(String, String)>()
                + 4 * std::mem::size_of::<usize>();
        }
        bytes.saturating_mul(2)
    }
}
