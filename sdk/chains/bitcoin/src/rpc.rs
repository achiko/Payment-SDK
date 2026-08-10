use std::{
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    BitcoinNetwork, BitcoinReceipt, BitcoinSignedTransaction, BitcoinTransactionId, BoxFuture,
    Satoshi, SatoshisPerKvb,
};
use bitcoin::{BlockHash as NativeBlockHash, hashes::Hash, hex::DisplayHex};
use indexing::{BlockHash, BlockHeight, BlockRef, SourceError};
use json_rpc::{JsonRpcClient, JsonRpcError, JsonRpcFailure, JsonRpcRequest, RawJson, RequestId};
use serde_json::{Map, Number, Value};

use crate::indexer::BitcoinBlock;

const CORE_31_VERSION_MINIMUM: u64 = 310_000;
const CORE_32_VERSION_MINIMUM: u64 = 320_000;
const SATOSHIS_PER_BITCOIN: u64 = 100_000_000;

/// Bitcoin Core's maximum accepted `maxfeerate` value, expressed in sat/kvB.
pub const BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB: u64 = SATOSHIS_PER_BITCOIN;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinRpcUtxo {
    pub transaction_id: [u8; 32],
    pub output_index: u32,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
    pub confirmations: u64,
    pub coinbase: bool,
}

/// One canonically fenced IX UTXO read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinUtxoSet {
    pub checkpoint: BlockRef,
    pub outputs: Vec<BitcoinRpcUtxo>,
}

/// Strict Bitcoin Core identity and readiness requirements shared by WS and IX.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinCoreConfig {
    pub expected_network: BitcoinNetwork,
    pub expected_genesis_hash: BlockHash,
}

