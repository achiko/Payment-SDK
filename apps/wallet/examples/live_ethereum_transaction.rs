//! Review, sign, and optionally broadcast one real native-ETH EIP-1559 transfer.
//!
//! Required environment variables:
//! - `ETH_RPC_URL`
//! - `ETH_PRIVATE_KEY`
//! - `ETH_TO`
//! - `ETH_VALUE_WEI`
//! - `ETH_CHAIN_ID`
//!
//! The default is review-only. Signing requires `ETH_SIGN_TRANSACTION=true`.
//! Broadcasting additionally requires
//! `ETH_BROADCAST_TRANSACTION=I_UNDERSTAND`. Never use a key holding funds you
//! cannot afford to lose while adapting this example.

use alloy_primitives::B256;
use alloy_signer::SignerSync as AlloySignerSync;
use alloy_signer_local::PrivateKeySigner;
use chain_ethereum::{
    BoxFuture as EthereumFuture, Ethereum, EthereumAddress, EthereumAsset, EthereumBuildContext,
    EthereumReceipt, EthereumRpc, EthereumSignedTransaction, EthereumTransactionId,
    EthereumTransferRequest, EthereumWallet, Wei,
};
use indexing::{BlockRef, SourceError};
use serde_json::{Map, Value, json};
use signer::{
    BoxFuture as SignerFuture, Curve, KeyLocator, KeyProvisionRequest, KeyProvisioner, OperationId,
    ProvisionedKey, PublicKey, PublicKeyFormat, SignRequest, SignablePayload, Signature,
    SignatureEncoding, SignatureScheme, Signer as WalletSigner, SignerCapabilities, SignerError,
    SignerErrorKind, SignerStatus,
};
use std::{
    env,
    error::Error,
    fmt,
    net::IpAddr,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use wallet_worker::WalletService;
use zeroize::Zeroize;

const LIVE_KEY_LOCATOR: &str = "environment:ETH_PRIVATE_KEY";
const BROADCAST_CONFIRMATION: &str = "I_UNDERSTAND";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = LiveTransactionConfig::from_environment()?;
    let rpc = HttpEthereumRpc::new(config.rpc_url)?;
    let actual_chain_id = rpc.chain_id().await?;
    if actual_chain_id != config.chain_id {
        return Err(ExampleError::new(format!(
            "RPC chain ID {actual_chain_id} does not match ETH_CHAIN_ID {}",
            config.chain_id
        ))
        .into());
    }

    let signer = EnvironmentEthereumSigner::from_environment()?;
    let sender = signer.address();
    let key = signer.locator().clone();
    let service = WalletService::<Ethereum, _, _, _>::new(
        EthereumWallet::new(config.chain_id, rpc),
        DisabledKeyProvisioner,
        signer,
    );
    let asset = EthereumAsset::Native;

    let balance = service.balance(&asset, &sender).await?;
    let unsigned = service
        .build_transfer(
            &asset,
            EthereumTransferRequest {
                signing_operation_id: OperationId::new("live-example-sign")?,
                key,
                from: sender.clone(),
                to: Some(config.recipient.clone()),
                value: config.value.clone(),
                data: Vec::new(),
            },
        )
        .await?;

    let maximum_fee = unsigned
        .max_fee_per_gas
        .checked_mul_u64(unsigned.gas_limit)
        .ok_or_else(|| ExampleError::new("maximum Ethereum fee overflowed U256"))?;
    let maximum_debit = unsigned
        .value
        .checked_add(&maximum_fee)
        .ok_or_else(|| ExampleError::new("Ethereum value plus maximum fee overflowed U256"))?;
    if balance.spendable < maximum_debit {
        return Err(ExampleError::new(format!(
            "pending balance {} wei cannot cover the maximum debit {} wei",
            display_wei(&balance.spendable),
            display_wei(&maximum_debit)
        ))
        .into());
    }

    print_review(&unsigned, &balance.spendable, &maximum_fee, &maximum_debit);

    if !config.sign {
        println!("\nReview only: set ETH_SIGN_TRANSACTION=true to sign this fresh transaction.");
        return Ok(());
    }

    let signed = service.sign_transaction(&asset, unsigned).await?;
    println!("\nSigned locally. Raw signed transaction bytes were not printed.");
    println!("local transaction hash: 0x{}", encode_hex(&signed.id.0));

    if !config.broadcast {
        println!(
            "Not broadcast: set ETH_BROADCAST_TRANSACTION={BROADCAST_CONFIRMATION} after reviewing every field."
        );
        return Ok(());
    }

    let local_id = signed.id.clone();
    let returned_id = service.broadcast(&asset, signed).await?;
    if returned_id != local_id {
        return Err(ExampleError::new(
            "RPC returned a transaction hash different from the locally computed hash",
        )
        .into());
    }

    println!("Broadcast accepted by the RPC node.");
    println!("transaction hash: 0x{}", encode_hex(&returned_id.0));
    println!("Node acceptance is not confirmation; monitor the receipt separately.");
    Ok(())
}

