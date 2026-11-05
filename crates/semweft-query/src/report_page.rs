use anyhow::Result;
use semweft_store::codec::{self, Codec};
use serde_json::Value;
use std::ops::Range;

pub(crate) struct ReportList {
    blocks: Vec<Vec<u8>>,
    length: usize,
}

impl ReportList {
    const BLOCK_SIZE: usize = 128;

    pub(crate) fn new(values: Vec<Value>) -> Result<Self> {
        let blocks = values
            .chunks(Self::BLOCK_SIZE)
            .map(|block| {
                crate::query::check_cancelled()?;
                Ok(codec::encode(&serde_json::to_vec(block)?, Codec::Lz4))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            blocks,
            length: values.len(),
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.length
    }

    pub(crate) fn values(&self, range: Range<usize>) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(range.len());
        let mut position = range.start;
        while position < range.end {
            let index = position / Self::BLOCK_SIZE;
            let decoded = codec::decode(&self.blocks[index])?;
            let block: Vec<Value> = serde_json::from_slice(&decoded)?;
            let begin = position % Self::BLOCK_SIZE;
            let finish = block.len().min(range.end - index * Self::BLOCK_SIZE);
            values.extend(block.into_iter().skip(begin).take(finish - begin));
            position += finish - begin;
        }
        Ok(values)
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        (std::mem::size_of::<Self>()
            + self.blocks.capacity() * std::mem::size_of::<Vec<u8>>()
            + self.blocks.iter().map(Vec::capacity).sum::<usize>())
        .saturating_mul(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(length: usize) -> Vec<Value> {
        (0..length)
            .map(|i| serde_json::json!({"index": i}))
            .collect()
    }

    #[test]
    fn empty_lists_return_no_values() {
        let list = ReportList::new(vec![]).unwrap();
        assert_eq!(list.len(), 0);
        assert!(list.values(0..0).unwrap().is_empty());
        assert!(list.estimated_bytes() > 0);
    }

    #[test]
    fn blocks_round_trip_exact_and_partial_ranges() {
        for length in [1, 127, 128, 129, 256, 257, 300] {
            let source = values(length);
            let list = ReportList::new(source.clone()).unwrap();
            assert_eq!(list.len(), length);
            assert_eq!(list.values(0..length).unwrap(), source);
            assert_eq!(
                list.values(0..0).unwrap(),
                Vec::<Value>::new(),
                "empty range at {length}"
            );
            assert_eq!(
                list.values(length..length).unwrap(),
                Vec::<Value>::new(),
                "empty range at the end"
            );
        }
    }

    #[test]
    fn ranges_spanning_block_boundaries_preserve_order() {
        let source = values(300);
        let list = ReportList::new(source.clone()).unwrap();
        for range in [0..128, 127..129, 128..256, 100..260, 255..300, 0..2] {
            assert_eq!(
                list.values(range.clone()).unwrap(),
                source[range.clone()],
                "{range:?}"
            );
        }
    }

    #[test]
    fn single_values_keep_their_json_shape() {
        let list = ReportList::new(vec![
            serde_json::json!({"symbol": "a", "depth": 2}),
            serde_json::json!([1, 2, 3]),
            serde_json::json!("text"),
            Value::Null,
        ])
        .unwrap();
        let decoded = list.values(0..4).unwrap();
        assert_eq!(decoded[0]["symbol"], "a");
        assert_eq!(decoded[1], serde_json::json!([1, 2, 3]));
        assert_eq!(decoded[2], serde_json::json!("text"));
        assert_eq!(decoded[3], Value::Null);
    }
}
