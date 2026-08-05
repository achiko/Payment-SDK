use std::{
    error::Error,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use alloy_primitives::keccak256;
use http::{HttpTransport, HttpTransportConfig, RetryPolicy};
use indexing::{BlockHash, BlockHeight, BlockRef, SourceError};
use json_rpc::{
    JsonRpcClient, JsonRpcError, JsonRpcFailure, JsonRpcRequest, RawJson, RequestId,
    TransportJsonRpcClient,
};
use serde_json::{Map, Value, json};

use crate::{
    BoxFuture, EthereumAddress, EthereumAsset, EthereumBuildContext, EthereumReceipt,
    EthereumSignedTransaction, EthereumTransactionId, EthereumTransferRequest, Wei,
};

const BASIS_POINTS_DENOMINATOR: u64 = 10_000;
const ERC20_BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];

/// Wallet-facing Ethereum RPC surface.
///
/// Canonical block synchronization is deliberately exposed through the
/// separate `EthereumIndexRpc`/`BlockSource` boundary. Wallet composition must
/// not acquire Indexer Service ownership by implementing this trait.
pub trait EthereumRpc: Send + Sync {
    fn balance<'a>(
        &'a self,
        address: EthereumAddress,
        asset: &'a EthereumAsset,
        at: Option<BlockRef>,
    ) -> BoxFuture<'a, Result<Wei, SourceError>>;

    fn nonce<'a>(&'a self, address: EthereumAddress) -> BoxFuture<'a, Result<u64, SourceError>>;

    /// Returns nonce, gas limit, and EIP-1559 fees for one concrete transfer.
    fn build_context<'a>(
        &'a self,
        request: &'a EthereumTransferRequest,
    ) -> BoxFuture<'a, Result<EthereumBuildContext, SourceError>>;

    fn receipt<'a>(
        &'a self,
        id: &'a EthereumTransactionId,
    ) -> BoxFuture<'a, Result<Option<EthereumReceipt>, SourceError>>;

    fn broadcast<'a>(
        &'a self,
        transaction: EthereumSignedTransaction,
    ) -> BoxFuture<'a, Result<EthereumTransactionId, SourceError>>;
}

/// Explicit transaction-construction safety limits applied to RPC results.
///
/// Providers remain the source of nonce, gas, and fee observations, but they
/// cannot cause the wallet to build a transaction above operator-selected
/// ceilings. A value over a ceiling is rejected rather than silently clamped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumRpcLimits {
    max_input_bytes: usize,
    gas_limit_margin_basis_points: u32,
    max_gas_limit: u64,
    max_fee_per_gas: Wei,
    max_priority_fee_per_gas: Wei,
    max_total_fee: Wei,
}

impl EthereumRpcLimits {
    pub fn new(
        max_input_bytes: usize,
        gas_limit_margin_basis_points: u32,
        max_gas_limit: u64,
        max_fee_per_gas: Wei,
        max_priority_fee_per_gas: Wei,
        max_total_fee: Wei,
    ) -> Result<Self, EthereumHttpRpcBuildError> {
        if max_input_bytes == 0 {
            return Err(invalid_configuration(
                "Ethereum RPC maximum transaction input size must be greater than zero",
            ));
        }
        if u64::from(gas_limit_margin_basis_points) > BASIS_POINTS_DENOMINATOR {
            return Err(invalid_configuration(
                "Ethereum RPC gas-limit margin must not exceed 10000 basis points",
            ));
        }
        if max_gas_limit == 0 {
            return Err(invalid_configuration(
                "Ethereum RPC maximum gas limit must be greater than zero",
            ));
        }
        if max_fee_per_gas.is_zero() || max_total_fee.is_zero() {
            return Err(invalid_configuration(
                "Ethereum RPC fee ceilings must be greater than zero",
            ));
        }
        if max_priority_fee_per_gas > max_fee_per_gas {
            return Err(invalid_configuration(
                "Ethereum RPC priority-fee ceiling must not exceed the max-fee ceiling",
            ));
        }

        Ok(Self {
            max_input_bytes,
            gas_limit_margin_basis_points,
            max_gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            max_total_fee,
        })
    }

    #[must_use]
    pub const fn max_input_bytes(&self) -> usize {
        self.max_input_bytes
    }

    #[must_use]
    pub const fn gas_limit_margin_basis_points(&self) -> u32 {
        self.gas_limit_margin_basis_points
    }

    #[must_use]
    pub const fn max_gas_limit(&self) -> u64 {
        self.max_gas_limit
    }

    #[must_use]
    pub const fn max_fee_per_gas(&self) -> &Wei {
        &self.max_fee_per_gas
    }

    #[must_use]
    pub const fn max_priority_fee_per_gas(&self) -> &Wei {
        &self.max_priority_fee_per_gas
    }

    #[must_use]
    pub const fn max_total_fee(&self) -> &Wei {
        &self.max_total_fee
    }
}

/// Complete production HTTP configuration for the wallet-facing Ethereum RPC.
///
/// Debug output includes header names for diagnostics, but never the endpoint
/// or header values because both may contain credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct EthereumHttpRpcConfig {
    endpoint: String,
    expected_chain_id: u64,
    request_timeout: Duration,
    max_response_bytes: usize,
    headers: Vec<(String, String)>,
    retry_policy: RetryPolicy,
    limits: EthereumRpcLimits,
}

