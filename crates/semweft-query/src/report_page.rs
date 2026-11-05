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