struct LiveTransactionConfig {
    rpc_url: String,
    chain_id: u64,
    recipient: EthereumAddress,
    value: Wei,
    sign: bool,
    broadcast: bool,
}

impl LiveTransactionConfig {
    fn from_environment() -> Result<Self, ExampleError> {
        let rpc_url = required_environment("ETH_RPC_URL")?;
        let chain_id = required_environment("ETH_CHAIN_ID")?
            .parse::<u64>()
            .map_err(|_| ExampleError::new("ETH_CHAIN_ID must be an unsigned integer"))?;
        if chain_id == 0 {
            return Err(ExampleError::new("ETH_CHAIN_ID must be non-zero"));
        }

        let recipient = parse_address(&required_environment("ETH_TO")?, "ETH_TO")?;
        if recipient.0 == [0; 20] {
            return Err(ExampleError::new("ETH_TO must not be the zero address"));
        }

        let value = required_environment("ETH_VALUE_WEI")?
            .parse::<u128>()
            .map(Wei::from_u128)
            .map_err(|_| {
                ExampleError::new("ETH_VALUE_WEI must be a base-10 unsigned integer in wei")
            })?;
        if value.is_zero() {
            return Err(ExampleError::new("ETH_VALUE_WEI must be greater than zero"));
        }

        let sign = environment_equals("ETH_SIGN_TRANSACTION", "true");
        let broadcast = environment_equals("ETH_BROADCAST_TRANSACTION", BROADCAST_CONFIRMATION);
        if broadcast && !sign {
            return Err(ExampleError::new(
                "broadcast requires ETH_SIGN_TRANSACTION=true as a separate approval",
            ));
        }

        Ok(Self {
            rpc_url,
            chain_id,
            recipient,
            value,
            sign,
            broadcast,
        })
    }
}

fn print_review(
    transaction: &chain_ethereum::UnsignedEthereumTransaction,
    balance: &Wei,
    maximum_fee: &Wei,
    maximum_debit: &Wei,
) {
    let recipient = transaction
        .to
        .as_ref()
        .map_or_else(|| "contract creation".to_owned(), address_hex);

    println!("Ethereum EIP-1559 transaction review");
    println!("chain ID: {}", transaction.chain_id);
    println!("from: {}", address_hex(&transaction.from));
    println!("to: {recipient}");
    println!("value: {} wei", display_wei(&transaction.value));
    println!("pending balance: {} wei", display_wei(balance));
    println!("nonce: {}", transaction.nonce);
    println!(
        "gas limit (RPC estimate + 20% margin): {}",
        transaction.gas_limit
    );
    println!(
        "max fee per gas: {} wei",
        display_wei(&transaction.max_fee_per_gas)
    );
    println!(
        "max priority fee per gas: {} wei",
        display_wei(&transaction.max_priority_fee_per_gas)
    );
    println!("maximum network fee: {} wei", display_wei(maximum_fee));
    println!("maximum total debit: {} wei", display_wei(maximum_debit));
}

#[derive(Clone)]
struct HttpEthereumRpc {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    next_request_id: Arc<AtomicU64>,
}

impl HttpEthereumRpc {
    fn new(endpoint: String) -> Result<Self, ExampleError> {
        let endpoint = reqwest::Url::parse(&endpoint)
            .map_err(|_| ExampleError::new("ETH_RPC_URL is not a valid URL"))?;
        if !secure_or_loopback_endpoint(&endpoint) {
            return Err(ExampleError::new(
                "ETH_RPC_URL must use HTTPS; plain HTTP is allowed only for loopback nodes",
            ));
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| ExampleError::new("could not construct the Ethereum HTTP client"))?;
        Ok(Self {
            client,
            endpoint,
            next_request_id: Arc::new(AtomicU64::new(1)),
        })
    }

