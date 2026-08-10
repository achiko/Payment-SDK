use std::{error::Error, fmt, fs, path::Path, time::Duration};

use chain_bitcoin::{
    BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB, BitcoinAddress, BitcoinAddressKind, BitcoinNetwork,
    Satoshi, SatoshisPerKvb,
};
use chain_identity::{AssetId, CanonicalAddress, ChainId};
use deposits::{MAX_COLLECTION_SPEND_RESOURCES, PolicyIdentity};
use indexing::IndexScope;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_POLICY_BYTES: u64 = 1024 * 1024;
const MAX_BATCH_DEPOSITS: usize = 1_000;

/// Fail-closed policy for one native-Bitcoin Payment Service database.
///
/// Collection limits intentionally have no serde or application defaults. An
/// operator must select every money-moving bound in the versioned policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinPaymentPolicy {
    pub version: u32,
    pub scope: IndexScope,
    pub network: BitcoinNetwork,
    pub deposit_address_kind: BitcoinAddressKind,
    pub deposit_ttl: Duration,
    pub asset: AssetId,
    pub master_destination: CanonicalAddress,
    pub minimum_collection: Satoshi,
    pub minimum_spend_confirmations: u64,
    pub requested_fee_rate: SatoshisPerKvb,
    pub maximum_fee_rate: SatoshisPerKvb,
    pub maximum_absolute_fee: Satoshi,
    pub maximum_deposits: usize,
    pub maximum_inputs: usize,
    pub digest: [u8; 32],
}

impl BitcoinPaymentPolicy {
    pub fn load(path: &Path) -> Result<Self, BitcoinPolicyError> {
        let metadata = fs::metadata(path).map_err(|error| {
            BitcoinPolicyError::with_source(
                BitcoinPolicyErrorKind::Io,
                "failed to inspect Bitcoin Payment Service policy",
                error,
            )
        })?;
        if !metadata.is_file() {
            return Err(invalid(
                "Bitcoin Payment Service policy path must identify a regular file",
            ));
        }
        if metadata.len() > MAX_POLICY_BYTES {
            return Err(BitcoinPolicyError::new(
                BitcoinPolicyErrorKind::TooLarge,
                "Bitcoin Payment Service policy exceeds the one-megabyte limit",
            ));
        }
        let bytes = fs::read(path).map_err(|error| {
            BitcoinPolicyError::with_source(
                BitcoinPolicyErrorKind::Io,
                "failed to read Bitcoin Payment Service policy",
                error,
            )
        })?;
        Self::from_json(&bytes)
    }

