//! Strict authenticated client for the stateless Bitcoin Wallet Service.
//!
//! The client retains chain-native Bitcoin types, validates every response
//! against the exact request, and never logs signed transaction bytes,
//! custody locators, operation IDs, credentials, or service endpoints.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::IpAddr,
    num::NonZeroU32,
    time::Duration,
};

use chain_bitcoin::{
    BitcoinAddress, BitcoinAddressKind, BitcoinNetwork, BitcoinOutPoint, BitcoinSignedTransaction,
    BitcoinSignedTransactionInspection, BitcoinTransactionId, BitcoinUtxo, Satoshi, SatoshisPerKvb,
    format_bitcoin_block_hash, parse_bitcoin_block_hash,
};
use chain_identity::{CanonicalAddress, ChainId};
use deposits::{
    BoxFuture, DepositAddressRequest, DepositAddressSource, DepositError, DepositErrorKind,
    GeneratedDepositAddress, SignedEnvelopeBytes,
};
use indexing::{BlockHeight, BlockRef};
use reqwest::{Method, StatusCode, Url, redirect::Policy};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use signer::{ChildIndex, DerivationPath, KeyLocator, OperationId};

use crate::config::{BearerSecret, IndexerEndpoint, WalletOptions};

const BITCOIN_CHAIN: &str = "bitcoin";
const NATIVE_ASSET: &str = "native";
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct BitcoinWalletClient {
    endpoint: IndexerEndpoint,
    bearer_token: BearerSecret,
    network: BitcoinNetwork,
    deposit_address_kind: BitcoinAddressKind,
    request_timeout: Duration,
    retry_attempts: NonZeroU32,
    retry_initial_backoff: Duration,
    retry_max_backoff: Duration,
    client: reqwest::Client,
}

/// One exact PS-reserved previous output supplied for collection signing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinCollectionInput {
    pub outpoint: BitcoinOutPoint,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
}

/// Exact inputs and custody locator associated with one deposit address.
#[derive(Clone, PartialEq, Eq)]
pub struct BitcoinWalletCollectionSource {
    pub address: BitcoinAddress,
    pub key_locator: KeyLocator,
    pub inputs: Vec<BitcoinCollectionInput>,
}

impl fmt::Debug for BitcoinWalletCollectionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitcoinWalletCollectionSource")
            .field("address", &self.address)
            .field("key_locator", &"[REDACTED]")
            .field("inputs", &self.inputs)
            .finish()
    }
}

/// One exact-input native Bitcoin collection signing request.
#[derive(Clone, PartialEq, Eq)]
pub struct BitcoinSignCollectionRequest {
    pub operation_id: OperationId,
    pub sources: Vec<BitcoinWalletCollectionSource>,
    pub destination: BitcoinAddress,
    pub fee_rate: SatoshisPerKvb,
}

impl fmt::Debug for BitcoinSignCollectionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitcoinSignCollectionRequest")
            .field("operation_id", &"[REDACTED]")
            .field("sources", &self.sources)
            .field("destination", &self.destination)
            .field("fee_rate", &self.fee_rate)
            .finish()
    }
}

/// Factual gross input attribution returned for one collection source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinWalletCollectionAttribution {
    pub address: BitcoinAddress,
    pub gross_input: Satoshi,
}

/// A locally verified signed Bitcoin collection that has not been broadcast.
///
/// `raw_transaction` has no `Debug` implementation and this aggregate's
/// custom formatter redacts it. PS must durably persist these exact bytes and
/// the expected transaction ID before asking Wallet Service to broadcast.
#[derive(Clone, PartialEq, Eq)]
pub struct BitcoinPreparedCollection {
    pub transaction_id: BitcoinTransactionId,
    pub raw_transaction: SignedEnvelopeBytes,
    pub inspection: BitcoinSignedTransactionInspection,
    pub fee: Satoshi,
    pub attribution: Vec<BitcoinWalletCollectionAttribution>,
}

impl fmt::Debug for BitcoinPreparedCollection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitcoinPreparedCollection")
            .field("transaction_id", &self.transaction_id)
            .field("raw_transaction", &"[REDACTED]")
            .field("inspection", &self.inspection)
            .field("fee", &self.fee)
            .field("attribution", &self.attribution)
            .finish()
    }
}

/// Current Bitcoin Core receipt facts returned by Wallet Service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinWalletReceipt {
    pub transaction_id: BitcoinTransactionId,
    pub included_in: Option<BlockRef>,
    pub confirmations: u64,
    pub replaced_by: Option<BitcoinTransactionId>,
}

impl BitcoinWalletClient {
    pub fn new(
        options: &WalletOptions,
        network: BitcoinNetwork,
        deposit_address_kind: BitcoinAddressKind,
    ) -> Result<Self, DepositError> {
        options
            .validate()
            .map_err(|error| invalid(error.to_string()))?;
        require_secure_endpoint(options.wallet_url.url())?;
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(options.request_timeout())
            .timeout(options.request_timeout())
            .build()
            .map_err(|_| unavailable("failed to construct the Bitcoin Wallet HTTP client"))?;
        Ok(Self {
            endpoint: options.wallet_url.clone(),
            bearer_token: options.bearer_token.clone(),
            network,
            deposit_address_kind,
            request_timeout: options.request_timeout(),
            retry_attempts: options
                .retry_attempts()
                .map_err(|error| invalid(error.to_string()))?,
            retry_initial_backoff: options.retry_initial_backoff(),
            retry_max_backoff: options.retry_max_backoff(),
            client,
        })
    }

