use indexing::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, SourceError};

use super::{
    Client,
    error::{map_json_rpc_error, source_error},
    transport::Client as Transport,
    wire::{
        format_bitcoin_block_hash, parse_bitcoin_block_hash, parse_object, required_string,
        required_u64,
    },
};

impl<C> Client<C>
where
    C: Transport,
{
    /// Returns the node's current canonical block hash at `height`.
    ///
    /// A transient height disappearance during a shorter, higher-work reorg is
    /// represented as `None`; callers decide whether to retry or fail closed.
    pub async fn canonical_hash(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockHash>, SourceError> {
        let raw = self
            .request_optional_result("getblockhash", serde_json::json!([height.0]), &[-8])
            .await?;
        raw.map(|raw| {
            let encoded: String = raw.deserialize().map_err(map_json_rpc_error)?;
            parse_bitcoin_block_hash(&encoded)
        })
        .transpose()
    }

    pub(crate) async fn header(
        &self,
        expected_hash: &BlockHash,
        expected_height: BlockHeight,
    ) -> Result<BlockRef, SourceError> {
        let raw = self
            .request_result(
                "getblockheader",
                serde_json::json!([format_bitcoin_block_hash(expected_hash)?, true]),
            )
            .await?;
        let result = parse_object(&raw, "Bitcoin getblockheader result")?;
        let height = BlockHeight(required_u64(
            &result,
            "height",
            "Bitcoin block-header height",
        )?);
        if expected_height != height {
            return Err(source_error(
                "Bitcoin block header does not match the requested height",
                true,
            ));
        }
        let hash = parse_bitcoin_block_hash(&required_string(
            &result,
            "hash",
            "Bitcoin block-header hash",
        )?)?;
        let parent = if height.0 == 0 {
            None
        } else {
            Some(BlockParent {
                position: BlockPosition(height.0 - 1),
                hash: parse_bitcoin_block_hash(&required_string(
                    &result,
                    "previousblockhash",
                    "Bitcoin previous block hash",
                )?)?,
            })
        };
        let timestamp = required_u64(&result, "time", "Bitcoin block-header timestamp")?;
        let header = BlockRef {
            position: BlockPosition(height.0),
            height,
            hash,
            parent,
            timestamp: Some(timestamp),
        };
        if header.hash != *expected_hash {
            return Err(source_error(
                "Bitcoin header lookup returned a different block hash",
                true,
            ));
        }
        Ok(header)
    }
}