    pub(crate) fn from_json(bytes: &[u8]) -> Result<Self, BitcoinPolicyError> {
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err(BitcoinPolicyError::new(
                BitcoinPolicyErrorKind::TooLarge,
                "Bitcoin Payment Service policy exceeds the one-megabyte limit",
            ));
        }
        let document: BitcoinPolicyDocument = serde_json::from_slice(bytes).map_err(|error| {
            BitcoinPolicyError::with_source(
                BitcoinPolicyErrorKind::InvalidJson,
                "Bitcoin Payment Service policy is not valid JSON",
                error,
            )
        })?;
        document.validate(bytes)
    }

    #[must_use]
    pub fn identity(&self) -> PolicyIdentity {
        PolicyIdentity {
            version: self.version.to_string(),
            digest: self.digest,
        }
    }

    #[must_use]
    pub fn digest_hex(&self) -> String {
        hex::encode(self.digest)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BitcoinPolicyDocument {
    version: u32,
    scope: BitcoinScopeDocument,
    deposit_address_kind: String,
    deposit_ttl_seconds: u64,
    master_destination: String,
    minimum_collection_satoshis: String,
    minimum_spend_confirmations: u64,
    requested_satoshis_per_kvb: String,
    maximum_satoshis_per_kvb: String,
    maximum_absolute_fee_satoshis: String,
    maximum_deposits: usize,
    maximum_inputs: usize,
}

impl BitcoinPolicyDocument {
    fn validate(self, bytes: &[u8]) -> Result<BitcoinPaymentPolicy, BitcoinPolicyError> {
        if self.version == 0 {
            return Err(invalid("policy version must be greater than zero"));
        }
        if self.scope.chain != "bitcoin" {
            return Err(invalid("Bitcoin policy scope chain must be `bitcoin`"));
        }
        let network = parse_network(&self.scope.network)?;
        let deposit_address_kind = match self.deposit_address_kind.as_str() {
            "p2wpkh" => BitcoinAddressKind::SegwitV0,
            "p2tr" => BitcoinAddressKind::Taproot,
            _ => {
                return Err(invalid(
                    "Bitcoin deposit address kind must be `p2wpkh` or `p2tr`",
                ));
            }
        };
        if self.deposit_ttl_seconds == 0 {
            return Err(invalid("deposit TTL must be greater than zero"));
        }

        let master = BitcoinAddress::parse_for_network(&self.master_destination, network)
            .map_err(|_| invalid("master destination is not canonical for the policy network"))?;
        if master.0 != self.master_destination {
            return Err(invalid(
                "master destination must use Bitcoin's canonical address encoding",
            ));
        }
        let minimum_collection =
            positive_satoshis(&self.minimum_collection_satoshis, "minimum collection")?;
        if self.minimum_spend_confirmations == 0 {
            return Err(invalid(
                "minimum spend confirmations must be greater than zero",
            ));
        }
        let requested_fee_rate =
            positive_fee_rate(&self.requested_satoshis_per_kvb, "requested fee rate")?;
        let maximum_fee_rate =
            positive_fee_rate(&self.maximum_satoshis_per_kvb, "maximum fee rate")?;
        if requested_fee_rate > maximum_fee_rate {
            return Err(invalid(
                "requested fee rate must not exceed maximum fee rate",
            ));
        }
        if maximum_fee_rate.satoshis_per_kvb() > BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB {
            return Err(invalid(
                "maximum fee rate must not exceed Bitcoin Core's 1 BTC/kvB ceiling",
            ));
        }
        let maximum_absolute_fee =
            positive_satoshis(&self.maximum_absolute_fee_satoshis, "maximum absolute fee")?;
        if self.maximum_deposits == 0 || self.maximum_deposits > MAX_BATCH_DEPOSITS {
            return Err(invalid(format!(
                "maximum deposits must be between 1 and {MAX_BATCH_DEPOSITS}",
            )));
        }
        if self.maximum_inputs == 0 || self.maximum_inputs > MAX_COLLECTION_SPEND_RESOURCES {
            return Err(invalid(format!(
                "maximum inputs must be between 1 and {MAX_COLLECTION_SPEND_RESOURCES}",
            )));
        }
        if self.maximum_inputs < self.maximum_deposits {
            return Err(invalid("maximum inputs must be at least maximum deposits"));
        }

        let chain = ChainId("bitcoin".to_owned());
        let scope = IndexScope {
            chain: chain.clone(),
            network: network.canonical_name().to_owned(),
        };
        Ok(BitcoinPaymentPolicy {
            version: self.version,
            scope,
            network,
            deposit_address_kind,
            deposit_ttl: Duration::from_secs(self.deposit_ttl_seconds),
            asset: AssetId {
                chain: chain.clone(),
                asset: "native".to_owned(),
            },
            master_destination: CanonicalAddress {
                chain,
                value: master.0,
            },
            minimum_collection,
            minimum_spend_confirmations: self.minimum_spend_confirmations,
            requested_fee_rate,
            maximum_fee_rate,
            maximum_absolute_fee,
            maximum_deposits: self.maximum_deposits,
            maximum_inputs: self.maximum_inputs,
            digest: Sha256::digest(bytes).into(),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BitcoinScopeDocument {
    chain: String,
    network: String,
}

fn parse_network(input: &str) -> Result<BitcoinNetwork, BitcoinPolicyError> {
    match input {
        "mainnet" => Ok(BitcoinNetwork::Mainnet),
        "testnet3" => Ok(BitcoinNetwork::Testnet3),
        "testnet4" => Ok(BitcoinNetwork::Testnet4),
        "signet" => Ok(BitcoinNetwork::Signet),
        "regtest" => Ok(BitcoinNetwork::Regtest),
        _ => Err(invalid(
            "Bitcoin network must be mainnet, testnet3, testnet4, signet, or regtest",
        )),
    }
}

fn canonical_u64(input: &str, name: &str) -> Result<u64, BitcoinPolicyError> {
    if input.is_empty()
        || (input.len() > 1 && input.starts_with('0'))
        || !input.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(format!(
            "{name} must be a canonical unsigned decimal string",
        )));
    }
    input
        .parse::<u64>()
        .map_err(|_| invalid(format!("{name} exceeds the u64 range")))
}

fn positive_satoshis(input: &str, name: &str) -> Result<Satoshi, BitcoinPolicyError> {
    let value = canonical_u64(input, name)?;
    if value == 0 {
        return Err(invalid(format!("{name} must be greater than zero")));
    }
    Ok(Satoshi(value))
}

fn positive_fee_rate(input: &str, name: &str) -> Result<SatoshisPerKvb, BitcoinPolicyError> {
    positive_satoshis(input, name).map(|value| SatoshisPerKvb::new(value.0))
}

fn invalid(message: impl Into<String>) -> BitcoinPolicyError {
    BitcoinPolicyError::new(BitcoinPolicyErrorKind::Invalid, message)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitcoinPolicyErrorKind {
    Io,
    TooLarge,
    InvalidJson,
    Invalid,
}

#[derive(Debug)]
pub struct BitcoinPolicyError {
    pub kind: BitcoinPolicyErrorKind,
    pub message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl BitcoinPolicyError {
    fn new(kind: BitcoinPolicyErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: BitcoinPolicyErrorKind,
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for BitcoinPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for BitcoinPolicyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGTEST_MASTER: &str = "bcrt1qtwxw3vnj3f29szvhvr84k0aekcrhh9cla5nxa0";

    fn policy(overrides: &str) -> Vec<u8> {
        format!(
            r#"{{
                "version": 1,
                "scope": {{"chain": "bitcoin", "network": "regtest"}},
                "deposit_address_kind": "p2wpkh",
                "deposit_ttl_seconds": 3600,
                "master_destination": "{REGTEST_MASTER}",
                "minimum_collection_satoshis": "10000",
                "minimum_spend_confirmations": 6,
                "requested_satoshis_per_kvb": "1000",
                "maximum_satoshis_per_kvb": "5000",
                "maximum_absolute_fee_satoshis": "50000",
                "maximum_deposits": 20,
                "maximum_inputs": 200{overrides}
            }}"#,
        )
        .into_bytes()
    }

    #[test]
    fn parses_every_explicit_bitcoin_collection_bound() {
        let policy = BitcoinPaymentPolicy::from_json(&policy(""))
            .expect("complete canonical Bitcoin policy must parse");

        assert_eq!(policy.scope.chain.0, "bitcoin");
        assert_eq!(policy.scope.network, "regtest");
        assert_eq!(policy.deposit_address_kind, BitcoinAddressKind::SegwitV0);
        assert_eq!(policy.minimum_collection, Satoshi(10_000));
        assert_eq!(policy.minimum_spend_confirmations, 6);
        assert_eq!(policy.requested_fee_rate.satoshis_per_kvb(), 1_000);
        assert_eq!(policy.maximum_fee_rate.satoshis_per_kvb(), 5_000);
        assert_eq!(policy.maximum_absolute_fee, Satoshi(50_000));
        assert_eq!(policy.maximum_deposits, 20);
        assert_eq!(policy.maximum_inputs, 200);
    }

    #[test]
    fn rejects_missing_or_unknown_collection_bounds() {
        let missing = String::from_utf8(policy("")).expect("fixture must be utf8");
        let missing = missing.replace("\n                \"maximum_inputs\": 200", "");
        assert!(BitcoinPaymentPolicy::from_json(missing.as_bytes()).is_err());

        let unknown = policy(",\n                \"automatic_collection\": true");
        assert!(BitcoinPaymentPolicy::from_json(&unknown).is_err());
    }

    #[test]
    fn rejects_wrong_network_address_and_unsafe_fee_bounds() {
        let wrong_address = String::from_utf8(policy("")).expect("fixture must be utf8");
        let wrong_address =
            wrong_address.replace(REGTEST_MASTER, "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080");
        assert!(BitcoinPaymentPolicy::from_json(wrong_address.as_bytes()).is_err());

        let excessive = String::from_utf8(policy("")).expect("fixture must be utf8");
        let excessive = excessive.replace(
            "\"maximum_satoshis_per_kvb\": \"5000\"",
            "\"maximum_satoshis_per_kvb\": \"100000001\"",
        );
        assert!(BitcoinPaymentPolicy::from_json(excessive.as_bytes()).is_err());

        let core_boundary = String::from_utf8(policy("")).expect("fixture must be utf8");
        let core_boundary = core_boundary.replace(
            "\"maximum_satoshis_per_kvb\": \"5000\"",
            "\"maximum_satoshis_per_kvb\": \"100000000\"",
        );
        assert!(BitcoinPaymentPolicy::from_json(core_boundary.as_bytes()).is_ok());
    }

    #[test]
    fn rejects_noncanonical_amounts_and_inverted_limits() {
        let leading_zero = String::from_utf8(policy("")).expect("fixture must be utf8");
        let leading_zero = leading_zero.replace(
            "\"minimum_collection_satoshis\": \"10000\"",
            "\"minimum_collection_satoshis\": \"010000\"",
        );
        assert!(BitcoinPaymentPolicy::from_json(leading_zero.as_bytes()).is_err());

        let inverted = String::from_utf8(policy("")).expect("fixture must be utf8");
        let inverted = inverted.replace("\"maximum_inputs\": 200", "\"maximum_inputs\": 10");
        assert!(BitcoinPaymentPolicy::from_json(inverted.as_bytes()).is_err());

        let above_storage_limit = String::from_utf8(policy("")).expect("fixture must be utf8");
        let above_storage_limit = above_storage_limit.replace(
            "\"maximum_inputs\": 200",
            &format!("\"maximum_inputs\": {}", MAX_COLLECTION_SPEND_RESOURCES + 1),
        );
        assert!(BitcoinPaymentPolicy::from_json(above_storage_limit.as_bytes()).is_err());
    }
}