    pub async fn readiness(&self) -> Result<bool, DepositError> {
        let url = self.route(&["health", "ready"])?;
        let response = self
            .client
            .get(url)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    unavailable("Bitcoin Wallet readiness request timed out")
                } else {
                    unavailable("Bitcoin Wallet readiness endpoint is unavailable")
                }
            })?;
        match response.status() {
            StatusCode::OK => Ok(true),
            StatusCode::SERVICE_UNAVAILABLE => Ok(false),
            _ => Err(protocol(
                "Bitcoin Wallet readiness endpoint returned an unexpected status",
            )),
        }
    }

    async fn generate_address(
        &self,
        request: DepositAddressRequest,
    ) -> Result<GeneratedDepositAddress, DepositError> {
        validate_deposit_address_request(&request, self.network)?;
        let body = GenerateAddressRequestDto {
            operation_id: request.operation_id,
            address_kind: self.deposit_address_kind.into(),
            key_purpose: request.key_purpose,
        };
        let response: GenerateAddressResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "bitcoin", "addresses"])?,
                &body,
            )
            .await?;
        let address = response_address(&response.address, self.network)?;
        let script_pubkey = address
            .script_pubkey_for_network(self.network)
            .map_err(|_| protocol("Bitcoin Wallet returned an address with an invalid script"))?;
        let kind_matches = match self.deposit_address_kind {
            BitcoinAddressKind::SegwitV0 => script_pubkey.is_p2wpkh(),
            BitcoinAddressKind::Taproot => script_pubkey.is_p2tr(),
        };
        if !kind_matches {
            return Err(protocol(
                "Bitcoin Wallet returned an address with a script kind that differs from policy",
            ));
        }
        Ok(GeneratedDepositAddress {
            address: CanonicalAddress {
                chain: ChainId(BITCOIN_CHAIN.to_owned()),
                value: address.0,
            },
            key: response.key_locator.into_locator()?,
        })
    }

    pub async fn sign_collection(
        &self,
        request: &BitcoinSignCollectionRequest,
    ) -> Result<BitcoinPreparedCollection, DepositError> {
        let validated = ValidatedCollection::from_request(request, self.network)?;
        let response: PreparedCollectionResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "bitcoin", "collections", "sign"])?,
                &validated.body,
            )
            .await?;
        response.into_domain(&validated, self.network)
    }

    /// Broadcasts the exact previously verified and persisted raw transaction.
    pub async fn broadcast(
        &self,
        expected_transaction_id: BitcoinTransactionId,
        raw_transaction: &SignedEnvelopeBytes,
    ) -> Result<BitcoinTransactionId, DepositError> {
        BitcoinSignedTransaction::from_consensus_bytes(
            expected_transaction_id,
            raw_transaction.as_bytes().to_vec(),
        )
        .map_err(|_| {
            invalid("signed Bitcoin transaction does not match its expected transaction ID")
        })?;
        let body = BroadcastRequestDto {
            expected_transaction_id: expected_transaction_id.to_string(),
            raw_transaction: hex_prefixed(raw_transaction.as_bytes()),
        };
        let response: BroadcastResponseDto = self
            .send_json_once(
                Method::POST,
                self.route(&["v1", "bitcoin", "transactions", "broadcast"])?,
                &body,
            )
            .await?;
        let returned = response_transaction_id(&response.transaction_id)?;
        if returned != expected_transaction_id {
            return Err(protocol(
                "Bitcoin Wallet broadcast response changed the expected transaction ID",
            ));
        }
        Ok(returned)
    }

    pub async fn receipt(
        &self,
        transaction_id: BitcoinTransactionId,
    ) -> Result<Option<BitcoinWalletReceipt>, DepositError> {
        let response: ReceiptResponseDto = self
            .send_json_safe(
                Method::POST,
                self.route(&["v1", "bitcoin", "receipts"])?,
                &ReceiptRequestDto {
                    transaction_id: transaction_id.to_string(),
                },
            )
            .await?;
        let returned = response_transaction_id(&response.transaction_id)?;
        if returned != transaction_id {
            return Err(protocol(
                "Bitcoin Wallet receipt response changed the requested transaction ID",
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
            .map_err(|_| invalid("Bitcoin Wallet endpoint cannot be used as a base URL"))?
            .clear()
            .extend(segments);
        Ok(url)
    }

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
            .map_err(|_| invalid("failed to encode Bitcoin Wallet request"))?;
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
                    return Err(unavailable("Bitcoin Wallet request timed out"));
                }
                Err(error) if error.is_connect() || error.is_request() => {
                    return Err(unavailable("Bitcoin Wallet endpoint is unavailable"));
                }
                Err(_) => return Err(unavailable("Bitcoin Wallet request failed")),
            }
        }
    }

    /// Broadcast transport ambiguity is returned to the durable workflow
    /// without an in-client retry. PS must check the receipt first and only a
    /// later workflow attempt may submit these same persisted bytes again.
    async fn send_json_once<T, B>(
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
            .map_err(|_| invalid("failed to encode Bitcoin Wallet request"))?;
        let response = self
            .client
            .request(method, url)
            .bearer_auth(self.bearer_token.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .timeout(self.request_timeout)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    unavailable("Bitcoin Wallet request timed out")
                } else {
                    unavailable("Bitcoin Wallet endpoint is unavailable")
                }
            })?;
        decode_response(response).await
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

impl fmt::Debug for BitcoinWalletClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitcoinWalletClient")
            .field("endpoint", &self.endpoint)
            .field("bearer_token", &self.bearer_token)
            .field("network", &self.network)
            .field("deposit_address_kind", &self.deposit_address_kind)
            .field("request_timeout", &self.request_timeout)
            .field("retry_attempts", &self.retry_attempts)
            .field("retry_initial_backoff", &self.retry_initial_backoff)
            .field("retry_max_backoff", &self.retry_max_backoff)
            .finish_non_exhaustive()
    }
}

impl DepositAddressSource for BitcoinWalletClient {
    fn address<'a>(
        &'a self,
        request: DepositAddressRequest,
    ) -> BoxFuture<'a, Result<GeneratedDepositAddress, DepositError>> {
        Box::pin(async move { self.generate_address(request).await })
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum AddressKindDto {
    P2wpkh,
    P2tr,
}