impl BitcoinCoreConfig {
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.expected_genesis_hash.0.len() != 32 {
            return Err(source_error(
                "configured Bitcoin genesis hash must be 32 bytes",
                false,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinCoreNodeStatus {
    pub version: u64,
    pub network: BitcoinNetwork,
    pub height: BlockHeight,
    pub best_block_hash: BlockHash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinPreflight {
    pub allowed: bool,
    pub reject_reason: Option<String>,
    pub virtual_size: Option<u64>,
    pub base_fee: Option<Satoshi>,
}

/// Credential-free Bitcoin Core method adapter over shared JSON-RPC framing.
///
/// HTTP transport, authentication headers, timeouts, and response-size limits
/// remain owned by `packages/*`. This value validates the Core 31 node and owns
/// only chain-native method semantics and request correlation.
pub struct BitcoinCoreClient<C> {
    client: C,
    config: BitcoinCoreConfig,
    next_request_id: AtomicU64,
}

impl<C> BitcoinCoreClient<C>
where
    C: JsonRpcClient,
{
    pub async fn connect(client: C, config: BitcoinCoreConfig) -> Result<Self, SourceError> {
        config.validate()?;
        let core = Self {
            client,
            config,
            next_request_id: AtomicU64::new(1),
        };
        core.readiness().await?;
        Ok(core)
    }

    #[must_use]
    pub fn config(&self) -> &BitcoinCoreConfig {
        &self.config
    }

    /// Revalidates identity and deployment prerequisites against live node state.
    pub async fn readiness(&self) -> Result<BitcoinCoreNodeStatus, SourceError> {
        let network_info = self
            .request_result("getnetworkinfo", serde_json::json!([]))
            .await?;
        let network_info = parse_object(&network_info, "Bitcoin getnetworkinfo result")?;
        let version = required_u64(&network_info, "version", "Bitcoin Core version")?;
        if !(CORE_31_VERSION_MINIMUM..CORE_32_VERSION_MINIMUM).contains(&version) {
            return Err(source_error(
                "Bitcoin RPC must be a Bitcoin Core 31.x node",
                false,
            ));
        }

        let chain_info = self
            .request_result("getblockchaininfo", serde_json::json!([]))
            .await?;
        let chain_info = parse_object(&chain_info, "Bitcoin getblockchaininfo result")?;
        let chain = required_string(&chain_info, "chain", "Bitcoin Core chain")?;
        let network = BitcoinNetwork::from_core_chain_name(&chain).ok_or_else(|| {
            source_error("Bitcoin Core returned an unsupported chain name", false)
        })?;
        if network != self.config.expected_network {
            return Err(source_error(
                "Bitcoin RPC network does not match configuration",
                false,
            ));
        }
        if required_bool(&chain_info, "pruned", "Bitcoin pruning status")? {
            return Err(source_error(
                "Bitcoin index source requires an unpruned node",
                false,
            ));
        }
        if required_bool(
            &chain_info,
            "initialblockdownload",
            "Bitcoin initial-block-download status",
        )? {
            return Err(source_error(
                "Bitcoin Core is still in initial block download",
                true,
            ));
        }
        let blocks = required_u64(&chain_info, "blocks", "Bitcoin block height")?;
        let headers = required_u64(&chain_info, "headers", "Bitcoin header height")?;
        if blocks != headers {
            return Err(source_error(
                "Bitcoin Core block and header heights are not synchronized",
                true,
            ));
        }
        let best_block_hash = parse_bitcoin_block_hash(&required_string(
            &chain_info,
            "bestblockhash",
            "Bitcoin best block hash",
        )?)?;

        let index_info = self
            .request_result("getindexinfo", serde_json::json!(["txindex"]))
            .await?;
        let index_info = parse_object(&index_info, "Bitcoin getindexinfo result")?;
        let txindex = index_info
            .get("txindex")
            .and_then(Value::as_object)
            .ok_or_else(|| source_error("Bitcoin Core transaction index is not enabled", false))?;
        if !required_bool(txindex, "synced", "Bitcoin transaction-index status")? {
            return Err(source_error(
                "Bitcoin Core transaction index is not synchronized",
                true,
            ));
        }
        if let Some(index_height) = txindex.get("best_block_height") {
            let index_height = index_height.as_u64().ok_or_else(|| {
                source_error(
                    "Bitcoin transaction-index height is not an unsigned integer",
                    true,
                )
            })?;
            if index_height != blocks {
                return Err(source_error(
                    "Bitcoin Core transaction index has not reached the canonical tip",
                    true,
                ));
            }
        }

        let genesis = self
            .request_result("getblockhash", serde_json::json!([0]))
            .await?;
        let genesis: String = genesis.deserialize().map_err(map_json_rpc_error)?;
        let genesis = parse_bitcoin_block_hash(&genesis)?;
        if genesis != self.config.expected_genesis_hash {
            return Err(source_error(
                "Bitcoin RPC genesis hash does not match configuration",
                false,
            ));
        }

        Ok(BitcoinCoreNodeStatus {
            version,
            network,
            height: BlockHeight(blocks),
            best_block_hash,
        })
    }

    pub async fn estimate_fee_rate(
        &self,
        target_blocks: u16,
    ) -> Result<SatoshisPerKvb, SourceError> {
        if target_blocks == 0 {
            return Err(source_error(
                "Bitcoin fee-estimation target must be greater than zero",
                false,
            ));
        }
        let raw = self
            .request_result(
                "estimatesmartfee",
                serde_json::json!([target_blocks, "conservative"]),
            )
            .await?;
        let result = parse_object(&raw, "Bitcoin estimatesmartfee result")?;
        let fee_rate = result.get("feerate").ok_or_else(|| {
            source_error("Bitcoin Core cannot currently estimate a fee rate", true)
        })?;
        let satoshis = parse_btc_amount(fee_rate, "Bitcoin estimated BTC/kvB fee rate")?;
        if satoshis == 0 {
            return Err(source_error("Bitcoin Core estimated a zero fee rate", true));
        }
        Ok(SatoshisPerKvb::new(satoshis))
    }

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

    pub async fn preflight(
        &self,
        transaction: &BitcoinSignedTransaction,
        max_fee_rate: SatoshisPerKvb,
    ) -> Result<BitcoinPreflight, SourceError> {
        let max_fee_rate = fee_rate_json(max_fee_rate)?;
        let raw = self
            .request_result(
                "testmempoolaccept",
                Value::Array(vec![
                    Value::Array(vec![Value::String(
                        transaction.consensus_bytes().to_lower_hex_string(),
                    )]),
                    Value::Number(max_fee_rate),
                ]),
            )
            .await?;
        let values: Vec<Value> = raw.deserialize().map_err(map_json_rpc_error)?;
        if values.len() != 1 {
            return Err(source_error(
                "Bitcoin testmempoolaccept returned an unexpected result count",
                true,
            ));
        }
        let result = values[0].as_object().ok_or_else(|| {
            source_error("Bitcoin testmempoolaccept result must be an object", true)
        })?;
        let returned_id = required_string(result, "txid", "Bitcoin preflight transaction ID")?
            .parse::<BitcoinTransactionId>()
            .map_err(|_| source_error("Bitcoin preflight returned an invalid txid", true))?;
        if returned_id != transaction.id() {
            return Err(source_error(
                "Bitcoin preflight returned a different transaction ID",
                true,
            ));
        }
        let allowed = required_bool(result, "allowed", "Bitcoin preflight allowance")?;
        let reject_reason = result
            .get("reject-reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let virtual_size = result.get("vsize").and_then(Value::as_u64);
        let base_fee = result
            .get("fees")
            .and_then(Value::as_object)
            .and_then(|fees| fees.get("base"))
            .map(|value| parse_btc_amount(value, "Bitcoin preflight base fee"))
            .transpose()?
            .map(Satoshi);
        Ok(BitcoinPreflight {
            allowed,
            reject_reason,
            virtual_size,
            base_fee,
        })
    }

    pub async fn broadcast(
        &self,
        transaction: BitcoinSignedTransaction,
        max_fee_rate: SatoshisPerKvb,
    ) -> Result<BitcoinTransactionId, SourceError> {
        let expected_id = transaction.id();
        let max_fee_rate = fee_rate_json(max_fee_rate)?;
        let raw = self
            .request_result(
                "sendrawtransaction",
                Value::Array(vec![
                    Value::String(transaction.consensus_bytes().to_lower_hex_string()),
                    Value::Number(max_fee_rate),
                ]),
            )
            .await?;
        let returned: String = raw.deserialize().map_err(map_json_rpc_error)?;
        let returned = returned
            .parse::<BitcoinTransactionId>()
            .map_err(|_| source_error("Bitcoin Core returned an invalid transaction ID", true))?;
        if returned != expected_id {
            return Err(source_error(
                "Bitcoin Core returned a different transaction ID after broadcast",
                true,
            ));
        }
        Ok(returned)
    }

    pub async fn receipt(
        &self,
        id: &BitcoinTransactionId,
    ) -> Result<Option<BitcoinReceipt>, SourceError> {
        let raw = match self
            .request_result_detailed(
                "getrawtransaction",
                serde_json::json!([id.to_string(), true]),
            )
            .await
        {
            Ok(raw) => raw,
            Err(failure) if failure.remote_code == Some(-5) => return Ok(None),
            Err(failure) => return Err(failure.error),
        };
        let result = parse_object(&raw, "Bitcoin getrawtransaction result")?;
        let returned = required_string(&result, "txid", "Bitcoin receipt transaction ID")?
            .parse::<BitcoinTransactionId>()
            .map_err(|_| source_error("Bitcoin receipt contains an invalid txid", true))?;
        if returned != *id {
            return Err(source_error(
                "Bitcoin receipt contains a different transaction ID",
                true,
            ));
        }
        let mut confirmations = result
            .get("confirmations")
            .map(|value| {
                value.as_i64().ok_or_else(|| {
                    source_error("Bitcoin receipt confirmations are not an integer", true)
                })
            })
            .transpose()?
            .unwrap_or(0);
        let included_in = if confirmations > 0 {
            let hash = required_string(&result, "blockhash", "Bitcoin receipt block hash")?;
            let expected_block_hash = parse_bitcoin_block_hash(&hash)?;
            let header = self
                .request_optional_result("getblockheader", serde_json::json!([hash, true]), &[-5])
                .await?;
            let Some(header) = header else {
                return Err(source_error(
                    "Bitcoin receipt block disappeared during header lookup",
                    true,
                ));
            };
            let header_object = parse_object(&header, "Bitcoin receipt block header")?;
            let header_confirmations = required_i64(
                &header_object,
                "confirmations",
                "Bitcoin receipt block confirmations",
            )?;
            if header_confirmations <= 0 {
                return Err(source_error(
                    "Bitcoin receipt block left the canonical chain during lookup",
                    true,
                ));
            }
            let included = parse_header(&header, None)?;
            if included.hash != expected_block_hash {
                return Err(source_error(
                    "Bitcoin receipt header does not match the transaction block hash",
                    true,
                ));
            }
            let canonical = self
                .request_optional_result(
                    "getblockhash",
                    serde_json::json!([included.height.0]),
                    &[-8],
                )
                .await?;
            let Some(canonical) = canonical else {
                return Err(source_error(
                    "Bitcoin receipt height disappeared during canonicality verification",
                    true,
                ));
            };
            let canonical: String = canonical.deserialize().map_err(map_json_rpc_error)?;
            if parse_bitcoin_block_hash(&canonical)? != included.hash {
                return Err(source_error(
                    "Bitcoin receipt block is no longer canonical",
                    true,
                ));
            }
            confirmations = header_confirmations;
            Some(included)
        } else {
            None
        };
        let confirmations = u64::try_from(confirmations.max(0))
            .map_err(|_| source_error("Bitcoin receipt confirmation count exceeds u64", true))?;
        Ok(Some(BitcoinReceipt {
            id: *id,
            included_in,
            confirmations,
            replaced_by: None,
        }))
    }

    pub(crate) async fn request_result(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, SourceError> {
        self.request_result_detailed(method, params)
            .await
            .map_err(|failure| failure.error)
    }

    pub(crate) async fn request_optional_result(
        &self,
        method: &'static str,
        params: Value,
        missing_codes: &[i64],
    ) -> Result<Option<RawJson>, SourceError> {
        match self.request_result_detailed(method, params).await {
            Ok(result) => Ok(Some(result)),
            Err(failure)
                if failure
                    .remote_code
                    .is_some_and(|code| missing_codes.contains(&code)) =>
            {
                Ok(None)
            }
            Err(failure) => Err(failure.error),
        }
    }

    async fn request_result_detailed(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, BitcoinCoreCallFailure> {
        let id = self.request_id().map_err(BitcoinCoreCallFailure::local)?;
        let request = JsonRpcRequest::new(id.clone(), method, &params)
            .map_err(map_json_rpc_error)
            .map_err(BitcoinCoreCallFailure::local)?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(map_json_rpc_error)
            .map_err(BitcoinCoreCallFailure::local)?;
        if response.id != id {
            return Err(BitcoinCoreCallFailure::local(source_error(
                "Bitcoin JSON-RPC response ID does not match its request",
                true,
            )));
        }
        match response.result {
            Ok(result) => Ok(result),
            Err(failure) => Err(BitcoinCoreCallFailure {
                remote_code: Some(failure.code),
                error: map_remote_failure(failure),
            }),
        }
    }

    fn request_id(&self) -> Result<RequestId, SourceError> {
        self.next_request_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(RequestId::Number)
            .map_err(|_| source_error("Bitcoin JSON-RPC request ID space is exhausted", false))
    }
}

/// Stateless node operations. Spendable-output ownership is deliberately a
/// separate IX-backed boundary.
pub trait BitcoinNodeRpc: Send + Sync {
    fn canonical_hash<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Option<BlockHash>, SourceError>>;

    fn estimate_fee_rate<'a>(
        &'a self,
        target_blocks: u16,
    ) -> BoxFuture<'a, Result<SatoshisPerKvb, SourceError>>;

    fn preflight<'a>(
        &'a self,
        transaction: &'a BitcoinSignedTransaction,
        max_fee_rate: SatoshisPerKvb,
    ) -> BoxFuture<'a, Result<BitcoinPreflight, SourceError>>;

    fn broadcast<'a>(
        &'a self,
        transaction: BitcoinSignedTransaction,
        max_fee_rate: SatoshisPerKvb,
    ) -> BoxFuture<'a, Result<BitcoinTransactionId, SourceError>>;

    fn receipt<'a>(
        &'a self,
        id: &'a BitcoinTransactionId,
    ) -> BoxFuture<'a, Result<Option<BitcoinReceipt>, SourceError>>;
}

impl<C> BitcoinNodeRpc for BitcoinCoreClient<C>
where
    C: JsonRpcClient,
{
    fn canonical_hash<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
        Box::pin(async move { BitcoinCoreClient::canonical_hash(self, height).await })
    }

    fn estimate_fee_rate<'a>(
        &'a self,
        target_blocks: u16,
    ) -> BoxFuture<'a, Result<SatoshisPerKvb, SourceError>> {
        Box::pin(async move { BitcoinCoreClient::estimate_fee_rate(self, target_blocks).await })
    }

    fn preflight<'a>(
        &'a self,
        transaction: &'a BitcoinSignedTransaction,
        max_fee_rate: SatoshisPerKvb,
    ) -> BoxFuture<'a, Result<BitcoinPreflight, SourceError>> {
        Box::pin(async move { BitcoinCoreClient::preflight(self, transaction, max_fee_rate).await })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: BitcoinSignedTransaction,
        max_fee_rate: SatoshisPerKvb,
    ) -> BoxFuture<'a, Result<BitcoinTransactionId, SourceError>> {
        Box::pin(async move { BitcoinCoreClient::broadcast(self, transaction, max_fee_rate).await })
    }

    fn receipt<'a>(
        &'a self,
        id: &'a BitcoinTransactionId,
    ) -> BoxFuture<'a, Result<Option<BitcoinReceipt>, SourceError>> {
        Box::pin(async move { BitcoinCoreClient::receipt(self, id).await })
    }
}

