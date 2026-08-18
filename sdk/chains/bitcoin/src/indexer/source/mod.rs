use std::collections::{BTreeMap, BTreeSet};

use futures_util::{StreamExt, stream};
use indexing::{
    BlockHash, BlockHeight, BlockRef, BlockSource, BoxFuture, ChainId, IndexScope, SourceError,
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
/// retained replay data contains only bounded value/address facts.
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
            .map(|raw| raw.map(|raw| raw.0))
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
                // unexpected value ensures only the compact, source-owned
                // shape can enter retained raw block data.
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
        Block::parse(raw_block, expected_height, Some(hash), self.config.network)
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

    fn block_at<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Self::Block, SourceError>> {
        Box::pin(async move {
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
            Ok(block)
        })
    }

    fn canonical_hash<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
        Box::pin(async move {
            if height > self.block_count().await? {
                return Ok(None);
            }
            self.optional_hash_at(height).await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness, absolute,
        consensus, hashes::Hash, hex::DisplayHex, transaction::Version,
    };
    use futures_executor::block_on;
    use json_rpc::{Error, Failure, RawJson, Request, Response};
    use serde_json::{Value, json};

    use super::*;

    #[derive(Clone)]
    struct ScriptedClient {
        replies: Arc<Mutex<VecDeque<ExpectedReply>>>,
    }

    struct ExpectedReply {
        method: &'static str,
        params: Option<Value>,
        result: Result<Value, i64>,
    }

    impl ScriptedClient {
        fn new(replies: Vec<ExpectedReply>) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies.into())),
            }
        }

        fn assert_exhausted(&self) {
            assert!(
                self.replies
                    .lock()
                    .expect("script lock must be healthy")
                    .is_empty(),
                "source made fewer requests than scripted"
            );
        }
    }

    impl Client for ScriptedClient {
        fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, Error>> {
            let expected = self
                .replies
                .lock()
                .expect("script lock must be healthy")
                .pop_front()
                .expect("source made more requests than scripted");
            assert_eq!(request.method, expected.method);
            if let Some(expected_params) = expected.params {
                let actual_params = request
                    .params
                    .deserialize::<Value>()
                    .expect("source request parameters must decode");
                assert_eq!(actual_params, expected_params);
            }
            let result = expected
                .result
                .map(|value| {
                    RawJson::from_serializable(&value).expect("scripted source result must encode")
                })
                .map_err(|code| Failure {
                    code,
                    message: "scripted source failure".to_owned(),
                    data: None,
                });
            Box::pin(async move {
                Ok(Response {
                    id: request.id,
                    result,
                })
            })
        }

        fn batch<'a>(
            &'a self,
            _requests: Vec<Request>,
        ) -> BoxFuture<'a, Result<Vec<Response>, Error>> {
            let _unused = Failure {
                code: -1,
                message: String::new(),
                data: None,
            };
            Box::pin(async move { Ok(Vec::new()) })
        }
    }

    fn reply(method: &'static str, result: Value) -> ExpectedReply {
        ExpectedReply {
            method,
            params: None,
            result: Ok(result),
        }
    }

    fn reply_for(method: &'static str, params: Value, result: Value) -> ExpectedReply {
        ExpectedReply {
            method,
            params: Some(params),
            result: Ok(result),
        }
    }

    fn failure(method: &'static str, code: i64) -> ExpectedReply {
        ExpectedReply {
            method,
            params: None,
            result: Err(code),
        }
    }

    fn hash(number: u8) -> String {
        let mut bytes = [0_u8; 32];
        bytes[0] = number;
        bitcoin::BlockHash::from_byte_array(bytes).to_string()
    }

    fn block_result() -> Value {
        let transaction = Transaction {
            version: Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 1]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(5_000_000_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        json!({
            "hash": hash(2),
            "height": 10,
            "previousblockhash": hash(3),
            "time": 100,
            "nTx": 1,
            "tx": [{
                "txid": transaction.compute_txid().to_string(),
                "hex": consensus::serialize(&transaction).to_lower_hex_string(),
                "vin": [{"coinbase": "01"}],
                "vout": [{
                    "value": 50.00000000,
                    "n": 0,
                    "scriptPubKey": {"hex": ""}
                }]
            }]
        })
    }

    fn btc_number(satoshis: u64) -> Value {
        let whole = satoshis / 100_000_000;
        let remainder = satoshis % 100_000_000;
        let lexical = if remainder == 0 {
            whole.to_string()
        } else {
            format!("{whole}.{remainder:08}")
                .trim_end_matches('0')
                .to_owned()
        };
        serde_json::from_str(&lexical).expect("test Bitcoin amount must encode")
    }

    fn transaction_result(transaction: &Transaction, inputs: Vec<Value>) -> Value {
        let outputs = transaction
            .output
            .iter()
            .enumerate()
            .map(|(index, output)| {
                json!({
                    "value": btc_number(output.value.to_sat()),
                    "n": index,
                    "scriptPubKey": {
                        "hex": output.script_pubkey.as_bytes().to_lower_hex_string(),
                    },
                })
            })
            .collect::<Vec<_>>();
        json!({
            "txid": transaction.compute_txid().to_string(),
            "hex": consensus::serialize(transaction).to_lower_hex_string(),
            "vin": inputs,
            "vout": outputs,
        })
    }

    fn external_prevout_block() -> (Value, Transaction, Transaction, Transaction) {
        let previous = Transaction {
            version: Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 1]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![
                TxOut {
                    value: Amount::from_sat(123_456_789),
                    script_pubkey: ScriptBuf::from_bytes(
                        [vec![0x00, 0x14], vec![0x11; 20]].concat(),
                    ),
                },
                TxOut {
                    value: Amount::from_sat(50_000),
                    script_pubkey: ScriptBuf::from_bytes(
                        [vec![0x51, 0x20], vec![0x22; 32]].concat(),
                    ),
                },
                TxOut {
                    value: Amount::from_sat(25_000),
                    // This historical script is deliberately large. The
                    // source must derive the absence of a canonical address
                    // and discard the script before retaining the block.
                    script_pubkey: ScriptBuf::from_bytes(vec![0x61; 65_536]),
                },
            ],
        };
        let spending = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: OutPoint {
                        txid: previous.compute_txid(),
                        vout: 0,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                },
                TxIn {
                    previous_output: OutPoint {
                        txid: previous.compute_txid(),
                        vout: 1,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                },
                TxIn {
                    previous_output: OutPoint {
                        txid: previous.compute_txid(),
                        vout: 2,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                },
            ],
            output: vec![TxOut {
                value: Amount::from_sat(123_521_789),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let child = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: spending.compute_txid(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(123_511_789),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let block = json!({
            "hash": hash(2),
            "height": 10,
            "previousblockhash": hash(3),
            "time": 100,
            "nTx": 2,
            "tx": [
                transaction_result(
                    &spending,
                    vec![
                        json!({"txid": previous.compute_txid().to_string(), "vout": 0}),
                        json!({"txid": previous.compute_txid().to_string(), "vout": 1}),
                        json!({"txid": previous.compute_txid().to_string(), "vout": 2}),
                    ],
                ),
                transaction_result(
                    &child,
                    vec![json!({"txid": spending.compute_txid().to_string(), "vout": 0})],
                ),
            ],
        });
        (block, previous, spending, child)
    }

    fn config() -> Config {
        Config {
            scope: IndexScope {
                chain: ChainId(crate::CHAIN.to_owned()),
                network: "regtest".to_owned(),
            },
            network: Network::Regtest,
            expected_genesis_hash: parse_bitcoin_block_hash(&hash(1))
                .expect("test genesis hash must parse"),
        }
    }

    fn connect_replies() -> Vec<ExpectedReply> {
        vec![
            reply("getnetworkinfo", json!({"version": 310000})),
            reply(
                "getblockchaininfo",
                json!({
                    "chain": "regtest",
                    "blocks": 10,
                    "headers": 10,
                    "bestblockhash": hash(2),
                    "initialblockdownload": false,
                    "pruned": false
                }),
            ),
            reply(
                "getindexinfo",
                json!({"txindex": {"synced": true, "best_block_height": 10}}),
            ),
            reply("getblockhash", Value::String(hash(1))),
        ]
    }

    #[test]
    fn numbered_block_fetch_retains_enriched_result_and_rechecks_canonical_hash() {
        let mut replies = connect_replies();
        replies.extend([
            reply("getblockhash", Value::String(hash(2))),
            reply_for("getblock", json!([hash(2), 2]), block_result()),
            reply("getblockhash", Value::String(hash(2))),
        ]);
        let source = block_on(Blocks::connect(ScriptedClient::new(replies), config()))
            .expect("valid scripted source must connect");

        let block = block_on(source.block_at(BlockHeight(10)))
            .expect("canonical verbosity-2 block must load");

        assert_eq!(block.reference.height, BlockHeight(10));
        assert_eq!(
            parse_bitcoin_block_hash(&hash(2)).expect("test hash must parse"),
            block.reference.hash
        );
        assert_eq!(
            serde_json::from_slice::<Value>(block.raw())
                .expect("retained block must be exact JSON"),
            block_result()
        );
    }

    #[test]
    fn external_prevouts_are_resolved_once_from_narrow_bounded_calls() {
        let (block_result, previous, spending, child) = external_prevout_block();
        let previous_id = previous.compute_txid().to_string();
        let mut replies = connect_replies();
        replies.extend([
            reply("getblockhash", Value::String(hash(2))),
            reply_for("getblock", json!([hash(2), 2]), block_result),
            reply_for(
                "getrawtransaction",
                json!([previous_id, true]),
                json!({
                    "txid": previous.compute_txid().to_string(),
                    "hex": consensus::serialize(&previous).to_lower_hex_string(),
                    "blockhash": hash(5),
                }),
            ),
            reply("getblockhash", Value::String(hash(2))),
        ]);
        let client = ScriptedClient::new(replies);
        let calls = client.clone();
        let source = block_on(Blocks::connect(client, config()))
            .expect("valid scripted source must connect");

        let block = block_on(source.block_at(BlockHeight(10)))
            .expect("external previous outputs must be enriched");
        calls.assert_exhausted();

        let raw: Value =
            serde_json::from_slice(block.raw()).expect("enriched block JSON must decode");
        let inputs = raw["tx"][0]["vin"]
            .as_array()
            .expect("spending inputs must be retained");
        assert_eq!(
            inputs[0]["prevout"],
            json!({
                "value_satoshis": 123_456_789_u64,
                "address": address_for_script(
                    &previous.output[0].script_pubkey,
                    Network::Regtest,
                )
                .expect("test P2WPKH script must have an address")
                .encoded(),
            })
        );
        assert_eq!(inputs[1]["prevout"]["value_satoshis"], json!(50_000_u64));
        assert_eq!(
            inputs[2]["prevout"],
            json!({"value_satoshis": 25_000_u64, "address": null})
        );
        for input in inputs {
            let prevout = input
                .get("prevout")
                .expect("external input must retain compact data");
            assert!(prevout.get("scriptPubKey").is_none());
            assert!(prevout.get("height").is_none());
            assert!(prevout.get("generated").is_none());
            assert!(
                serde_json::to_vec(prevout)
                    .expect("compact data must encode")
                    .len()
                    <= MAX_COMPACT_PREVOUT_JSON_BYTES
            );
        }
        assert!(
            block.raw().len() < previous.output[2].script_pubkey.len(),
            "historical script bytes must not survive retained enrichment"
        );
        assert_eq!(
            raw["tx"][0]["txid"],
            Value::String(spending.compute_txid().to_string())
        );
        assert!(
            raw["tx"][1]["vin"][0].get("prevout").is_none(),
            "same-block previous outputs must remain locally resolved"
        );
        assert_eq!(
            raw["tx"][1]["txid"],
            Value::String(child.compute_txid().to_string())
        );
    }

    #[test]
    fn external_prevout_lookup_must_return_confirmed_transaction_data() {
        let (block_result, previous, _, _) = external_prevout_block();
        let mut replies = connect_replies();
        replies.extend([
            reply("getblockhash", Value::String(hash(2))),
            reply_for("getblock", json!([hash(2), 2]), block_result),
            reply_for(
                "getrawtransaction",
                json!([previous.compute_txid().to_string(), true]),
                json!({
                    "txid": previous.compute_txid().to_string(),
                    "hex": consensus::serialize(&previous).to_lower_hex_string(),
                }),
            ),
        ]);
        let client = ScriptedClient::new(replies);
        let calls = client.clone();
        let source = block_on(Blocks::connect(client, config()))
            .expect("valid scripted source must connect");

        let error = block_on(source.block_at(BlockHeight(10)))
            .expect_err("mempool-only previous-output data must retry");
        calls.assert_exhausted();
        assert!(error.retryable);
        assert!(error.message.contains("block hash"));
    }

    #[test]
    fn external_prevout_bound_is_above_consensus_maximum_and_fails_before_growth() {
        const CONSENSUS_MAX_BLOCK_WEIGHT: usize = 4_000_000;
        const MINIMUM_INPUT_WEIGHT: usize = 41 * 4;

        let consensus_upper_bound = CONSENSUS_MAX_BLOCK_WEIGHT / MINIMUM_INPUT_WEIGHT;
        assert_eq!(consensus_upper_bound, 24_390);
        assert!(MAX_EXTERNAL_PREVOUTS_PER_BLOCK > consensus_upper_bound);
        assert_eq!(MAX_COMPACT_PREVOUT_TOTAL_BYTES, 4_800_000);

        let transaction_id = TransactionId([0x33; 32]);
        let mut outputs = BTreeMap::new();
        let mut count = 0;
        for output_index in 0..MAX_EXTERNAL_PREVOUTS_PER_BLOCK {
            record_external_prevout(
                &mut outputs,
                &mut count,
                transaction_id,
                u32::try_from(output_index).expect("test output index must fit u32"),
            )
            .expect("consensus-complete safety window must remain accepted");
        }
        let error = record_external_prevout(
            &mut outputs,
            &mut count,
            transaction_id,
            u32::try_from(MAX_EXTERNAL_PREVOUTS_PER_BLOCK).expect("test output index must fit u32"),
        )
        .expect_err("the first out-of-bound prevout must fail before insertion");
        assert!(!error.retryable);
        assert_eq!(count, MAX_EXTERNAL_PREVOUTS_PER_BLOCK);
        assert_eq!(
            outputs
                .get(&transaction_id)
                .expect("bounded transaction entry must exist")
                .len(),
            MAX_EXTERNAL_PREVOUTS_PER_BLOCK
        );
    }

    #[test]
    fn numbered_block_fetch_rejects_same_height_reorg_race() {
        let mut replies = connect_replies();
        replies.extend([
            reply("getblockhash", Value::String(hash(2))),
            reply_for("getblock", json!([hash(2), 2]), block_result()),
            reply("getblockhash", Value::String(hash(4))),
        ]);
        let source = block_on(Blocks::connect(ScriptedClient::new(replies), config()))
            .expect("valid scripted source must connect");

        let error = block_on(source.block_at(BlockHeight(10)))
            .expect_err("same-height canonical replacement must retry");

        assert!(error.retryable);
        assert!(error.message.contains("changed"));
    }

    #[test]
    fn disappearing_height_is_optional_for_canonical_hash_and_retryable_for_tip() {
        let mut canonical_replies = connect_replies();
        canonical_replies.extend([
            reply("getblockcount", json!(10)),
            failure("getblockhash", -8),
        ]);
        let canonical = block_on(Blocks::connect(
            ScriptedClient::new(canonical_replies),
            config(),
        ))
        .expect("valid scripted source must connect");
        assert_eq!(
            block_on(canonical.canonical_hash(BlockHeight(10)))
                .expect("a vanished reorg height is not a fatal source error"),
            None
        );

        let mut tip_replies = connect_replies();
        tip_replies.extend([
            reply("getblockcount", json!(10)),
            failure("getblockhash", -8),
        ]);
        let tip = block_on(Blocks::connect(ScriptedClient::new(tip_replies), config()))
            .expect("valid scripted source must connect");
        let error = block_on(tip.tip()).expect_err("tip height race must retry");
        assert!(error.retryable);
    }
}
