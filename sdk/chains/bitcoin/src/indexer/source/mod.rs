use std::collections::{BTreeMap, BTreeSet};

use futures_util::{StreamExt, stream};
use indexing::{
    BlockHash, BlockHeight, BlockPosition, BlockRef, BlockSource, BoxFuture, ChainId, IndexScope,
    SourceError,
};
use json_rpc::Client;
use serde_json::Value;

use crate::{Network, TransactionId};

use super::{Block, Outpoint, model::address_for_script};
use crate::rpc::{
    Client as RpcClient, CoreConfig, format_bitcoin_block_hash, parse_bitcoin_block_hash,
    parse_header, source_error,
};

// Every non-coinbase input consumes at least a 36-byte outpoint, one-byte
// empty-script length, and four-byte sequence in non-witness serialization.
// At four weight units per byte, a 4,000,000-WU block can therefore contain at
// most floor(4,000,000 / 164) = 24,390 external prevouts even before mandatory
// block and transaction overhead. This deliberately loose ceiling remains
// complete for every consensus-valid block.
const MAX_EXTERNAL_PREVOUTS_PER_BLOCK: usize = 25_000;
const MAX_IN_FLIGHT_PREVOUT_REQUESTS: usize = 4;
const MAX_COMPACT_ADDRESS_BYTES: usize = 128;
const MAX_COMPACT_PREVOUT_JSON_BYTES: usize = 192;
const MAX_COMPACT_PREVOUT_TOTAL_BYTES: usize =
    MAX_EXTERNAL_PREVOUTS_PER_BLOCK * MAX_COMPACT_PREVOUT_JSON_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub scope: IndexScope,
    pub network: Network,
    pub expected_genesis_hash: BlockHash,
}

impl Config {
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.scope.chain != ChainId(crate::CHAIN.to_owned()) {
            return Err(source_error(
                "Bitcoin index source scope must use the bitcoin chain ID",
                false,
            ));
        }
        if self.scope.network != self.network.canonical_name() {
            return Err(source_error(
                "Bitcoin index source scope network does not match configuration",
                false,
            ));
        }
        CoreConfig {
            expected_network: self.network,
            expected_genesis_hash: self.expected_genesis_hash.clone(),
        }
        .validate()
    }
}

/// Canonical numbered-block source using bounded Bitcoin Core 31 RPC results.
///
/// The source fetches the block at verbosity 2, then resolves only external
/// previous transactions through at most four concurrent, transport-bounded calls.
/// Full historical scripts are validated and discarded at the source boundary;
/// parsed blocks contain only the value/address facts interpretation needs.
pub struct Blocks<C> {
    client: RpcClient<C>,
    config: Config,
}

