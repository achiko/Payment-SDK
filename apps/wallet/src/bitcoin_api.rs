//! Authenticated-transport-ready, stateless Bitcoin Wallet HTTP adapter.
//!
//! Authentication, body limits, health, and transport policy are applied by
//! `http_support::service_router`, exactly as for the Ethereum adapter. This
//! module owns only strict Bitcoin wire decoding and delegation to injected,
//! object-safe operations; it owns no UTXO reservations or workflow state.

use std::{collections::BTreeSet, future::Future, pin::Pin, sync::Arc};

use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use chain_bitcoin::{
    BitcoinAddress, BitcoinAddressKind, BitcoinCollectionAttribution, BitcoinCollectionRequirement,
    BitcoinGenerateAddress, BitcoinNetwork, BitcoinOutput, BitcoinReceipt,
    BitcoinSignedTransaction, BitcoinTransactionId, BitcoinUtxo, Satoshi, SatoshisPerKvb,
    format_bitcoin_block_hash,
};
use chain_contract::{Balance, ChainError, ChainErrorKind, GeneratedAddress};
use serde::{Deserialize, Serialize};
use signer::{ChildIndex, DerivationPath, KeyLocator, OperationId};
use uuid::Uuid;

pub const ADDRESS_PATH: &str = "/v1/bitcoin/addresses";
pub const BALANCE_PATH: &str = "/v1/bitcoin/balances";
pub const SIGN_TRANSFER_PATH: &str = "/v1/bitcoin/transfers/sign";
pub const COLLECTION_REQUIREMENTS_PATH: &str = "/v1/bitcoin/collections/requirements";
pub const SIGN_COLLECTION_PATH: &str = "/v1/bitcoin/collections/sign";
pub const BROADCAST_PATH: &str = "/v1/bitcoin/transactions/broadcast";
pub const RECEIPT_PATH: &str = "/v1/bitcoin/receipts";

pub type OperationFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One exact, already-reserved Bitcoin previous output supplied by PS.
///
/// The production operation must construct a chain-native [`chain_bitcoin::BitcoinUtxo`]
/// through the validating chain constructor. In particular, it must not trust
/// an HTTP caller to choose a satisfaction weight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinExactInput {
    pub transaction_id: BitcoinTransactionId,
    pub output_index: u32,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
    pub address: BitcoinAddress,
    pub key: KeyLocator,
}

impl BitcoinExactInput {
    /// Converts this boundary selection into the chain-native signing model,
    /// deriving satisfaction weight only after script/address verification.
    pub fn to_chain_utxo(&self, network: BitcoinNetwork) -> Result<BitcoinUtxo, ChainError> {
        BitcoinUtxo::from_exact_selection(
            network,
            &self.address,
            self.key.clone(),
            self.transaction_id,
            self.output_index,
            self.value,
            self.script_pubkey.clone(),
        )
    }
}

/// Exact-input transfer request accepted by the production operation boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinTransferSignRequest {
    pub signing_operation_id: OperationId,
    pub inputs: Vec<BitcoinExactInput>,
    pub recipients: Vec<BitcoinOutput>,
    pub change_address: BitcoinAddress,
    pub fee_rate: SatoshisPerKvb,
}

/// Factual collection-prerequisite query. IX/PS workflow state is not included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinCollectionRequirementsRequest {
    pub sources: Vec<BitcoinAddress>,
}

/// Exact inputs attributed to one deposit address in a batch collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinCollectionSourceInputs {
    pub address: BitcoinAddress,
    pub key: KeyLocator,
    pub inputs: Vec<BitcoinExactInput>,
}

/// Exact-input batch collection request accepted by the operation boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinCollectionSignRequest {
    pub signing_operation_id: OperationId,
    pub sources: Vec<BitcoinCollectionSourceInputs>,
    pub destination: BitcoinAddress,
    pub fee_rate: SatoshisPerKvb,
}

/// Signed transfer plus the review data PS needs before persisting exact bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinPreparedTransaction {
    pub transaction: BitcoinSignedTransaction,
    pub inputs: Vec<BitcoinExactInput>,
    pub outputs: Vec<BitcoinOutput>,
    pub fee: Satoshi,
    pub virtual_size: u64,
}

/// Signed batch collection plus gross input attribution per source address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinPreparedCollection {
    pub prepared: BitcoinPreparedTransaction,
    pub attribution: Vec<BitcoinCollectionAttribution>,
}

/// Object-safe stateless Bitcoin operations used by the HTTP adapter.
///
/// Implementations may use injected Core, IX, and custody clients, but must not
/// persist reservations, deposits, retries, or collection workflow state.
pub trait BitcoinWalletOperations: Send + Sync {
    fn generate_address<'a>(
        &'a self,
        request: BitcoinGenerateAddress,
    ) -> OperationFuture<'a, Result<GeneratedAddress<BitcoinAddress>, ChainError>>;

    fn balance<'a>(
        &'a self,
        address: BitcoinAddress,
    ) -> OperationFuture<'a, Result<Balance<Satoshi>, ChainError>>;

    fn sign_transfer<'a>(
        &'a self,
        request: BitcoinTransferSignRequest,
    ) -> OperationFuture<'a, Result<BitcoinPreparedTransaction, ChainError>>;

    fn collection_requirements<'a>(
        &'a self,
        request: BitcoinCollectionRequirementsRequest,
    ) -> OperationFuture<'a, Result<Vec<BitcoinCollectionRequirement>, ChainError>>;

    fn sign_collection<'a>(
        &'a self,
        request: BitcoinCollectionSignRequest,
    ) -> OperationFuture<'a, Result<BitcoinPreparedCollection, ChainError>>;

    fn broadcast<'a>(
        &'a self,
        transaction: BitcoinSignedTransaction,
    ) -> OperationFuture<'a, Result<BitcoinTransactionId, ChainError>>;

    fn receipt<'a>(
        &'a self,
        transaction_id: BitcoinTransactionId,
    ) -> OperationFuture<'a, Result<Option<BitcoinReceipt>, ChainError>>;
}

