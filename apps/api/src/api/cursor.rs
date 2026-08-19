use serde::{Deserialize, Serialize};

use super::error::ApiError;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HistoryCursor {
    chain: String,
    network: String,
    transaction: String,
    height: u64,
    checkpoint: Option<CursorBlock>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorBlock {
    height: u64,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<u64>,
}

impl HistoryCursor {
    pub fn encode(cursor: &indexing::HistoryCursor) -> Result<String, ApiError> {
        use base64::Engine;

        let bytes = serde_json::to_vec(&Self {
            chain: cursor.position.transaction.scope.chain.0.clone(),
            network: cursor.position.transaction.scope.network.clone(),
            transaction: cursor.position.transaction.value.clone(),
            height: cursor.position.height.0,
            checkpoint: cursor.checkpoint.as_ref().map(|block| CursorBlock {
                height: block.height.0,
                hash: hex::encode(&block.hash.0),
                parent_hash: block.parent_hash.as_ref().map(|hash| hex::encode(&hash.0)),
                timestamp: block.timestamp,
            }),
        })
        .map_err(|_| Self::invalid_response("history cursor could not be encoded"))?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(value: &str) -> Result<indexing::HistoryCursor, ApiError> {
        use base64::Engine;

        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| Self::invalid_request("history cursor is invalid"))?;
        let cursor: Self = serde_json::from_slice(&bytes)
            .map_err(|_| Self::invalid_request("history cursor is invalid"))?;
        if cursor.chain.is_empty() || cursor.network.is_empty() || cursor.transaction.is_empty() {
            return Err(Self::invalid_request("history cursor is invalid"));
        }
        let scope = indexing::IndexScope {
            chain: indexing::ChainId(cursor.chain),
            network: cursor.network,
        };
        let checkpoint = cursor
            .checkpoint
            .map(|block| {
                Ok::<_, ApiError>(base::BlockRef {
                    height: base::BlockHeight(block.height),
                    hash: base::BlockHash(block.decoded_hash()?),
                    parent_hash: block
                        .parent_hash
                        .map(|hash| CursorBlock::decode_hash(&hash).map(base::BlockHash))
                        .transpose()?,
                    timestamp: block.timestamp,
                })
            })
            .transpose()?;
        Ok(indexing::HistoryCursor {
            checkpoint,
            position: indexing::HistoryPosition {
                height: base::BlockHeight(cursor.height),
                transaction: indexing::TransactionRef {
                    scope,
                    value: cursor.transaction,
                },
            },
        })
    }

    fn invalid_request(message: impl Into<String>) -> ApiError {
        ApiError::invalid_request(message)
    }

    fn invalid_response(message: impl Into<String>) -> ApiError {
        ApiError::invalid_response(message)
    }
}

impl CursorBlock {
    fn decoded_hash(&self) -> Result<Vec<u8>, ApiError> {
        Self::decode_hash(&self.hash)
    }

    fn decode_hash(value: &str) -> Result<Vec<u8>, ApiError> {
        let bytes = hex::decode(value)
            .map_err(|_| HistoryCursor::invalid_request("history cursor is invalid"))?;
        if bytes.is_empty() {
            return Err(HistoryCursor::invalid_request("history cursor is invalid"));
        }
        Ok(bytes)
    }
}