impl<C> Blocks<C>
where
    C: Client,
{
    pub async fn connect(client: C, config: Config) -> Result<Self, SourceError> {
        config.validate()?;
        let client = RpcClient::connect(
            client,
            CoreConfig {
                expected_network: config.network,
                expected_genesis_hash: config.expected_genesis_hash.clone(),
            },
        )
        .await?;
        Ok(Self { client, config })
    }

    /// Builds block reads over an already connected shared RPC client.
    pub fn from_client(client: RpcClient<C>, config: Config) -> Result<Self, SourceError> {
        config.validate()?;
        let expected = client.config();
        if expected.expected_network != config.network
            || expected.expected_genesis_hash != config.expected_genesis_hash
        {
            return Err(source_error(
                "Bitcoin block adapter configuration does not match its RPC client",
                false,
            ));
        }
        Ok(Self { client, config })
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub fn client(&self) -> &RpcClient<C> {
        &self.client
    }

    async fn block_count(&self) -> Result<BlockHeight, SourceError> {
        self.client
            .request_result("getblockcount", serde_json::json!([]))
            .await?
            .deserialize::<u64>()
            .map(BlockHeight)
            .map_err(|error| source_error(error.to_string(), true))
    }

    async fn optional_hash_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockHash>, SourceError> {
        let raw = self
            .client
            .request_optional_result("getblockhash", serde_json::json!([height.0]), &[-8])
            .await?;
        raw.map(|raw| {
            let hash: String = raw
                .deserialize()
                .map_err(|error| source_error(error.to_string(), true))?;
            parse_bitcoin_block_hash(&hash)
        })
        .transpose()
    }

    async fn hash_at(&self, height: BlockHeight) -> Result<BlockHash, SourceError> {
        self.optional_hash_at(height).await?.ok_or_else(|| {
            source_error(
                "Bitcoin canonical height changed while its hash was being read",
                true,
            )
        })
    }

    async fn header(&self, hash: &BlockHash, height: BlockHeight) -> Result<BlockRef, SourceError> {
        let raw = self
            .client
            .request_result(
                "getblockheader",
                serde_json::json!([format_bitcoin_block_hash(hash)?, true]),
            )
            .await?;
        let header = parse_header(&raw, Some(height))?;
        if header.hash != *hash {
            return Err(source_error(
                "Bitcoin header lookup returned a different block hash",
                true,
            ));
        }
        Ok(header)
    }

    async fn raw_block(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>, SourceError> {
        self.client
            .request_optional_result(
                "getblock",
                serde_json::json!([format_bitcoin_block_hash(hash)?, 2]),
                &[-5],
            )
            .await
            .map(|raw| raw.map(|raw| raw.into_bytes()))
    }

    async fn resolve_outputs(
        &self,
        transaction_id: TransactionId,
        required_outputs: &BTreeSet<u32>,
    ) -> Result<BTreeMap<u32, ResolvedOutput>, SourceError> {
        let raw = self
            .client
            .request_optional_result(
                "getrawtransaction",
                serde_json::json!([transaction_id.to_string(), true]),
                &[-5],
            )
            .await?
            .ok_or_else(|| {
                source_error(
                    format!(
                        "Bitcoin previous transaction {transaction_id} disappeared during block ingestion"
                    ),
                    true,
                )
            })?;
        let value: Value = raw
            .deserialize()
            .map_err(|error| source_error(error.to_string(), true))?;
        let object = value.as_object().ok_or_else(|| {
            source_error("Bitcoin getrawtransaction result must be an object", true)
        })?;
        let returned_id = required_string(object, "txid", "Bitcoin previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| source_error("Bitcoin previous transaction ID is invalid", true))?;
        if returned_id != transaction_id {
            return Err(source_error(
                "Bitcoin previous-transaction lookup returned a different transaction ID",
                true,
            ));
        }
        let transaction = decode_consensus_transaction(object, transaction_id)?;
        let confirmed_block_hash = required_string(
            object,
            "blockhash",
            "Bitcoin previous transaction block hash",
        )?;
        parse_bitcoin_block_hash(confirmed_block_hash)?;

        let mut outputs = BTreeMap::new();
        for output_index in required_outputs {
            let index = usize::try_from(*output_index).map_err(|_| {
                source_error(
                    "Bitcoin previous transaction output index exceeds usize",
                    true,
                )
            })?;
            let output = transaction.output.get(index).ok_or_else(|| {
                source_error(
                    format!(
                        "Bitcoin previous transaction {transaction_id} has no output {output_index}"
                    ),
                    true,
                )
            })?;
            let address = address_for_script(&output.script_pubkey, self.config.network);
            validate_compact_address(address.as_ref())?;
            outputs.insert(
                *output_index,
                ResolvedOutput {
                    value_satoshis: output.value.to_sat(),
                    address,
                },
            );
        }

        Ok(outputs)
    }

    async fn enrich_prevouts(&self, raw_block: Vec<u8>) -> Result<Vec<u8>, SourceError> {
        let mut value: Value = serde_json::from_slice(&raw_block)
            .map_err(|_| source_error("Bitcoin block result is not valid JSON", true))?;
        let transaction_values = value
            .as_object()
            .and_then(|object| object.get("tx"))
            .and_then(Value::as_array)
            .ok_or_else(|| source_error("Bitcoin block transactions must be an array", true))?;

        let mut transactions = Vec::with_capacity(transaction_values.len());
        let mut transaction_ids = BTreeSet::new();
        for transaction_value in transaction_values {
            let object = transaction_value
                .as_object()
                .ok_or_else(|| source_error("Bitcoin block transaction must be an object", true))?;
            let transaction_id = required_string(object, "txid", "Bitcoin block transaction ID")?
                .parse::<TransactionId>()
                .map_err(|_| source_error("Bitcoin block transaction ID is invalid", true))?;
            let transaction = decode_consensus_transaction(object, transaction_id)?;
            validate_input_claims(transaction_value, &transaction)?;
            if !transaction_ids.insert(transaction_id) {
                return Err(source_error(
                    "Bitcoin block contains duplicate transaction IDs",
                    true,
                ));
            }
            transactions.push((transaction_id, transaction));
        }

        let mut earlier_outputs = BTreeSet::new();
        let mut external_outputs: BTreeMap<TransactionId, BTreeSet<u32>> = BTreeMap::new();
        let mut external_prevout_count = 0_usize;
        for (transaction_id, transaction) in &transactions {
            for input in &transaction.input {
                if input.previous_output.is_null() {
                    continue;
                }
                let previous_id = TransactionId::from(input.previous_output.txid);
                let outpoint = Outpoint {
                    transaction_id: previous_id,
                    output_index: input.previous_output.vout,
                };
                if earlier_outputs.contains(&outpoint) {
                    continue;
                }
                if transaction_ids.contains(&previous_id) {
                    return Err(source_error(
                        "Bitcoin block transaction spends an output that was not created earlier in the block",
                        true,
                    ));
                }
                record_external_prevout(
                    &mut external_outputs,
                    &mut external_prevout_count,
                    previous_id,
                    input.previous_output.vout,
                )?;
            }
            for output_index in 0..transaction.output.len() {
                let output_index = u32::try_from(output_index)
                    .map_err(|_| source_error("Bitcoin block output index exceeds u32", true))?;
                earlier_outputs.insert(Outpoint {
                    transaction_id: *transaction_id,
                    output_index,
                });
            }
        }
        let compact_data_budget = external_prevout_count
            .checked_mul(MAX_COMPACT_PREVOUT_JSON_BYTES)
            .ok_or_else(|| source_error("Bitcoin compact prevout data budget overflowed", false))?;
        if compact_data_budget > MAX_COMPACT_PREVOUT_TOTAL_BYTES {
            return Err(source_error(
                "Bitcoin compact prevout data exceeds its aggregate bound",
                false,
            ));
        }

        let mut resolved = BTreeMap::new();
        let requests =
            external_outputs
                .into_iter()
                .map(|(transaction_id, output_indexes)| async move {
                    self.resolve_outputs(transaction_id, &output_indexes)
                        .await
                        .map(|data| (transaction_id, data))
                });
        let mut requests = stream::iter(requests).buffer_unordered(MAX_IN_FLIGHT_PREVOUT_REQUESTS);
        while let Some(result) = requests.next().await {
            let (transaction_id, data) = result?;
            resolved.insert(transaction_id, data);
        }

        let transaction_values = value
            .as_object_mut()
            .and_then(|object| object.get_mut("tx"))
            .and_then(Value::as_array_mut)
            .ok_or_else(|| source_error("Bitcoin block transactions must be an array", true))?;
        let mut earlier_outputs = BTreeSet::new();
        for ((transaction_id, transaction), transaction_value) in
            transactions.iter().zip(transaction_values)
        {
            let inputs = transaction_value
                .as_object_mut()
                .and_then(|object| object.get_mut("vin"))
                .and_then(Value::as_array_mut)
                .ok_or_else(|| source_error("Bitcoin transaction inputs must be an array", true))?;
            for (input, native_input) in inputs.iter_mut().zip(&transaction.input) {
                if native_input.previous_output.is_null() {
                    continue;
                }
                let input = input.as_object_mut().ok_or_else(|| {
                    source_error("Bitcoin transaction input must be an object", true)
                })?;
                // Verbosity 2 does not include this field. Removing any
                // unexpected value ensures parsing sees only the compact,
                // source-owned previous-output shape.
                input.remove("prevout");
                let previous_id = TransactionId::from(native_input.previous_output.txid);
                let outpoint = Outpoint {
                    transaction_id: previous_id,
                    output_index: native_input.previous_output.vout,
                };
                if earlier_outputs.contains(&outpoint) {
                    continue;
                }
                let previous_transaction = resolved.get(&previous_id).ok_or_else(|| {
                    source_error(
                        "Bitcoin external previous transaction was not resolved",
                        true,
                    )
                })?;
                let output = previous_transaction
                    .get(&native_input.previous_output.vout)
                    .ok_or_else(|| {
                        source_error("Bitcoin external previous output was not resolved", true)
                    })?;
                let prevout = output.compact_json()?;
                input.insert("prevout".to_owned(), prevout);
            }
            for output_index in 0..transaction.output.len() {
                let output_index = u32::try_from(output_index)
                    .map_err(|_| source_error("Bitcoin block output index exceeds u32", true))?;
                earlier_outputs.insert(Outpoint {
                    transaction_id: *transaction_id,
                    output_index,
                });
            }
        }

        serde_json::to_vec(&value)
            .map_err(|_| source_error("Bitcoin enriched block JSON could not be encoded", true))
    }

    async fn fetch_block(
        &self,
        hash: &BlockHash,
        expected_height: Option<BlockHeight>,
    ) -> Result<Option<Block>, SourceError> {
        let Some(raw_block) = self.raw_block(hash).await? else {
            return Ok(None);
        };
        let raw_block = self.enrich_prevouts(raw_block).await?;
        Block::parse(&raw_block, expected_height, Some(hash), self.config.network)
            .map(Some)
            .map_err(|error| source_error(error.to_string(), true))
    }
}

mod prevout;

use prevout::*;
impl<C> BlockSource for Blocks<C>
where
    C: Client,
{
    type Block = Block;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move {
            let height = self.block_count().await?;
            let hash = self.hash_at(height).await?;
            self.header(&hash, height).await
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
                    "Bitcoin block range requires ordered positions and a positive limit",
                    false,
                ));
            }
            let mut position = start.0;
            let end = end.0;
            let mut blocks = Vec::with_capacity(limit.min(64));
            while position <= end && blocks.len() < limit {
                let height = BlockHeight(position);
                let first_hash = self.hash_at(height).await?;
                let block = self
                    .fetch_block(&first_hash, Some(height))
                    .await?
                    .ok_or_else(|| {
                        source_error("Bitcoin Core no longer exposes the requested block", true)
                    })?;
                let second_hash = self.hash_at(height).await?;
                if first_hash != second_hash {
                    return Err(source_error(
                        "Bitcoin canonical block changed while it was being fetched",
                        true,
                    ));
                }
                blocks.push(block);
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
            if height > self.block_count().await? {
                return Ok(None);
            }
            let Some(hash) = self.optional_hash_at(height).await? else {
                return Ok(None);
            };
            self.header(&hash, height).await.map(Some)
        })
    }
}

#[cfg(test)]
#[path = "../source_test.rs"]
mod test;
