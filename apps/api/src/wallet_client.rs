use std::{fmt, net::IpAddr, num::NonZeroU32, time::Duration};

use chain_ethereum::{
    EthereumAddress, EthereumEip1559FeeInspection, EthereumSignedTransaction, EthereumTransactionId,
};
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use deposits::{
    BoxFuture, DepositAddressRequest, DepositAddressSource, DepositError, DepositErrorKind,
    GeneratedDepositAddress, SignedEnvelopeBytes,
};
use indexing::{BlockHash, BlockHeight, BlockRef};
use reqwest::{Method, StatusCode, Url, redirect::Policy};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use signer::{ChildIndex, DerivationPath, KeyLocator, OperationId};

use crate::config::{BearerSecret, IndexerEndpoint, WalletOptions};

const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const ETHEREUM_CHAIN: &str = "ethereum";
const NATIVE_ASSET: &str = "native";

#[derive(Clone)]
pub struct WalletClient {
    endpoint: IndexerEndpoint,
    bearer_token: BearerSecret,
    request_timeout: Duration,
    retry_attempts: NonZeroU32,
    retry_initial_backoff: Duration,
    retry_max_backoff: Duration,
    client: reqwest::Client,
}

/// Factual Ethereum balance returned by the stateless Wallet Service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletBalance {
    pub confirmed: AtomicAmount,
    pub pending: AtomicAmount,
    pub spendable: AtomicAmount,
}

/// One native Ethereum transfer signing request. PS uses this for the
/// gas-funding leg of an ERC-20 collection workflow.
#[derive(Clone, PartialEq, Eq)]
pub struct NativeTransferRequest {
    pub operation_id: OperationId,
    pub key_locator: KeyLocator,
    pub from: CanonicalAddress,
    pub to: CanonicalAddress,
    pub value: AtomicAmount,
}

impl fmt::Debug for NativeTransferRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeTransferRequest")
            .field("operation_id", &"[REDACTED]")
            .field("key_locator", &"[REDACTED]")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("value", &self.value)
            .finish()
    }
}

/// A signed transaction that has not been broadcast.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedTransaction {
    pub transaction_id: CanonicalTransactionId,
    pub signed_envelope: SignedEnvelopeBytes,
}

impl fmt::Debug for PreparedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedTransaction")
            .field("transaction_id", &self.transaction_id)
            .field("signed_envelope", &"[REDACTED]")
            .finish()
    }
}

impl PreparedTransaction {
    pub(crate) fn inspect_eip1559_fees(
        &self,
    ) -> Result<EthereumEip1559FeeInspection, DepositError> {
        inspect_signed_envelope_fees(&self.transaction_id, &self.signed_envelope)
    }
}

/// Input shared by native collection requirement and signing calls.
#[derive(Clone, PartialEq, Eq)]
pub struct NativeCollectionRequest {
    pub operation_id: OperationId,
    pub key_locator: KeyLocator,
    pub from: CanonicalAddress,
    pub destination: CanonicalAddress,
}

impl fmt::Debug for NativeCollectionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeCollectionRequest")
            .field("operation_id", &"[REDACTED]")
            .field("key_locator", &"[REDACTED]")
            .field("from", &self.from)
            .field("destination", &self.destination)
            .finish()
    }
}

/// Input shared by ERC-20 collection requirement and signing calls.
#[derive(Clone, PartialEq, Eq)]
pub struct Erc20CollectionRequest {
    pub operation_id: OperationId,
    pub key_locator: KeyLocator,
    pub token: CanonicalAddress,
    pub from: CanonicalAddress,
    pub destination: CanonicalAddress,
    /// `None` asks WS to query and sweep the complete token balance.
    pub amount: Option<AtomicAmount>,
}

impl fmt::Debug for Erc20CollectionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Erc20CollectionRequest")
            .field("operation_id", &"[REDACTED]")
            .field("key_locator", &"[REDACTED]")
            .field("token", &self.token)
            .field("from", &self.from)
            .field("destination", &self.destination)
            .field("amount", &self.amount)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum CollectionRequest {
    Native(NativeCollectionRequest),
    Erc20(Erc20CollectionRequest),
}

impl fmt::Debug for CollectionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native(request) => request.fmt(formatter),
            Self::Erc20(request) => request.fmt(formatter),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalletCollectionRequirement {
    NativeGasBalance {
        address: CanonicalAddress,
        current: AtomicAmount,
        required: AtomicAmount,
        deficit: AtomicAmount,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletCollectionAttribution {
    pub address: CanonicalAddress,
    pub asset: AssetId,
    /// Gross deposit debit. A separately observed native gas fee is not included.
    pub gross_debit: AtomicAmount,
}

/// A signed collection result that has not been broadcast.
///
/// The envelope remains opaque and its storage type deliberately has no
/// `Debug` implementation. PS must persist these exact bytes before broadcast.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedCollection {
    pub transaction_id: CanonicalTransactionId,
    pub signed_envelope: SignedEnvelopeBytes,
    pub attribution: Vec<WalletCollectionAttribution>,
}

impl fmt::Debug for PreparedCollection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedCollection")
            .field("transaction_id", &self.transaction_id)
            .field("signed_envelope", &"[REDACTED]")
            .field("attribution", &self.attribution)
            .finish()
    }
}

