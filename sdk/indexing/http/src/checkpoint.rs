use indexing::{BlockHash, BlockHeight, BlockRef, IndexError};
use serde::Deserialize;

use crate::wire::{parse_hex, parse_u64};

#[derive(Deserialize)]
pub(crate) struct BlockDto {
    height: String,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<String>,
}

impl BlockDto {
    pub(crate) fn convert(self) -> Result<BlockRef, IndexError> {
        Ok(BlockRef {
            height: BlockHeight(parse_u64(&self.height, "checkpoint height")?),
            hash: BlockHash(parse_hex(&self.hash, "checkpoint hash")?),
            parent_hash: self
                .parent_hash
                .map(|value| parse_hex(&value, "checkpoint parent hash").map(BlockHash))
                .transpose()?,
            timestamp: self
                .timestamp
                .map(|value| parse_u64(&value, "checkpoint timestamp"))
                .transpose()?,
        })
    }
}