    async fn chain_id(&self) -> Result<u64, SourceError> {
        self.rpc_u64("eth_chainId", json!([])).await
    }

    async fn rpc_u64(&self, method: &str, params: Value) -> Result<u64, SourceError> {
        let value = self.rpc_value(method, params).await?;
        let quantity = value
            .as_str()
            .ok_or_else(|| invalid_rpc_response(method, "result is not a hex quantity"))?;
        parse_quantity_u64(quantity)
            .map_err(|message| invalid_rpc_response(method, message.as_str()))
    }

    async fn rpc_wei(&self, method: &str, params: Value) -> Result<Wei, SourceError> {
        let value = self.rpc_value(method, params).await?;
        let quantity = value
            .as_str()
            .ok_or_else(|| invalid_rpc_response(method, "result is not a hex quantity"))?;
        parse_quantity_wei(quantity)
            .map_err(|message| invalid_rpc_response(method, message.as_str()))
    }

    async fn rpc_value(&self, method: &str, params: Value) -> Result<Value, SourceError> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let response = self
            .client
            .post(self.endpoint.clone())
            .json(&json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }))
            .send()
            .await
            .map_err(|_| SourceError {
                message: format!("Ethereum RPC HTTP request for {method} failed"),
                retryable: true,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(SourceError {
                message: format!("Ethereum RPC HTTP request for {method} returned {status}"),
                retryable: status.as_u16() == 429 || status.is_server_error(),
            });
        }

        let body = response.json::<Value>().await.map_err(|_| SourceError {
            message: format!("Ethereum RPC response for {method} was not valid JSON"),
            retryable: false,
        })?;
        if body.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || body.get("id").and_then(Value::as_u64) != Some(request_id)
        {
            return Err(invalid_rpc_response(
                method,
                "response version or request ID does not match",
            ));
        }
        if let Some(error) = body.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(sanitized_rpc_message)
                .unwrap_or_else(|| "unknown RPC error".to_owned());
            return Err(SourceError {
                message: format!("Ethereum RPC {method} failed ({code}): {message}"),
                retryable: code == -32_005,
            });
        }

        body.get("result")
            .cloned()
            .ok_or_else(|| invalid_rpc_response(method, "response has no result field"))
    }
}

impl fmt::Debug for HttpEthereumRpc {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpEthereumRpc")
            .field("endpoint", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl EthereumRpc for HttpEthereumRpc {
    fn balance<'a>(
        &'a self,
        address: EthereumAddress,
        asset: &'a EthereumAsset,
        _at: Option<BlockRef>,
    ) -> EthereumFuture<'a, Result<Wei, SourceError>> {
        Box::pin(async move {
            if asset != &EthereumAsset::Native {
                return Err(unsupported_rpc("ERC-20 balance lookup"));
            }
            self.rpc_wei("eth_getBalance", json!([address_hex(&address), "pending"]))
                .await
        })
    }