impl PreparedCollection {
    pub(crate) fn inspect_eip1559_fees(
        &self,
    ) -> Result<EthereumEip1559FeeInspection, DepositError> {
        inspect_signed_envelope_fees(&self.transaction_id, &self.signed_envelope)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletReceipt {
    pub transaction_id: CanonicalTransactionId,
    pub included_in: Option<BlockRef>,
    pub succeeded: Option<bool>,
    pub confirmations: u64,
}

impl WalletClient {
    pub(crate) fn new(options: &WalletOptions) -> Result<Self, DepositError> {
        options
            .validate()
            .map_err(|error| invalid(error.to_string()))?;
        require_secure_endpoint(options.wallet_url.url())?;
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(options.request_timeout())
            .timeout(options.request_timeout())
            .build()
            .map_err(|_| unavailable("failed to construct the Wallet Service HTTP client"))?;
        Ok(Self {
            endpoint: options.wallet_url.clone(),
            bearer_token: options.bearer_token.clone(),
            request_timeout: options.request_timeout(),
            retry_attempts: options
                .retry_attempts()
                .map_err(|error| invalid(error.to_string()))?,
            retry_initial_backoff: options.retry_initial_backoff(),
            retry_max_backoff: options.retry_max_backoff(),
            client,
        })
    }

    async fn generate_address(
        &self,
        request: DepositAddressRequest,
    ) -> Result<GeneratedDepositAddress, DepositError> {
        if request.scope.chain.0 != ETHEREUM_CHAIN
            || request.scope.network.trim().is_empty()
            || request.asset.chain != request.scope.chain
            || request.key_purpose.trim().is_empty()
        {
            return Err(invalid(
                "Wallet Service address request has an invalid Ethereum scope, asset, or purpose",
            ));
        }
        OperationId::new(request.operation_id.clone())
            .map_err(|_| invalid("Wallet Service address operation ID is invalid"))?;
        let asset = asset_dto(&request.asset)?;
        let url = self.route(&["v1", "ethereum", "addresses"])?;
        let body = AddressRequestDto {
            operation_id: request.operation_id,
            asset,
            key_purpose: request.key_purpose,
        };
        // Retrying is safe because the exact caller-owned operation ID and body
        // are reused; this client never creates a replacement operation ID.
        let response: AddressResponseDto = self.send_json_safe(Method::POST, url, &body).await?;
        let address = canonical_address(&response.address)?;
        Ok(GeneratedDepositAddress {
            address,
            key: response.key_locator.try_into()?,
        })
    }

    pub(crate) async fn readiness(&self) -> Result<bool, DepositError> {
        let url = self.route(&["health", "ready"])?;
        let response = self
            .client
            .get(url)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    unavailable("Wallet Service readiness request timed out")
                } else {
                    unavailable("Wallet Service readiness endpoint is unavailable")
                }
            })?;
        match response.status() {
            StatusCode::OK => Ok(true),
            StatusCode::SERVICE_UNAVAILABLE => Ok(false),
            _ => Err(protocol(
                "Wallet Service readiness endpoint returned an unexpected status",
            )),
        }
    }

    /// Reads a factual native or ERC-20 balance. This read-only call may be
    /// retried after transient transport failures.
    pub async fn balance(
        &self,
        asset: &AssetId,
        address: &CanonicalAddress,
    ) -> Result<WalletBalance, DepositError> {
        let body = BalanceRequestDto {
            asset: asset_dto(asset)?,
            address: canonical_address_value(address)?,
        };
        let response: BalanceResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "ethereum", "balances"])?,
                &body,
            )
            .await?;
        Ok(WalletBalance {
            confirmed: canonical_amount(&response.confirmed, "confirmed balance")?,
            pending: canonical_amount(&response.pending, "pending balance")?,
            spendable: canonical_amount(&response.spendable, "spendable balance")?,
        })
    }

    /// Reports factual prerequisites without signing or broadcasting.
    pub async fn collection_requirements(
        &self,
        request: &CollectionRequest,
    ) -> Result<Vec<WalletCollectionRequirement>, DepositError> {
        let (collection, expected_from, native) = match request {
            CollectionRequest::Native(request) => (
                CollectionRequestDto::from_native(request)?,
                canonical_address(&request.from.value)?,
                true,
            ),
            CollectionRequest::Erc20(request) => (
                CollectionRequestDto::from_erc20(request)?,
                canonical_address(&request.from.value)?,
                false,
            ),
        };
        let response: CollectionRequirementsResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "ethereum", "collections", "requirements"])?,
                &collection,
            )
            .await?;
        if native && !response.requirements.is_empty() {
            return Err(protocol(
                "Wallet Service returned gas requirements for a native collection",
            ));
        }
        if response.requirements.len() > 1 {
            return Err(protocol(
                "Wallet Service returned duplicate Ethereum collection requirements",
            ));
        }
        response
            .requirements
            .into_iter()
            .map(|requirement| requirement.into_domain(&expected_from))
            .collect()
    }

    /// Builds and signs one native transfer without broadcasting it.
    ///
    /// PS uses this stateless operation for a durable ERC-20 gas-funding leg.
    /// Transient retries reuse the exact operation ID and request body.
    pub async fn sign_native_transfer(
        &self,
        request: &NativeTransferRequest,
    ) -> Result<PreparedTransaction, DepositError> {
        if request.value.is_zero() {
            return Err(invalid("native transfer value must be greater than zero"));
        }
        let body = NativeTransferRequestDto::from_domain(request)?;
        let response: SignedTransactionResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "ethereum", "transfers", "native", "sign"])?,
                &body,
            )
            .await?;
        response.into_domain()
    }

    /// Builds and signs one native collection without broadcasting it.
    /// Transient retries always reuse the exact caller-provided operation ID.
    pub async fn sign_native_collection(
        &self,
        request: &NativeCollectionRequest,
    ) -> Result<PreparedCollection, DepositError> {
        let expected_from = canonical_address(&request.from.value)?;
        let expected_asset = native_asset();
        let body = NativeCollectionRequestDto::from_domain(request)?;
        let response: PreparedCollectionResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "ethereum", "collections", "native", "sign"])?,
                &body,
            )
            .await?;
        response.into_domain(&expected_from, &expected_asset, None)
    }

    /// Builds and signs one ERC-20 collection without broadcasting it.
    /// Transient retries always reuse the exact caller-provided operation ID.
    pub async fn sign_erc20_collection(
        &self,
        request: &Erc20CollectionRequest,
    ) -> Result<PreparedCollection, DepositError> {
        let expected_from = canonical_address(&request.from.value)?;
        let expected_asset = token_asset(&request.token)?;
        let body = Erc20CollectionRequestDto::from_domain(request)?;
        let response: PreparedCollectionResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "ethereum", "collections", "erc20", "sign"])?,
                &body,
            )
            .await?;
        response.into_domain(&expected_from, &expected_asset, request.amount.as_ref())
    }

    /// Broadcasts the exact previously persisted envelope.
    ///
    /// The Keccak-256 transaction ID relationship is checked locally before
    /// any request. Retrying is safe because both the exact bytes and expected
    /// ID remain unchanged.
    pub async fn broadcast(
        &self,
        expected_transaction_id: &CanonicalTransactionId,
        signed_envelope: &SignedEnvelopeBytes,
    ) -> Result<CanonicalTransactionId, DepositError> {
        let expected = ethereum_transaction_id(expected_transaction_id)?;
        EthereumSignedTransaction::from_envelope(expected, signed_envelope.as_bytes().to_vec())
            .map_err(|_| {
                invalid("signed Ethereum envelope does not match its expected transaction ID")
            })?;
        let body = BroadcastRequestDto {
            expected_transaction_id: expected_transaction_id.value.clone(),
            signed_envelope: hex_prefixed(signed_envelope.as_bytes()),
        };
        let response: BroadcastResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "ethereum", "transactions", "broadcast"])?,
                &body,
            )
            .await?;
        let returned = canonical_transaction_id(&response.transaction_id)?;
        if &returned != expected_transaction_id {
            return Err(protocol(
                "Wallet Service broadcast response changed the expected transaction ID",
            ));
        }
        Ok(returned)
    }

    /// Reads the current factual receipt for a canonical Ethereum transaction.
    pub async fn receipt(
        &self,
        transaction_id: &CanonicalTransactionId,
    ) -> Result<Option<WalletReceipt>, DepositError> {
        ethereum_transaction_id(transaction_id)?;
        let body = ReceiptRequestDto {
            transaction_id: transaction_id.value.clone(),
        };
        let response: ReceiptResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "ethereum", "receipts"])?,
                &body,
            )
            .await?;
        let returned = canonical_transaction_id(&response.transaction_id)?;
        if &returned != transaction_id {
            return Err(protocol(
                "Wallet Service receipt response changed the requested transaction ID",
            ));
        }
        response
            .receipt
            .map(|receipt| receipt.into_domain(returned))
            .transpose()
    }

    fn route(&self, segments: &[&str]) -> Result<Url, DepositError> {
        let mut url = self.endpoint.url().clone();
        url.path_segments_mut()
            .map_err(|_| invalid("Wallet Service endpoint cannot be used as a base URL"))?
            .clear()
            .extend(segments);
        Ok(url)
    }

    /// Sends only requests that are safe to repeat verbatim. The serialized
    /// body is created once and cloned for retries, so signing operation IDs
    /// and signed envelope bytes cannot change between attempts.
    async fn send_json_safe<T, B>(
        &self,
        method: Method,
        url: Url,
        body: &B,
    ) -> Result<T, DepositError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let body = serde_json::to_vec(body)
            .map_err(|_| invalid("failed to encode Wallet Service request"))?;
        let mut attempt = 1_u32;
        loop {
            let response = self
                .client
                .request(method.clone(), url.clone())
                .bearer_auth(self.bearer_token.expose())
                .header(reqwest::header::ACCEPT, "application/json")
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .timeout(self.request_timeout)
                .body(body.clone())
                .send()
                .await;
            match response {
                Ok(response)
                    if retryable_status(response.status())
                        && attempt < self.retry_attempts.get() =>
                {
                    drop(response);
                    tokio::time::sleep(self.backoff_after(attempt)).await;
                    attempt += 1;
                }
                Err(error)
                    if (error.is_timeout() || error.is_connect() || error.is_request())
                        && attempt < self.retry_attempts.get() =>
                {
                    tokio::time::sleep(self.backoff_after(attempt)).await;
                    attempt += 1;
                }
                Ok(response) => return decode_response(response).await,
                Err(error) if error.is_timeout() => {
                    return Err(unavailable("Wallet Service request timed out"));
                }
                Err(error) if error.is_connect() || error.is_request() => {
                    return Err(unavailable("Wallet Service endpoint is unavailable"));
                }
                Err(_) => return Err(unavailable("Wallet Service request failed")),
            }
        }
    }

    fn backoff_after(&self, failed_attempt: u32) -> Duration {
        let exponent = failed_attempt.saturating_sub(1).min(31);
        let multiplier = 1_u32 << exponent;
        self.retry_initial_backoff
            .checked_mul(multiplier)
            .unwrap_or(self.retry_max_backoff)
            .min(self.retry_max_backoff)
    }
}