impl EthereumHttpRpcConfig {
    pub fn new(
        endpoint: impl Into<String>,
        expected_chain_id: u64,
        request_timeout: Duration,
        max_response_bytes: usize,
        retry_policy: RetryPolicy,
        limits: EthereumRpcLimits,
    ) -> Result<Self, EthereumHttpRpcBuildError> {
        let endpoint = endpoint.into();
        if endpoint.trim().is_empty() {
            return Err(invalid_configuration(
                "Ethereum RPC HTTP endpoint must not be empty",
            ));
        }
        if expected_chain_id == 0 {
            return Err(invalid_configuration(
                "expected Ethereum chain ID must be non-zero",
            ));
        }
        if request_timeout.is_zero() {
            return Err(invalid_configuration(
                "Ethereum RPC request timeout must be greater than zero",
            ));
        }
        if max_response_bytes == 0 {
            return Err(invalid_configuration(
                "Ethereum RPC response-size limit must be greater than zero",
            ));
        }

        Ok(Self {
            endpoint,
            expected_chain_id,
            request_timeout,
            max_response_bytes,
            headers: Vec::new(),
            retry_policy,
            limits,
        })
    }

    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    #[must_use]
    pub const fn expected_chain_id(&self) -> u64 {
        self.expected_chain_id
    }

    #[must_use]
    pub const fn limits(&self) -> &EthereumRpcLimits {
        &self.limits
    }
}