/// IX-backed spendable-output lookup. Reservation state remains PS-owned.
pub trait BitcoinUtxoSource: Send + Sync {
    fn utxos<'a>(
        &'a self,
        addresses: Vec<crate::BitcoinAddress>,
    ) -> BoxFuture<'a, Result<BitcoinUtxoSet, SourceError>>;
}

/// Compatibility surface for the existing one-adapter wallet. New production
/// composition should inject [`BitcoinNodeRpc`] and [`BitcoinUtxoSource`]
/// independently.
pub trait BitcoinRpc: Send + Sync {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>>;

    fn block_at<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<BitcoinBlock, SourceError>>;

    fn utxos<'a>(
        &'a self,
        scripts: Vec<Vec<u8>>,
    ) -> BoxFuture<'a, Result<Vec<BitcoinRpcUtxo>, SourceError>>;

    fn estimate_fee_rate<'a>(&'a self) -> BoxFuture<'a, Result<SatoshisPerKvb, SourceError>>;

    fn broadcast<'a>(
        &'a self,
        transaction: BitcoinSignedTransaction,
    ) -> BoxFuture<'a, Result<BitcoinTransactionId, SourceError>>;

    fn receipt<'a>(
        &'a self,
        id: &'a BitcoinTransactionId,
    ) -> BoxFuture<'a, Result<Option<BitcoinReceipt>, SourceError>>;
}