    fn nonce<'a>(
        &'a self,
        address: EthereumAddress,
    ) -> EthereumFuture<'a, Result<u64, SourceError>> {
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
    ) -> EthereumFuture<'a, Result<EthereumBuildContext, SourceError>> {
        Box::pin(async move {
            let chain_id = self.chain_id().await?;
            let nonce = self.nonce(request.from.clone()).await?;
            let mut transaction = Map::new();
            transaction.insert("from".to_owned(), json!(address_hex(&request.from)));
            if let Some(to) = &request.to {
                transaction.insert("to".to_owned(), json!(address_hex(to)));
            }
            transaction.insert("value".to_owned(), json!(wei_quantity(&request.value)));
            transaction.insert(
                "data".to_owned(),
                json!(format!("0x{}", encode_hex(&request.data))),
            );

            let estimated_gas_limit = self
                .rpc_u64("eth_estimateGas", json!([Value::Object(transaction)]))
                .await?;
            let gas_limit = gas_limit_with_margin(estimated_gas_limit)?;
            let max_priority_fee_per_gas =
                self.rpc_wei("eth_maxPriorityFeePerGas", json!([])).await?;
            let latest_block = self
                .rpc_value("eth_getBlockByNumber", json!(["latest", false]))
                .await?;
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
                    parse_quantity_wei(value).map_err(|message| {
                        invalid_rpc_response("eth_getBlockByNumber", message.as_str())
                    })
                })?;
            let max_fee_per_gas = base_fee
                .checked_mul_u64(2)
                .and_then(|fee| fee.checked_add(&max_priority_fee_per_gas))
                .ok_or_else(|| invalid_rpc_response("fee calculation", "fee overflowed U256"))?;

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
        _id: &'a EthereumTransactionId,
    ) -> EthereumFuture<'a, Result<Option<EthereumReceipt>, SourceError>> {
        Box::pin(async { Err(unsupported_rpc("receipt monitoring")) })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: EthereumSignedTransaction,
    ) -> EthereumFuture<'a, Result<EthereumTransactionId, SourceError>> {
        Box::pin(async move {
            let result = self
                .rpc_value(
                    "eth_sendRawTransaction",
                    json!([format!("0x{}", encode_hex(&transaction.envelope))]),
                )
                .await?;
            let returned = result.as_str().ok_or_else(|| {
                invalid_rpc_response("eth_sendRawTransaction", "result is not a transaction hash")
            })?;
            let returned = EthereumTransactionId(
                parse_fixed_hex::<32>(returned, "transaction hash").map_err(|message| {
                    invalid_rpc_response("eth_sendRawTransaction", message.as_str())
                })?,
            );
            if returned != transaction.id {
                return Err(invalid_rpc_response(
                    "eth_sendRawTransaction",
                    "node hash differs from the local transaction hash",
                ));
            }
            Ok(returned)
        })
    }
}

struct EnvironmentEthereumSigner {
    inner: PrivateKeySigner,
    locator: KeyLocator,
}

impl EnvironmentEthereumSigner {
    fn from_environment() -> Result<Self, ExampleError> {
        let mut private_key = required_environment("ETH_PRIVATE_KEY")?;
        let parsed = PrivateKeySigner::from_str(private_key.trim());
        private_key.zeroize();
        let inner = parsed.map_err(|_| {
            ExampleError::new("ETH_PRIVATE_KEY is not a valid 32-byte secp256k1 private key")
        })?;
        Ok(Self {
            inner,
            locator: KeyLocator::Identifier(LIVE_KEY_LOCATOR.to_owned()),
        })
    }

    fn address(&self) -> EthereumAddress {
        EthereumAddress(self.inner.address().into_array())
    }

    fn locator(&self) -> &KeyLocator {
        &self.locator
    }
}

impl fmt::Debug for EnvironmentEthereumSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvironmentEthereumSigner")
            .field("address", &address_hex(&self.address()))
            .field("private_key", &"[redacted]")
            .finish()
    }
}

impl WalletSigner for EnvironmentEthereumSigner {
    fn capabilities(&self) -> SignerCapabilities {
        SignerCapabilities {
            curves: vec![Curve::Secp256k1],
            schemes: vec![SignatureScheme::EcdsaSecp256k1],
            can_sign_messages: false,
            can_sign_digests: true,
            requires_user_interaction: false,
        }
    }

    fn status<'a>(&'a self) -> SignerFuture<'a, Result<SignerStatus, SignerError>> {
        Box::pin(async { Ok(SignerStatus::Available) })
    }

    fn public_key<'a>(
        &'a self,
        key: &'a KeyLocator,
        curve: Curve,
        format: PublicKeyFormat,
    ) -> SignerFuture<'a, Result<PublicKey, SignerError>> {
        Box::pin(async move {
            self.validate_key(key)?;
            if curve != Curve::Secp256k1 {
                return Err(signer_error(
                    SignerErrorKind::UnsupportedCurve,
                    "the live Ethereum example supports secp256k1 only",
                ));
            }
            Ok(PublicKey {
                curve,
                format,
                bytes: encode_public_key(self.inner.public_key().as_slice(), format),
            })
        })
    }

    fn sign<'a>(
        &'a self,
        request: SignRequest,
    ) -> SignerFuture<'a, Result<Signature, SignerError>> {
        Box::pin(async move {
            self.validate_key(&request.key)?;
            if request.scheme != SignatureScheme::EcdsaSecp256k1
                || request.encoding != SignatureEncoding::Recoverable
                || request.key_tweak.is_some()
            {
                return Err(signer_error(
                    SignerErrorKind::UnsupportedOperation,
                    "the live Ethereum example requires untweaked recoverable secp256k1 ECDSA",
                ));
            }
            let SignablePayload::Digest(digest) = request.payload else {
                return Err(signer_error(
                    SignerErrorKind::UnsupportedOperation,
                    "the live Ethereum example signs precomputed digests only",
                ));
            };
            let digest: [u8; 32] = digest.bytes.try_into().map_err(|_| {
                signer_error(
                    SignerErrorKind::InvalidRequest,
                    "Ethereum signing digest must contain exactly 32 bytes",
                )
            })?;
            let signature = AlloySignerSync::sign_hash_sync(&self.inner, &B256::from(digest))
                .map_err(|_| {
                    signer_error(
                        SignerErrorKind::Other,
                        "environment Ethereum signing failed",
                    )
                })?;
            Ok(Signature {
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Recoverable,
                bytes: signature.as_bytes().to_vec(),
            })
        })
    }
}