impl fmt::Debug for EthereumHttpRpcConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        formatter
            .debug_struct("EthereumHttpRpcConfig")
            .field("endpoint", &"[REDACTED]")
            .field("expected_chain_id", &self.expected_chain_id)
            .field("request_timeout", &self.request_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("header_names", &header_names)
            .field("retry_policy", &self.retry_policy)
            .field("limits", &self.limits)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EthereumHttpRpcBuildErrorKind {
    InvalidConfiguration,
    HttpTransport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumHttpRpcBuildError {
    pub kind: EthereumHttpRpcBuildErrorKind,
    pub message: String,
}

impl fmt::Display for EthereumHttpRpcBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for EthereumHttpRpcBuildError {}

fn invalid_configuration(message: impl Into<String>) -> EthereumHttpRpcBuildError {
    EthereumHttpRpcBuildError {
        kind: EthereumHttpRpcBuildErrorKind::InvalidConfiguration,
        message: message.into(),
    }
}

type ProductionJsonRpcClient = TransportJsonRpcClient<HttpTransport>;

/// Chain-owned Ethereum methods over injected JSON-RPC framing.
///
/// Production construction uses `packages/http`; deterministic tests and
/// specialized composition may inject another `JsonRpcClient` implementation.
pub struct EthereumHttpRpc<C = ProductionJsonRpcClient> {
    client: C,
    expected_chain_id: u64,
    limits: EthereumRpcLimits,
    next_request_id: AtomicU64,
}

impl EthereumHttpRpc<ProductionJsonRpcClient> {
    pub fn new(config: EthereumHttpRpcConfig) -> Result<Self, EthereumHttpRpcBuildError> {
        let mut transport_config =
            HttpTransportConfig::new(config.endpoint.clone(), config.request_timeout);
        transport_config.max_response_bytes = config.max_response_bytes;
        transport_config.default_headers = config.headers;
        transport_config.retry_policy = config.retry_policy;
        let transport =
            HttpTransport::new(transport_config).map_err(|_| EthereumHttpRpcBuildError {
                kind: EthereumHttpRpcBuildErrorKind::HttpTransport,
                message: "failed to construct Ethereum RPC HTTP transport".to_owned(),
            })?;
        let client = TransportJsonRpcClient::new(transport, config.endpoint);
        Self::with_client(client, config.expected_chain_id, config.limits)
    }
}

impl<C> EthereumHttpRpc<C> {
    pub fn with_client(
        client: C,
        expected_chain_id: u64,
        limits: EthereumRpcLimits,
    ) -> Result<Self, EthereumHttpRpcBuildError> {
        if expected_chain_id == 0 {
            return Err(invalid_configuration(
                "expected Ethereum chain ID must be non-zero",
            ));
        }
        Ok(Self {
            client,
            expected_chain_id,
            limits,
            next_request_id: AtomicU64::new(1),
        })
    }

    #[must_use]
    pub const fn expected_chain_id(&self) -> u64 {
        self.expected_chain_id
    }

    #[must_use]
    pub const fn limits(&self) -> &EthereumRpcLimits {
        &self.limits
    }
}

impl<C> fmt::Debug for EthereumHttpRpc<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EthereumHttpRpc")
            .field("client", &"[REDACTED]")
            .field("expected_chain_id", &self.expected_chain_id)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl<C> EthereumHttpRpc<C>
where
    C: JsonRpcClient,
{
    /// Verifies that the configured provider is reachable and serves the
    /// expected Ethereum chain without acquiring any indexing state.
    pub async fn verify_chain_id(&self) -> Result<(), SourceError> {
        let actual = self.chain_id().await?;
        if actual != self.expected_chain_id {
            return Err(source_error(
                format!(
                    "Ethereum RPC chain ID {actual} does not match configured chain ID {}",
                    self.expected_chain_id
                ),
                false,
            ));
        }
        Ok(())
    }

    async fn chain_id(&self) -> Result<u64, SourceError> {
        self.rpc_u64("eth_chainId", json!([])).await
    }

    async fn rpc_u64(&self, method: &'static str, params: Value) -> Result<u64, SourceError> {
        let raw = self.request_result(method, params).await?;
        let value: String = raw.deserialize().map_err(map_json_rpc_error)?;
        parse_quantity_u64(&value).map_err(|message| invalid_rpc_response(method, message))
    }

    async fn rpc_wei(&self, method: &'static str, params: Value) -> Result<Wei, SourceError> {
        let raw = self.request_result(method, params).await?;
        let value: String = raw.deserialize().map_err(map_json_rpc_error)?;
        parse_quantity_wei(&value).map_err(|message| invalid_rpc_response(method, message))
    }

    async fn request_result(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, SourceError> {
        self.request_result_detailed(method, params)
            .await
            .map_err(|error| error.into_source(method))
    }

    async fn request_result_detailed(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, RpcCallError> {
        let id = self.request_id().map_err(RpcCallError::Local)?;
        let request = JsonRpcRequest::new(id.clone(), method, &params)
            .map_err(map_json_rpc_error)
            .map_err(RpcCallError::Local)?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(map_json_rpc_error)
            .map_err(RpcCallError::Local)?;
        if response.id != id {
            return Err(RpcCallError::Local(source_error(
                "Ethereum JSON-RPC response ID does not match its request",
                false,
            )));
        }
        response.result.map_err(RpcCallError::Remote)
    }

    fn request_id(&self) -> Result<RequestId, SourceError> {
        self.next_request_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(RequestId::Number)
            .map_err(|_| source_error("Ethereum JSON-RPC request ID space is exhausted", false))
    }

    async fn confirm_known_transaction(
        &self,
        expected: &EthereumTransactionId,
    ) -> Result<bool, SourceError> {
        let raw = self
            .request_result(
                "eth_getTransactionByHash",
                json!([transaction_id_hex(expected)]),
            )
            .await?;
        let value: Value = raw.deserialize().map_err(map_json_rpc_error)?;
        if value.is_null() {
            return Ok(false);
        }
        let returned = value.get("hash").and_then(Value::as_str).ok_or_else(|| {
            invalid_rpc_response("eth_getTransactionByHash", "transaction object has no hash")
        })?;
        let returned = parse_transaction_id(returned, "eth_getTransactionByHash")?;
        if &returned != expected {
            return Err(invalid_rpc_response(
                "eth_getTransactionByHash",
                "transaction object hash does not match the lookup",
            ));
        }
        Ok(true)
    }
}

impl<C> EthereumRpc for EthereumHttpRpc<C>
where
    C: JsonRpcClient,
{
    fn balance<'a>(
        &'a self,
        address: EthereumAddress,
        asset: &'a EthereumAsset,
        at: Option<BlockRef>,
    ) -> BoxFuture<'a, Result<Wei, SourceError>> {
        Box::pin(async move {
            let block = block_parameter(at)?;
            match asset {
                EthereumAsset::Native => {
                    self.rpc_wei("eth_getBalance", json!([address_hex(&address), block]))
                        .await
                }
                EthereumAsset::Erc20(token) => {
                    let raw = self
                        .request_result(
                            "eth_call",
                            json!([{
                                "to": address_hex(token),
                                "data": erc20_balance_of_call(&address),
                            }, block]),
                        )
                        .await?;
                    let value: String = raw.deserialize().map_err(map_json_rpc_error)?;
                    parse_abi_word(&value)
                        .map(Wei)
                        .map_err(|message| invalid_rpc_response("eth_call", message))
                }
            }
        })
    }

    fn nonce<'a>(&'a self, address: EthereumAddress) -> BoxFuture<'a, Result<u64, SourceError>> {
        Box::pin(async move {
            self.rpc_u64(
                "eth_getTransactionCount",
                json!([address_hex(&address), "pending"]),
            )
            .await
        })
    }

    fn build_context<'a>(
        &'a self,
        request: &'a EthereumTransferRequest,
    ) -> BoxFuture<'a, Result<EthereumBuildContext, SourceError>> {
        Box::pin(async move {
            if request.data.len() > self.limits.max_input_bytes {
                return Err(source_error(
                    "Ethereum transaction input exceeds the configured size limit",
                    false,
                ));
            }
            if request.to.is_none() && request.data.is_empty() {
                return Err(source_error(
                    "Ethereum contract creation requires non-empty init code",
                    false,
                ));
            }

            self.verify_chain_id().await?;
            let chain_id = self.expected_chain_id;
            let nonce = self.nonce(request.from.clone()).await?;
            let mut transaction = Map::new();
            transaction.insert("from".to_owned(), json!(address_hex(&request.from)));
            if let Some(to) = &request.to {
                transaction.insert("to".to_owned(), json!(address_hex(to)));
            }
            transaction.insert("value".to_owned(), json!(wei_quantity(&request.value)));
            transaction.insert("data".to_owned(), json!(data_hex(&request.data)));

            let estimated_gas_limit = self
                .rpc_u64("eth_estimateGas", json!([Value::Object(transaction)]))
                .await?;
            if estimated_gas_limit == 0 {
                return Err(invalid_rpc_response(
                    "eth_estimateGas",
                    "estimated gas limit is zero",
                ));
            }
            let gas_limit = gas_limit_with_margin(
                estimated_gas_limit,
                self.limits.gas_limit_margin_basis_points,
            )?;
            if gas_limit > self.limits.max_gas_limit {
                return Err(source_error(
                    "Ethereum estimated gas limit exceeds the configured ceiling",
                    false,
                ));
            }

            let max_priority_fee_per_gas =
                self.rpc_wei("eth_maxPriorityFeePerGas", json!([])).await?;
            if max_priority_fee_per_gas > self.limits.max_priority_fee_per_gas {
                return Err(source_error(
                    "Ethereum priority fee exceeds the configured ceiling",
                    false,
                ));
            }
            let latest_block = self
                .request_result("eth_getBlockByNumber", json!(["latest", false]))
                .await?;
            let latest_block: Value = latest_block.deserialize().map_err(map_json_rpc_error)?;
            let base_fee = latest_block
                .get("baseFeePerGas")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    invalid_rpc_response(
                        "eth_getBlockByNumber",
                        "latest block has no EIP-1559 baseFeePerGas",
                    )
                })
                .and_then(|value| {
                    parse_quantity_wei(value)
                        .map_err(|message| invalid_rpc_response("eth_getBlockByNumber", message))
                })?;
            let max_fee_per_gas = base_fee
                .checked_mul_u64(2)
                .and_then(|fee| fee.checked_add(&max_priority_fee_per_gas))
                .ok_or_else(|| {
                    invalid_rpc_response("eth_getBlockByNumber", "fee calculation overflowed U256")
                })?;
            if max_fee_per_gas > self.limits.max_fee_per_gas {
                return Err(source_error(
                    "Ethereum max fee per gas exceeds the configured ceiling",
                    false,
                ));
            }
            if max_fee_per_gas < max_priority_fee_per_gas {
                return Err(invalid_rpc_response(
                    "eth_getBlockByNumber",
                    "calculated max fee is below the priority fee",
                ));
            }
            let total_fee = max_fee_per_gas.checked_mul_u64(gas_limit).ok_or_else(|| {
                invalid_rpc_response("fee calculation", "total fee overflowed U256")
            })?;
            if total_fee > self.limits.max_total_fee {
                return Err(source_error(
                    "Ethereum maximum transaction fee exceeds the configured ceiling",
                    false,
                ));
            }

            Ok(EthereumBuildContext {
                chain_id,
                nonce,
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            })
        })
    }

    fn receipt<'a>(
        &'a self,
        id: &'a EthereumTransactionId,
    ) -> BoxFuture<'a, Result<Option<EthereumReceipt>, SourceError>> {
        Box::pin(async move {
            let raw = self
                .request_result("eth_getTransactionReceipt", json!([transaction_id_hex(id)]))
                .await?;
            let value: Value = raw.deserialize().map_err(map_json_rpc_error)?;
            if value.is_null() {
                return Ok(None);
            }
            let receipt = parse_receipt(&value, id)?;
            let confirmations = match &receipt.included_in {
                Some(block) => {
                    let tip = self.rpc_u64("eth_blockNumber", json!([])).await?;
                    if tip < block.height.0 {
                        return Err(source_error(
                            "Ethereum RPC tip is below the transaction receipt block",
                            true,
                        ));
                    }
                    tip.checked_sub(block.height.0)
                        .and_then(|distance| distance.checked_add(1))
                        .ok_or_else(|| {
                            invalid_rpc_response(
                                "eth_blockNumber",
                                "receipt confirmation count overflowed u64",
                            )
                        })?
                }
                None => 0,
            };
            Ok(Some(EthereumReceipt {
                confirmations,
                ..receipt
            }))
        })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: EthereumSignedTransaction,
    ) -> BoxFuture<'a, Result<EthereumTransactionId, SourceError>> {
        Box::pin(async move {
            let computed = EthereumTransactionId(keccak256(&transaction.envelope).0);
            if computed != transaction.id {
                return Err(source_error(
                    "signed Ethereum envelope hash does not match its transaction ID",
                    false,
                ));
            }
            let result = self
                .request_result_detailed(
                    "eth_sendRawTransaction",
                    json!([data_hex(&transaction.envelope)]),
                )
                .await;
            let raw = match result {
                Ok(raw) => raw,
                Err(RpcCallError::Remote(failure)) if is_already_known(&failure) => {
                    if self.confirm_known_transaction(&computed).await? {
                        return Ok(computed);
                    }
                    return Err(source_error(
                        "Ethereum RPC reported an already-known transaction but did not expose the matching hash",
                        true,
                    ));
                }
                Err(error) => return Err(error.into_source("eth_sendRawTransaction")),
            };
            let returned: String = raw.deserialize().map_err(map_json_rpc_error)?;
            let returned = parse_transaction_id(&returned, "eth_sendRawTransaction")?;
            if returned != computed {
                return Err(invalid_rpc_response(
                    "eth_sendRawTransaction",
                    "node hash differs from the locally computed transaction hash",
                ));
            }
            Ok(returned)
        })
    }
}