impl BitcoinNetwork {
    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet3 => "testnet3",
            Self::Testnet4 => "testnet4",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        }
    }

    pub const fn core_chain_name(self) -> &'static str {
        match self {
            Self::Mainnet => "main",
            Self::Testnet3 => "test",
            Self::Testnet4 => "testnet4",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        }
    }

    pub(crate) const fn from_core_chain_name(name: &str) -> Option<Self> {
        match name.as_bytes() {
            b"main" => Some(Self::Mainnet),
            b"test" => Some(Self::Testnet3),
            b"testnet4" => Some(Self::Testnet4),
            b"signet" => Some(Self::Signet),
            b"regtest" => Some(Self::Regtest),
            _ => None,
        }
    }

    pub(crate) const fn native(self) -> bitcoin::Network {
        match self {
            Self::Mainnet => bitcoin::Network::Bitcoin,
            Self::Testnet3 => bitcoin::Network::Testnet,
            Self::Testnet4 => bitcoin::Network::Testnet4,
            Self::Signet => bitcoin::Network::Signet,
            Self::Regtest => bitcoin::Network::Regtest,
        }
    }
}

pub(crate) fn parse_header(
    raw: &RawJson,
    expected_height: Option<BlockHeight>,
) -> Result<BlockRef, SourceError> {
    let result = parse_object(raw, "Bitcoin getblockheader result")?;
    let height = BlockHeight(required_u64(
        &result,
        "height",
        "Bitcoin block-header height",
    )?);
    if expected_height.is_some_and(|expected| expected != height) {
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
    let parent_hash = if height.0 == 0 {
        None
    } else {
        Some(parse_bitcoin_block_hash(&required_string(
            &result,
            "previousblockhash",
            "Bitcoin previous block hash",
        )?)?)
    };
    let timestamp = required_u64(&result, "time", "Bitcoin block-header timestamp")?;
    Ok(BlockRef {
        height,
        hash,
        parent_hash,
        timestamp: Some(timestamp),
    })
}

pub fn parse_bitcoin_block_hash(value: &str) -> Result<BlockHash, SourceError> {
    value
        .parse::<NativeBlockHash>()
        .map(|hash| BlockHash(hash.to_byte_array().to_vec()))
        .map_err(|_| source_error("Bitcoin RPC returned an invalid block hash", true))
}

pub fn format_bitcoin_block_hash(hash: &BlockHash) -> Result<String, SourceError> {
    let bytes: [u8; 32] = hash
        .0
        .as_slice()
        .try_into()
        .map_err(|_| source_error("Bitcoin block hash must be 32 bytes", false))?;
    Ok(NativeBlockHash::from_byte_array(bytes).to_string())
}

fn parse_object(raw: &RawJson, context: &'static str) -> Result<Map<String, Value>, SourceError> {
    raw.deserialize::<Value>()
        .map_err(map_json_rpc_error)?
        .as_object()
        .cloned()
        .ok_or_else(|| source_error(format!("{context} must be an object"), true))
}

fn required_string(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<String, SourceError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| source_error(format!("{context} is missing or invalid"), true))
}