impl EnvironmentEthereumSigner {
    fn validate_key(&self, key: &KeyLocator) -> Result<(), SignerError> {
        if key != &self.locator {
            return Err(signer_error(
                SignerErrorKind::KeyNotFound,
                "environment Ethereum key locator was not found",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct DisabledKeyProvisioner;

impl KeyProvisioner for DisabledKeyProvisioner {
    fn provision<'a>(
        &'a self,
        _request: KeyProvisionRequest,
    ) -> SignerFuture<'a, Result<ProvisionedKey, SignerError>> {
        Box::pin(async {
            Err(signer_error(
                SignerErrorKind::UnsupportedOperation,
                "the live transaction example loads an existing funded key and cannot provision",
            ))
        })
    }
}

#[derive(Debug)]
struct ExampleError {
    message: String,
}

impl ExampleError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ExampleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ExampleError {}

fn required_environment(name: &str) -> Result<String, ExampleError> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ExampleError::new(format!("required environment variable {name} is missing"))
        })
}

fn environment_equals(name: &str, expected: &str) -> bool {
    env::var(name).is_ok_and(|value| value.trim() == expected)
}

fn parse_address(value: &str, name: &str) -> Result<EthereumAddress, ExampleError> {
    parse_fixed_hex::<20>(value, name)
        .map(EthereumAddress)
        .map_err(ExampleError::new)
}

fn parse_fixed_hex<const N: usize>(value: &str, name: &str) -> Result<[u8; N], String> {
    let raw = value
        .trim()
        .strip_prefix("0x")
        .or_else(|| value.trim().strip_prefix("0X"))
        .unwrap_or(value.trim());
    if raw.len() != N * 2 {
        return Err(format!(
            "{name} must contain exactly {} hexadecimal bytes",
            N
        ));
    }

    let mut decoded = [0_u8; N];
    for (index, pair) in raw.as_bytes().chunks_exact(2).enumerate() {
        let high =
            hex_nibble(pair[0]).ok_or_else(|| format!("{name} contains non-hexadecimal data"))?;
        let low =
            hex_nibble(pair[1]).ok_or_else(|| format!("{name} contains non-hexadecimal data"))?;
        decoded[index] = (high << 4) | low;
    }
    Ok(decoded)
}

fn parse_quantity_u64(value: &str) -> Result<u64, String> {
    let raw = quantity_digits(value)?;
    u64::from_str_radix(raw, 16).map_err(|_| "hex quantity exceeds u64".to_owned())
}

fn parse_quantity_wei(value: &str) -> Result<Wei, String> {
    let raw = quantity_digits(value)?;
    if raw.len() > 64 {
        return Err("hex quantity exceeds 256 bits".to_owned());
    }

    let mut decoded = [0_u8; 32];
    for (index, byte) in raw.as_bytes().iter().rev().enumerate() {
        let nibble =
            hex_nibble(*byte).ok_or_else(|| "hex quantity contains invalid data".to_owned())?;
        let target = 31 - (index / 2);
        if index % 2 == 0 {
            decoded[target] |= nibble;
        } else {
            decoded[target] |= nibble << 4;
        }
    }
    Ok(Wei(decoded))
}

fn quantity_digits(value: &str) -> Result<&str, String> {
    let raw = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .ok_or_else(|| "hex quantity has no 0x prefix".to_owned())?;
    if raw.is_empty() {
        return Err("hex quantity is empty".to_owned());
    }
    Ok(raw)
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
    format!("0x{}", encode_hex(&address.0))
}

