use std::sync::atomic::{AtomicU8, Ordering};

use indexing::{
    BlockHash, BlockHeight, BlockPosition, BlockRef, BlockSource, BoxFuture, IndexScope,
    SourceError,
};
use json_rpc::{Client as JsonClient, Error, Failure, RawJson};
use serde_json::{Value, value::RawValue};

use crate::rpc::client::{CallError, Client};

use super::{
    Block,
    model::{ParsedBlock, ParsedReceipt, encode_hex, parse_quantity},
};

const RECEIPTS_UNKNOWN: u8 = 0;
const RECEIPTS_BY_BLOCK: u8 = 1;
const RECEIPTS_BY_TRANSACTION: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    pub scope: IndexScope,
    pub expected_chain_id: u64,
    pub expected_genesis_hash: BlockHash,
}

impl SourceConfig {
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.scope.chain.0 != "ethereum" {
            return Err(source_error(
                "Ethereum index source scope must use the ethereum chain ID",
                false,
            ));
        }
        if self.scope.network.trim().is_empty() {
            return Err(source_error(
                "Ethereum index source network slug must not be empty",
                false,
            ));
        }
        if self.expected_genesis_hash.0.len() != 32 {
            return Err(source_error(
                "configured Ethereum genesis hash must be 32 bytes",
                false,
            ));
        }
        Ok(())
    }
}

/// Authoritative numbered-block source over generic JSON-RPC execution.
///
/// Construction verifies `eth_chainId` and block zero before the value can be
/// used as a `BlockSource`. Receipt capability is discovered once and an
/// official method-not-found response permanently selects the batched fallback.
pub struct BlockClient<C> {
    client: Client<C>,
    config: SourceConfig,
    receipt_mode: AtomicU8,
}