fn required_u64(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<u64, SourceError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| source_error(format!("{context} is missing or invalid"), true))
}

fn required_i64(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<i64, SourceError> {
    object
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| source_error(format!("{context} is missing or invalid"), true))
}

fn required_bool(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<bool, SourceError> {
    object
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| source_error(format!("{context} is missing or invalid"), true))
}

fn parse_btc_amount(value: &Value, context: &'static str) -> Result<u64, SourceError> {
    let lexical = value
        .as_number()
        .map(Number::to_string)
        .ok_or_else(|| source_error(format!("{context} must be a JSON number"), true))?;
    if lexical.starts_with('-') || lexical.contains(['e', 'E', '+']) {
        return Err(source_error(
            format!("{context} must be a non-negative fixed-point decimal"),
            true,
        ));
    }
    let mut parts = lexical.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 8
    {
        return Err(source_error(
            format!("{context} is not an exact Bitcoin amount"),
            true,
        ));
    }
    let whole = whole
        .parse::<u64>()
        .map_err(|_| source_error(format!("{context} exceeds u64 satoshis"), true))?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        let parsed = fraction
            .parse::<u64>()
            .map_err(|_| source_error(format!("{context} is invalid"), true))?;
        let power = u32::try_from(8_usize.saturating_sub(fraction.len()))
            .map_err(|_| source_error(format!("{context} precision is invalid"), true))?;
        parsed
            .checked_mul(10_u64.pow(power))
            .ok_or_else(|| source_error(format!("{context} exceeds u64 satoshis"), true))?
    };
    whole
        .checked_mul(SATOSHIS_PER_BITCOIN)
        .and_then(|satoshis| satoshis.checked_add(fraction))
        .ok_or_else(|| source_error(format!("{context} exceeds u64 satoshis"), true))
}

