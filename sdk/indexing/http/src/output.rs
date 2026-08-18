use indexing::{
    AssetId, BlockHash, BlockHeight, BlockRef, CanonicalAddress, IndexError, IndexScope,
    IndexedOutput, OutputCursor, OutputId, OutputPage, OutputSnapshot, RebuildGeneration,
    TransactionRef,
};
use serde::Deserialize;

use crate::{
    checkpoint::BlockDto,
    wire::{encode_hex, invalid_response, parse_decimal, parse_hex, parse_u64},
};

#[derive(Deserialize)]
pub(crate) struct OutputsDto {
    generation: String,
    revision: String,
    checkpoint: Option<BlockDto>,
    outputs: Vec<OutputDto>,
    next: Option<String>,
}

impl OutputsDto {
    pub(crate) fn convert(
        self,
        scope: &IndexScope,
        address: &CanonicalAddress,
    ) -> Result<OutputPage, IndexError> {
        let snapshot = OutputSnapshot {
            generation: RebuildGeneration(parse_u64(&self.generation, "output generation")?),
            revision: parse_u64(&self.revision, "output revision")?,
            checkpoint: self.checkpoint.map(BlockDto::convert).transpose()?,
        };
        Ok(OutputPage {
            outputs: self
                .outputs
                .into_iter()
                .map(|output| output.convert(scope, address))
                .collect::<Result<Vec<_>, _>>()?,
            next: self.next.map(|value| decode_cursor(&value)).transpose()?,
            snapshot,
        })
    }
}

#[derive(Deserialize)]
struct OutputDto {
    transaction_id: String,
    output_index: String,
    asset: String,
    amount: String,
    evidence: String,
    address: String,
    created_height: String,
    coinbase: bool,
}

impl OutputDto {
    fn convert(
        self,
        scope: &IndexScope,
        requested_address: &CanonicalAddress,
    ) -> Result<IndexedOutput, IndexError> {
        if self.address != requested_address.value {
            return Err(invalid_response(
                "indexed output belongs to a different address",
            ));
        }
        let output_index = self
            .output_index
            .parse::<u32>()
            .map_err(|_| invalid_response("output index is not an unsigned 32-bit integer"))?;
        Ok(IndexedOutput {
            id: OutputId {
                transaction: TransactionRef {
                    scope: scope.clone(),
                    value: self.transaction_id,
                },
                index: output_index,
            },
            address: requested_address.clone(),
            asset: AssetId {
                chain: scope.chain.clone(),
                asset: self.asset,
            },
            amount: parse_decimal(&self.amount)?,
            evidence: parse_hex(&self.evidence, "output evidence")?,
            created_at: BlockHeight(parse_u64(&self.created_height, "output creation height")?),
            coinbase: self.coinbase,
        })
    }
}

pub(crate) fn encode_cursor(cursor: &OutputCursor) -> String {
    let (height, hash, parent, timestamp) = cursor.snapshot.checkpoint.as_ref().map_or_else(
        || {
            (
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
            )
        },
        |checkpoint| {
            (
                checkpoint.height.0.to_string(),
                encode_hex(&checkpoint.hash.0),
                checkpoint
                    .parent_hash
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |hash| encode_hex(&hash.0)),
                checkpoint
                    .timestamp
                    .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            )
        },
    );
    format!(
        "{}:{}:{height}:{hash}:{parent}:{timestamp}:{}",
        cursor.snapshot.generation.0,
        cursor.snapshot.revision,
        encode_hex(&cursor.position)
    )
}

fn decode_cursor(value: &str) -> Result<OutputCursor, IndexError> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 7 {
        return Err(invalid_response("output cursor has an invalid shape"));
    }
    let checkpoint = if parts[2] == "-" {
        if parts[3..6].iter().any(|part| *part != "-") {
            return Err(invalid_response(
                "output cursor has an inconsistent empty checkpoint",
            ));
        }
        None
    } else {
        Some(BlockRef {
            height: BlockHeight(parse_u64(parts[2], "cursor checkpoint height")?),
            hash: BlockHash(parse_hex(parts[3], "cursor checkpoint hash")?),
            parent_hash: if parts[4] == "-" {
                None
            } else {
                Some(BlockHash(parse_hex(parts[4], "cursor parent hash")?))
            },
            timestamp: if parts[5] == "-" {
                None
            } else {
                Some(parse_u64(parts[5], "cursor checkpoint timestamp")?)
            },
        })
    };
    let position = parse_hex(parts[6], "cursor position")?;
    if position.is_empty() {
        return Err(invalid_response("output cursor position is empty"));
    }
    Ok(OutputCursor {
        snapshot: OutputSnapshot {
            generation: RebuildGeneration(parse_u64(parts[0], "cursor generation")?),
            revision: parse_u64(parts[1], "cursor revision")?,
            checkpoint,
        },
        position,
    })
}