enum RpcCallError {
    Local(SourceError),
    Remote(JsonRpcFailure),
}

impl RpcCallError {
    fn into_source(self, method: &'static str) -> SourceError {
        match self {
            Self::Local(error) => error,
            Self::Remote(failure) => source_error(
                format!(
                    "Ethereum JSON-RPC {method} failed with code {}",
                    failure.code
                ),
                remote_failure_is_retryable(&failure),
            ),
        }
    }
}

fn remote_failure_is_retryable(failure: &JsonRpcFailure) -> bool {
    if matches!(failure.code, -32_605 | -32_603 | -32_005) {
        return true;
    }
    let message = failure.message.to_ascii_lowercase();
    [
        "rate limit",
        "too many requests",
        "temporarily unavailable",
        "timeout",
        "timed out",
        "try again",
        "overloaded",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn is_already_known(failure: &JsonRpcFailure) -> bool {
    let message = failure.message.to_ascii_lowercase();
    message.contains("already known") || message.contains("known transaction")
}

fn parse_receipt(
    value: &Value,
    expected_id: &EthereumTransactionId,
) -> Result<EthereumReceipt, SourceError> {
    let object = value.as_object().ok_or_else(|| {
        invalid_rpc_response("eth_getTransactionReceipt", "receipt is not an object")
    })?;
    let transaction_hash = object
        .get("transactionHash")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            invalid_rpc_response(
                "eth_getTransactionReceipt",
                "receipt has no transaction hash",
            )
        })?;
    let transaction_id = parse_transaction_id(transaction_hash, "eth_getTransactionReceipt")?;
    if &transaction_id != expected_id {
        return Err(invalid_rpc_response(
            "eth_getTransactionReceipt",
            "receipt transaction hash does not match the request",
        ));
    }

    let block_number = optional_string(object.get("blockNumber"), "receipt block number")?;
    let block_hash = optional_string(object.get("blockHash"), "receipt block hash")?;
    let included_in = match (block_number, block_hash) {
        (None, None) => None,
        (Some(number), Some(hash)) => Some(BlockRef {
            height: BlockHeight(
                parse_quantity_u64(number).map_err(|message| {
                    invalid_rpc_response("eth_getTransactionReceipt", message)
                })?,
            ),
            hash: BlockHash(
                parse_fixed_data::<32>(hash, "receipt block hash")
                    .map_err(|message| invalid_rpc_response("eth_getTransactionReceipt", message))?
                    .to_vec(),
            ),
            parent_hash: None,
            timestamp: None,
        }),
        _ => {
            return Err(invalid_rpc_response(
                "eth_getTransactionReceipt",
                "receipt block number and hash must both be present or null",
            ));
        }
    };
    let succeeded = match optional_string(object.get("status"), "receipt status")? {
        None => None,
        Some(status) => match parse_quantity_u64(status)
            .map_err(|message| invalid_rpc_response("eth_getTransactionReceipt", message))?
        {
            0 => Some(false),
            1 => Some(true),
            _ => {
                return Err(invalid_rpc_response(
                    "eth_getTransactionReceipt",
                    "receipt status must be 0x0 or 0x1",
                ));
            }
        },
    };

    Ok(EthereumReceipt {
        id: transaction_id,
        included_in,
        succeeded,
        confirmations: 0,
    })
}