fn fee_rate_json(fee_rate: SatoshisPerKvb) -> Result<Number, SourceError> {
    let satoshis = fee_rate.satoshis_per_kvb();
    if satoshis == 0 {
        return Err(source_error(
            "Bitcoin maximum fee rate must be greater than zero",
            false,
        ));
    }
    if satoshis > BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB {
        return Err(source_error(
            "Bitcoin maximum fee rate exceeds Bitcoin Core's 1 BTC/kvB limit",
            false,
        ));
    }
    let whole = satoshis / SATOSHIS_PER_BITCOIN;
    let remainder = satoshis % SATOSHIS_PER_BITCOIN;
    let lexical = if remainder == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{remainder:08}")
            .trim_end_matches('0')
            .to_owned()
    };
    Number::from_str(&lexical)
        .map_err(|_| source_error("Bitcoin maximum fee rate could not be encoded", false))
}

#[derive(Debug)]
struct BitcoinCoreCallFailure {
    remote_code: Option<i64>,
    error: SourceError,
}

impl BitcoinCoreCallFailure {
    fn local(error: SourceError) -> Self {
        Self {
            remote_code: None,
            error,
        }
    }
}

fn map_json_rpc_error(error: JsonRpcError) -> SourceError {
    source_error(error.to_string(), error.is_retryable())
}

fn map_remote_failure(failure: JsonRpcFailure) -> SourceError {
    let retryable = failure.code == -28 || failure.is_server_error();
    source_error(
        format!("Bitcoin JSON-RPC request failed with code {}", failure.code),
        retryable,
    )
}

