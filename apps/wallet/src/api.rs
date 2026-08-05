use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use chain_contract::{Balance, ChainError, ChainErrorKind, GeneratedAddress};
use chain_ethereum::{
    EthereumAddress, EthereumAsset, EthereumCollectionAttribution, EthereumCollectionRequest,
    EthereumCollectionRequirement, EthereumPreparedCollection, EthereumReceipt,
    EthereumSignedTransaction, EthereumTransactionId, EthereumTransferRequest, Wei,
};
use chain_identity::AtomicAmount;
use serde::{Deserialize, Serialize};
use signer::{ChildIndex, DerivationPath, KeyLocator, OperationId};
use uuid::Uuid;

pub const ADDRESS_PATH: &str = "/v1/ethereum/addresses";
pub const BALANCE_PATH: &str = "/v1/ethereum/balances";
pub const SIGN_NATIVE_TRANSFER_PATH: &str = "/v1/ethereum/transfers/native/sign";
pub const SIGN_ERC20_TRANSFER_PATH: &str = "/v1/ethereum/transfers/erc20/sign";
pub const COLLECTION_REQUIREMENTS_PATH: &str = "/v1/ethereum/collections/requirements";
pub const SIGN_NATIVE_COLLECTION_PATH: &str = "/v1/ethereum/collections/native/sign";
pub const SIGN_ERC20_COLLECTION_PATH: &str = "/v1/ethereum/collections/erc20/sign";
pub const BROADCAST_PATH: &str = "/v1/ethereum/transactions/broadcast";
pub const RECEIPT_PATH: &str = "/v1/ethereum/receipts";

pub type OperationFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object-safe stateless Ethereum operations used by the HTTP adapter.
///
/// The production implementation delegates to chain-native wallet types;
/// deterministic tests can inject a fake without any RPC or custody network.
pub trait EthereumWalletOperations: Send + Sync {
    fn generate_address<'a>(
        &'a self,
        asset: EthereumAsset,
        operation_id: OperationId,
        key_purpose: String,
    ) -> OperationFuture<'a, Result<GeneratedAddress<EthereumAddress>, ChainError>>;

    fn balance<'a>(
        &'a self,
        asset: EthereumAsset,
        address: EthereumAddress,
    ) -> OperationFuture<'a, Result<Balance<Wei>, ChainError>>;

    fn sign_transfer<'a>(
        &'a self,
        asset: EthereumAsset,
        request: EthereumTransferRequest,
    ) -> OperationFuture<'a, Result<EthereumSignedTransaction, ChainError>>;

    fn collection_requirements<'a>(
        &'a self,
        asset: EthereumAsset,
        request: EthereumCollectionRequest,
    ) -> OperationFuture<'a, Result<Vec<EthereumCollectionRequirement>, ChainError>>;

    fn prepare_collection<'a>(
        &'a self,
        asset: EthereumAsset,
        request: EthereumCollectionRequest,
    ) -> OperationFuture<'a, Result<EthereumPreparedCollection, ChainError>>;

    fn broadcast<'a>(
        &'a self,
        transaction: EthereumSignedTransaction,
    ) -> OperationFuture<'a, Result<EthereumTransactionId, ChainError>>;

    fn receipt<'a>(
        &'a self,
        transaction_id: EthereumTransactionId,
    ) -> OperationFuture<'a, Result<Option<EthereumReceipt>, ChainError>>;
}