fn optional_string<'a>(
    value: Option<&'a Value>,
    field: &'static str,
) -> Result<Option<&'a str>, SourceError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid_rpc_response(
            "eth_getTransactionReceipt",
            format!("{field} is not a string or null"),
        )),
    }
}

fn block_parameter(at: Option<BlockRef>) -> Result<Value, SourceError> {
    let Some(block) = at else {
        return Ok(Value::String("pending".to_owned()));
    };
    if block.hash.0.len() != 32 {
        return Err(source_error(
            "Ethereum balance block hash must contain exactly 32 bytes",
            false,
        ));
    }
    Ok(json!({
        "blockHash": data_hex(&block.hash.0),
        "requireCanonical": true,
    }))
}

fn erc20_balance_of_call(address: &EthereumAddress) -> String {
    let mut call = [0_u8; 36];
    call[..4].copy_from_slice(&ERC20_BALANCE_OF_SELECTOR);
    call[16..].copy_from_slice(&address.0);
    data_hex(&call)
}

fn parse_quantity_u64(value: &str) -> Result<u64, &'static str> {
    let digits = quantity_digits(value)?;
    u64::from_str_radix(digits, 16).map_err(|_| "hex quantity exceeds u64")
}

fn parse_quantity_wei(value: &str) -> Result<Wei, &'static str> {
    let digits = quantity_digits(value)?;
    if digits.len() > 64 {
        return Err("hex quantity exceeds 256 bits");
    }
    decode_hex_right_aligned::<32>(digits).map(Wei)
}

fn quantity_digits(value: &str) -> Result<&str, &'static str> {
    let digits = value
        .strip_prefix("0x")
        .ok_or("hex quantity has no 0x prefix")?;
    if digits.is_empty() {
        return Err("hex quantity is empty");
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return Err("hex quantity contains a leading zero");
    }
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("hex quantity contains invalid data");
    }
    Ok(digits)
}

fn parse_abi_word(value: &str) -> Result<[u8; 32], &'static str> {
    parse_fixed_data(value, "ERC-20 balance result")
}

fn parse_fixed_data<const N: usize>(
    value: &str,
    _field: &'static str,
) -> Result<[u8; N], &'static str> {
    let digits = value
        .strip_prefix("0x")
        .ok_or("hex data has no 0x prefix")?;
    if digits.len() != N * 2 {
        return Err("hex data has an invalid length");
    }
    decode_hex_right_aligned(digits)
}