pub(crate) fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute,
        consensus, transaction::Version,
    };
    use futures_executor::block_on;
    use json_rpc::{JsonRpcResponse, RawJson};
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct ScriptedClient {
        replies: Arc<Mutex<VecDeque<ExpectedReply>>>,
    }

    struct ExpectedReply {
        method: &'static str,
        result: Result<Value, i64>,
    }

    impl ScriptedClient {
        fn new(replies: Vec<ExpectedReply>) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies.into())),
            }
        }
    }

    impl JsonRpcClient for ScriptedClient {
        fn request<'a>(
            &'a self,
            request: JsonRpcRequest,
        ) -> BoxFuture<'a, Result<JsonRpcResponse, JsonRpcError>> {
            let expected = self
                .replies
                .lock()
                .expect("script lock must be healthy")
                .pop_front()
                .expect("Core client made more calls than scripted");
            assert_eq!(request.method, expected.method);
            let result = expected
                .result
                .map(|value| RawJson::from_serializable(&value).expect("reply JSON must encode"))
                .map_err(|code| JsonRpcFailure {
                    code,
                    message: "scripted failure".to_owned(),
                    data: None,
                });
            Box::pin(async move {
                Ok(JsonRpcResponse {
                    id: request.id,
                    result,
                })
            })
        }

        fn batch<'a>(
            &'a self,
            _requests: Vec<JsonRpcRequest>,
        ) -> BoxFuture<'a, Result<Vec<JsonRpcResponse>, JsonRpcError>> {
            Box::pin(async move { Ok(Vec::new()) })
        }
    }

    fn success(method: &'static str, result: Value) -> ExpectedReply {
        ExpectedReply {
            method,
            result: Ok(result),
        }
    }

    fn readiness_replies() -> Vec<ExpectedReply> {
        vec![
            success("getnetworkinfo", serde_json::json!({"version": 310000})),
            success(
                "getblockchaininfo",
                serde_json::json!({
                    "chain": "regtest",
                    "blocks": 10,
                    "headers": 10,
                    "bestblockhash": format!("{:064x}", 2),
                    "initialblockdownload": false,
                    "pruned": false
                }),
            ),
            success(
                "getindexinfo",
                serde_json::json!({"txindex": {"synced": true, "best_block_height": 10}}),
            ),
            success("getblockhash", Value::String(format!("{:064x}", 1))),
        ]
    }

    fn config() -> BitcoinCoreConfig {
        BitcoinCoreConfig {
            expected_network: BitcoinNetwork::Regtest,
            expected_genesis_hash: parse_bitcoin_block_hash(&format!("{:064x}", 1))
                .expect("test genesis hash must parse"),
        }
    }

    fn signed_transaction() -> BitcoinSignedTransaction {
        let transaction = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([3; 32]), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let id = BitcoinTransactionId::from(transaction.compute_txid());
        BitcoinSignedTransaction::from_consensus_bytes(id, consensus::serialize(&transaction))
            .expect("test transaction must be internally consistent")
    }

    #[test]
    fn connect_validates_core_31_identity_and_readiness() {
        let core = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(readiness_replies()),
            config(),
        ))
        .expect("valid scripted Core node must connect");

        assert_eq!(core.config(), &config());
    }

    #[test]
    fn connect_rejects_pruned_node() {
        let replies = vec![
            success("getnetworkinfo", serde_json::json!({"version": 310000})),
            success(
                "getblockchaininfo",
                serde_json::json!({
                    "chain": "regtest",
                    "blocks": 10,
                    "headers": 10,
                    "bestblockhash": format!("{:064x}", 2),
                    "initialblockdownload": false,
                    "pruned": true
                }),
            ),
        ];
        let error = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(replies),
            config(),
        ))
        .err()
        .expect("pruned Core node must fail");

        assert!(!error.retryable);
        assert!(error.message.contains("unpruned"));
    }

    #[test]
    fn core_warmup_failure_is_retryable() {
        let error = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(vec![ExpectedReply {
                method: "getnetworkinfo",
                result: Err(-28),
            }]),
            config(),
        ))
        .err()
        .expect("Core warmup must not connect");

        assert!(error.retryable);
    }

    #[test]
    fn fee_estimate_converts_exact_btc_per_kvb_without_float() {
        let mut replies = readiness_replies();
        replies.push(success(
            "estimatesmartfee",
            serde_json::json!({"feerate": 0.00001001, "blocks": 6}),
        ));
        let core = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(replies),
            config(),
        ))
        .expect("valid scripted Core node must connect");

        let rate = block_on(core.estimate_fee_rate(6)).expect("fee estimate must parse");

        assert_eq!(rate.satoshis_per_kvb(), 1_001);
    }

    #[test]
    fn preflight_preserves_rejection_reason_and_exact_fee() {
        let signed = signed_transaction();
        let base_fee = Number::from_str("0.00000123").expect("test fixed-point fee must encode");
        let mut replies = readiness_replies();
        replies.push(success(
            "testmempoolaccept",
            serde_json::json!([{
                "txid": signed.id().to_string(),
                "allowed": false,
                "reject-reason": "missing-inputs",
                "vsize": 82,
                "fees": {"base": base_fee}
            }]),
        ));
        let core = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(replies),
            config(),
        ))
        .expect("valid scripted Core node must connect");

        let result = block_on(core.preflight(&signed, SatoshisPerKvb::new(10_000)))
            .expect("preflight result must parse");

        assert!(!result.allowed);
        assert_eq!(result.reject_reason.as_deref(), Some("missing-inputs"));
        assert_eq!(result.base_fee, Some(Satoshi(123)));
    }

    #[test]
    fn core_max_fee_rate_boundary_is_enforced_before_rpc() {
        assert!(
            fee_rate_json(SatoshisPerKvb::new(
                BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB,
            ))
            .is_ok()
        );
        let error = fee_rate_json(SatoshisPerKvb::new(
            BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB + 1,
        ))
        .expect_err("fee rates above Core's limit must fail locally");
        assert!(!error.retryable);
    }

    #[test]
    fn broadcast_rejects_a_mismatched_returned_txid() {
        let signed = signed_transaction();
        let mut replies = readiness_replies();
        replies.push(success(
            "sendrawtransaction",
            Value::String(format!("{:064x}", 9)),
        ));
        let core = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(replies),
            config(),
        ))
        .expect("valid scripted Core node must connect");

        let error = block_on(core.broadcast(signed, SatoshisPerKvb::new(10_000)))
            .expect_err("mismatched broadcast ID must fail");

        assert!(error.message.contains("different transaction ID"));
    }

    #[test]
    fn receipt_uses_txindex_result_and_canonical_block_header() {
        let signed = signed_transaction();
        let block_hash = format!("{:064x}", 7);
        let parent_hash = format!("{:064x}", 6);
        let mut replies = readiness_replies();
        replies.extend([
            success(
                "getrawtransaction",
                serde_json::json!({
                    "txid": signed.id().to_string(),
                    "blockhash": block_hash.clone(),
                    "confirmations": 2
                }),
            ),
            success(
                "getblockheader",
                serde_json::json!({
                    "hash": block_hash.clone(),
                    "height": 10,
                    "previousblockhash": parent_hash,
                    "time": 100,
                    "confirmations": 2
                }),
            ),
            success("getblockhash", Value::String(block_hash)),
        ]);
        let core = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(replies),
            config(),
        ))
        .expect("valid scripted Core node must connect");

        let receipt = block_on(core.receipt(&signed.id()))
            .expect("receipt lookup must succeed")
            .expect("txindex transaction must exist");

        assert_eq!(receipt.id, signed.id());
        assert_eq!(receipt.confirmations, 2);
        assert_eq!(
            receipt
                .included_in
                .expect("confirmed transaction has a block")
                .height,
            BlockHeight(10)
        );
    }

    #[test]
    fn receipt_rejects_a_header_that_left_the_active_chain() {
        let signed = signed_transaction();
        let block_hash = format!("{:064x}", 7);
        let mut replies = readiness_replies();
        replies.extend([
            success(
                "getrawtransaction",
                serde_json::json!({
                    "txid": signed.id().to_string(),
                    "blockhash": block_hash.clone(),
                    "confirmations": 2
                }),
            ),
            success(
                "getblockheader",
                serde_json::json!({
                    "hash": block_hash,
                    "height": 10,
                    "previousblockhash": format!("{:064x}", 6),
                    "time": 100,
                    "confirmations": -1
                }),
            ),
        ]);
        let core = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(replies),
            config(),
        ))
        .expect("valid scripted Core node must connect");

        let error = block_on(core.receipt(&signed.id()))
            .expect_err("inactive block receipt must fail closed");
        assert!(error.retryable);
        assert!(error.message.contains("canonical chain"));
    }

    #[test]
    fn receipt_rejects_a_header_for_a_different_block_hash() {
        let signed = signed_transaction();
        let transaction_block_hash = format!("{:064x}", 7);
        let mut replies = readiness_replies();
        replies.extend([
            success(
                "getrawtransaction",
                serde_json::json!({
                    "txid": signed.id().to_string(),
                    "blockhash": transaction_block_hash,
                    "confirmations": 2
                }),
            ),
            success(
                "getblockheader",
                serde_json::json!({
                    "hash": format!("{:064x}", 8),
                    "height": 10,
                    "previousblockhash": format!("{:064x}", 6),
                    "time": 100,
                    "confirmations": 2
                }),
            ),
        ]);
        let core = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(replies),
            config(),
        ))
        .expect("valid scripted Core node must connect");

        let error = block_on(core.receipt(&signed.id()))
            .expect_err("mismatched header hash must fail closed");
        assert!(error.retryable);
        assert!(error.message.contains("does not match"));
    }

    #[test]
    fn receipt_treats_a_disappearing_block_header_as_retryable() {
        let signed = signed_transaction();
        let mut replies = readiness_replies();
        replies.extend([
            success(
                "getrawtransaction",
                serde_json::json!({
                    "txid": signed.id().to_string(),
                    "blockhash": format!("{:064x}", 7),
                    "confirmations": 2
                }),
            ),
            ExpectedReply {
                method: "getblockheader",
                result: Err(-5),
            },
        ]);
        let core = block_on(BitcoinCoreClient::connect(
            ScriptedClient::new(replies),
            config(),
        ))
        .expect("valid scripted Core node must connect");

        let error = block_on(core.receipt(&signed.id()))
            .expect_err("a reorged-away block header must be retried");
        assert!(error.retryable);
        assert!(error.message.contains("disappeared"));
    }
}