#[derive(Clone)]
struct ApiState {
    network: BitcoinNetwork,
    operations: Arc<dyn BitcoinWalletOperations>,
}

pub fn router(network: BitcoinNetwork, operations: Arc<dyn BitcoinWalletOperations>) -> Router {
    Router::new()
        .route(ADDRESS_PATH, post(generate_address))
        .route(BALANCE_PATH, post(balance))
        .route(SIGN_TRANSFER_PATH, post(sign_transfer))
        .route(COLLECTION_REQUIREMENTS_PATH, post(collection_requirements))
        .route(SIGN_COLLECTION_PATH, post(sign_collection))
        .route(BROADCAST_PATH, post(broadcast))
        .route(RECEIPT_PATH, post(receipt))
        .with_state(ApiState {
            network,
            operations,
        })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateAddressRequest {
    operation_id: String,
    address_kind: AddressKindDto,
    key_purpose: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AddressKindDto {
    P2wpkh,
    P2tr,
}

impl From<AddressKindDto> for BitcoinAddressKind {
    fn from(value: AddressKindDto) -> Self {
        match value {
            AddressKindDto::P2wpkh => Self::SegwitV0,
            AddressKindDto::P2tr => Self::Taproot,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct GenerateAddressResponse {
    address: String,
    key_locator: KeyLocatorDto,
}

async fn generate_address(
    State(state): State<ApiState>,
    payload: Result<Json<GenerateAddressRequest>, JsonRejection>,
) -> ApiResult<Json<GenerateAddressResponse>> {
    let request = json_payload(payload)?;
    validate_key_purpose(&request.key_purpose)?;
    let generated = state
        .operations
        .generate_address(BitcoinGenerateAddress::new(
            state.network,
            request.address_kind.into(),
            operation_id(&request.operation_id)?,
            request.key_purpose,
        ))
        .await
        .map_err(ApiError::from_chain)?;
    canonical_address(&generated.address.0, state.network).map_err(|_| {
        ApiError::internal(
            "Bitcoin address generation returned an invalid configured-network address",
        )
    })?;
    Ok(Json(GenerateAddressResponse {
        address: generated.address.0,
        key_locator: KeyLocatorDto::from(generated.key),
    }))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BalanceRequest {
    address: String,
}

#[derive(Clone, Debug, Serialize)]
struct BalanceResponse {
    confirmed_satoshis: String,
    pending_satoshis: String,
    spendable_satoshis: String,
}

async fn balance(
    State(state): State<ApiState>,
    payload: Result<Json<BalanceRequest>, JsonRejection>,
) -> ApiResult<Json<BalanceResponse>> {
    let request = json_payload(payload)?;
    let result = state
        .operations
        .balance(canonical_address(&request.address, state.network)?)
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(BalanceResponse {
        confirmed_satoshis: result.confirmed.0.to_string(),
        pending_satoshis: result.pending.0.to_string(),
        spendable_satoshis: result.spendable.0.to_string(),
    }))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignTransferRequest {
    operation_id: String,
    inputs: Vec<ExactInputDto>,
    recipients: Vec<OutputDto>,
    change_address: String,
    fee_rate_satoshis_per_kvb: String,
}

async fn sign_transfer(
    State(state): State<ApiState>,
    payload: Result<Json<SignTransferRequest>, JsonRejection>,
) -> ApiResult<Json<PreparedTransactionResponse>> {
    let request = json_payload(payload)?;
    if request.inputs.is_empty() || request.recipients.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_transaction",
            "Bitcoin transfer requires at least one exact input and one recipient",
        ));
    }
    let inputs = request
        .inputs
        .into_iter()
        .map(|input| input.into_exact_input(state.network))
        .collect::<ApiResult<Vec<_>>>()?;
    validate_unique_outpoints(inputs.iter())?;
    let recipients = request
        .recipients
        .into_iter()
        .map(|output| output.into_output(state.network))
        .collect::<ApiResult<Vec<_>>>()?;
    let prepared = state
        .operations
        .sign_transfer(BitcoinTransferSignRequest {
            signing_operation_id: operation_id(&request.operation_id)?,
            inputs,
            recipients,
            change_address: canonical_address(&request.change_address, state.network)?,
            fee_rate: fee_rate(&request.fee_rate_satoshis_per_kvb)?,
        })
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(PreparedTransactionResponse::from(prepared)))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionRequirementsHttpRequest {
    sources: Vec<CollectionRequirementSourceDto>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionRequirementSourceDto {
    address: String,
}

#[derive(Clone, Debug, Serialize)]
struct CollectionRequirementsResponse {
    requirements: Vec<CollectionRequirementDto>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CollectionRequirementDto {
    NoSpendableOutputs { address: String },
}

async fn collection_requirements(
    State(state): State<ApiState>,
    payload: Result<Json<CollectionRequirementsHttpRequest>, JsonRejection>,
) -> ApiResult<Json<CollectionRequirementsResponse>> {
    let request = json_payload(payload)?;
    if request.sources.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_collection",
            "Bitcoin collection requirements need at least one source address",
        ));
    }
    let sources = request
        .sources
        .into_iter()
        .map(|source| canonical_address(&source.address, state.network))
        .collect::<ApiResult<Vec<_>>>()?;
    validate_unique_addresses(sources.iter())?;
    let requirements = state
        .operations
        .collection_requirements(BitcoinCollectionRequirementsRequest { sources })
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(CollectionRequirementsResponse {
        requirements: requirements
            .into_iter()
            .map(|requirement| match requirement {
                BitcoinCollectionRequirement::NoSpendableOutputs { address } => {
                    CollectionRequirementDto::NoSpendableOutputs { address: address.0 }
                }
            })
            .collect(),
    }))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignCollectionRequest {
    operation_id: String,
    sources: Vec<CollectionSourceDto>,
    destination: String,
    fee_rate_satoshis_per_kvb: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionSourceDto {
    address: String,
    key_locator: KeyLocatorDto,
    inputs: Vec<CollectionInputDto>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionInputDto {
    transaction_id: String,
    output_index: String,
    value_satoshis: String,
    script_pubkey: String,
}

async fn sign_collection(
    State(state): State<ApiState>,
    payload: Result<Json<SignCollectionRequest>, JsonRejection>,
) -> ApiResult<Json<PreparedCollectionResponse>> {
    let request = json_payload(payload)?;
    if request.sources.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_collection",
            "Bitcoin collection requires at least one source",
        ));
    }
    let mut sources = Vec::with_capacity(request.sources.len());
    for source in request.sources {
        if source.inputs.is_empty() {
            return Err(ApiError::bad_request(
                "invalid_collection",
                "every Bitcoin collection source must supply at least one exact input",
            ));
        }
        let address = canonical_address(&source.address, state.network)?;
        let key = source.key_locator.into_locator()?;
        let inputs = source
            .inputs
            .into_iter()
            .map(|input| input.into_exact_input(state.network, &address, &key))
            .collect::<ApiResult<Vec<_>>>()?;
        sources.push(BitcoinCollectionSourceInputs {
            address,
            key,
            inputs,
        });
    }
    validate_unique_addresses(sources.iter().map(|source| &source.address))?;
    validate_unique_outpoints(sources.iter().flat_map(|source| source.inputs.iter()))?;
    let prepared = state
        .operations
        .sign_collection(BitcoinCollectionSignRequest {
            signing_operation_id: operation_id(&request.operation_id)?,
            sources,
            destination: canonical_address(&request.destination, state.network)?,
            fee_rate: fee_rate(&request.fee_rate_satoshis_per_kvb)?,
        })
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(PreparedCollectionResponse::from(prepared)))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactInputDto {
    transaction_id: String,
    output_index: String,
    value_satoshis: String,
    script_pubkey: String,
    address: String,
    key_locator: KeyLocatorDto,
}

impl ExactInputDto {
    fn into_exact_input(self, network: BitcoinNetwork) -> ApiResult<BitcoinExactInput> {
        let address = canonical_address(&self.address, network)?;
        let key = self.key_locator.into_locator()?;
        CollectionInputDto {
            transaction_id: self.transaction_id,
            output_index: self.output_index,
            value_satoshis: self.value_satoshis,
            script_pubkey: self.script_pubkey,
        }
        .into_exact_input(network, &address, &key)
    }
}

impl CollectionInputDto {
    fn into_exact_input(
        self,
        network: BitcoinNetwork,
        address: &BitcoinAddress,
        key: &KeyLocator,
    ) -> ApiResult<BitcoinExactInput> {
        let input = BitcoinExactInput {
            transaction_id: transaction_id(&self.transaction_id)?,
            output_index: decimal_u32(&self.output_index, "output_index")?,
            value: Satoshi(nonzero_decimal_u64(&self.value_satoshis, "value_satoshis")?),
            script_pubkey: canonical_hex(&self.script_pubkey, "script_pubkey")?,
            address: address.clone(),
            key: key.clone(),
        };
        input.to_chain_utxo(network).map_err(|_| {
            ApiError::bad_request(
                "invalid_input",
                "Bitcoin input script_pubkey must match a supported P2WPKH or P2TR address",
            )
        })?;
        Ok(input)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputDto {
    address: String,
    value_satoshis: String,
}

impl OutputDto {
    fn into_output(self, network: BitcoinNetwork) -> ApiResult<BitcoinOutput> {
        Ok(BitcoinOutput {
            address: canonical_address(&self.address, network)?,
            value: Satoshi(nonzero_decimal_u64(&self.value_satoshis, "value_satoshis")?),
        })
    }
}

#[derive(Clone, Serialize)]
struct PreparedTransactionResponse {
    transaction_id: String,
    raw_transaction: String,
    selected_outpoints: Vec<OutpointDto>,
    outputs: Vec<OutputResponseDto>,
    fee_satoshis: String,
    virtual_size: String,
}

impl std::fmt::Debug for PreparedTransactionResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedTransactionResponse")
            .field("transaction_id", &self.transaction_id)
            .field("raw_transaction", &"[REDACTED]")
            .field("selected_outpoints", &self.selected_outpoints)
            .field("outputs", &self.outputs)
            .field("fee_satoshis", &self.fee_satoshis)
            .field("virtual_size", &self.virtual_size)
            .finish()
    }
}

impl From<BitcoinPreparedTransaction> for PreparedTransactionResponse {
    fn from(prepared: BitcoinPreparedTransaction) -> Self {
        Self {
            transaction_id: prepared.transaction.id().to_string(),
            raw_transaction: hex_prefixed(prepared.transaction.consensus_bytes()),
            selected_outpoints: prepared
                .inputs
                .into_iter()
                .map(|input| OutpointDto {
                    transaction_id: input.transaction_id.to_string(),
                    output_index: input.output_index.to_string(),
                })
                .collect(),
            outputs: prepared
                .outputs
                .into_iter()
                .map(|output| OutputResponseDto {
                    address: output.address.0,
                    value_satoshis: output.value.0.to_string(),
                })
                .collect(),
            fee_satoshis: prepared.fee.0.to_string(),
            virtual_size: prepared.virtual_size.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct OutpointDto {
    transaction_id: String,
    output_index: String,
}

#[derive(Clone, Debug, Serialize)]
struct OutputResponseDto {
    address: String,
    value_satoshis: String,
}

#[derive(Clone, Serialize)]
struct PreparedCollectionResponse {
    #[serde(flatten)]
    prepared: PreparedTransactionResponse,
    attribution: Vec<CollectionAttributionDto>,
}

impl std::fmt::Debug for PreparedCollectionResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedCollectionResponse")
            .field("prepared", &self.prepared)
            .field("attribution", &self.attribution)
            .finish()
    }
}

impl From<BitcoinPreparedCollection> for PreparedCollectionResponse {
    fn from(collection: BitcoinPreparedCollection) -> Self {
        Self {
            prepared: PreparedTransactionResponse::from(collection.prepared),
            attribution: collection
                .attribution
                .into_iter()
                .map(|item| CollectionAttributionDto {
                    address: item.address.0,
                    gross_input_satoshis: item.gross_input.0.to_string(),
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct CollectionAttributionDto {
    address: String,
    gross_input_satoshis: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BroadcastRequest {
    expected_transaction_id: String,
    raw_transaction: String,
}

impl std::fmt::Debug for BroadcastRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BroadcastRequest")
            .field("expected_transaction_id", &self.expected_transaction_id)
            .field("raw_transaction", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Serialize)]
struct BroadcastResponse {
    transaction_id: String,
}

async fn broadcast(
    State(state): State<ApiState>,
    payload: Result<Json<BroadcastRequest>, JsonRejection>,
) -> ApiResult<Json<BroadcastResponse>> {
    let request = json_payload(payload)?;
    let expected_id = transaction_id(&request.expected_transaction_id)?;
    let raw_transaction = canonical_hex(&request.raw_transaction, "raw_transaction")?;
    let transaction = BitcoinSignedTransaction::from_consensus_bytes(expected_id, raw_transaction)
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_signed_transaction",
                "raw transaction does not match the expected Bitcoin transaction ID",
            )
        })?;
    let transaction_id = state
        .operations
        .broadcast(transaction)
        .await
        .map_err(ApiError::from_chain)?;
    if transaction_id != expected_id {
        return Err(ApiError::internal(
            "Bitcoin broadcast returned a different transaction ID",
        ));
    }
    Ok(Json(BroadcastResponse {
        transaction_id: transaction_id.to_string(),
    }))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptRequest {
    transaction_id: String,
}

#[derive(Clone, Debug, Serialize)]
struct ReceiptResponse {
    transaction_id: String,
    receipt: Option<ReceiptDto>,
}

#[derive(Clone, Debug, Serialize)]
struct ReceiptDto {
    included_in: Option<BlockRefDto>,
    confirmations: u64,
    replaced_by: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct BlockRefDto {
    height: u64,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<u64>,
}

async fn receipt(
    State(state): State<ApiState>,
    payload: Result<Json<ReceiptRequest>, JsonRejection>,
) -> ApiResult<Json<ReceiptResponse>> {
    let request = json_payload(payload)?;
    let transaction_id = transaction_id(&request.transaction_id)?;
    let receipt = state
        .operations
        .receipt(transaction_id)
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(ReceiptResponse {
        transaction_id: transaction_id.to_string(),
        receipt: receipt.map(receipt_dto).transpose()?,
    }))
}

fn receipt_dto(receipt: BitcoinReceipt) -> ApiResult<ReceiptDto> {
    Ok(ReceiptDto {
        included_in: receipt
            .included_in
            .map(|block| {
                Ok(BlockRefDto {
                    height: block.height.0,
                    hash: format_bitcoin_block_hash(&block.hash).map_err(|_| {
                        ApiError::internal(
                            "Bitcoin receipt contained an invalid canonical block hash",
                        )
                    })?,
                    parent_hash: block
                        .parent_hash
                        .as_ref()
                        .map(format_bitcoin_block_hash)
                        .transpose()
                        .map_err(|_| {
                            ApiError::internal(
                                "Bitcoin receipt contained an invalid parent block hash",
                            )
                        })?,
                    timestamp: block.timestamp,
                })
            })
            .transpose()?,
        confirmations: receipt.confirmations,
        replaced_by: receipt.replaced_by.map(|id| id.to_string()),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum KeyLocatorDto {
    Identifier { value: String },
    DerivationPath { children: Vec<ChildIndexDto> },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ChildIndexDto {
    index: u32,
    hardened: bool,
}

impl KeyLocatorDto {
    fn into_locator(self) -> ApiResult<KeyLocator> {
        match self {
            Self::Identifier { value } => {
                if value.is_empty()
                    || value.len() > 4_096
                    || value.bytes().any(|byte| byte.is_ascii_control())
                {
                    return Err(ApiError::bad_request(
                        "invalid_key_locator",
                        "key locator identifier is invalid",
                    ));
                }
                Ok(KeyLocator::Identifier(value))
            }
            Self::DerivationPath { children } => {
                if children.is_empty() || children.len() > 64 {
                    return Err(ApiError::bad_request(
                        "invalid_key_locator",
                        "key locator derivation path is invalid",
                    ));
                }
                Ok(KeyLocator::DerivationPath(DerivationPath(
                    children
                        .into_iter()
                        .map(|child| ChildIndex {
                            index: child.index,
                            hardened: child.hardened,
                        })
                        .collect(),
                )))
            }
        }
    }
}

impl From<KeyLocator> for KeyLocatorDto {
    fn from(value: KeyLocator) -> Self {
        match value {
            KeyLocator::Identifier(value) => Self::Identifier { value },
            KeyLocator::DerivationPath(DerivationPath(children)) => Self::DerivationPath {
                children: children
                    .into_iter()
                    .map(|child| ChildIndexDto {
                        index: child.index,
                        hardened: child.hardened,
                    })
                    .collect(),
            },
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    body: ErrorEnvelope,
}

impl ApiError {
    fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, false)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            message,
            false,
        )
    }

    fn new(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            body: ErrorEnvelope {
                code: code.into(),
                message: message.into(),
                retryable,
                request_id: format!("ws-request-{}", Uuid::now_v7()),
            },
        }
    }

    fn from_chain(error: ChainError) -> Self {
        match error.kind {
            ChainErrorKind::InvalidAddress => Self::bad_request(
                "invalid_address",
                "Bitcoin address or ownership metadata is invalid",
            ),
            ChainErrorKind::InvalidTransaction => Self::bad_request(
                "invalid_transaction",
                "Bitcoin transaction request is invalid",
            ),
            ChainErrorKind::InsufficientFunds => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "insufficient_funds",
                "Bitcoin inputs cannot satisfy this operation",
                false,
            ),
            ChainErrorKind::FeeUnavailable | ChainErrorKind::RpcUnavailable => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "bitcoin_rpc_unavailable",
                "Bitcoin services are temporarily unavailable",
                true,
            ),
            ChainErrorKind::Signer => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "custody_unavailable",
                "custody could not complete the operation",
                true,
            ),
            ChainErrorKind::Rejected => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "transaction_rejected",
                "Bitcoin transaction was rejected",
                false,
            ),
            ChainErrorKind::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "transaction_not_found",
                "Bitcoin transaction does not exist",
                false,
            ),
            ChainErrorKind::Other => {
                Self::internal("Wallet Service could not complete the operation")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    code: String,
    message: String,
    retryable: bool,
    request_id: String,
}

fn json_payload<T>(payload: Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    payload.map(|Json(value)| value).map_err(|_| {
        ApiError::bad_request(
            "invalid_json",
            "request body must be valid JSON within the configured size limit",
        )
    })
}

fn operation_id(value: &str) -> ApiResult<OperationId> {
    OperationId::new(value).map_err(|_| {
        ApiError::bad_request(
            "invalid_operation_id",
            "operation_id must be a non-empty opaque value without whitespace",
        )
    })
}

fn validate_key_purpose(value: &str) -> ApiResult<()> {
    if value.trim().is_empty()
        || value.len() > 1_024
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(ApiError::bad_request(
            "invalid_key_purpose",
            "key_purpose must contain between 1 and 1024 safe bytes",
        ));
    }
    Ok(())
}

fn canonical_address(value: &str, network: BitcoinNetwork) -> ApiResult<BitcoinAddress> {
    let address = BitcoinAddress::parse_for_network(value, network).map_err(|_| {
        ApiError::bad_request(
            "invalid_address",
            "Bitcoin address must be canonical and belong to the configured network",
        )
    })?;
    if address.0 != value {
        return Err(ApiError::bad_request(
            "invalid_address",
            "Bitcoin address must be canonical and belong to the configured network",
        ));
    }
    Ok(address)
}

fn transaction_id(value: &str) -> ApiResult<BitcoinTransactionId> {
    let transaction_id = value.parse::<BitcoinTransactionId>().map_err(|_| {
        ApiError::bad_request(
            "invalid_transaction_id",
            "transaction ID must be canonical lowercase hexadecimal",
        )
    })?;
    if transaction_id.to_string() != value {
        return Err(ApiError::bad_request(
            "invalid_transaction_id",
            "transaction ID must be canonical lowercase hexadecimal",
        ));
    }
    Ok(transaction_id)
}

fn fee_rate(value: &str) -> ApiResult<SatoshisPerKvb> {
    nonzero_decimal_u64(value, "fee_rate_satoshis_per_kvb").map(SatoshisPerKvb::new)
}

fn nonzero_decimal_u64(value: &str, field: &str) -> ApiResult<u64> {
    let parsed = decimal_u64(value, field)?;
    if parsed == 0 {
        return Err(ApiError::bad_request(
            "invalid_integer",
            format!("{field} must be greater than zero"),
        ));
    }
    Ok(parsed)
}

fn decimal_u32(value: &str, field: &str) -> ApiResult<u32> {
    let parsed = decimal_u64(value, field)?;
    u32::try_from(parsed).map_err(|_| {
        ApiError::bad_request(
            "invalid_integer",
            format!("{field} exceeds the supported unsigned 32-bit range"),
        )
    })
}

fn decimal_u64(value: &str, field: &str) -> ApiResult<u64> {
    let parsed = value.parse::<u64>().map_err(|_| {
        ApiError::bad_request(
            "invalid_integer",
            format!("{field} must be a canonical unsigned decimal string"),
        )
    })?;
    if parsed.to_string() != value {
        return Err(ApiError::bad_request(
            "invalid_integer",
            format!("{field} must be a canonical unsigned decimal string"),
        ));
    }
    Ok(parsed)
}

fn canonical_hex(value: &str, field: &str) -> ApiResult<Vec<u8>> {
    let hexadecimal = value.strip_prefix("0x").ok_or_else(|| {
        ApiError::bad_request("invalid_hex", format!("{field} must have a 0x prefix"))
    })?;
    if hexadecimal.is_empty() || hexadecimal.len() % 2 != 0 {
        return Err(ApiError::bad_request(
            "invalid_hex",
            format!("{field} must contain complete bytes"),
        ));
    }
    let decoded = hex::decode(hexadecimal).map_err(|_| {
        ApiError::bad_request(
            "invalid_hex",
            format!("{field} contains invalid hexadecimal"),
        )
    })?;
    if hex_prefixed(&decoded) != value {
        return Err(ApiError::bad_request(
            "invalid_hex",
            format!("{field} must use canonical lowercase hexadecimal"),
        ));
    }
    Ok(decoded)
}

fn hex_prefixed(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn validate_unique_outpoints<'a>(
    inputs: impl IntoIterator<Item = &'a BitcoinExactInput>,
) -> ApiResult<()> {
    let mut seen = BTreeSet::new();
    for input in inputs {
        if !seen.insert((input.transaction_id, input.output_index)) {
            return Err(ApiError::bad_request(
                "duplicate_input",
                "Bitcoin input outpoints must be unique",
            ));
        }
    }
    Ok(())
}

fn validate_unique_addresses<'a>(
    addresses: impl IntoIterator<Item = &'a BitcoinAddress>,
) -> ApiResult<()> {
    let mut seen = BTreeSet::new();
    for address in addresses {
        if !seen.insert(address) {
            return Err(ApiError::bad_request(
                "duplicate_source",
                "Bitcoin collection source addresses must be unique",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use chain_bitcoin::BitcoinAddressGenerator;
    use chain_contract::DepositAddressGenerator;
    use http_support::{
        BearerToken, HealthState, HttpServerConfig, RequestLimits, TransportSecurity,
        service_router,
    };
    use indexing::{BlockHash, BlockHeight, BlockRef};
    use serde_json::{Value, json};
    use signer_local::LocalSigner;
    use tower::ServiceExt;

    use super::*;

    const RAW_TRANSACTION_HEX: &str = concat!(
        "020000000001010707070707070707070707070707070707070707070707070707070707070707",
        "0300000000fdffffff0110a400000000000000010e7369676e65642d7769746e65737300000000"
    );
    const TRANSACTION_ID: &str = "e9176f8317e0f47796115e8c04a6e60ffb18b31a984cadfa9cf5b77d9db40986";

    struct FakeOperations {
        p2wpkh: GeneratedAddress<BitcoinAddress>,
        p2tr: GeneratedAddress<BitcoinAddress>,
        wrong_network: BitcoinAddress,
        broadcasts: AtomicUsize,
        transfers: AtomicUsize,
        collections: AtomicUsize,
    }

    impl FakeOperations {
        async fn new() -> Self {
            let keys = LocalSigner::ephemeral_for_testing();
            let p2wpkh = generated_address(
                &keys,
                BitcoinNetwork::Regtest,
                BitcoinAddressKind::SegwitV0,
                "test-regtest-p2wpkh",
            )
            .await;
            let p2tr = generated_address(
                &keys,
                BitcoinNetwork::Regtest,
                BitcoinAddressKind::Taproot,
                "test-regtest-p2tr",
            )
            .await;
            let wrong_network = generated_address(
                &keys,
                BitcoinNetwork::Mainnet,
                BitcoinAddressKind::SegwitV0,
                "test-mainnet-p2wpkh",
            )
            .await
            .address;
            Self {
                p2wpkh,
                p2tr,
                wrong_network,
                broadcasts: AtomicUsize::new(0),
                transfers: AtomicUsize::new(0),
                collections: AtomicUsize::new(0),
            }
        }

        fn signed() -> BitcoinSignedTransaction {
            BitcoinSignedTransaction::from_consensus_bytes(
                TRANSACTION_ID
                    .parse()
                    .expect("test Bitcoin transaction ID must parse"),
                hex::decode(RAW_TRANSACTION_HEX)
                    .expect("test Bitcoin transaction bytes must decode"),
            )
            .expect("test Bitcoin transaction ID must match its exact bytes")
        }

        fn exact_input(&self) -> BitcoinExactInput {
            let script = self
                .p2wpkh
                .address
                .script_pubkey_for_network(BitcoinNetwork::Regtest)
                .expect("test address must produce a script")
                .into_bytes();
            BitcoinExactInput {
                transaction_id: BitcoinTransactionId([7; 32]),
                output_index: 3,
                value: Satoshi(43_000),
                script_pubkey: script,
                address: self.p2wpkh.address.clone(),
                key: self.p2wpkh.key.clone(),
            }
        }

        fn prepared(&self) -> BitcoinPreparedTransaction {
            BitcoinPreparedTransaction {
                transaction: Self::signed(),
                inputs: vec![self.exact_input()],
                outputs: vec![BitcoinOutput {
                    address: self.p2tr.address.clone(),
                    value: Satoshi(42_000),
                }],
                fee: Satoshi(1_000),
                virtual_size: 111,
            }
        }
    }

    impl BitcoinWalletOperations for FakeOperations {
        fn generate_address<'a>(
            &'a self,
            request: BitcoinGenerateAddress,
        ) -> OperationFuture<'a, Result<GeneratedAddress<BitcoinAddress>, ChainError>> {
            Box::pin(async move {
                if request.network != BitcoinNetwork::Regtest {
                    return Err(ChainError {
                        kind: ChainErrorKind::InvalidAddress,
                        message: "test operation received the wrong network".to_owned(),
                    });
                }
                match request.kind {
                    BitcoinAddressKind::SegwitV0 => Ok(self.p2wpkh.clone()),
                    BitcoinAddressKind::Taproot => Ok(self.p2tr.clone()),
                }
            })
        }

        fn balance<'a>(
            &'a self,
            _address: BitcoinAddress,
        ) -> OperationFuture<'a, Result<Balance<Satoshi>, ChainError>> {
            Box::pin(async {
                Ok(Balance {
                    confirmed: Satoshi(43_000),
                    pending: Satoshi(2_000),
                    spendable: Satoshi(42_000),
                })
            })
        }

        fn sign_transfer<'a>(
            &'a self,
            request: BitcoinTransferSignRequest,
        ) -> OperationFuture<'a, Result<BitcoinPreparedTransaction, ChainError>> {
            self.transfers.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if request.inputs != vec![self.exact_input()]
                    || request.fee_rate.satoshis_per_kvb() != 1_500
                {
                    return Err(ChainError {
                        kind: ChainErrorKind::InvalidTransaction,
                        message: "test operation received unexpected exact inputs".to_owned(),
                    });
                }
                Ok(self.prepared())
            })
        }

        fn collection_requirements<'a>(
            &'a self,
            request: BitcoinCollectionRequirementsRequest,
        ) -> OperationFuture<'a, Result<Vec<BitcoinCollectionRequirement>, ChainError>> {
            Box::pin(async move {
                if request.sources != vec![self.p2wpkh.address.clone()] {
                    return Err(ChainError {
                        kind: ChainErrorKind::InvalidTransaction,
                        message: "test operation received unexpected sources".to_owned(),
                    });
                }
                Ok(Vec::new())
            })
        }

        fn sign_collection<'a>(
            &'a self,
            request: BitcoinCollectionSignRequest,
        ) -> OperationFuture<'a, Result<BitcoinPreparedCollection, ChainError>> {
            self.collections.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if request.sources.len() != 1
                    || request.sources[0].inputs != vec![self.exact_input()]
                {
                    return Err(ChainError {
                        kind: ChainErrorKind::InvalidTransaction,
                        message: "test operation received unexpected collection inputs".to_owned(),
                    });
                }
                Ok(BitcoinPreparedCollection {
                    prepared: self.prepared(),
                    attribution: vec![BitcoinCollectionAttribution {
                        address: self.p2wpkh.address.clone(),
                        key: self.p2wpkh.key.clone(),
                        gross_input: Satoshi(43_000),
                    }],
                })
            })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: BitcoinSignedTransaction,
        ) -> OperationFuture<'a, Result<BitcoinTransactionId, ChainError>> {
            self.broadcasts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if transaction.consensus_bytes()
                    != hex::decode(RAW_TRANSACTION_HEX)
                        .expect("test Bitcoin transaction bytes must decode")
                {
                    return Err(ChainError {
                        kind: ChainErrorKind::InvalidTransaction,
                        message: "test operation received different signed bytes".to_owned(),
                    });
                }
                Ok(transaction.id())
            })
        }

        fn receipt<'a>(
            &'a self,
            transaction_id: BitcoinTransactionId,
        ) -> OperationFuture<'a, Result<Option<BitcoinReceipt>, ChainError>> {
            Box::pin(async move {
                Ok(Some(BitcoinReceipt {
                    id: transaction_id,
                    included_in: Some(BlockRef {
                        height: BlockHeight(101),
                        hash: BlockHash((0_u8..32).collect()),
                        parent_hash: Some(BlockHash((32_u8..64).collect())),
                        timestamp: Some(1_700_000_000),
                    }),
                    confirmations: 6,
                    replaced_by: None,
                }))
            })
        }
    }

    async fn generated_address(
        keys: &LocalSigner,
        network: BitcoinNetwork,
        kind: BitcoinAddressKind,
        operation: &str,
    ) -> GeneratedAddress<BitcoinAddress> {
        BitcoinAddressGenerator
            .generate_address(
                BitcoinGenerateAddress::new(
                    network,
                    kind,
                    OperationId::new(operation).expect("test operation ID must be valid"),
                    "bitcoin-api-test",
                ),
                keys,
            )
            .await
            .expect("test Bitcoin address must be generated")
    }

    fn test_router(fake: Arc<FakeOperations>) -> Router {
        let config = HttpServerConfig::new(
            "127.0.0.1:8083".parse().expect("test bind must parse"),
            TransportSecurity::PlaintextLoopback,
            Some(BearerToken::new("wallet-secret").expect("test token must be valid")),
            RequestLimits::new(1024 * 1024, 10, 10).expect("test limits must be valid"),
        );
        let operations: Arc<dyn BitcoinWalletOperations> = fake;
        service_router(
            router(BitcoinNetwork::Regtest, operations),
            &config,
            HealthState::new(true),
        )
        .expect("test router must compose")
    }

    fn json_request(path: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::AUTHORIZATION, "Bearer wallet-secret")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("test request must build")
    }

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response body must be readable");
        serde_json::from_slice(&bytes).expect("response body must be JSON")
    }

    fn exact_input_json(fake: &FakeOperations) -> Value {
        let input = fake.exact_input();
        json!({
            "transaction_id": input.transaction_id.to_string(),
            "output_index": input.output_index.to_string(),
            "value_satoshis": input.value.0.to_string(),
            "script_pubkey": hex_prefixed(&input.script_pubkey),
            "address": input.address.0,
            "key_locator": serde_json::to_value(KeyLocatorDto::from(input.key))
                .expect("test key locator must serialize")
        })
    }

    fn collection_input_json(fake: &FakeOperations) -> Value {
        let input = fake.exact_input();
        json!({
            "transaction_id": input.transaction_id.to_string(),
            "output_index": input.output_index.to_string(),
            "value_satoshis": input.value.0.to_string(),
            "script_pubkey": hex_prefixed(&input.script_pubkey)
        })
    }

    #[tokio::test]
    async fn authenticated_routes_cover_the_stateless_bitcoin_flow() {
        let fake = Arc::new(FakeOperations::new().await);
        let app = test_router(Arc::clone(&fake));

        let response = app
            .clone()
            .oneshot(json_request(
                ADDRESS_PATH,
                json!({
                    "operation_id": "generate-p2tr-address",
                    "address_kind": "p2tr",
                    "key_purpose": "deposit-address"
                }),
            ))
            .await
            .expect("address request must complete");
        assert_eq!(response.status(), StatusCode::OK);
        let address_body = body_json(response).await;
        assert_eq!(address_body["address"], fake.p2tr.address.0);

        let response = app
            .clone()
            .oneshot(json_request(
                BALANCE_PATH,
                json!({"address": fake.p2wpkh.address.0}),
            ))
            .await
            .expect("balance request must complete");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["spendable_satoshis"], "42000");

        let response = app
            .clone()
            .oneshot(json_request(
                SIGN_TRANSFER_PATH,
                json!({
                    "operation_id": "sign-exact-transfer",
                    "inputs": [exact_input_json(&fake)],
                    "recipients": [{
                        "address": fake.p2tr.address.0,
                        "value_satoshis": "42000"
                    }],
                    "change_address": fake.p2wpkh.address.0,
                    "fee_rate_satoshis_per_kvb": "1500"
                }),
            ))
            .await
            .expect("transfer signing request must complete");
        assert_eq!(response.status(), StatusCode::OK);
        let transfer_body = body_json(response).await;
        assert_eq!(transfer_body["transaction_id"], TRANSACTION_ID);
        assert_eq!(
            transfer_body["raw_transaction"],
            format!("0x{RAW_TRANSACTION_HEX}")
        );
        assert_eq!(transfer_body["fee_satoshis"], "1000");
        assert_eq!(transfer_body["selected_outpoints"][0]["output_index"], "3");

        let response = app
            .clone()
            .oneshot(json_request(
                COLLECTION_REQUIREMENTS_PATH,
                json!({"sources": [{"address": fake.p2wpkh.address.0}]}),
            ))
            .await
            .expect("collection requirements request must complete");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["requirements"], json!([]));

        let response = app
            .clone()
            .oneshot(json_request(
                SIGN_COLLECTION_PATH,
                json!({
                    "operation_id": "sign-exact-collection",
                    "sources": [{
                        "address": fake.p2wpkh.address.0,
                        "key_locator": serde_json::to_value(KeyLocatorDto::from(
                            fake.p2wpkh.key.clone()
                        )).expect("test key locator must serialize"),
                        "inputs": [collection_input_json(&fake)]
                    }],
                    "destination": fake.p2tr.address.0,
                    "fee_rate_satoshis_per_kvb": "1500"
                }),
            ))
            .await
            .expect("collection signing request must complete");
        assert_eq!(response.status(), StatusCode::OK);
        let collection_body = body_json(response).await;
        assert_eq!(
            collection_body["attribution"][0]["gross_input_satoshis"],
            "43000"
        );

        let response = app
            .clone()
            .oneshot(json_request(
                BROADCAST_PATH,
                json!({
                    "expected_transaction_id": TRANSACTION_ID,
                    "raw_transaction": format!("0x{RAW_TRANSACTION_HEX}")
                }),
            ))
            .await
            .expect("broadcast request must complete");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["transaction_id"], TRANSACTION_ID);

        let response = app
            .oneshot(json_request(
                RECEIPT_PATH,
                json!({"transaction_id": TRANSACTION_ID}),
            ))
            .await
            .expect("receipt request must complete");
        assert_eq!(response.status(), StatusCode::OK);
        let receipt_body = body_json(response).await;
        assert_eq!(receipt_body["receipt"]["confirmations"], 6);
        assert_eq!(
            receipt_body["receipt"]["included_in"]["hash"],
            "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100"
        );
        assert_eq!(
            receipt_body["receipt"]["included_in"]["parent_hash"],
            "3f3e3d3c3b3a393837363534333231302f2e2d2c2b2a29282726252423222120"
        );

        assert_eq!(fake.transfers.load(Ordering::SeqCst), 1);
        assert_eq!(fake.collections.load(Ordering::SeqCst), 1);
        assert_eq!(fake.broadcasts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn strict_json_network_and_decimal_validation_fail_before_operations() {
        let fake = Arc::new(FakeOperations::new().await);
        let app = test_router(Arc::clone(&fake));

        let response = app
            .clone()
            .oneshot(json_request(
                BALANCE_PATH,
                json!({"address": fake.p2wpkh.address.0, "unexpected": true}),
            ))
            .await
            .expect("strict JSON request must complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["code"], "invalid_json");

        let response = app
            .clone()
            .oneshot(json_request(
                BALANCE_PATH,
                json!({"address": fake.wrong_network.0}),
            ))
            .await
            .expect("wrong-network request must complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["code"], "invalid_address");

        let mut input = exact_input_json(&fake);
        input["value_satoshis"] = json!("043000");
        let response = app
            .oneshot(json_request(
                SIGN_TRANSFER_PATH,
                json!({
                    "operation_id": "reject-noncanonical-decimal",
                    "inputs": [input],
                    "recipients": [{
                        "address": fake.p2tr.address.0,
                        "value_satoshis": "42000"
                    }],
                    "change_address": fake.p2wpkh.address.0,
                    "fee_rate_satoshis_per_kvb": "1500"
                }),
            ))
            .await
            .expect("noncanonical decimal request must complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["code"], "invalid_integer");
        assert_eq!(fake.transfers.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn broadcast_verifies_transaction_id_and_exact_bytes_before_side_effect() {
        let fake = Arc::new(FakeOperations::new().await);
        let app = test_router(Arc::clone(&fake));
        let wrong_id = "00".repeat(32);
        let raw = format!("0x{RAW_TRANSACTION_HEX}");

        let response = app
            .oneshot(json_request(
                BROADCAST_PATH,
                json!({
                    "expected_transaction_id": wrong_id,
                    "raw_transaction": raw
                }),
            ))
            .await
            .expect("mismatched broadcast request must complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["code"], "invalid_signed_transaction");
        assert!(!body.to_string().contains(RAW_TRANSACTION_HEX));
        assert_eq!(fake.broadcasts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn protected_routes_reject_missing_bearer_token() {
        let fake = Arc::new(FakeOperations::new().await);
        let app = test_router(fake);
        let request = Request::builder()
            .method("POST")
            .uri(BALANCE_PATH)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .expect("test request must build");

        let response = app
            .oneshot(request)
            .await
            .expect("unauthenticated request must complete");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn debug_output_redacts_signed_transaction_bytes() {
        let fake = FakeOperations::new().await;
        let prepared = PreparedTransactionResponse::from(fake.prepared());
        let broadcast = BroadcastRequest {
            expected_transaction_id: TRANSACTION_ID.to_owned(),
            raw_transaction: format!("0x{RAW_TRANSACTION_HEX}"),
        };

        let prepared_debug = format!("{prepared:?}");
        let broadcast_debug = format!("{broadcast:?}");

        assert!(prepared_debug.contains("[REDACTED]"));
        assert!(!prepared_debug.contains(RAW_TRANSACTION_HEX));
        assert!(broadcast_debug.contains("[REDACTED]"));
        assert!(!broadcast_debug.contains(RAW_TRANSACTION_HEX));
    }
}