pub fn router(operations: Arc<dyn EthereumWalletOperations>) -> Router {
    Router::new()
        .route(ADDRESS_PATH, post(generate_address))
        .route(BALANCE_PATH, post(balance))
        .route(SIGN_NATIVE_TRANSFER_PATH, post(sign_native_transfer))
        .route(SIGN_ERC20_TRANSFER_PATH, post(sign_erc20_transfer))
        .route(COLLECTION_REQUIREMENTS_PATH, post(collection_requirements))
        .route(SIGN_NATIVE_COLLECTION_PATH, post(sign_native_collection))
        .route(SIGN_ERC20_COLLECTION_PATH, post(sign_erc20_collection))
        .route(BROADCAST_PATH, post(broadcast))
        .route(RECEIPT_PATH, post(receipt))
        .with_state(operations)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateAddressRequest {
    operation_id: String,
    asset: AssetDto,
    key_purpose: String,
}

#[derive(Clone, Debug, Serialize)]
struct GenerateAddressResponse {
    address: String,
    key_locator: KeyLocatorDto,
}

async fn generate_address(
    State(operations): State<Arc<dyn EthereumWalletOperations>>,
    payload: Result<Json<GenerateAddressRequest>, JsonRejection>,
) -> ApiResult<Json<GenerateAddressResponse>> {
    let request = json_payload(payload)?;
    let operation_id = operation_id(&request.operation_id)?;
    validate_key_purpose(&request.key_purpose)?;
    let generated = operations
        .generate_address(
            request.asset.into_asset()?,
            operation_id,
            request.key_purpose,
        )
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(GenerateAddressResponse {
        address: generated.address.to_string(),
        key_locator: KeyLocatorDto::from(generated.key),
    }))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BalanceRequest {
    asset: AssetDto,
    address: String,
}

#[derive(Clone, Debug, Serialize)]
struct BalanceResponse {
    confirmed: String,
    pending: String,
    spendable: String,
}

async fn balance(
    State(operations): State<Arc<dyn EthereumWalletOperations>>,
    payload: Result<Json<BalanceRequest>, JsonRejection>,
) -> ApiResult<Json<BalanceResponse>> {
    let request = json_payload(payload)?;
    let result = operations
        .balance(
            request.asset.into_asset()?,
            canonical_address(&request.address)?,
        )
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(BalanceResponse {
        confirmed: decimal(&result.confirmed),
        pending: decimal(&result.pending),
        spendable: decimal(&result.spendable),
    }))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignNativeTransferRequest {
    operation_id: String,
    key_locator: KeyLocatorDto,
    from: String,
    to: String,
    value: String,
}

async fn sign_native_transfer(
    State(operations): State<Arc<dyn EthereumWalletOperations>>,
    payload: Result<Json<SignNativeTransferRequest>, JsonRejection>,
) -> ApiResult<Json<SignedTransactionResponse>> {
    let request = json_payload(payload)?;
    let transfer = EthereumTransferRequest::native(
        operation_id(&request.operation_id)?,
        request.key_locator.into_locator()?,
        canonical_address(&request.from)?,
        canonical_address(&request.to)?,
        decimal_wei(&request.value, "value")?,
    );
    let signed = operations
        .sign_transfer(EthereumAsset::Native, transfer)
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(SignedTransactionResponse::from(signed)))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignErc20TransferRequest {
    operation_id: String,
    key_locator: KeyLocatorDto,
    token: String,
    from: String,
    to: String,
    amount: String,
}

async fn sign_erc20_transfer(
    State(operations): State<Arc<dyn EthereumWalletOperations>>,
    payload: Result<Json<SignErc20TransferRequest>, JsonRejection>,
) -> ApiResult<Json<SignedTransactionResponse>> {
    let request = json_payload(payload)?;
    let token = canonical_address(&request.token)?;
    let transfer = EthereumTransferRequest::erc20(
        operation_id(&request.operation_id)?,
        request.key_locator.into_locator()?,
        canonical_address(&request.from)?,
        token.clone(),
        canonical_address(&request.to)?,
        decimal_wei(&request.amount, "amount")?,
    );
    let signed = operations
        .sign_transfer(EthereumAsset::Erc20(token), transfer)
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(SignedTransactionResponse::from(signed)))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionRequirementsRequest {
    #[serde(flatten)]
    collection: CollectionRequestDto,
}

async fn collection_requirements(
    State(operations): State<Arc<dyn EthereumWalletOperations>>,
    payload: Result<Json<CollectionRequirementsRequest>, JsonRejection>,
) -> ApiResult<Json<CollectionRequirementsResponse>> {
    let request = json_payload(payload)?;
    let (asset, request) = request.collection.into_collection()?;
    let requirements = operations
        .collection_requirements(asset, request)
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(CollectionRequirementsResponse {
        requirements: requirements
            .into_iter()
            .map(CollectionRequirementDto::from)
            .collect(),
    }))
}