impl fmt::Debug for WalletClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WalletClient")
            .field("endpoint", &self.endpoint)
            .field("bearer_token", &self.bearer_token)
            .field("request_timeout", &self.request_timeout)
            .field("retry_attempts", &self.retry_attempts)
            .field("retry_initial_backoff", &self.retry_initial_backoff)
            .field("retry_max_backoff", &self.retry_max_backoff)
            .finish_non_exhaustive()
    }
}

impl DepositAddressSource for WalletClient {
    fn address<'a>(
        &'a self,
        request: DepositAddressRequest,
    ) -> BoxFuture<'a, Result<GeneratedDepositAddress, DepositError>> {
        Box::pin(async move { self.generate_address(request).await })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum AssetDto {
    Native,
    Erc20 { token: String },
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct AddressRequestDto {
    operation_id: String,
    asset: AssetDto,
    key_purpose: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddressResponseDto {
    address: String,
    key_locator: KeyLocatorDto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum KeyLocatorDto {
    Identifier { value: String },
    DerivationPath { children: Vec<ChildIndexDto> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildIndexDto {
    index: u32,
    hardened: bool,
}

impl KeyLocatorDto {
    fn from_locator(value: &KeyLocator) -> Result<Self, DepositError> {
        match value {
            KeyLocator::Identifier(value) => {
                validate_locator_identifier(value)?;
                Ok(Self::Identifier {
                    value: value.clone(),
                })
            }
            KeyLocator::DerivationPath(DerivationPath(children)) => {
                validate_derivation_path(children)?;
                Ok(Self::DerivationPath {
                    children: children
                        .iter()
                        .map(|child| ChildIndexDto {
                            index: child.index,
                            hardened: child.hardened,
                        })
                        .collect(),
                })
            }
        }
    }
}

impl TryFrom<KeyLocatorDto> for KeyLocator {
    type Error = DepositError;

    fn try_from(value: KeyLocatorDto) -> Result<Self, Self::Error> {
        match value {
            KeyLocatorDto::Identifier { value } => {
                validate_locator_identifier(&value)?;
                Ok(Self::Identifier(value))
            }
            KeyLocatorDto::DerivationPath { children } => {
                let children: Vec<_> = children
                    .into_iter()
                    .map(|child| ChildIndex {
                        index: child.index,
                        hardened: child.hardened,
                    })
                    .collect();
                validate_derivation_path(&children)?;
                Ok(Self::DerivationPath(DerivationPath(children)))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct BalanceRequestDto {
    asset: AssetDto,
    address: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BalanceResponseDto {
    confirmed: String,
    pending: String,
    spendable: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct NativeTransferRequestDto {
    operation_id: String,
    key_locator: KeyLocatorDto,
    from: String,
    to: String,
    value: String,
}

impl NativeTransferRequestDto {
    fn from_domain(request: &NativeTransferRequest) -> Result<Self, DepositError> {
        Ok(Self {
            operation_id: request.operation_id.as_str().to_owned(),
            key_locator: KeyLocatorDto::from_locator(&request.key_locator)?,
            from: canonical_address_value(&request.from)?,
            to: canonical_address_value(&request.to)?,
            value: request.value.to_string(),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedTransactionResponseDto {
    transaction_id: String,
    signed_envelope: String,
}

impl SignedTransactionResponseDto {
    fn into_domain(self) -> Result<PreparedTransaction, DepositError> {
        prepared_transaction(self.transaction_id, self.signed_envelope)
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    fn from_native(request: &NativeCollectionRequest) -> Result<Self, DepositError> {
        Ok(Self::Native {
            operation_id: request.operation_id.as_str().to_owned(),
            key_locator: KeyLocatorDto::from_locator(&request.key_locator)?,
            from: canonical_address_value(&request.from)?,
            destination: canonical_address_value(&request.destination)?,
        })
    }

    fn from_erc20(request: &Erc20CollectionRequest) -> Result<Self, DepositError> {
        Ok(Self::Erc20 {
            operation_id: request.operation_id.as_str().to_owned(),
            key_locator: KeyLocatorDto::from_locator(&request.key_locator)?,
            token: canonical_address_value(&request.token)?,
            from: canonical_address_value(&request.from)?,
            destination: canonical_address_value(&request.destination)?,
            amount: request.amount.as_ref().map(ToString::to_string),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionRequirementsResponseDto {
    requirements: Vec<CollectionRequirementDto>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CollectionRequirementDto {
    NativeGasBalance {
        address: String,
        current: String,
        required: String,
        deficit: String,
    },
}

impl CollectionRequirementDto {
    fn into_domain(
        self,
        expected_from: &CanonicalAddress,
    ) -> Result<WalletCollectionRequirement, DepositError> {
        match self {
            Self::NativeGasBalance {
                address,
                current,
                required,
                deficit,
            } => {
                let address = canonical_address(&address)?;
                if &address != expected_from {
                    return Err(protocol(
                        "Wallet Service requirement address differs from the collection source",
                    ));
                }
                let current = canonical_amount(&current, "current gas balance")?;
                let required = canonical_amount(&required, "required gas balance")?;
                let deficit = canonical_amount(&deficit, "gas balance deficit")?;
                let expected_deficit = required.checked_sub(&current).unwrap_or(AtomicAmount::ZERO);
                if deficit != expected_deficit || deficit.is_zero() {
                    return Err(protocol(
                        "Wallet Service returned an inconsistent gas balance deficit",
                    ));
                }
                Ok(WalletCollectionRequirement::NativeGasBalance {
                    address,
                    current,
                    required,
                    deficit,
                })
            }
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct NativeCollectionRequestDto {
    operation_id: String,
    key_locator: KeyLocatorDto,
    from: String,
    destination: String,
}

impl NativeCollectionRequestDto {
    fn from_domain(request: &NativeCollectionRequest) -> Result<Self, DepositError> {
        Ok(Self {
            operation_id: request.operation_id.as_str().to_owned(),
            key_locator: KeyLocatorDto::from_locator(&request.key_locator)?,
            from: canonical_address_value(&request.from)?,
            destination: canonical_address_value(&request.destination)?,
        })
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct Erc20CollectionRequestDto {
    operation_id: String,
    key_locator: KeyLocatorDto,
    token: String,
    from: String,
    destination: String,
    amount: Option<String>,
}

impl Erc20CollectionRequestDto {
    fn from_domain(request: &Erc20CollectionRequest) -> Result<Self, DepositError> {
        Ok(Self {
            operation_id: request.operation_id.as_str().to_owned(),
            key_locator: KeyLocatorDto::from_locator(&request.key_locator)?,
            token: canonical_address_value(&request.token)?,
            from: canonical_address_value(&request.from)?,
            destination: canonical_address_value(&request.destination)?,
            amount: request.amount.as_ref().map(ToString::to_string),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedCollectionResponseDto {
    transaction_id: String,
    signed_envelope: String,
    attribution: Vec<CollectionAttributionDto>,
}

impl PreparedCollectionResponseDto {
    fn into_domain(
        self,
        expected_from: &CanonicalAddress,
        expected_asset: &AssetId,
        requested_amount: Option<&AtomicAmount>,
    ) -> Result<PreparedCollection, DepositError> {
        let transaction = prepared_transaction(self.transaction_id, self.signed_envelope)?;
        if self.attribution.len() != 1 {
            return Err(protocol(
                "Wallet Service must return exactly one Ethereum collection attribution",
            ));
        }
        let attribution = self
            .attribution
            .into_iter()
            .next()
            .ok_or_else(|| protocol("Wallet Service collection attribution is missing"))?
            .into_domain()?;
        if &attribution.address != expected_from || &attribution.asset != expected_asset {
            return Err(protocol(
                "Wallet Service collection attribution differs from the request",
            ));
        }
        if attribution.gross_debit.is_zero()
            || requested_amount.is_some_and(|amount| amount != &attribution.gross_debit)
        {
            return Err(protocol(
                "Wallet Service collection attribution has an invalid gross debit",
            ));
        }
        Ok(PreparedCollection {
            transaction_id: transaction.transaction_id,
            signed_envelope: transaction.signed_envelope,
            attribution: vec![attribution],
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionAttributionDto {
    address: String,
    asset: AssetDto,
    gross_debit: String,
}

impl CollectionAttributionDto {
    fn into_domain(self) -> Result<WalletCollectionAttribution, DepositError> {
        Ok(WalletCollectionAttribution {
            address: canonical_address(&self.address)?,
            asset: asset_from_dto(self.asset)?,
            gross_debit: canonical_amount(&self.gross_debit, "collection gross debit")?,
        })
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct BroadcastRequestDto {
    expected_transaction_id: String,
    signed_envelope: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BroadcastResponseDto {
    transaction_id: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ReceiptRequestDto {
    transaction_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptResponseDto {
    transaction_id: String,
    receipt: Option<ReceiptDto>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptDto {
    included_in: Option<BlockRefDto>,
    succeeded: Option<bool>,
    confirmations: u64,
}

impl ReceiptDto {
    fn into_domain(
        self,
        transaction_id: CanonicalTransactionId,
    ) -> Result<WalletReceipt, DepositError> {
        Ok(WalletReceipt {
            transaction_id,
            included_in: self.included_in.map(BlockRefDto::into_domain).transpose()?,
            succeeded: self.succeeded,
            confirmations: self.confirmations,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockRefDto {
    height: u64,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<u64>,
}

impl BlockRefDto {
    fn into_domain(self) -> Result<BlockRef, DepositError> {
        Ok(BlockRef {
            height: BlockHeight(self.height),
            hash: BlockHash(canonical_fixed_hex(&self.hash, 32, "block hash")?),
            parent_hash: self
                .parent_hash
                .map(|hash| canonical_fixed_hex(&hash, 32, "parent block hash").map(BlockHash))
                .transpose()?,
            timestamp: self.timestamp,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorDto {
    code: String,
    #[allow(dead_code)]
    message: String,
    retryable: bool,
    #[allow(dead_code)]
    request_id: String,
}

async fn decode_response<T>(response: reqwest::Response) -> Result<T, DepositError>
where
    T: DeserializeOwned,
{
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        return Err(remote_error(status, &body));
    }
    serde_json::from_slice(&body)
        .map_err(|_| protocol("Wallet Service returned an invalid JSON response"))
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, DepositError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(protocol("Wallet Service response exceeds the size limit"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| unavailable("failed to read Wallet Service response"))?
    {
        let next = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| protocol("Wallet Service response size overflowed"))?;
        if next > MAX_RESPONSE_BYTES {
            return Err(protocol("Wallet Service response exceeds the size limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn remote_error(status: StatusCode, body: &[u8]) -> DepositError {
    let decoded = serde_json::from_slice::<ErrorDto>(body).ok();
    let code = decoded.as_ref().map(|error| error.code.as_str());
    match code {
        Some("operation_changed" | "conflict") => {
            conflict("Wallet Service operation ID was reused with different request content")
        }
        Some("insufficient_funds" | "transaction_rejected") => {
            invalid_state("Wallet Service cannot currently satisfy the operation")
        }
        Some("transaction_not_found") => not_found("Wallet Service transaction does not exist"),
        Some(
            "invalid_request"
            | "invalid_json"
            | "invalid_operation_id"
            | "invalid_key_locator"
            | "invalid_address"
            | "invalid_amount"
            | "invalid_hex"
            | "invalid_signed_envelope"
            | "invalid_transaction"
            | "unsupported_asset",
        ) => invalid("Wallet Service rejected the operation request"),
        _ if status == StatusCode::CONFLICT => conflict("Wallet Service request conflicts"),
        _ if status == StatusCode::NOT_FOUND => not_found("Wallet Service resource does not exist"),
        _ if status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY => {
            invalid("Wallet Service rejected the operation request")
        }
        _ if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN => {
            invalid("Wallet Service authentication was rejected")
        }
        _ if retryable_status(status) || decoded.as_ref().is_some_and(|error| error.retryable) => {
            unavailable("Wallet Service is temporarily unavailable")
        }
        _ => unavailable("Wallet Service request failed"),
    }
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn require_secure_endpoint(url: &Url) -> Result<(), DepositError> {
    if url.scheme() == "https" {
        return Ok(());
    }
    if url.scheme() != "http" {
        return Err(invalid("Wallet Service endpoint must use HTTP or HTTPS"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| invalid("Wallet Service endpoint must contain a host"))?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if loopback {
        Ok(())
    } else {
        Err(invalid(
            "plain HTTP Wallet Service endpoints are allowed only on loopback",
        ))
    }
}

fn native_asset() -> AssetId {
    AssetId {
        chain: ChainId(ETHEREUM_CHAIN.to_owned()),
        asset: NATIVE_ASSET.to_owned(),
    }
}

fn token_asset(token: &CanonicalAddress) -> Result<AssetId, DepositError> {
    Ok(AssetId {
        chain: ChainId(ETHEREUM_CHAIN.to_owned()),
        asset: canonical_address_value(token)?,
    })
}

fn asset_dto(asset: &AssetId) -> Result<AssetDto, DepositError> {
    if asset.chain.0 != ETHEREUM_CHAIN {
        return Err(invalid("Wallet Service asset does not belong to Ethereum"));
    }
    if asset.asset == NATIVE_ASSET {
        Ok(AssetDto::Native)
    } else {
        let token = canonical_address(&asset.asset)?;
        Ok(AssetDto::Erc20 { token: token.value })
    }
}

fn asset_from_dto(asset: AssetDto) -> Result<AssetId, DepositError> {
    match asset {
        AssetDto::Native => Ok(native_asset()),
        AssetDto::Erc20 { token } => {
            let token = canonical_address(&token)?;
            token_asset(&token)
        }
    }
}

fn canonical_address(value: &str) -> Result<CanonicalAddress, DepositError> {
    let address = value
        .parse::<EthereumAddress>()
        .map_err(|_| protocol("Wallet Service Ethereum address is invalid"))?;
    if address.to_string() != value {
        return Err(protocol("Wallet Service Ethereum address is not canonical"));
    }
    Ok(CanonicalAddress {
        chain: ChainId(ETHEREUM_CHAIN.to_owned()),
        value: value.to_owned(),
    })
}

fn canonical_address_value(address: &CanonicalAddress) -> Result<String, DepositError> {
    if address.chain.0 != ETHEREUM_CHAIN {
        return Err(invalid("address does not belong to Ethereum"));
    }
    canonical_address(&address.value).map(|address| address.value)
}

fn canonical_transaction_id(value: &str) -> Result<CanonicalTransactionId, DepositError> {
    let id = value
        .parse::<EthereumTransactionId>()
        .map_err(|_| protocol("Wallet Service Ethereum transaction ID is invalid"))?;
    Ok(CanonicalTransactionId {
        chain: ChainId(ETHEREUM_CHAIN.to_owned()),
        value: id.to_string(),
    })
}

fn ethereum_transaction_id(
    transaction_id: &CanonicalTransactionId,
) -> Result<EthereumTransactionId, DepositError> {
    if transaction_id.chain.0 != ETHEREUM_CHAIN {
        return Err(invalid("transaction ID does not belong to Ethereum"));
    }
    let id = transaction_id
        .value
        .parse::<EthereumTransactionId>()
        .map_err(|_| invalid("Ethereum transaction ID is not canonical"))?;
    if id.to_string() != transaction_id.value {
        return Err(invalid("Ethereum transaction ID is not canonical"));
    }
    Ok(id)
}

fn prepared_transaction(
    transaction_id: String,
    signed_envelope: String,
) -> Result<PreparedTransaction, DepositError> {
    let id = transaction_id
        .parse::<EthereumTransactionId>()
        .map_err(|_| protocol("Wallet Service returned an invalid Ethereum transaction ID"))?;
    let envelope = canonical_hex(&signed_envelope, "signed envelope")?;
    let signed = EthereumSignedTransaction::from_envelope(id, envelope).map_err(|_| {
        protocol("Wallet Service signed envelope does not match its transaction ID")
    })?;
    Ok(PreparedTransaction {
        transaction_id: canonical_transaction_id(&signed.id.to_string())?,
        signed_envelope: SignedEnvelopeBytes::new(signed.envelope)?,
    })
}

/// Revalidates the transaction-ID relationship, exact EIP-2718 encoding, and
/// EIP-1559 fee fields without formatting or returning the opaque envelope.
pub(crate) fn inspect_signed_envelope_fees(
    transaction_id: &CanonicalTransactionId,
    signed_envelope: &SignedEnvelopeBytes,
) -> Result<EthereumEip1559FeeInspection, DepositError> {
    let expected = ethereum_transaction_id(transaction_id)?;
    let signed =
        EthereumSignedTransaction::from_envelope(expected, signed_envelope.as_bytes().to_vec())
            .map_err(|_| {
                invalid("signed Ethereum envelope does not match its expected transaction ID")
            })?;
    signed
        .inspect_eip1559_fees()
        .map_err(|error| invalid(error.to_string()))
}

fn canonical_amount(value: &str, field: &str) -> Result<AtomicAmount, DepositError> {
    AtomicAmount::from_decimal_str(value)
        .map_err(|_| protocol(format!("Wallet Service returned an invalid {field}")))
}

fn validate_locator_identifier(value: &str) -> Result<(), DepositError> {
    if value.is_empty() || value.len() > 4_096 || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(protocol("Wallet Service key locator identifier is invalid"));
    }
    Ok(())
}

fn validate_derivation_path(children: &[ChildIndex]) -> Result<(), DepositError> {
    if children.is_empty() || children.len() > 64 {
        return Err(protocol("Wallet Service key derivation path is invalid"));
    }
    Ok(())
}

fn canonical_hex(value: &str, field: &str) -> Result<Vec<u8>, DepositError> {
    let hexadecimal = value
        .strip_prefix("0x")
        .ok_or_else(|| protocol(format!("Wallet Service {field} is missing its 0x prefix")))?;
    if hexadecimal.is_empty() || hexadecimal.len() % 2 != 0 {
        return Err(protocol(format!(
            "Wallet Service {field} does not contain complete bytes"
        )));
    }
    let mut decoded = Vec::with_capacity(hexadecimal.len() / 2);
    for pair in hexadecimal.as_bytes().chunks_exact(2) {
        let high = hex_nibble(pair[0])
            .ok_or_else(|| protocol(format!("Wallet Service {field} contains invalid hex")))?;
        let low = hex_nibble(pair[1])
            .ok_or_else(|| protocol(format!("Wallet Service {field} contains invalid hex")))?;
        decoded.push((high << 4) | low);
    }
    if hex_prefixed(&decoded) != value {
        return Err(protocol(format!(
            "Wallet Service {field} is not canonical lowercase hex"
        )));
    }
    Ok(decoded)
}

fn canonical_fixed_hex(
    value: &str,
    byte_length: usize,
    field: &str,
) -> Result<Vec<u8>, DepositError> {
    let decoded = canonical_hex(value, field)?;
    if decoded.len() != byte_length {
        return Err(protocol(format!(
            "Wallet Service {field} has an invalid byte length"
        )));
    }
    Ok(decoded)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hex_prefixed(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(2 + bytes.len() * 2);
    output.push_str("0x");
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn invalid(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}

fn conflict(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::Conflict,
        message: message.into(),
    }
}

fn invalid_state(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvalidState,
        message: message.into(),
    }
}

fn not_found(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::NotFound,
        message: message.into(),
    }
}

fn unavailable(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::Other,
        message: message.into(),
    }
}

fn protocol(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        body::Bytes,
        extract::{Request, State},
        http::header,
        response::{IntoResponse, Response},
        routing::any,
    };
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

    use super::*;

    const HELLO_TRANSACTION_ID: &str =
        "0x1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8";
    const FROM: &str = "0x1111111111111111111111111111111111111111";
    const DESTINATION: &str = "0x2222222222222222222222222222222222222222";
    const TOKEN: &str = "0x3333333333333333333333333333333333333333";

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ObservedRequest {
        path: String,
        authorization: Option<String>,
        body: Value,
    }

    #[derive(Clone, Default)]
    struct DoubleState {
        observed: Arc<Mutex<Vec<ObservedRequest>>>,
        native_sign_attempts: Arc<AtomicUsize>,
        broadcast_attempts: Arc<AtomicUsize>,
    }

    async fn wallet_double(State(state): State<DoubleState>, request: Request) -> Response {
        let path = request.uri().path().to_owned();
        let authorization = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .expect("HTTP-double request body must be readable");
        let body: Value =
            serde_json::from_slice(&body).expect("HTTP-double request body must be JSON");
        state.observed.lock().await.push(ObservedRequest {
            path: path.clone(),
            authorization,
            body,
        });

        match path.as_str() {
            "/v1/ethereum/balances" => Json(json!({
                "confirmed": "12",
                "pending": "1",
                "spendable": "11"
            }))
            .into_response(),
            "/v1/ethereum/collections/requirements" => {
                let body = state
                    .observed
                    .lock()
                    .await
                    .last()
                    .expect("recorded requirement request must exist")
                    .body
                    .clone();
                if body["kind"] == "native" {
                    Json(json!({ "requirements": [] })).into_response()
                } else {
                    Json(json!({
                        "requirements": [{
                            "kind": "native_gas_balance",
                            "address": FROM,
                            "current": "1",
                            "required": "3",
                            "deficit": "2"
                        }]
                    }))
                    .into_response()
                }
            }
            "/v1/ethereum/transfers/native/sign" => Json(json!({
                "transaction_id": HELLO_TRANSACTION_ID,
                "signed_envelope": "0x68656c6c6f"
            }))
            .into_response(),
            "/v1/ethereum/collections/native/sign" => {
                if state.native_sign_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "code": "custody_unavailable",
                            "message": "temporary",
                            "retryable": true,
                            "request_id": "request-1"
                        })),
                    )
                        .into_response();
                }
                prepared_response(AssetDto::Native)
            }
            "/v1/ethereum/collections/erc20/sign" => prepared_response(AssetDto::Erc20 {
                token: TOKEN.to_owned(),
            }),
            "/v1/ethereum/transactions/broadcast" => {
                if state.broadcast_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "code": "ethereum_rpc_unavailable",
                            "message": "temporary",
                            "retryable": true,
                            "request_id": "request-2"
                        })),
                    )
                        .into_response();
                }
                Json(json!({ "transaction_id": HELLO_TRANSACTION_ID })).into_response()
            }
            "/v1/ethereum/receipts" => Json(json!({
                "transaction_id": HELLO_TRANSACTION_ID,
                "receipt": {
                    "included_in": {
                        "height": 7,
                        "hash": format!("0x{}", "44".repeat(32)),
                        "parent_hash": format!("0x{}", "55".repeat(32)),
                        "timestamp": 10
                    },
                    "succeeded": true,
                    "confirmations": 3
                }
            }))
            .into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    fn prepared_response(asset: AssetDto) -> Response {
        Json(json!({
            "transaction_id": HELLO_TRANSACTION_ID,
            "signed_envelope": "0x68656c6c6f",
            "attribution": [{
                "address": FROM,
                "asset": asset,
                "gross_debit": "7"
            }]
        }))
        .into_response()
    }

    async fn spawn_double(router: Router) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("HTTP-double listener must bind");
        let address = listener
            .local_addr()
            .expect("HTTP-double listener address must exist");
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("HTTP-double server must run");
        });
        (format!("http://{address}"), task)
    }

    fn options(endpoint: &str, retry_attempts: u32) -> WalletOptions {
        WalletOptions {
            wallet_url: endpoint.parse().expect("endpoint must parse"),
            bearer_token: "wallet-secret".parse().expect("token must parse"),
            request_timeout_seconds: 2,
            retry_attempts,
            retry_initial_millis: 0,
            retry_max_millis: 0,
        }
    }

    fn address(value: &str) -> CanonicalAddress {
        CanonicalAddress {
            chain: ChainId(ETHEREUM_CHAIN.to_owned()),
            value: value.to_owned(),
        }
    }

    fn operation() -> OperationId {
        OperationId::new("collection-operation-7").expect("test operation ID must be valid")
    }

    fn locator() -> KeyLocator {
        KeyLocator::Identifier("opaque-key-7".to_owned())
    }

    #[tokio::test]
    async fn typed_collection_flow_preserves_ids_envelopes_and_safe_retries() {
        let state = DoubleState::default();
        let router = Router::new()
            .fallback(any(wallet_double))
            .with_state(state.clone());
        let (endpoint, server) = spawn_double(router).await;
        let client = WalletClient::new(&options(&endpoint, 2)).expect("client must build");
        let native = NativeCollectionRequest {
            operation_id: operation(),
            key_locator: locator(),
            from: address(FROM),
            destination: address(DESTINATION),
        };
        let token = Erc20CollectionRequest {
            operation_id: operation(),
            key_locator: locator(),
            token: address(TOKEN),
            from: address(FROM),
            destination: address(DESTINATION),
            amount: Some(AtomicAmount::from_decimal_str("7").expect("test amount must be valid")),
        };

        let balance = client
            .balance(&native_asset(), &native.from)
            .await
            .expect("balance must succeed");
        assert_eq!(balance.confirmed.to_string(), "12");
        assert_eq!(balance.pending.to_string(), "1");
        assert_eq!(balance.spendable.to_string(), "11");
        assert!(
            client
                .collection_requirements(&CollectionRequest::Native(native.clone()))
                .await
                .expect("native requirements must succeed")
                .is_empty()
        );
        let requirements = client
            .collection_requirements(&CollectionRequest::Erc20(token.clone()))
            .await
            .expect("token requirements must succeed");
        assert_eq!(requirements.len(), 1);

        let gas_funding = client
            .sign_native_transfer(&NativeTransferRequest {
                operation_id: OperationId::new("gas-funding-operation-7")
                    .expect("test operation ID must be valid"),
                key_locator: locator(),
                from: address(DESTINATION),
                to: address(FROM),
                value: AtomicAmount::from_decimal_str("2").expect("test amount must be valid"),
            })
            .await
            .expect("gas-funding signing must succeed");
        assert_eq!(gas_funding.transaction_id.value, HELLO_TRANSACTION_ID);
        assert_eq!(gas_funding.signed_envelope.as_bytes(), b"hello");

        let native_prepared = client
            .sign_native_collection(&native)
            .await
            .expect("native signing must succeed after one safe retry");
        assert_eq!(native_prepared.signed_envelope.as_bytes(), b"hello");
        assert_eq!(native_prepared.transaction_id.value, HELLO_TRANSACTION_ID);
        assert_eq!(native_prepared.attribution[0].asset, native_asset());
        let token_prepared = client
            .sign_erc20_collection(&token)
            .await
            .expect("token signing must succeed");
        assert_eq!(token_prepared.attribution[0].asset.asset, TOKEN);

        let broadcast_id = client
            .broadcast(
                &native_prepared.transaction_id,
                &native_prepared.signed_envelope,
            )
            .await
            .expect("exact-envelope broadcast must succeed after one safe retry");
        assert_eq!(broadcast_id, native_prepared.transaction_id);
        let receipt = client
            .receipt(&broadcast_id)
            .await
            .expect("receipt read must succeed")
            .expect("receipt must exist");
        assert_eq!(receipt.transaction_id, broadcast_id);
        assert_eq!(receipt.confirmations, 3);
        assert_eq!(
            receipt
                .included_in
                .expect("included receipt must have a block")
                .hash
                .0,
            vec![0x44; 32]
        );

        assert_eq!(state.native_sign_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(state.broadcast_attempts.load(Ordering::SeqCst), 2);
        let observed = state.observed.lock().await;
        assert!(
            observed.iter().all(|request| {
                request.authorization.as_deref() == Some("Bearer wallet-secret")
            })
        );
        let native_attempts: Vec<_> = observed
            .iter()
            .filter(|request| request.path == "/v1/ethereum/collections/native/sign")
            .collect();
        assert_eq!(native_attempts.len(), 2);
        assert_eq!(native_attempts[0].body, native_attempts[1].body);
        assert_eq!(
            native_attempts[0].body["operation_id"],
            "collection-operation-7"
        );
        let broadcast_attempts: Vec<_> = observed
            .iter()
            .filter(|request| request.path.ends_with("/transactions/broadcast"))
            .collect();
        assert_eq!(broadcast_attempts.len(), 2);
        assert_eq!(broadcast_attempts[0].body, broadcast_attempts[1].body);
        assert_eq!(
            broadcast_attempts[0].body["signed_envelope"],
            "0x68656c6c6f"
        );
        let gas_funding = observed
            .iter()
            .find(|request| request.path.ends_with("/transfers/native/sign"))
            .expect("gas-funding signing request must be observed");
        assert_eq!(gas_funding.body["operation_id"], "gas-funding-operation-7");
        assert_eq!(gas_funding.body["value"], "2");
        assert_eq!(gas_funding.body["from"], DESTINATION);
        assert_eq!(gas_funding.body["to"], FROM);
        drop(observed);
        server.abort();
    }

    #[tokio::test]
    async fn malformed_or_oversized_wallet_responses_are_rejected() {
        async fn malformed() -> impl IntoResponse {
            Json(json!({
                "transaction_id": format!("0x{}", "00".repeat(32)),
                "signed_envelope": "0x68656c6c6f",
                "attribution": [{
                    "address": FROM,
                    "asset": { "kind": "native" },
                    "gross_debit": "7"
                }],
                "unexpected": true
            }))
        }
        let (endpoint, server) = spawn_double(Router::new().fallback(any(malformed))).await;
        let client = WalletClient::new(&options(&endpoint, 1)).expect("client must build");
        let error = client
            .sign_native_collection(&NativeCollectionRequest {
                operation_id: operation(),
                key_locator: locator(),
                from: address(FROM),
                destination: address(DESTINATION),
            })
            .await
            .expect_err("unknown response fields must be rejected");
        assert_eq!(error.kind, DepositErrorKind::InvariantViolation);
        server.abort();

        async fn oversized() -> Bytes {
            Bytes::from(vec![b'x'; MAX_RESPONSE_BYTES + 1])
        }
        let (endpoint, server) = spawn_double(Router::new().fallback(any(oversized))).await;
        let client = WalletClient::new(&options(&endpoint, 1)).expect("client must build");
        let error = client
            .balance(&native_asset(), &address(FROM))
            .await
            .expect_err("oversized response must be rejected");
        assert_eq!(error.kind, DepositErrorKind::InvariantViolation);
        assert!(error.message.contains("size limit"));
        server.abort();
    }

    #[tokio::test]
    async fn mismatched_envelope_is_rejected_before_broadcast() {
        let state = DoubleState::default();
        let router = Router::new()
            .fallback(any(wallet_double))
            .with_state(state.clone());
        let (endpoint, server) = spawn_double(router).await;
        let client = WalletClient::new(&options(&endpoint, 1)).expect("client must build");
        let wrong_id = CanonicalTransactionId {
            chain: ChainId(ETHEREUM_CHAIN.to_owned()),
            value: format!("0x{}", "00".repeat(32)),
        };
        let envelope =
            SignedEnvelopeBytes::new(b"hello".to_vec()).expect("test envelope must be valid");
        let error = client
            .broadcast(&wrong_id, &envelope)
            .await
            .expect_err("mismatched envelope must fail locally");
        assert_eq!(error.kind, DepositErrorKind::InvariantViolation);
        assert_eq!(state.broadcast_attempts.load(Ordering::SeqCst), 0);
        assert!(state.observed.lock().await.is_empty());
        server.abort();
    }

    #[test]
    fn debug_output_redacts_endpoints_credentials_locators_and_envelopes() {
        let options = WalletOptions {
            wallet_url: "http://127.0.0.1:8082"
                .parse()
                .expect("endpoint must parse"),
            bearer_token: "wallet-secret".parse().expect("token must parse"),
            request_timeout_seconds: 1,
            retry_attempts: 1,
            retry_initial_millis: 0,
            retry_max_millis: 0,
        };
        let output = format!(
            "{:?}",
            WalletClient::new(&options).expect("client must build")
        );
        assert!(!output.contains("wallet-secret"));
        assert!(!output.contains("127.0.0.1"));
        assert!(output.contains("[REDACTED]"));

        let request = NativeCollectionRequest {
            operation_id: operation(),
            key_locator: locator(),
            from: address(FROM),
            destination: address(DESTINATION),
        };
        let request_debug = format!("{request:?}");
        assert!(!request_debug.contains("collection-operation-7"));
        assert!(!request_debug.contains("opaque-key-7"));

        let transfer = NativeTransferRequest {
            operation_id: OperationId::new("gas-funding-secret-id")
                .expect("test operation ID must be valid"),
            key_locator: locator(),
            from: address(DESTINATION),
            to: address(FROM),
            value: AtomicAmount::from_decimal_str("2").expect("test amount must be valid"),
        };
        let transfer_debug = format!("{transfer:?}");
        assert!(!transfer_debug.contains("gas-funding-secret-id"));
        assert!(!transfer_debug.contains("opaque-key-7"));

        let prepared = PreparedCollection {
            transaction_id: canonical_transaction_id(HELLO_TRANSACTION_ID)
                .expect("test transaction ID must be canonical"),
            signed_envelope: SignedEnvelopeBytes::new(b"hello".to_vec())
                .expect("test envelope must be valid"),
            attribution: Vec::new(),
        };
        assert!(!format!("{prepared:?}").contains("68656c6c6f"));
    }

    #[test]
    fn non_loopback_plain_http_and_noncanonical_values_are_rejected() {
        let error = WalletClient::new(&options("http://example.com", 1))
            .expect_err("external plaintext endpoint must fail");
        assert_eq!(error.kind, DepositErrorKind::InvariantViolation);
        assert!(canonical_address("0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").is_err());
        assert!(canonical_amount("01", "amount").is_err());
        assert!(canonical_fixed_hex("0xAA", 1, "hash").is_err());
    }
}
