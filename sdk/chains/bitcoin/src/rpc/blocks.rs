use indexing::{BlockHash, BlockHeight, SourceError};

use super::{
    Client, error::map_json_rpc_error, transport::Client as Transport,
    wire::parse_bitcoin_block_hash,
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
}
