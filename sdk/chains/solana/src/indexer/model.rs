use std::{fmt, str::FromStr};

use indexing::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, IndexedBlock};
use serde::Deserialize;
use solana_hash::Hash;

/// Exact finalized Solana block result plus its validated canonical identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    reference: BlockRef,
    raw: Vec<u8>,
}

impl Block {
    pub(crate) fn parse(slot: u64, raw: Vec<u8>) -> Result<Self, ParseError> {
        let wire: BlockWire = serde_json::from_slice(&raw)
            .map_err(|_| ParseError::new("Solana getBlock result has an invalid shape"))?;
        let hash = parse_hash(&wire.blockhash, "blockhash")?;
        let parent_hash = parse_hash(&wire.previous_blockhash, "previous blockhash")?;
        let height = wire
            .block_height
            .ok_or_else(|| ParseError::new("Solana finalized block has no produced height"))?;
        let timestamp = wire
            .block_time
            .map(|value| {
                u64::try_from(value)
                    .map_err(|_| ParseError::new("Solana block time must not be negative"))
            })
            .transpose()?;
        let parent = if slot == 0 {
            if wire.parent_slot != 0 {
                return Err(ParseError::new("Solana genesis parent slot must be zero"));
            }
            None
        } else {
            if wire.parent_slot >= slot {
                return Err(ParseError::new(
                    "Solana block parent slot must precede its slot",
                ));
            }
            Some(BlockParent {
                position: BlockPosition(wire.parent_slot),
                hash: parent_hash,
            })
        };

        Ok(Self {
            reference: BlockRef {
                position: BlockPosition(slot),
                height: BlockHeight(height),
                hash,
                parent,
                timestamp,
            },
            raw,
        })
    }

    #[must_use]
    pub const fn reference(&self) -> &BlockRef {
        &self.reference
    }

    #[must_use]
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
}

impl IndexedBlock for Block {
    fn block_ref(&self) -> BlockRef {
        self.reference.clone()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockWire {
    blockhash: String,
    previous_blockhash: String,
    parent_slot: u64,
    #[serde(rename = "transactions")]
    _transactions: Vec<serde_json::Value>,
    block_time: Option<i64>,
    block_height: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParseError {
    message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn parse_hash(text: &str, field: &'static str) -> Result<BlockHash, ParseError> {
    let hash = Hash::from_str(text)
        .map_err(|_| ParseError::new(format!("Solana {field} is not a canonical hash")))?;
    if hash.to_string() != text {
        return Err(ParseError::new(format!(
            "Solana {field} is not canonically encoded"
        )));
    }
    Ok(BlockHash(hash.to_bytes().to_vec()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn raw(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    fn block() -> serde_json::Value {
        json!({
            "blockhash": "11111111111111111111111111111111",
            "previousBlockhash": "11111111111111111111111111111111",
            "parentSlot": 6,
            "transactions": [],
            "blockTime": 123,
            "blockHeight": 4,
        })
    }

    #[test]
    fn retains_exact_raw_result_and_builds_sparse_reference() {
        let bytes = raw(block());
        let parsed = Block::parse(7, bytes.clone()).unwrap();
        assert_eq!(parsed.raw(), bytes);
        assert_eq!(parsed.reference().position, BlockPosition(7));
        assert_eq!(parsed.reference().height, BlockHeight(4));
        assert_eq!(parsed.reference().timestamp, Some(123));
        assert_eq!(
            parsed.reference().parent.as_ref().unwrap().position,
            BlockPosition(6)
        );
    }

    #[test]
    fn requires_height_transactions_canonical_hashes_and_strict_parent() {
        for malformed in [
            json!({
                "blockhash": "bad",
                "previousBlockhash": "11111111111111111111111111111111",
                "parentSlot": 6,
                "transactions": [],
                "blockTime": 1,
                "blockHeight": 4,
            }),
            json!({
                "blockhash": "11111111111111111111111111111111",
                "previousBlockhash": "bad",
                "parentSlot": 6,
                "transactions": [],
                "blockTime": 1,
                "blockHeight": 4,
            }),
            json!({
                "blockhash": "11111111111111111111111111111111",
                "previousBlockhash": "11111111111111111111111111111111",
                "parentSlot": 7,
                "transactions": [],
                "blockTime": 1,
                "blockHeight": 4,
            }),
            json!({
                "blockhash": "11111111111111111111111111111111",
                "previousBlockhash": "11111111111111111111111111111111",
                "parentSlot": 6,
                "blockTime": 1,
                "blockHeight": 4,
            }),
            json!({
                "blockhash": "11111111111111111111111111111111",
                "previousBlockhash": "11111111111111111111111111111111",
                "parentSlot": 6,
                "transactions": [],
                "blockTime": 1,
                "blockHeight": null,
            }),
        ] {
            assert!(Block::parse(7, raw(malformed)).is_err());
        }
    }

    #[test]
    fn accepts_only_a_zero_parent_for_genesis_and_non_negative_time() {
        let mut genesis = block();
        genesis["parentSlot"] = json!(0);
        assert!(Block::parse(0, raw(genesis)).is_ok());

        let mut bad_genesis = block();
        bad_genesis["parentSlot"] = json!(1);
        assert!(Block::parse(0, raw(bad_genesis)).is_err());

        let mut negative_time = block();
        negative_time["blockTime"] = json!(-1);
        assert!(Block::parse(7, raw(negative_time)).is_err());
    }
}