impl<C> BlockClient<C>
where
    C: JsonClient,
{
    pub async fn connect(client: C, config: SourceConfig) -> Result<Self, SourceError> {
        Self::from_rpc(Client::new(client), config).await
    }

    pub async fn from_rpc(client: Client<C>, config: SourceConfig) -> Result<Self, SourceError> {
        config.validate()?;
        let source = Self {
            client,
            config,
            receipt_mode: AtomicU8::new(RECEIPTS_UNKNOWN),
        };
        source.verify_chains().await?;
        Ok(source)
    }

    #[must_use]
    pub fn config(&self) -> &SourceConfig {
        &self.config
    }

    async fn verify_chains(&self) -> Result<(), SourceError> {
        let chain_id = self
            .request_result("eth_chainId", serde_json::json!([]))
            .await?;
        let chain_id: String = chain_id.deserialize().map_err(map_json_rpc_error)?;
        let chain_id = parse_quantity(&chain_id, "chain ID")
            .map_err(|error| source_error(error.to_string(), false))?;
        let chain_id = u64::try_from(chain_id)
            .map_err(|_| source_error("Ethereum chain ID exceeds u64", false))?;
        if chain_id != self.config.expected_chain_id {
            return Err(source_error(
                "Ethereum RPC chain ID does not match configuration",
                false,
            ));
        }

        let raw_genesis = self
            .request_result("eth_getBlockByNumber", serde_json::json!(["0x0", false]))
            .await?;
        if is_json_null(&raw_genesis)? {
            return Err(source_error(
                "Ethereum RPC does not expose the genesis block",
                false,
            ));
        }
        let genesis = ParsedBlock::parse(raw_genesis.as_bytes(), Some(BlockHeight(0)), false)
            .map_err(|error| source_error(error.to_string(), false))?;
        if genesis.reference.hash != self.config.expected_genesis_hash {
            return Err(source_error(
                "Ethereum RPC genesis hash does not match configuration",
                false,
            ));
        }
        Ok(())
    }

    async fn fetch_block(
        &self,
        tag: String,
        expected_height: Option<BlockHeight>,
        full_transactions: bool,
    ) -> Result<(RawJson, super::model::ParsedBlock), SourceError> {
        let raw = self
            .request_result(
                "eth_getBlockByNumber",
                serde_json::json!([tag, full_transactions]),
            )
            .await?;
        if is_json_null(&raw)? {
            return Err(source_error(
                "Ethereum RPC does not currently expose the requested block",
                true,
            ));
        }
        let parsed = ParsedBlock::parse(raw.as_bytes(), expected_height, full_transactions)
            .map_err(|error| source_error(error.to_string(), true))?;
        Ok((raw, parsed))
    }

    async fn fetch_receipts(
        &self,
        block: &super::model::ParsedBlock,
    ) -> Result<Vec<Vec<u8>>, SourceError> {
        if block.transactions.is_empty() {
            return Ok(Vec::new());
        }

        let mode = self.receipt_mode.load(Ordering::Acquire);
        if mode != RECEIPTS_BY_TRANSACTION {
            match self.fetch_block_receipts(block).await {
                Ok(receipts) => {
                    self.receipt_mode
                        .store(RECEIPTS_BY_BLOCK, Ordering::Release);
                    return Ok(receipts);
                }
                Err(CallFailure {
                    remote_code: Some(-32_601),
                    ..
                }) => {
                    self.receipt_mode
                        .store(RECEIPTS_BY_TRANSACTION, Ordering::Release);
                }
                Err(error) => return Err(error.error),
            }
        }

        self.fetch_transaction_receipts(block).await
    }

    async fn fetch_block_receipts(
        &self,
        block: &super::model::ParsedBlock,
    ) -> Result<Vec<Vec<u8>>, CallFailure> {
        let hash = encode_hex(&block.reference.hash.0);
        let raw = self
            .request_result_detailed("eth_getBlockReceipts", serde_json::json!([hash]))
            .await?;
        let values: Vec<Box<RawValue>> = raw.deserialize().map_err(|error| CallFailure {
            remote_code: None,
            error: map_json_rpc_error(error),
        })?;
        Ok(values
            .into_iter()
            .map(|value| value.get().as_bytes().to_vec())
            .collect())
    }

    async fn fetch_transaction_receipts(
        &self,
        block: &super::model::ParsedBlock,
    ) -> Result<Vec<Vec<u8>>, SourceError> {
        let mut requests = Vec::with_capacity(block.transactions.len());
        for transaction in &block.transactions {
            let hash = encode_hex(&transaction.hash);
            requests.push(("eth_getTransactionReceipt", serde_json::json!([hash])));
        }
        self.client
            .batch(requests)
            .await?
            .into_iter()
            .map(|result| {
                let raw = match result {
                    Ok(raw) => raw,
                    Err(failure) => return Err(map_remote_failure(failure)),
                };
                if is_json_null(&raw)? {
                    return Err(source_error(
                        "Ethereum transaction receipt is temporarily unavailable",
                        true,
                    ));
                }
                Ok(raw.into_bytes())
            })
            .collect()
    }

    async fn request_result(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, SourceError> {
        self.request_result_detailed(method, params)
            .await
            .map_err(|failure| failure.error)
    }

    async fn request_result_detailed(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, CallFailure> {
        match self.client.call(method, params).await {
            Ok(result) => Ok(result),
            Err(CallError::Local(error)) => Err(CallFailure::local(error)),
            Err(CallError::Remote(failure)) => Err(CallFailure {
                remote_code: Some(failure.code),
                error: map_remote_failure(failure),
            }),
        }
    }
}

impl<C> BlockSource for BlockClient<C>
where
    C: JsonClient,
{
    type Block = Block;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move {
            let (_, block) = self.fetch_block("latest".to_owned(), None, false).await?;
            Ok(block.reference)
        })
    }

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        Box::pin(async move {
            if limit == 0 || start > end {
                return Err(source_error(
                    "Ethereum block range requires ordered positions and a positive limit",
                    false,
                ));
            }
            let mut position = start.0;
            let mut blocks = Vec::with_capacity(limit.min(64));
            while position <= end.0 && blocks.len() < limit {
                let height = BlockHeight(position);
                let tag = format!("0x{:x}", height.0);
                let (raw_block, parsed) = self.fetch_block(tag, Some(height), true).await?;
                let raw_receipts = self.fetch_receipts(&parsed).await?;
                ParsedReceipt::parse_all(&raw_receipts, &parsed)
                    .map_err(|error| source_error(error.to_string(), true))?;
                blocks.push(Block {
                    reference: parsed.reference,
                    raw_block: raw_block.into_bytes(),
                    raw_receipts,
                });
                let Some(next) = position.checked_add(1) else {
                    break;
                };
                position = next;
            }
            Ok(blocks)
        })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        Box::pin(async move {
            let height = BlockHeight(position.0);
            let tag = format!("0x{:x}", position.0);
            let raw = self
                .request_result("eth_getBlockByNumber", serde_json::json!([tag, false]))
                .await?;
            if is_json_null(&raw)? {
                return Ok(None);
            }
            let block = ParsedBlock::parse(raw.as_bytes(), Some(height), false)
                .map_err(|error| source_error(error.to_string(), true))?;
            Ok(Some(block.reference))
        })
    }
}

#[derive(Debug)]
struct CallFailure {
    remote_code: Option<i64>,
    error: SourceError,
}

impl CallFailure {
    fn local(error: SourceError) -> Self {
        Self {
            remote_code: None,
            error,
        }
    }
}

fn map_json_rpc_error(error: Error) -> SourceError {
    source_error(error.to_string(), error.is_retryable())
}

fn map_remote_failure(failure: Failure) -> SourceError {
    source_error(
        format!(
            "Ethereum JSON-RPC request failed with code {}",
            failure.code
        ),
        failure.is_server_error(),
    )
}

fn is_json_null(raw: &RawJson) -> Result<bool, SourceError> {
    raw.deserialize::<Value>()
        .map(|value| value.is_null())
        .map_err(map_json_rpc_error)
}

fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
#[path = "test.rs"]
mod tests;