#[derive(Clone, Debug, Serialize)]
struct CollectionRequirementsResponse {
    requirements: Vec<CollectionRequirementDto>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CollectionRequirementDto {
    NativeGasBalance {
        address: String,
        current: String,
        required: String,
        deficit: String,
    },
}

impl From<EthereumCollectionRequirement> for CollectionRequirementDto {
    fn from(value: EthereumCollectionRequirement) -> Self {
        match value {
            EthereumCollectionRequirement::NativeGasBalance {
                address,
                current,
                required,
                deficit,
            } => Self::NativeGasBalance {
                address: address.to_string(),
                current: decimal(&current),
                required: decimal(&required),
                deficit: decimal(&deficit),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeCollectionRequest {
    operation_id: String,
    key_locator: KeyLocatorDto,
    from: String,
    destination: String,
}

async fn sign_native_collection(
    State(operations): State<Arc<dyn EthereumWalletOperations>>,
    payload: Result<Json<NativeCollectionRequest>, JsonRejection>,
) -> ApiResult<Json<PreparedCollectionResponse>> {
    let request = json_payload(payload)?;
    let request = EthereumCollectionRequest::Native {
        signing_operation_id: operation_id(&request.operation_id)?,
        from: canonical_address(&request.from)?,
        key: request.key_locator.into_locator()?,
        destination: canonical_address(&request.destination)?,
    };
    let prepared = operations
        .prepare_collection(EthereumAsset::Native, request)
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(PreparedCollectionResponse::from(prepared)))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Erc20CollectionRequest {
    operation_id: String,
    key_locator: KeyLocatorDto,
    token: String,
    from: String,
    destination: String,
    amount: Option<String>,
}

async fn sign_erc20_collection(
    State(operations): State<Arc<dyn EthereumWalletOperations>>,
    payload: Result<Json<Erc20CollectionRequest>, JsonRejection>,
) -> ApiResult<Json<PreparedCollectionResponse>> {
    let request = json_payload(payload)?;
    let token = canonical_address(&request.token)?;
    let collection = EthereumCollectionRequest::Token {
        signing_operation_id: operation_id(&request.operation_id)?,
        token: token.clone(),
        from: canonical_address(&request.from)?,
        key: request.key_locator.into_locator()?,
        destination: canonical_address(&request.destination)?,
        amount: request
            .amount
            .as_deref()
            .map(|amount| decimal_wei(amount, "amount"))
            .transpose()?,
    };
    let prepared = operations
        .prepare_collection(EthereumAsset::Erc20(token), collection)
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(PreparedCollectionResponse::from(prepared)))
}

#[derive(Clone, Serialize)]
struct SignedTransactionResponse {
    transaction_id: String,
    signed_envelope: String,
}

impl std::fmt::Debug for SignedTransactionResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignedTransactionResponse")
            .field("transaction_id", &self.transaction_id)
            .field("signed_envelope", &"[REDACTED]")
            .finish()
    }
}

impl From<EthereumSignedTransaction> for SignedTransactionResponse {
    fn from(transaction: EthereumSignedTransaction) -> Self {
        Self {
            transaction_id: transaction.id.to_string(),
            signed_envelope: hex_prefixed(&transaction.envelope),
        }
    }
}

#[derive(Clone, Serialize)]
struct PreparedCollectionResponse {
    transaction_id: String,
    signed_envelope: String,
    attribution: Vec<CollectionAttributionDto>,
}

impl std::fmt::Debug for PreparedCollectionResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedCollectionResponse")
            .field("transaction_id", &self.transaction_id)
            .field("signed_envelope", &"[REDACTED]")
            .field("attribution", &self.attribution)
            .finish()
    }
}