impl From<BitcoinAddressKind> for AddressKindDto {
    fn from(value: BitcoinAddressKind) -> Self {
        match value {
            BitcoinAddressKind::SegwitV0 => Self::P2wpkh,
            BitcoinAddressKind::Taproot => Self::P2tr,
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct GenerateAddressRequestDto {
    operation_id: String,
    address_kind: AddressKindDto,
    key_purpose: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateAddressResponseDto {
    address: String,
    key_locator: KeyLocatorDto,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum KeyLocatorDto {
    Identifier { value: String },
    DerivationPath { children: Vec<ChildIndexDto> },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildIndexDto {
    index: u32,
    hardened: bool,
}

impl KeyLocatorDto {
    fn from_locator(locator: &KeyLocator) -> Result<Self, DepositError> {
        match locator {
            KeyLocator::Identifier(value) => {
                validate_locator_identifier(value, false)?;
                Ok(Self::Identifier {
                    value: value.clone(),
                })
            }
            KeyLocator::DerivationPath(DerivationPath(children)) => {
                validate_derivation_path(children, false)?;
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

    fn into_locator(self) -> Result<KeyLocator, DepositError> {
        match self {
            Self::Identifier { value } => {
                validate_locator_identifier(&value, true)?;
                Ok(KeyLocator::Identifier(value))
            }
            Self::DerivationPath { children } => {
                let children = children
                    .into_iter()
                    .map(|child| ChildIndex {
                        index: child.index,
                        hardened: child.hardened,
                    })
                    .collect::<Vec<_>>();
                validate_derivation_path(&children, true)?;
                Ok(KeyLocator::DerivationPath(DerivationPath(children)))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SignCollectionRequestDto {
    operation_id: String,
    sources: Vec<CollectionSourceDto>,
    destination: String,
    fee_rate_satoshis_per_kvb: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct CollectionSourceDto {
    address: String,
    key_locator: KeyLocatorDto,
    inputs: Vec<CollectionInputDto>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct CollectionInputDto {
    transaction_id: String,
    output_index: String,
    value_satoshis: String,
    script_pubkey: String,
}

struct ValidatedCollection {
    body: SignCollectionRequestDto,
    destination: BitcoinAddress,
    outpoints: BTreeSet<BitcoinOutPoint>,
    gross_by_address: BTreeMap<BitcoinAddress, Satoshi>,
    total_input: Satoshi,
}

impl ValidatedCollection {
    fn from_request(
        request: &BitcoinSignCollectionRequest,
        network: BitcoinNetwork,
    ) -> Result<Self, DepositError> {
        if request.sources.is_empty() {
            return Err(invalid("Bitcoin collection requires at least one source"));
        }
        if request.fee_rate.satoshis_per_kvb() == 0 {
            return Err(invalid(
                "Bitcoin collection fee rate must be greater than zero",
            ));
        }
        let destination = request_address(&request.destination.0, network)?;
        let mut addresses = BTreeSet::new();
        let mut outpoints = BTreeSet::new();
        let mut gross_by_address = BTreeMap::new();
        let mut total_input = 0_u64;
        let mut source_dtos = Vec::with_capacity(request.sources.len());
        for source in &request.sources {
            let address = request_address(&source.address.0, network)?;
            if !addresses.insert(address.clone()) {
                return Err(invalid(
                    "Bitcoin collection source addresses must be unique",
                ));
            }
            if source.inputs.is_empty() {
                return Err(invalid(
                    "every Bitcoin collection source must supply at least one exact input",
                ));
            }
            let key_locator = KeyLocatorDto::from_locator(&source.key_locator)?;
            let mut gross = 0_u64;
            let mut input_dtos = Vec::with_capacity(source.inputs.len());
            for input in &source.inputs {
                if input.value.0 == 0 {
                    return Err(invalid("Bitcoin collection input value must be nonzero"));
                }
                if !outpoints.insert(input.outpoint) {
                    return Err(invalid("Bitcoin collection outpoints must be unique"));
                }
                BitcoinUtxo::from_exact_selection(
                    network,
                    &address,
                    source.key_locator.clone(),
                    input.outpoint.transaction_id,
                    input.outpoint.output_index,
                    input.value,
                    input.script_pubkey.clone(),
                )
                .map_err(|_| {
                    invalid("Bitcoin collection input script must match a supported source address")
                })?;
                gross = gross
                    .checked_add(input.value.0)
                    .ok_or_else(|| invalid("Bitcoin source input value overflowed u64"))?;
                total_input = total_input
                    .checked_add(input.value.0)
                    .ok_or_else(|| invalid("Bitcoin collection input value overflowed u64"))?;
                input_dtos.push(CollectionInputDto {
                    transaction_id: input.outpoint.transaction_id.to_string(),
                    output_index: input.outpoint.output_index.to_string(),
                    value_satoshis: input.value.0.to_string(),
                    script_pubkey: hex_prefixed(&input.script_pubkey),
                });
            }
            gross_by_address.insert(address.clone(), Satoshi(gross));
            source_dtos.push(CollectionSourceDto {
                address: address.0,
                key_locator,
                inputs: input_dtos,
            });
        }
        Ok(Self {
            body: SignCollectionRequestDto {
                operation_id: request.operation_id.as_str().to_owned(),
                sources: source_dtos,
                destination: destination.0.clone(),
                fee_rate_satoshis_per_kvb: request.fee_rate.satoshis_per_kvb().to_string(),
            },
            destination,
            outpoints,
            gross_by_address,
            total_input: Satoshi(total_input),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedCollectionResponseDto {
    transaction_id: String,
    raw_transaction: String,
    selected_outpoints: Vec<OutpointDto>,
    outputs: Vec<OutputResponseDto>,
    fee_satoshis: String,
    virtual_size: String,
    attribution: Vec<CollectionAttributionDto>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutpointDto {
    transaction_id: String,
    output_index: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputResponseDto {
    address: String,
    value_satoshis: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionAttributionDto {
    address: String,
    gross_input_satoshis: String,
}

impl PreparedCollectionResponseDto {
    fn into_domain(
        self,
        expected: &ValidatedCollection,
        network: BitcoinNetwork,
    ) -> Result<BitcoinPreparedCollection, DepositError> {
        let transaction_id = response_transaction_id(&self.transaction_id)?;
        let raw = response_hex(&self.raw_transaction, "raw transaction")?;
        let transaction = BitcoinSignedTransaction::from_consensus_bytes(transaction_id, raw)
            .map_err(|_| {
                protocol("Bitcoin Wallet raw transaction does not match its transaction ID")
            })?;
        let inspection = transaction
            .inspect()
            .map_err(|_| protocol("Bitcoin Wallet returned an invalid signed transaction"))?;

        let selected = parse_response_outpoints(self.selected_outpoints)?;
        let inspected = inspection
            .inputs
            .iter()
            .map(|input| input.outpoint)
            .collect::<BTreeSet<_>>();
        if inspection.inputs.len() != expected.outpoints.len()
            || selected != expected.outpoints
            || inspected != expected.outpoints
        {
            return Err(protocol(
                "Bitcoin Wallet signed transaction inputs differ from the exact reservation",
            ));
        }

        if self.outputs.len() != inspection.outputs.len() || self.outputs.len() != 1 {
            return Err(protocol(
                "Bitcoin Wallet collection must return exactly one destination output",
            ));
        }
        let output = self
            .outputs
            .into_iter()
            .next()
            .ok_or_else(|| protocol("Bitcoin Wallet collection output is missing"))?;
        let output_address = response_address(&output.address, network)?;
        let output_value = Satoshi(response_nonzero_decimal(
            &output.value_satoshis,
            "output value_satoshis",
        )?);
        let inspected_output = inspection
            .outputs
            .first()
            .ok_or_else(|| protocol("Bitcoin Wallet signed transaction output is missing"))?;
        let expected_script = output_address
            .script_pubkey_for_network(network)
            .map_err(|_| protocol("Bitcoin Wallet output address is invalid"))?;
        if output_address != expected.destination
            || output_value != inspected_output.value
            || expected_script.as_bytes() != inspected_output.script_pubkey
        {
            return Err(protocol(
                "Bitcoin Wallet output metadata differs from the signed transaction or destination",
            ));
        }

        let fee = Satoshi(response_nonzero_decimal(
            &self.fee_satoshis,
            "fee_satoshis",
        )?);
        let expected_fee = expected
            .total_input
            .0
            .checked_sub(output_value.0)
            .ok_or_else(|| protocol("Bitcoin Wallet collection output exceeds its inputs"))?;
        if fee.0 != expected_fee {
            return Err(protocol(
                "Bitcoin Wallet fee differs from exact input and output values",
            ));
        }
        let virtual_size = response_nonzero_decimal(&self.virtual_size, "virtual_size")?;
        if virtual_size != inspection.virtual_size {
            return Err(protocol(
                "Bitcoin Wallet virtual size differs from the signed transaction",
            ));
        }

        let attribution = parse_attribution(self.attribution, expected, network)?;
        let raw_transaction = SignedEnvelopeBytes::new(transaction.into_consensus_bytes())?;
        Ok(BitcoinPreparedCollection {
            transaction_id,
            raw_transaction,
            inspection,
            fee,
            attribution,
        })
    }
}

fn parse_response_outpoints(
    outpoints: Vec<OutpointDto>,
) -> Result<BTreeSet<BitcoinOutPoint>, DepositError> {
    let mut parsed = BTreeSet::new();
    for outpoint in outpoints {
        let outpoint = BitcoinOutPoint {
            transaction_id: response_transaction_id(&outpoint.transaction_id)?,
            output_index: response_decimal_u32(&outpoint.output_index, "output_index")?,
        };
        if !parsed.insert(outpoint) {
            return Err(protocol(
                "Bitcoin Wallet returned duplicate selected outpoints",
            ));
        }
    }
    Ok(parsed)
}

fn parse_attribution(
    attribution: Vec<CollectionAttributionDto>,
    expected: &ValidatedCollection,
    network: BitcoinNetwork,
) -> Result<Vec<BitcoinWalletCollectionAttribution>, DepositError> {
    if attribution.len() != expected.gross_by_address.len() {
        return Err(protocol(
            "Bitcoin Wallet attribution does not cover every collection source",
        ));
    }
    let mut returned = BTreeSet::new();
    let mut total = 0_u64;
    let mut result = Vec::with_capacity(attribution.len());
    for item in attribution {
        let address = response_address(&item.address, network)?;
        if !returned.insert(address.clone()) {
            return Err(protocol(
                "Bitcoin Wallet returned duplicate source attribution",
            ));
        }
        let gross_input = Satoshi(response_nonzero_decimal(
            &item.gross_input_satoshis,
            "gross_input_satoshis",
        )?);
        if expected.gross_by_address.get(&address) != Some(&gross_input) {
            return Err(protocol(
                "Bitcoin Wallet source attribution differs from the exact inputs",
            ));
        }
        total = total
            .checked_add(gross_input.0)
            .ok_or_else(|| protocol("Bitcoin Wallet attribution overflowed u64"))?;
        result.push(BitcoinWalletCollectionAttribution {
            address,
            gross_input,
        });
    }
    if total != expected.total_input.0 {
        return Err(protocol(
            "Bitcoin Wallet attribution total differs from the exact inputs",
        ));
    }
    Ok(result)
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct BroadcastRequestDto {
    expected_transaction_id: String,
    raw_transaction: String,
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
    confirmations: u64,
    replaced_by: Option<String>,
}

impl ReceiptDto {
    fn into_domain(
        self,
        transaction_id: BitcoinTransactionId,
    ) -> Result<BitcoinWalletReceipt, DepositError> {
        let included_in = self.included_in.map(BlockRefDto::into_domain).transpose()?;
        if (included_in.is_some() && self.confirmations == 0)
            || (included_in.is_none() && self.confirmations != 0)
        {
            return Err(protocol(
                "Bitcoin Wallet receipt inclusion and confirmations are inconsistent",
            ));
        }
        let replaced_by = self
            .replaced_by
            .map(|id| response_transaction_id(&id))
            .transpose()?;
        if replaced_by == Some(transaction_id) || (included_in.is_some() && replaced_by.is_some()) {
            return Err(protocol(
                "Bitcoin Wallet receipt replacement facts are inconsistent",
            ));
        }
        Ok(BitcoinWalletReceipt {
            transaction_id,
            included_in,
            confirmations: self.confirmations,
            replaced_by,
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
        let hash = response_block_hash(&self.hash, "block hash")?;
        let parent_hash = self
            .parent_hash
            .map(|value| response_block_hash(&value, "parent block hash"))
            .transpose()?;
        if self.height == 0 && parent_hash.is_some() {
            return Err(protocol(
                "Bitcoin genesis block must not contain a parent hash",
            ));
        }
        if self.height > 0 && parent_hash.is_none() {
            return Err(protocol(
                "Bitcoin non-genesis block must contain a parent hash",
            ));
        }
        Ok(BlockRef {
            height: BlockHeight(self.height),
            hash,
            parent_hash,
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
        .map_err(|_| protocol("Bitcoin Wallet returned an invalid JSON response"))
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, DepositError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(protocol("Bitcoin Wallet response exceeds the size limit"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| unavailable("failed to read Bitcoin Wallet response"))?
    {
        let next = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| protocol("Bitcoin Wallet response size overflowed"))?;
        if next > MAX_RESPONSE_BYTES {
            return Err(protocol("Bitcoin Wallet response exceeds the size limit"));
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
            conflict("Bitcoin Wallet operation ID was reused with different request content")
        }
        Some("insufficient_funds" | "transaction_rejected") => {
            invalid_state("Bitcoin Wallet cannot currently satisfy the operation")
        }
        Some("transaction_not_found") => not_found("Bitcoin Wallet transaction does not exist"),
        Some(
            "invalid_request"
            | "invalid_json"
            | "invalid_operation_id"
            | "invalid_key_locator"
            | "invalid_key_purpose"
            | "invalid_address"
            | "invalid_amount"
            | "invalid_hex"
            | "invalid_integer"
            | "invalid_input"
            | "invalid_collection"
            | "invalid_signed_transaction"
            | "invalid_transaction"
            | "invalid_transaction_id"
            | "unsupported_asset",
        ) => invalid("Bitcoin Wallet rejected the operation request"),
        _ if status == StatusCode::CONFLICT => conflict("Bitcoin Wallet request conflicts"),
        _ if status == StatusCode::NOT_FOUND => not_found("Bitcoin Wallet resource does not exist"),
        _ if status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY => {
            invalid("Bitcoin Wallet rejected the operation request")
        }
        _ if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN => {
            invalid("Bitcoin Wallet authentication was rejected")
        }
        _ if retryable_status(status) || decoded.as_ref().is_some_and(|error| error.retryable) => {
            unavailable("Bitcoin Wallet is temporarily unavailable")
        }
        _ => unavailable("Bitcoin Wallet request failed"),
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
        return Err(invalid("Bitcoin Wallet endpoint must use HTTP or HTTPS"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| invalid("Bitcoin Wallet endpoint must contain a host"))?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if loopback {
        Ok(())
    } else {
        Err(invalid(
            "plain HTTP Bitcoin Wallet endpoints are allowed only on loopback",
        ))
    }
}

fn validate_deposit_address_request(
    request: &DepositAddressRequest,
    network: BitcoinNetwork,
) -> Result<(), DepositError> {
    if request.scope.chain.0 != BITCOIN_CHAIN
        || request.scope.network != network.canonical_name()
        || request.asset.chain != request.scope.chain
        || request.asset.asset != NATIVE_ASSET
    {
        return Err(invalid(
            "Bitcoin Wallet address request has an invalid scope, network, or asset",
        ));
    }
    OperationId::new(request.operation_id.clone())
        .map_err(|_| invalid("Bitcoin Wallet address operation ID is invalid"))?;
    if request.key_purpose.trim().is_empty()
        || request.key_purpose.len() > 1_024
        || request
            .key_purpose
            .bytes()
            .any(|byte| byte.is_ascii_control())
    {
        return Err(invalid("Bitcoin Wallet key purpose is invalid"));
    }
    Ok(())
}

fn request_address(value: &str, network: BitcoinNetwork) -> Result<BitcoinAddress, DepositError> {
    let address = BitcoinAddress::parse_for_network(value, network)
        .map_err(|_| invalid("Bitcoin address is invalid or belongs to another network"))?;
    if address.0 != value {
        return Err(invalid("Bitcoin address is not canonical"));
    }
    Ok(address)
}

fn response_address(value: &str, network: BitcoinNetwork) -> Result<BitcoinAddress, DepositError> {
    let address = BitcoinAddress::parse_for_network(value, network)
        .map_err(|_| protocol("Bitcoin Wallet returned an invalid or wrong-network address"))?;
    if address.0 != value {
        return Err(protocol("Bitcoin Wallet returned a non-canonical address"));
    }
    Ok(address)
}

fn response_transaction_id(value: &str) -> Result<BitcoinTransactionId, DepositError> {
    let id = value
        .parse::<BitcoinTransactionId>()
        .map_err(|_| protocol("Bitcoin Wallet returned an invalid transaction ID"))?;
    if id.to_string() != value {
        return Err(protocol(
            "Bitcoin Wallet returned a non-canonical transaction ID",
        ));
    }
    Ok(id)
}

fn response_decimal(value: &str, field: &str) -> Result<u64, DepositError> {
    let parsed = value.parse::<u64>().map_err(|_| {
        protocol(format!(
            "Bitcoin Wallet returned an invalid {field} decimal value"
        ))
    })?;
    if parsed.to_string() != value {
        return Err(protocol(format!(
            "Bitcoin Wallet returned a non-canonical {field} decimal value"
        )));
    }
    Ok(parsed)
}

fn response_nonzero_decimal(value: &str, field: &str) -> Result<u64, DepositError> {
    let parsed = response_decimal(value, field)?;
    if parsed == 0 {
        return Err(protocol(format!(
            "Bitcoin Wallet returned a zero {field} decimal value"
        )));
    }
    Ok(parsed)
}

fn response_decimal_u32(value: &str, field: &str) -> Result<u32, DepositError> {
    let parsed = response_decimal(value, field)?;
    u32::try_from(parsed)
        .map_err(|_| protocol(format!("Bitcoin Wallet returned an oversized {field}")))
}

fn response_hex(value: &str, field: &str) -> Result<Vec<u8>, DepositError> {
    let hexadecimal = value.strip_prefix("0x").ok_or_else(|| {
        protocol(format!(
            "Bitcoin Wallet returned {field} without its 0x prefix"
        ))
    })?;
    if hexadecimal.is_empty() || hexadecimal.len() % 2 != 0 {
        return Err(protocol(format!(
            "Bitcoin Wallet returned incomplete {field} bytes"
        )));
    }
    let mut decoded = Vec::with_capacity(hexadecimal.len() / 2);
    for pair in hexadecimal.as_bytes().chunks_exact(2) {
        let high = hex_nibble(pair[0])
            .ok_or_else(|| protocol(format!("Bitcoin Wallet returned invalid {field} hex")))?;
        let low = hex_nibble(pair[1])
            .ok_or_else(|| protocol(format!("Bitcoin Wallet returned invalid {field} hex")))?;
        decoded.push((high << 4) | low);
    }
    if hex_prefixed(&decoded) != value {
        return Err(protocol(format!(
            "Bitcoin Wallet returned non-canonical lowercase {field} hex"
        )));
    }
    Ok(decoded)
}

fn response_block_hash(value: &str, field: &str) -> Result<indexing::BlockHash, DepositError> {
    let hash = parse_bitcoin_block_hash(value)
        .map_err(|_| protocol(format!("Bitcoin Wallet returned an invalid {field}")))?;
    let canonical = format_bitcoin_block_hash(&hash)
        .map_err(|_| protocol(format!("Bitcoin Wallet returned an invalid {field}")))?;
    if canonical != value {
        return Err(protocol(format!(
            "Bitcoin Wallet returned a non-canonical {field}"
        )));
    }
    Ok(hash)
}

fn validate_locator_identifier(value: &str, response: bool) -> Result<(), DepositError> {
    if value.is_empty() || value.len() > 4_096 || value.bytes().any(|byte| byte.is_ascii_control())
    {
        let message = "Bitcoin Wallet key locator identifier is invalid";
        return Err(if response {
            protocol(message)
        } else {
            invalid(message)
        });
    }
    Ok(())
}

fn validate_derivation_path(children: &[ChildIndex], response: bool) -> Result<(), DepositError> {
    if children.is_empty() || children.len() > 64 {
        let message = "Bitcoin Wallet key derivation path is invalid";
        return Err(if response {
            protocol(message)
        } else {
            invalid(message)
        });
    }
    Ok(())
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
        extract::{Request, State},
        http::header,
        response::{IntoResponse, Response},
        routing::any,
    };
    use bitcoin::{
        Address, Amount, CompressedPublicKey, OutPoint, PublicKey, ScriptBuf, Sequence,
        Transaction, TxIn, TxOut, Txid, Witness, XOnlyPublicKey, absolute, consensus, hashes::Hash,
        secp256k1::Secp256k1, transaction::Version,
    };
    use chain_identity::AssetId;
    use deposits::IdempotencyKey;
    use indexing::IndexScope;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

    use super::*;

    #[derive(Clone)]
    struct ObservedRequest {
        path: String,
        authorization: Option<String>,
        body: Value,
    }

    #[derive(Clone)]
    struct DoubleState {
        observed: Arc<Mutex<Vec<ObservedRequest>>>,
        prepared: Value,
        generated_address: BitcoinAddress,
        sign_attempts: Arc<AtomicUsize>,
        broadcast_attempts: Arc<AtomicUsize>,
        retry_once: bool,
    }

    async fn wallet_double(State(state): State<DoubleState>, request: Request) -> Response {
        let path = request.uri().path().to_owned();
        let authorization = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(request.into_body(), 16 * 1024 * 1024)
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
            "/v1/bitcoin/addresses" => Json(json!({
                "address": state.generated_address.0.clone(),
                "key_locator": { "kind": "identifier", "value": "opaque-key-7" }
            }))
            .into_response(),
            "/v1/bitcoin/collections/sign" => {
                if state.retry_once && state.sign_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return transient_error("request-sign-1");
                }
                Json(state.prepared.clone()).into_response()
            }
            "/v1/bitcoin/transactions/broadcast" => {
                if state.retry_once && state.broadcast_attempts.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return transient_error("request-broadcast-1");
                }
                Json(json!({
                    "transaction_id": state.prepared["transaction_id"]
                }))
                .into_response()
            }
            "/v1/bitcoin/receipts" => Json(json!({
                "transaction_id": state.prepared["transaction_id"],
                "receipt": {
                    "included_in": {
                        "height": 7,
                        "hash": format!("{:064x}", 7),
                        "parent_hash": format!("{:064x}", 6),
                        "timestamp": 10
                    },
                    "confirmations": 3,
                    "replaced_by": null
                }
            }))
            .into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    fn transient_error(request_id: &str) -> Response {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "code": "bitcoin_rpc_unavailable",
                "message": "temporary",
                "retryable": true,
                "request_id": request_id
            })),
        )
            .into_response()
    }

    async fn spawn_double(state: DoubleState) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("HTTP-double listener must bind");
        let address = listener
            .local_addr()
            .expect("HTTP-double listener address must exist");
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(any(wallet_double)).with_state(state),
            )
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

    fn p2wpkh(public_key: &[u8; 33]) -> BitcoinAddress {
        let public_key = PublicKey::from_slice(public_key).expect("test public key must parse");
        let compressed =
            CompressedPublicKey::try_from(public_key).expect("test key must be compressed");
        BitcoinAddress(Address::p2wpkh(&compressed, bitcoin::Network::Regtest).to_string())
    }

    fn source_address() -> BitcoinAddress {
        p2wpkh(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
    }

    fn destination_address() -> BitcoinAddress {
        p2wpkh(&[
            0x02, 0xc6, 0x04, 0x7f, 0x94, 0x41, 0xed, 0x7d, 0x6d, 0x30, 0x45, 0x40, 0x6e, 0x95,
            0xc0, 0x7c, 0xd8, 0x5c, 0x77, 0x8e, 0x4b, 0x8c, 0xef, 0x3c, 0xa7, 0xab, 0xac, 0x09,
            0xb9, 0x5c, 0x70, 0x9e, 0xe5,
        ])
    }

    fn taproot_address() -> BitcoinAddress {
        let key = XOnlyPublicKey::from_slice(&[
            0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
            0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b,
            0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test x-only key must parse");
        BitcoinAddress(
            Address::p2tr(
                &Secp256k1::verification_only(),
                key,
                None,
                bitcoin::Network::Regtest,
            )
            .to_string(),
        )
    }

    fn fixture_for(
        source: BitcoinAddress,
        previous: BitcoinTransactionId,
        operation_id: &str,
    ) -> (BitcoinSignCollectionRequest, Value) {
        let destination = destination_address();
        let outpoint = BitcoinOutPoint {
            transaction_id: previous,
            output_index: 3,
        };
        let script = source
            .script_pubkey_for_network(BitcoinNetwork::Regtest)
            .expect("source script must derive")
            .into_bytes();
        let transaction = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array(previous.0), 3),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::from_slice(&[b"deterministic-test-witness"]),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(42_000),
                script_pubkey: destination
                    .script_pubkey_for_network(BitcoinNetwork::Regtest)
                    .expect("destination script must derive"),
            }],
        };
        let transaction_id = BitcoinTransactionId::from(transaction.compute_txid());
        let raw = consensus::serialize(&transaction);
        let request = BitcoinSignCollectionRequest {
            operation_id: OperationId::new(operation_id).expect("operation ID must be valid"),
            sources: vec![BitcoinWalletCollectionSource {
                address: source.clone(),
                key_locator: KeyLocator::Identifier("opaque-key-7".to_owned()),
                inputs: vec![BitcoinCollectionInput {
                    outpoint,
                    value: Satoshi(43_000),
                    script_pubkey: script,
                }],
            }],
            destination: destination.clone(),
            fee_rate: SatoshisPerKvb::new(1_500),
        };
        let response = json!({
            "transaction_id": transaction_id.to_string(),
            "raw_transaction": hex_prefixed(&raw),
            "selected_outpoints": [{
                "transaction_id": previous.to_string(),
                "output_index": "3"
            }],
            "outputs": [{
                "address": destination.0,
                "value_satoshis": "42000"
            }],
            "fee_satoshis": "1000",
            "virtual_size": transaction.vsize().to_string(),
            "attribution": [{
                "address": source.0,
                "gross_input_satoshis": "43000"
            }]
        });
        (request, response)
    }

    fn fixture() -> (BitcoinSignCollectionRequest, Value) {
        fixture_for(
            source_address(),
            BitcoinTransactionId([7; 32]),
            "bitcoin-collection-operation-7",
        )
    }

    fn deposit_address_request(operation_id: &str) -> DepositAddressRequest {
        DepositAddressRequest {
            scope: IndexScope {
                chain: ChainId(BITCOIN_CHAIN.to_owned()),
                network: "regtest".to_owned(),
            },
            asset: AssetId {
                chain: ChainId(BITCOIN_CHAIN.to_owned()),
                asset: NATIVE_ASSET.to_owned(),
            },
            operation_id: operation_id.to_owned(),
            key_purpose: "bitcoin-deposit".to_owned(),
            idempotency_key: IdempotencyKey(format!("business-{operation_id}")),
        }
    }

    fn double_state(prepared: Value, retry_once: bool) -> DoubleState {
        let generated_address = BitcoinAddress(
            prepared["attribution"][0]["address"]
                .as_str()
                .expect("fixture attribution address must be a string")
                .to_owned(),
        );
        DoubleState {
            observed: Arc::new(Mutex::new(Vec::new())),
            prepared,
            generated_address,
            sign_attempts: Arc::new(AtomicUsize::new(0)),
            broadcast_attempts: Arc::new(AtomicUsize::new(0)),
            retry_once,
        }
    }

    #[tokio::test]
    async fn typed_flow_retries_signing_but_returns_broadcast_ambiguity_for_receipt_recovery() {
        let (request, prepared_response) = fixture();
        let state = double_state(prepared_response, true);
        let (endpoint, server) = spawn_double(state.clone()).await;
        let client = BitcoinWalletClient::new(
            &options(&endpoint, 2),
            BitcoinNetwork::Regtest,
            BitcoinAddressKind::SegwitV0,
        )
        .expect("client must build");

        let generated = client
            .address(deposit_address_request("bitcoin-address-operation-7"))
            .await
            .expect("address generation must succeed");
        assert_eq!(generated.address.value, source_address().0);
        assert_eq!(
            generated.key,
            KeyLocator::Identifier("opaque-key-7".to_owned())
        );

        let prepared = client
            .sign_collection(&request)
            .await
            .expect("signing must succeed after an identical safe retry");
        assert_eq!(
            prepared.inspection.inputs[0].outpoint,
            request.sources[0].inputs[0].outpoint
        );
        assert_eq!(prepared.inspection.outputs[0].value, Satoshi(42_000));
        assert_eq!(prepared.fee, Satoshi(1_000));
        assert_eq!(prepared.attribution[0].gross_input, Satoshi(43_000));

        let broadcast_error = client
            .broadcast(prepared.transaction_id, &prepared.raw_transaction)
            .await
            .expect_err("ambiguous broadcast must return without an in-client retry");
        assert_eq!(broadcast_error.kind, DepositErrorKind::Other);
        let receipt = client
            .receipt(prepared.transaction_id)
            .await
            .expect("receipt request must succeed")
            .expect("receipt must exist");
        assert_eq!(receipt.confirmations, 3);
        assert_eq!(
            receipt
                .included_in
                .expect("confirmed receipt must include a block")
                .height,
            BlockHeight(7)
        );

        let observed = state.observed.lock().await;
        assert!(
            observed.iter().all(|request| {
                request.authorization.as_deref() == Some("Bearer wallet-secret")
            })
        );
        let signs = observed
            .iter()
            .filter(|request| request.path == "/v1/bitcoin/collections/sign")
            .collect::<Vec<_>>();
        assert_eq!(signs.len(), 2);
        assert!(signs[0].body == signs[1].body);
        let broadcasts = observed
            .iter()
            .filter(|request| request.path == "/v1/bitcoin/transactions/broadcast")
            .collect::<Vec<_>>();
        assert_eq!(broadcasts.len(), 1);
        drop(observed);
        server.abort();
    }

    #[tokio::test]
    async fn taproot_address_kind_and_exact_input_survive_the_http_boundary() {
        let source = taproot_address();
        let (request, prepared_response) = fixture_for(
            source.clone(),
            BitcoinTransactionId([8; 32]),
            "bitcoin-taproot-collection-operation",
        );
        let expected_script = source
            .script_pubkey_for_network(BitcoinNetwork::Regtest)
            .expect("Taproot source script must derive")
            .into_bytes();
        let state = double_state(prepared_response, false);
        let (endpoint, server) = spawn_double(state.clone()).await;
        let client = BitcoinWalletClient::new(
            &options(&endpoint, 1),
            BitcoinNetwork::Regtest,
            BitcoinAddressKind::Taproot,
        )
        .expect("client must build");

        let generated = client
            .address(deposit_address_request("bitcoin-taproot-address-operation"))
            .await
            .expect("Taproot address generation must succeed");
        assert_eq!(generated.address.value, source.0);

        let prepared = client
            .sign_collection(&request)
            .await
            .expect("exact Taproot input signing response must validate");
        assert_eq!(
            prepared.inspection.inputs[0].outpoint,
            request.sources[0].inputs[0].outpoint
        );
        assert_eq!(prepared.attribution[0].address, source);

        let observed = state.observed.lock().await;
        let address_request = observed
            .iter()
            .find(|request| request.path == "/v1/bitcoin/addresses")
            .expect("address request must be observed");
        assert_eq!(address_request.body["address_kind"].as_str(), Some("p2tr"));
        let sign_request = observed
            .iter()
            .find(|request| request.path == "/v1/bitcoin/collections/sign")
            .expect("sign request must be observed");
        assert_eq!(
            sign_request.body["sources"][0]["address"].as_str(),
            Some(request.sources[0].address.0.as_str())
        );
        assert_eq!(
            sign_request.body["sources"][0]["inputs"][0]["script_pubkey"].as_str(),
            Some(hex_prefixed(&expected_script).as_str())
        );
        drop(observed);
        server.abort();
    }

    #[tokio::test]
    async fn rejects_generated_address_whose_script_kind_differs_from_policy() {
        let (_request, prepared_response) = fixture();
        let state = double_state(prepared_response, false);
        let (endpoint, server) = spawn_double(state).await;
        let client = BitcoinWalletClient::new(
            &options(&endpoint, 1),
            BitcoinNetwork::Regtest,
            BitcoinAddressKind::Taproot,
        )
        .expect("client must build");

        let error = client
            .address(deposit_address_request("bitcoin-wrong-script-kind"))
            .await
            .expect_err("a P2WPKH response must not satisfy a P2TR policy");

        assert_eq!(error.kind, DepositErrorKind::InvariantViolation);
        assert!(error.message.contains("script kind"));
        server.abort();
    }

    #[tokio::test]
    async fn rejects_response_fee_that_disagrees_with_exact_inputs_and_raw_output() {
        let (request, mut prepared_response) = fixture();
        prepared_response["fee_satoshis"] = json!("999");
        let state = double_state(prepared_response, false);
        let (endpoint, server) = spawn_double(state).await;
        let client = BitcoinWalletClient::new(
            &options(&endpoint, 1),
            BitcoinNetwork::Regtest,
            BitcoinAddressKind::SegwitV0,
        )
        .expect("client must build");

        let error = client
            .sign_collection(&request)
            .await
            .expect_err("inconsistent fee must be rejected");
        assert_eq!(error.kind, DepositErrorKind::InvariantViolation);
        assert!(error.message.contains("fee differs"));
        server.abort();
    }

    #[test]
    fn debug_output_redacts_endpoint_token_operation_locator_and_raw_transaction() {
        let (request, response) = fixture();
        let endpoint = "http://127.0.0.1:43199";
        let client = BitcoinWalletClient::new(
            &options(endpoint, 1),
            BitcoinNetwork::Regtest,
            BitcoinAddressKind::SegwitV0,
        )
        .expect("client must build");
        let client_debug = format!("{client:?}");
        assert!(!client_debug.contains(endpoint));
        assert!(!client_debug.contains("wallet-secret"));
        let request_debug = format!("{request:?}");
        assert!(!request_debug.contains("bitcoin-collection-operation-7"));
        assert!(!request_debug.contains("opaque-key-7"));

        let validated = ValidatedCollection::from_request(&request, BitcoinNetwork::Regtest)
            .expect("request must validate");
        let response: PreparedCollectionResponseDto =
            serde_json::from_value(response).expect("fixture response must decode");
        let prepared = response
            .into_domain(&validated, BitcoinNetwork::Regtest)
            .expect("fixture response must validate");
        let prepared_debug = format!("{prepared:?}");
        assert!(!prepared_debug.contains(&hex_prefixed(prepared.raw_transaction.as_bytes())));
    }
}