fn decode_hex_right_aligned<const N: usize>(digits: &str) -> Result<[u8; N], &'static str> {
    if digits.len() > N * 2 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("hex data contains invalid data");
    }
    let mut decoded = [0_u8; N];
    let byte_offset = N - digits.len().div_ceil(2);
    let mut output = byte_offset;
    let mut input = 0;
    if digits.len() % 2 == 1 {
        decoded[output] =
            hex_nibble(digits.as_bytes()[0]).ok_or("hex data contains invalid data")?;
        output += 1;
        input = 1;
    }
    while input < digits.len() {
        let high = hex_nibble(digits.as_bytes()[input]).ok_or("hex data contains invalid data")?;
        let low =
            hex_nibble(digits.as_bytes()[input + 1]).ok_or("hex data contains invalid data")?;
        decoded[output] = (high << 4) | low;
        output += 1;
        input += 2;
    }
    Ok(decoded)
}

fn parse_transaction_id(
    value: &str,
    method: &'static str,
) -> Result<EthereumTransactionId, SourceError> {
    parse_fixed_data::<32>(value, "transaction hash")
        .map(EthereumTransactionId)
        .map_err(|message| invalid_rpc_response(method, message))
}

fn gas_limit_with_margin(estimated: u64, margin_basis_points: u32) -> Result<u64, SourceError> {
    let numerator = u128::from(estimated)
        .checked_mul(u128::from(margin_basis_points))
        .ok_or_else(|| invalid_rpc_response("eth_estimateGas", "gas margin overflowed"))?;
    let margin = numerator
        .checked_add(u128::from(BASIS_POINTS_DENOMINATOR - 1))
        .map(|value| value / u128::from(BASIS_POINTS_DENOMINATOR))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| invalid_rpc_response("eth_estimateGas", "gas margin exceeds u64"))?;
    estimated
        .checked_add(margin)
        .ok_or_else(|| invalid_rpc_response("eth_estimateGas", "gas limit with margin exceeds u64"))
}

fn wei_quantity(value: &Wei) -> String {
    let Some(first_non_zero) = value.0.iter().position(|byte| *byte != 0) else {
        return "0x0".to_owned();
    };
    let bytes = &value.0[first_non_zero..];
    let mut encoded = String::with_capacity(2 + bytes.len() * 2);
    encoded.push_str("0x");
    if bytes[0] < 16 {
        encoded.push(hex_digit(bytes[0]));
    } else {
        encoded.push(hex_digit(bytes[0] >> 4));
        encoded.push(hex_digit(bytes[0] & 0x0f));
    }
    for byte in &bytes[1..] {
        encoded.push(hex_digit(byte >> 4));
        encoded.push(hex_digit(byte & 0x0f));
    }
    encoded
}

fn address_hex(address: &EthereumAddress) -> String {
    data_hex(&address.0)
}

fn transaction_id_hex(id: &EthereumTransactionId) -> String {
    data_hex(&id.0)
}

fn data_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(2 + bytes.len() * 2);
    encoded.push_str("0x");
    for byte in bytes {
        encoded.push(hex_digit(byte >> 4));
        encoded.push(hex_digit(byte & 0x0f));
    }
    encoded
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_digit(nibble: u8) -> char {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    char::from(HEX[usize::from(nibble & 0x0f)])
}

fn map_json_rpc_error(error: JsonRpcError) -> SourceError {
    source_error(error.to_string(), error.is_retryable())
}

fn invalid_rpc_response(method: &'static str, message: impl fmt::Display) -> SourceError {
    source_error(
        format!("Ethereum RPC {method} returned an invalid response: {message}"),
        false,
    )
}

fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        num::NonZeroU32,
        sync::{Arc, Mutex},
    };

    use futures_executor::block_on;
    use json_rpc::JsonRpcResponse;

    use super::*;

    #[derive(Clone)]
    struct ScriptedClient {
        state: Arc<Mutex<ScriptState>>,
    }

    struct ScriptState {
        replies: VecDeque<ExpectedReply>,
        requests: Vec<(String, Value)>,
    }

    struct ExpectedReply {
        method: &'static str,
        result: Result<RawJson, JsonRpcFailure>,
    }

    impl ScriptedClient {
        fn new(replies: Vec<ExpectedReply>) -> Self {
            Self {
                state: Arc::new(Mutex::new(ScriptState {
                    replies: replies.into(),
                    requests: Vec::new(),
                })),
            }
        }

        fn requests(&self) -> Vec<(String, Value)> {
            self.state
                .lock()
                .expect("script lock must be healthy")
                .requests
                .clone()
        }
    }

    impl JsonRpcClient for ScriptedClient {
        fn request<'a>(
            &'a self,
            request: JsonRpcRequest,
        ) -> crate::BoxFuture<'a, Result<JsonRpcResponse, JsonRpcError>> {
            let response = {
                let mut state = self.state.lock().expect("script lock must be healthy");
                let expected = state
                    .replies
                    .pop_front()
                    .expect("adapter made more requests than scripted");
                assert_eq!(request.method, expected.method);
                let params = request
                    .params
                    .deserialize::<Value>()
                    .expect("adapter params must be valid JSON");
                state.requests.push((request.method, params));
                JsonRpcResponse {
                    id: request.id,
                    result: expected.result,
                }
            };
            Box::pin(async move { Ok(response) })
        }

        fn batch<'a>(
            &'a self,
            _requests: Vec<JsonRpcRequest>,
        ) -> crate::BoxFuture<'a, Result<Vec<JsonRpcResponse>, JsonRpcError>> {
            Box::pin(async { panic!("wallet RPC adapter does not issue JSON-RPC batches") })
        }
    }

    fn success(method: &'static str, value: Value) -> ExpectedReply {
        ExpectedReply {
            method,
            result: Ok(
                RawJson::from_serializable(&value).expect("scripted RPC result must serialize")
            ),
        }
    }

    fn failure(method: &'static str, code: i64, message: &str) -> ExpectedReply {
        ExpectedReply {
            method,
            result: Err(JsonRpcFailure {
                code,
                message: message.to_owned(),
                data: None,
            }),
        }
    }

    fn limits() -> EthereumRpcLimits {
        EthereumRpcLimits::new(
            1024,
            2_000,
            1_000_000,
            Wei::from_u128(1_000_000_000_000),
            Wei::from_u128(100_000_000_000),
            Wei::from_u128(1_000_000_000_000_000_000),
        )
        .expect("test RPC limits must be valid")
    }

    fn rpc(client: ScriptedClient) -> EthereumHttpRpc<ScriptedClient> {
        EthereumHttpRpc::with_client(client, 31_337, limits())
            .expect("test RPC configuration must be valid")
    }

    fn transfer(data: Vec<u8>) -> EthereumTransferRequest {
        EthereumTransferRequest {
            key: signer::KeyLocator::Identifier("test-key".to_owned()),
            signing_operation_id: signer::OperationId::new("rpc-test-sign")
                .expect("test signing operation ID must be valid"),
            from: EthereumAddress([0x11; 20]),
            to: Some(EthereumAddress([0x22; 20])),
            value: Wei::from_u128(7),
            data,
        }
    }

    #[test]
    fn reads_native_and_erc20_balances_with_exact_block_behavior() {
        let client = ScriptedClient::new(vec![
            success("eth_getBalance", json!("0x2a")),
            success("eth_call", json!(format!("0x{}", "00".repeat(31) + "2b"))),
        ]);
        let rpc = rpc(client.clone());
        let block = BlockRef {
            height: BlockHeight(9),
            hash: BlockHash(vec![0xaa; 32]),
            parent_hash: None,
            timestamp: None,
        };

        assert_eq!(
            block_on(rpc.balance(EthereumAddress([0x11; 20]), &EthereumAsset::Native, None))
                .expect("native balance must parse"),
            Wei::from_u128(42)
        );
        assert_eq!(
            block_on(rpc.balance(
                EthereumAddress([0x11; 20]),
                &EthereumAsset::Erc20(EthereumAddress([0x33; 20])),
                Some(block),
            ))
            .expect("token balance must parse"),
            Wei::from_u128(43)
        );

        let requests = client.requests();
        assert_eq!(requests[0].1[1], json!("pending"));
        assert_eq!(
            requests[1].1[1],
            json!({
                "blockHash": format!("0x{}", "aa".repeat(32)),
                "requireCanonical": true,
            })
        );
        assert_eq!(
            requests[1].1[0]["data"],
            json!(format!("0x70a08231{}{}", "00".repeat(12), "11".repeat(20)))
        );
    }

    #[test]
    fn builds_checked_eip1559_context_and_preserves_input() {
        let client = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_getTransactionCount", json!("0x4")),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            success(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
        ]);
        let rpc = rpc(client.clone());

        let context = block_on(rpc.build_context(&transfer(vec![0xde, 0xad])))
            .expect("bounded build context must succeed");

        assert_eq!(context.chain_id, 31_337);
        assert_eq!(context.nonce, 4);
        assert_eq!(context.gas_limit, 25_200);
        assert_eq!(
            context.max_priority_fee_per_gas,
            Wei::from_u128(1_000_000_000)
        );
        assert_eq!(context.max_fee_per_gas, Wei::from_u128(5_000_000_000));
        let requests = client.requests();
        assert_eq!(requests[2].1[0]["data"], json!("0xdead"));
        assert_eq!(requests[2].1[0]["value"], json!("0x7"));
    }

    #[test]
    fn wrong_chain_id_fails_before_transaction_queries() {
        let client = ScriptedClient::new(vec![success("eth_chainId", json!("0x1"))]);
        let rpc = rpc(client.clone());

        let error = block_on(rpc.build_context(&transfer(Vec::new())))
            .expect_err("wrong chain identity must fail closed");

        assert!(!error.retryable);
        assert_eq!(client.requests().len(), 1);
    }

    #[test]
    fn malformed_quantities_and_abi_results_are_rejected() {
        let client = ScriptedClient::new(vec![
            success("eth_getBalance", json!("0x00")),
            success("eth_call", json!("0x01")),
        ]);
        let rpc = rpc(client);

        assert!(
            block_on(rpc.balance(EthereumAddress([1; 20]), &EthereumAsset::Native, None))
                .expect_err("non-canonical quantity must fail")
                .message
                .contains("leading zero")
        );
        assert!(
            block_on(rpc.balance(
                EthereumAddress([1; 20]),
                &EthereumAsset::Erc20(EthereumAddress([2; 20])),
                None,
            ))
            .expect_err("short ABI word must fail")
            .message
            .contains("invalid length")
        );
    }

    #[test]
    fn estimate_gas_revert_is_terminal_and_provider_message_is_not_exposed() {
        let client = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_getTransactionCount", json!("0x0")),
            failure(
                "eth_estimateGas",
                -32_000,
                "execution reverted: Bearer secret https://user:password@example.invalid",
            ),
        ]);
        let rpc = rpc(client);

        let error = block_on(rpc.build_context(&transfer(Vec::new())))
            .expect_err("a deterministic revert must fail");

        assert!(!error.retryable);
        assert!(error.message.contains("-32000"));
        for secret in ["Bearer secret", "password", "example.invalid"] {
            assert!(!error.message.contains(secret));
        }
    }

    #[test]
    fn reads_and_validates_receipt_with_confirmations() {
        let id = EthereumTransactionId([0xcc; 32]);
        let client = ScriptedClient::new(vec![
            success(
                "eth_getTransactionReceipt",
                json!({
                    "transactionHash": transaction_id_hex(&id),
                    "blockNumber": "0xa",
                    "blockHash": format!("0x{}", "aa".repeat(32)),
                    "status": "0x1",
                }),
            ),
            success("eth_blockNumber", json!("0xc")),
        ]);
        let rpc = rpc(client);

        let receipt = block_on(rpc.receipt(&id))
            .expect("receipt request must succeed")
            .expect("scripted receipt must exist");

        assert_eq!(receipt.id, id);
        assert_eq!(receipt.confirmations, 3);
        assert_eq!(receipt.succeeded, Some(true));
        assert_eq!(
            receipt
                .included_in
                .expect("receipt must be included")
                .height,
            BlockHeight(10)
        );
    }

    #[test]
    fn exact_envelope_broadcast_rejects_a_mismatched_provider_hash() {
        let envelope = vec![0x02, 0x01, 0x02, 0x03];
        let id = EthereumTransactionId(keccak256(&envelope).0);
        let client = ScriptedClient::new(vec![success(
            "eth_sendRawTransaction",
            json!(format!("0x{}", "dd".repeat(32))),
        )]);
        let rpc = rpc(client.clone());

        let error = block_on(rpc.broadcast(EthereumSignedTransaction {
            id,
            envelope: envelope.clone(),
        }))
        .expect_err("provider hash mismatch must fail");

        assert!(!error.retryable);
        assert_eq!(client.requests()[0].1, json!([data_hex(&envelope)]));
    }

    #[test]
    fn already_known_succeeds_only_after_matching_hash_lookup() {
        let envelope = vec![0x02, 0xaa, 0xbb];
        let id = EthereumTransactionId(keccak256(&envelope).0);
        let matching = ScriptedClient::new(vec![
            failure("eth_sendRawTransaction", -32_000, "already known"),
            success(
                "eth_getTransactionByHash",
                json!({"hash": transaction_id_hex(&id)}),
            ),
        ]);
        let matching_rpc = rpc(matching);

        assert_eq!(
            block_on(matching_rpc.broadcast(EthereumSignedTransaction {
                id: id.clone(),
                envelope: envelope.clone(),
            }))
            .expect("matching already-known transaction must be idempotent"),
            id
        );

        let mismatched = ScriptedClient::new(vec![
            failure("eth_sendRawTransaction", -32_000, "already known"),
            success(
                "eth_getTransactionByHash",
                json!({"hash": format!("0x{}", "ee".repeat(32))}),
            ),
        ]);
        let mismatched_rpc = rpc(mismatched);
        let error = block_on(mismatched_rpc.broadcast(EthereumSignedTransaction { id, envelope }))
            .expect_err("different known hash must not be accepted");
        assert!(!error.retryable);
    }

    #[test]
    fn configured_input_and_fee_ceilings_fail_closed() {
        let no_calls = ScriptedClient::new(Vec::new());
        let bounded_rpc = rpc(no_calls.clone());
        let error = block_on(bounded_rpc.build_context(&transfer(vec![0; 1025])))
            .expect_err("oversized input must fail before RPC");
        assert!(!error.retryable);
        assert!(no_calls.requests().is_empty());

        let high_fee = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_getTransactionCount", json!("0x0")),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x1")),
            success(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x100000000000"}),
            ),
        ]);
        let error = block_on(rpc(high_fee).build_context(&transfer(Vec::new())))
            .expect_err("fee above the configured ceiling must fail");
        assert!(!error.retryable);
        assert!(error.message.contains("configured ceiling"));
    }

    #[test]
    fn debug_and_build_errors_redact_endpoint_and_authorization() {
        let retry = RetryPolicy::new(
            NonZeroU32::new(2).expect("two is non-zero"),
            Duration::from_millis(1),
            Duration::from_millis(2),
        )
        .expect("test retry policy must be valid");
        let config = EthereumHttpRpcConfig::new(
            "https://user:password@example.invalid/rpc?key=query-secret",
            31_337,
            Duration::from_secs(1),
            1024,
            retry,
            limits(),
        )
        .expect("test HTTP RPC configuration must be valid")
        .with_header("authorization", "Bearer header-secret");
        let config_debug = format!("{config:?}");
        let rpc = EthereumHttpRpc::new(config).expect("test HTTP adapter must construct");
        let rpc_debug = format!("{rpc:?}");

        for output in [config_debug, rpc_debug] {
            for secret in ["password", "query-secret", "header-secret"] {
                assert!(!output.contains(secret));
            }
            assert!(output.contains("[REDACTED]"));
        }

        let invalid = EthereumHttpRpcConfig::new(
            "https://user:invalid-secret@[",
            31_337,
            Duration::from_secs(1),
            1024,
            RetryPolicy::no_retry(),
            limits(),
        )
        .expect("syntax validation is delegated to the HTTP transport");
        let error = EthereumHttpRpc::new(invalid).expect_err("invalid endpoint must fail");
        assert_eq!(error.kind, EthereumHttpRpcBuildErrorKind::HttpTransport);
        assert!(!format!("{error:?}").contains("invalid-secret"));
        assert!(!error.to_string().contains("invalid-secret"));
    }
}