impl From<EthereumPreparedCollection> for PreparedCollectionResponse {
    fn from(prepared: EthereumPreparedCollection) -> Self {
        let transaction = SignedTransactionResponse::from(prepared.transaction);
        Self {
            transaction_id: transaction.transaction_id,
            signed_envelope: transaction.signed_envelope,
            attribution: prepared
                .attribution
                .into_iter()
                .map(CollectionAttributionDto::from)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct CollectionAttributionDto {
    address: String,
    asset: AssetDto,
    gross_debit: String,
}

impl From<EthereumCollectionAttribution> for CollectionAttributionDto {
    fn from(value: EthereumCollectionAttribution) -> Self {
        Self {
            address: value.address.to_string(),
            asset: AssetDto::from(value.asset),
            gross_debit: decimal(&value.gross_debit),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BroadcastRequest {
    expected_transaction_id: String,
    signed_envelope: String,
}

impl std::fmt::Debug for BroadcastRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BroadcastRequest")
            .field("expected_transaction_id", &self.expected_transaction_id)
            .field("signed_envelope", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Serialize)]
struct BroadcastResponse {
    transaction_id: String,
}

async fn broadcast(
    State(operations): State<Arc<dyn EthereumWalletOperations>>,
    payload: Result<Json<BroadcastRequest>, JsonRejection>,
) -> ApiResult<Json<BroadcastResponse>> {
    let request = json_payload(payload)?;
    let id = transaction_id(&request.expected_transaction_id)?;
    let envelope = canonical_hex(&request.signed_envelope, "signed envelope")?;
    let signed = EthereumSignedTransaction::from_envelope(id, envelope).map_err(|_| {
        ApiError::bad_request(
            "invalid_signed_envelope",
            "signed envelope does not match the expected transaction ID",
        )
    })?;
    let id = operations
        .broadcast(signed)
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(BroadcastResponse {
        transaction_id: id.to_string(),
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
    succeeded: Option<bool>,
    confirmations: u64,
}

#[derive(Clone, Debug, Serialize)]
struct BlockRefDto {
    height: u64,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<u64>,
}

async fn receipt(
    State(operations): State<Arc<dyn EthereumWalletOperations>>,
    payload: Result<Json<ReceiptRequest>, JsonRejection>,
) -> ApiResult<Json<ReceiptResponse>> {
    let request = json_payload(payload)?;
    let id = transaction_id(&request.transaction_id)?;
    let receipt = operations
        .receipt(id.clone())
        .await
        .map_err(ApiError::from_chain)?;
    Ok(Json(ReceiptResponse {
        transaction_id: id.to_string(),
        receipt: receipt.map(|receipt| ReceiptDto {
            included_in: receipt.included_in.map(|block| BlockRefDto {
                height: block.height.0,
                hash: hex_prefixed(&block.hash.0),
                parent_hash: block.parent_hash.map(|hash| hex_prefixed(&hash.0)),
                timestamp: block.timestamp,
            }),
            succeeded: receipt.succeeded,
            confirmations: receipt.confirmations,
        }),
    }))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CollectionRequestDto {
    Native {
        operation_id: String,
        key_locator: KeyLocatorDto,
        from: String,
        destination: String,
    },
    Erc20 {
        operation_id: String,
        key_locator: KeyLocatorDto,
        token: String,
        from: String,
        destination: String,
        amount: Option<String>,
    },
}

impl CollectionRequestDto {
    fn into_collection(self) -> ApiResult<(EthereumAsset, EthereumCollectionRequest)> {
        match self {
            Self::Native {
                operation_id: id,
                key_locator,
                from,
                destination,
            } => Ok((
                EthereumAsset::Native,
                EthereumCollectionRequest::Native {
                    signing_operation_id: operation_id(&id)?,
                    from: canonical_address(&from)?,
                    key: key_locator.into_locator()?,
                    destination: canonical_address(&destination)?,
                },
            )),
            Self::Erc20 {
                operation_id: id,
                key_locator,
                token,
                from,
                destination,
                amount,
            } => {
                let token = canonical_address(&token)?;
                Ok((
                    EthereumAsset::Erc20(token.clone()),
                    EthereumCollectionRequest::Token {
                        signing_operation_id: operation_id(&id)?,
                        token,
                        from: canonical_address(&from)?,
                        key: key_locator.into_locator()?,
                        destination: canonical_address(&destination)?,
                        amount: amount
                            .as_deref()
                            .map(|amount| decimal_wei(amount, "amount"))
                            .transpose()?,
                    },
                ))
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum AssetDto {
    Native,
    Erc20 { token: String },
}

impl AssetDto {
    fn into_asset(self) -> ApiResult<EthereumAsset> {
        match self {
            Self::Native => Ok(EthereumAsset::Native),
            Self::Erc20 { token } => Ok(EthereumAsset::Erc20(canonical_address(&token)?)),
        }
    }
}

impl From<EthereumAsset> for AssetDto {
    fn from(value: EthereumAsset) -> Self {
        match value {
            EthereumAsset::Native => Self::Native,
            EthereumAsset::Erc20(token) => Self::Erc20 {
                token: token.to_string(),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum KeyLocatorDto {
    Identifier { value: String },
    DerivationPath { children: Vec<ChildIndexDto> },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
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
                "Ethereum address or ownership metadata is invalid",
            ),
            ChainErrorKind::InvalidTransaction => Self::bad_request(
                "invalid_transaction",
                "Ethereum transaction request is invalid",
            ),
            ChainErrorKind::InsufficientFunds => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "insufficient_funds",
                "Ethereum balance cannot satisfy this operation",
                false,
            ),
            ChainErrorKind::FeeUnavailable | ChainErrorKind::RpcUnavailable => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "ethereum_rpc_unavailable",
                "Ethereum RPC is temporarily unavailable",
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
                "Ethereum transaction was rejected",
                false,
            ),
            ChainErrorKind::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "transaction_not_found",
                "Ethereum transaction does not exist",
                false,
            ),
            ChainErrorKind::Other => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Wallet Service could not complete the operation",
                false,
            ),
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

fn canonical_address(value: &str) -> ApiResult<EthereumAddress> {
    let address = value.parse::<EthereumAddress>().map_err(|_| {
        ApiError::bad_request(
            "invalid_address",
            "Ethereum address must be a canonical lowercase 0x-prefixed value",
        )
    })?;
    if address.to_string() != value {
        return Err(ApiError::bad_request(
            "invalid_address",
            "Ethereum address must be a canonical lowercase 0x-prefixed value",
        ));
    }
    Ok(address)
}

fn transaction_id(value: &str) -> ApiResult<EthereumTransactionId> {
    value.parse().map_err(|_| {
        ApiError::bad_request(
            "invalid_transaction_id",
            "transaction ID must be canonical lowercase 0x-prefixed hexadecimal",
        )
    })
}

fn decimal_wei(value: &str, field: &str) -> ApiResult<Wei> {
    AtomicAmount::from_decimal_str(value)
        .map(|amount| Wei(amount.0))
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_amount",
                format!("{field} must be a canonical unsigned U256 decimal"),
            )
        })
}

fn decimal(value: &Wei) -> String {
    AtomicAmount(value.0).to_decimal_string()
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use alloy_primitives::keccak256;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use http_support::{
        BearerToken, HealthState, HttpServerConfig, RequestLimits, TransportSecurity,
        service_router,
    };
    use indexing::{BlockHash, BlockHeight, BlockRef};
    use serde_json::{Value, json};
    use signer::{Curve, PublicKey, PublicKeyFormat};
    use tower::ServiceExt;

    use super::*;

    struct FakeOperations {
        broadcasts: AtomicUsize,
        prepared: AtomicUsize,
    }

    impl FakeOperations {
        fn signed() -> EthereumSignedTransaction {
            let envelope = vec![0x02, 0x01, 0x02];
            EthereumSignedTransaction::from_envelope(
                EthereumTransactionId(keccak256(&envelope).0),
                envelope,
            )
            .expect("test envelope hash must match")
        }
    }

    impl EthereumWalletOperations for FakeOperations {
        fn generate_address<'a>(
            &'a self,
            _asset: EthereumAsset,
            _operation_id: OperationId,
            _key_purpose: String,
        ) -> OperationFuture<'a, Result<GeneratedAddress<EthereumAddress>, ChainError>> {
            Box::pin(async {
                Ok(GeneratedAddress {
                    address: EthereumAddress([0x11; 20]),
                    key: KeyLocator::Identifier("opaque-key-7".to_owned()),
                    public_key: PublicKey {
                        curve: Curve::Secp256k1,
                        format: PublicKeyFormat::Raw,
                        bytes: vec![0x99; 64],
                    },
                })
            })
        }

        fn balance<'a>(
            &'a self,
            _asset: EthereumAsset,
            _address: EthereumAddress,
        ) -> OperationFuture<'a, Result<Balance<Wei>, ChainError>> {
            Box::pin(async {
                Ok(Balance {
                    confirmed: Wei::from_u128(12),
                    pending: Wei::ZERO,
                    spendable: Wei::from_u128(12),
                })
            })
        }

        fn sign_transfer<'a>(
            &'a self,
            _asset: EthereumAsset,
            _request: EthereumTransferRequest,
        ) -> OperationFuture<'a, Result<EthereumSignedTransaction, ChainError>> {
            Box::pin(async { Ok(Self::signed()) })
        }

        fn collection_requirements<'a>(
            &'a self,
            _asset: EthereumAsset,
            _request: EthereumCollectionRequest,
        ) -> OperationFuture<'a, Result<Vec<EthereumCollectionRequirement>, ChainError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn prepare_collection<'a>(
            &'a self,
            _asset: EthereumAsset,
            _request: EthereumCollectionRequest,
        ) -> OperationFuture<'a, Result<EthereumPreparedCollection, ChainError>> {
            self.prepared.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(EthereumPreparedCollection {
                    transaction: Self::signed(),
                    attribution: vec![EthereumCollectionAttribution {
                        address: EthereumAddress([0x11; 20]),
                        asset: EthereumAsset::Native,
                        gross_debit: Wei::from_u128(7),
                    }],
                })
            })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: EthereumSignedTransaction,
        ) -> OperationFuture<'a, Result<EthereumTransactionId, ChainError>> {
            self.broadcasts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(transaction.id) })
        }

        fn receipt<'a>(
            &'a self,
            transaction_id: EthereumTransactionId,
        ) -> OperationFuture<'a, Result<Option<EthereumReceipt>, ChainError>> {
            Box::pin(async move {
                Ok(Some(EthereumReceipt {
                    id: transaction_id,
                    included_in: Some(BlockRef {
                        height: BlockHeight(4),
                        hash: BlockHash(vec![0x22; 32]),
                        parent_hash: None,
                        timestamp: Some(10),
                    }),
                    succeeded: Some(true),
                    confirmations: 2,
                }))
            })
        }
    }

    fn test_router(
        fake: Arc<FakeOperations>,
        ready: bool,
        max_body: usize,
    ) -> (Router, HealthState) {
        let health = HealthState::new(ready);
        let config = HttpServerConfig::new(
            "127.0.0.1:8082".parse().expect("test bind must parse"),
            TransportSecurity::PlaintextLoopback,
            Some(BearerToken::new("wallet-secret").expect("test token must be valid")),
            RequestLimits::new(max_body, 10, 10).expect("test limits must be valid"),
        );
        let operations: Arc<dyn EthereumWalletOperations> = fake;
        (
            service_router(router(operations), &config, health.clone())
                .expect("test router must compose"),
            health,
        )
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

    #[tokio::test]
    async fn routes_require_authentication_but_health_is_detail_free_and_public() {
        let fake = Arc::new(FakeOperations {
            broadcasts: AtomicUsize::new(0),
            prepared: AtomicUsize::new(0),
        });
        let (router, health) = test_router(fake, false, 4096);
        let unauthorized = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(BALANCE_PATH)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .expect("test request must build"),
            )
            .await
            .expect("router must answer");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let not_ready = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(http_support::READINESS_PATH)
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must answer");
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        health.set_ready(true);
        let ready = router
            .oneshot(
                Request::builder()
                    .uri(http_support::READINESS_PATH)
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must answer");
        assert_eq!(ready.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn address_response_contains_only_canonical_address_and_opaque_locator() {
        let fake = Arc::new(FakeOperations {
            broadcasts: AtomicUsize::new(0),
            prepared: AtomicUsize::new(0),
        });
        let (router, _) = test_router(fake, true, 4096);
        let response = router
            .oneshot(json_request(
                ADDRESS_PATH,
                json!({
                    "operation_id": "deposit-operation-7",
                    "asset": { "kind": "native" },
                    "key_purpose": "deposit:7"
                }),
            ))
            .await
            .expect("router must answer");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(
            body,
            json!({
                "address": "0x1111111111111111111111111111111111111111",
                "key_locator": { "kind": "identifier", "value": "opaque-key-7" }
            })
        );
        assert!(!body.to_string().contains("public_key"));
        assert!(!body.to_string().contains("9999"));
    }

    #[tokio::test]
    async fn signing_and_collection_preparation_do_not_broadcast() {
        let fake = Arc::new(FakeOperations {
            broadcasts: AtomicUsize::new(0),
            prepared: AtomicUsize::new(0),
        });
        let (router, _) = test_router(Arc::clone(&fake), true, 4096);
        let sign = router
            .clone()
            .oneshot(json_request(
                SIGN_NATIVE_COLLECTION_PATH,
                json!({
                    "operation_id": "collection-7",
                    "key_locator": { "kind": "identifier", "value": "opaque-key-7" },
                    "from": "0x1111111111111111111111111111111111111111",
                    "destination": "0x2222222222222222222222222222222222222222"
                }),
            ))
            .await
            .expect("router must answer");
        assert_eq!(sign.status(), StatusCode::OK);
        assert_eq!(fake.prepared.load(Ordering::SeqCst), 1);
        assert_eq!(fake.broadcasts.load(Ordering::SeqCst), 0);
        let signed = body_json(sign).await;

        let broadcast_response = router
            .oneshot(json_request(
                BROADCAST_PATH,
                json!({
                    "expected_transaction_id": signed["transaction_id"],
                    "signed_envelope": signed["signed_envelope"]
                }),
            ))
            .await
            .expect("router must answer");
        assert_eq!(broadcast_response.status(), StatusCode::OK);
        assert_eq!(fake.broadcasts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_dtos_and_oversize_bodies_use_sanitized_json_errors() {
        let fake = Arc::new(FakeOperations {
            broadcasts: AtomicUsize::new(0),
            prepared: AtomicUsize::new(0),
        });
        let (router, _) = test_router(fake, true, 128);
        let invalid = router
            .clone()
            .oneshot(json_request(
                BALANCE_PATH,
                json!({
                    "asset": { "kind": "native" },
                    "address": "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                }),
            ))
            .await
            .expect("router must answer");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(invalid).await["code"], "invalid_address");

        let oversized = router
            .oneshot(json_request(
                ADDRESS_PATH,
                json!({
                    "operation_id": "x".repeat(200),
                    "asset": { "kind": "native" },
                    "key_purpose": "deposit"
                }),
            ))
            .await
            .expect("router must answer");
        assert_eq!(oversized.status(), StatusCode::BAD_REQUEST);
        let body = body_json(oversized).await;
        assert_eq!(body["code"], "invalid_json");
        assert!(!body.to_string().contains(&"x".repeat(100)));
    }

    #[test]
    fn signed_envelope_dtos_redact_debug_output() {
        let signed = SignedTransactionResponse::from(FakeOperations::signed());
        assert!(!format!("{signed:?}").contains("0x020102"));
        let broadcast = BroadcastRequest {
            expected_transaction_id: signed.transaction_id,
            signed_envelope: signed.signed_envelope,
        };
        assert!(!format!("{broadcast:?}").contains("0x020102"));
    }
}