fn display_wei(amount: &Wei) -> String {
    amount
        .checked_to_u128()
        .map_or_else(|| wei_quantity(amount), |value| value.to_string())
}

fn encode_public_key(raw: &[u8], format: PublicKeyFormat) -> Vec<u8> {
    debug_assert_eq!(raw.len(), 64);
    match format {
        PublicKeyFormat::Raw => raw.to_vec(),
        PublicKeyFormat::XOnly => raw[..32].to_vec(),
        PublicKeyFormat::Uncompressed => {
            let mut encoded = Vec::with_capacity(65);
            encoded.push(0x04);
            encoded.extend_from_slice(raw);
            encoded
        }
        PublicKeyFormat::Compressed => {
            let mut encoded = Vec::with_capacity(33);
            encoded.push(if raw[63] & 1 == 0 { 0x02 } else { 0x03 });
            encoded.extend_from_slice(&raw[..32]);
            encoded
        }
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
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

fn signer_error(kind: SignerErrorKind, message: impl Into<String>) -> SignerError {
    SignerError {
        kind,
        message: message.into(),
    }
}

fn secure_or_loopback_endpoint(endpoint: &reqwest::Url) -> bool {
    if endpoint.scheme() == "https" {
        return true;
    }
    if endpoint.scheme() != "http" {
        return false;
    }

    endpoint.host_str().is_some_and(|host| {
        host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn gas_limit_with_margin(estimated: u64) -> Result<u64, SourceError> {
    let margin = estimated.div_ceil(5);
    estimated
        .checked_add(margin)
        .ok_or_else(|| invalid_rpc_response("eth_estimateGas", "gas-limit margin overflowed u64"))
}

fn invalid_rpc_response(method: &str, message: &str) -> SourceError {
    SourceError {
        message: format!("Ethereum RPC {method} returned an invalid response: {message}"),
        retryable: false,
    }
}

fn unsupported_rpc(operation: &str) -> SourceError {
    SourceError {
        message: format!("live Ethereum transaction example does not implement {operation}"),
        retryable: false,
    }
}

fn sanitized_rpc_message(message: &str) -> String {
    let cleaned: String = message
        .chars()
        .filter(|character| !character.is_control())
        .take(240)
        .collect();
    redact_long_hex(&cleaned)
}

fn redact_long_hex(value: &str) -> String {
    let characters: Vec<char> = value.chars().collect();
    let mut redacted = String::with_capacity(value.len());
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '0' && characters.get(index + 1).is_some_and(|value| *value == 'x')
        {
            let mut end = index + 2;
            while characters
                .get(end)
                .is_some_and(|value| value.is_ascii_hexdigit())
            {
                end += 1;
            }
            if end - (index + 2) >= 32 {
                redacted.push_str("0x[redacted]");
                index = end;
                continue;
            }
        }

        redacted.push(characters[index]);
        index += 1;
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixed_addresses_without_accepting_invalid_input() {
        let address = parse_address("0x1111111111111111111111111111111111111111", "address")
            .expect("valid address should parse");
        assert_eq!(address.0, [0x11; 20]);
        assert!(parse_address("0x1234", "address").is_err());
        assert!(parse_address("0xgg11111111111111111111111111111111111111", "address").is_err());
    }

    #[test]
    fn converts_rpc_quantities_without_losing_precision() {
        assert_eq!(parse_quantity_u64("0x2a"), Ok(42));
        assert_eq!(parse_quantity_wei("0x0"), Ok(Wei::ZERO));

        let amount = Wei::from_u128(1_000_000_000_000_000);
        assert_eq!(parse_quantity_wei(&wei_quantity(&amount)), Ok(amount));
    }

    #[test]
    fn restricts_insecure_rpc_and_redacts_long_hex_errors() {
        assert!(secure_or_loopback_endpoint(
            &reqwest::Url::parse("https://rpc.example").expect("URL should parse")
        ));
        assert!(secure_or_loopback_endpoint(
            &reqwest::Url::parse("http://127.0.0.1:8545").expect("URL should parse")
        ));
        assert!(!secure_or_loopback_endpoint(
            &reqwest::Url::parse("http://rpc.example").expect("URL should parse")
        ));

        let message = format!("rejected 0x{}", "ab".repeat(80));
        assert_eq!(sanitized_rpc_message(&message), "rejected 0x[redacted]");
    }
}
