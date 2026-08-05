use std::{collections::BTreeMap, error::Error, fmt, fs, path::Path, time::Duration};

use chain_ethereum::EthereumAddress;
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, ChainId};
use deposits::PolicyIdentity;
use indexing::IndexScope;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use signer::KeyLocator;

const MAX_POLICY_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentPolicy {
    pub version: u32,
    pub scope: IndexScope,
    pub ethereum_chain_id: u64,
    pub deposit_ttl: Duration,
    pub assets: BTreeMap<AssetId, AssetPolicy>,
    pub fees: EthereumFeePolicy,
    pub gas_funder: GasFunderPolicy,
    pub digest: [u8; 32],
}

impl PaymentPolicy {
    pub fn load(path: &Path) -> Result<Self, PolicyError> {
        let metadata = fs::metadata(path).map_err(|error| {
            PolicyError::with_source(
                PolicyErrorKind::Io,
                "failed to inspect Payment Service policy",
                error,
            )
        })?;
        if !metadata.is_file() {
            return Err(PolicyError::new(
                PolicyErrorKind::Invalid,
                "Payment Service policy path must identify a regular file",
            ));
        }
        if metadata.len() > MAX_POLICY_BYTES {
            return Err(PolicyError::new(
                PolicyErrorKind::TooLarge,
                "Payment Service policy exceeds the one-megabyte limit",
            ));
        }
        let bytes = fs::read(path).map_err(|error| {
            PolicyError::with_source(
                PolicyErrorKind::Io,
                "failed to read Payment Service policy",
                error,
            )
        })?;
        Self::from_json(&bytes)
    }

    pub(crate) fn from_json(bytes: &[u8]) -> Result<Self, PolicyError> {
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err(PolicyError::new(
                PolicyErrorKind::TooLarge,
                "Payment Service policy exceeds the one-megabyte limit",
            ));
        }
        let document: PolicyDocument = serde_json::from_slice(bytes).map_err(|error| {
            PolicyError::with_source(
                PolicyErrorKind::InvalidJson,
                "Payment Service policy is not valid JSON",
                error,
            )
        })?;
        document.validate(bytes)
    }

    pub fn asset(&self, asset: &AssetId) -> Result<&AssetPolicy, PolicyError> {
        self.assets.get(asset).ok_or_else(|| {
            PolicyError::new(
                PolicyErrorKind::UnsupportedAsset,
                "asset is not enabled by the active Payment Service policy",
            )
        })
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
        encode_hex(&self.digest)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetPolicy {
    pub asset: AssetId,
    pub master_destination: CanonicalAddress,
    pub minimum_collection_amount: AtomicAmount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumFeePolicy {
    pub max_fee_per_gas: AtomicAmount,
    pub max_priority_fee_per_gas: AtomicAmount,
    pub max_gas_limit: u64,
    pub max_total_fee: AtomicAmount,
}

#[derive(Clone, PartialEq, Eq)]
pub struct GasFunderPolicy {
    pub address: CanonicalAddress,
    pub key: KeyLocator,
    pub maximum_funding_amount: AtomicAmount,
}

impl fmt::Debug for GasFunderPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GasFunderPolicy")
            .field("address", &self.address)
            .field("key", &"[REDACTED]")
            .field("maximum_funding_amount", &self.maximum_funding_amount)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDocument {
    version: u32,
    scope: ScopeDocument,
    deposit_ttl_seconds: u64,
    assets: Vec<AssetDocument>,
    fees: FeeDocument,
    gas_funder: GasFunderDocument,
}

impl PolicyDocument {
    fn validate(self, bytes: &[u8]) -> Result<PaymentPolicy, PolicyError> {
        if self.version == 0 {
            return Err(invalid("policy version must be greater than zero"));
        }
        if self.scope.chain != "ethereum" {
            return Err(invalid("Ethereum v1 policy scope chain must be `ethereum`"));
        }
        if self.scope.network.trim().is_empty() {
            return Err(invalid("policy network must not be empty"));
        }
        if self.scope.chain_id == 0 {
            return Err(invalid(
                "Ethereum policy chain ID must be greater than zero",
            ));
        }
        if self.deposit_ttl_seconds == 0 {
            return Err(invalid("deposit TTL must be greater than zero"));
        }
        if self.assets.is_empty() {
            return Err(invalid("policy asset allowlist must not be empty"));
        }

        let chain = ChainId("ethereum".to_owned());
        let mut assets = BTreeMap::new();
        for document in self.assets {
            let asset_name = canonical_asset(&document.asset)?;
            let asset = AssetId {
                chain: chain.clone(),
                asset: asset_name,
            };
            let policy = AssetPolicy {
                asset: asset.clone(),
                master_destination: canonical_address(&document.master_destination)?,
                minimum_collection_amount: positive_amount(
                    &document.minimum_collection_amount,
                    "minimum collection amount",
                )?,
            };
            if assets.insert(asset, policy).is_some() {
                return Err(invalid("policy contains a duplicate asset"));
            }
        }

        let fees = EthereumFeePolicy {
            max_fee_per_gas: positive_amount(&self.fees.max_fee_per_gas, "maximum fee per gas")?,
            max_priority_fee_per_gas: self
                .fees
                .max_priority_fee_per_gas
                .parse()
                .map_err(|_| invalid("maximum priority fee per gas is invalid"))?,
            max_gas_limit: self.fees.max_gas_limit,
            max_total_fee: positive_amount(&self.fees.max_total_fee, "maximum total fee")?,
        };
        if fees.max_gas_limit == 0 {
            return Err(invalid("maximum gas limit must be greater than zero"));
        }
        if fees.max_priority_fee_per_gas > fees.max_fee_per_gas {
            return Err(invalid(
                "maximum priority fee per gas must not exceed maximum fee per gas",
            ));
        }

        let key = self.gas_funder.key_locator.trim();
        if key.is_empty() {
            return Err(invalid("gas-funder key locator must not be empty"));
        }
        let gas_funder = GasFunderPolicy {
            address: canonical_address(&self.gas_funder.address)?,
            key: KeyLocator::Identifier(key.to_owned()),
            maximum_funding_amount: positive_amount(
                &self.gas_funder.maximum_funding_amount,
                "maximum gas-funding amount",
            )?,
        };

        Ok(PaymentPolicy {
            version: self.version,
            scope: IndexScope {
                chain,
                network: self.scope.network,
            },
            ethereum_chain_id: self.scope.chain_id,
            deposit_ttl: Duration::from_secs(self.deposit_ttl_seconds),
            assets,
            fees,
            gas_funder,
            digest: Sha256::digest(bytes).into(),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeDocument {
    chain: String,
    network: String,
    chain_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetDocument {
    asset: String,
    master_destination: String,
    minimum_collection_amount: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeeDocument {
    max_fee_per_gas: String,
    max_priority_fee_per_gas: String,
    max_gas_limit: u64,
    max_total_fee: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GasFunderDocument {
    address: String,
    key_locator: String,
    maximum_funding_amount: String,
}

fn canonical_asset(input: &str) -> Result<String, PolicyError> {
    if input == "native" {
        return Ok(input.to_owned());
    }
    let address = input
        .parse::<EthereumAddress>()
        .map_err(|_| invalid("ERC-20 asset must be a canonical Ethereum address"))?;
    let canonical = address.to_string();
    if input != canonical {
        return Err(invalid(
            "ERC-20 asset address must use lowercase canonical hexadecimal",
        ));
    }
    Ok(canonical)
}

fn canonical_address(input: &str) -> Result<CanonicalAddress, PolicyError> {
    let address = input
        .parse::<EthereumAddress>()
        .map_err(|_| invalid("policy contains an invalid Ethereum address"))?;
    let canonical = address.to_string();
    if input != canonical {
        return Err(invalid(
            "Ethereum policy addresses must use lowercase canonical hexadecimal",
        ));
    }
    Ok(address.into())
}

fn positive_amount(input: &str, name: &str) -> Result<AtomicAmount, PolicyError> {
    let amount = input
        .parse::<AtomicAmount>()
        .map_err(|_| invalid(format!("{name} is not a canonical atomic amount")))?;
    if amount.is_zero() {
        return Err(invalid(format!("{name} must be greater than zero")));
    }
    Ok(amount)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn invalid(message: impl Into<String>) -> PolicyError {
    PolicyError::new(PolicyErrorKind::Invalid, message)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyErrorKind {
    Io,
    TooLarge,
    InvalidJson,
    Invalid,
    UnsupportedAsset,
}

#[derive(Debug)]
pub struct PolicyError {
    pub kind: PolicyErrorKind,
    pub message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl PolicyError {
    fn new(kind: PolicyErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: PolicyErrorKind,
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

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for PolicyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_policy() -> Vec<u8> {
        br#"{
            "version": 1,
            "scope": {"chain": "ethereum", "network": "test", "chain_id": 1},
            "deposit_ttl_seconds": 3600,
            "assets": [
                {
                    "asset": "native",
                    "master_destination": "0x1111111111111111111111111111111111111111",
                    "minimum_collection_amount": "1000"
                },
                {
                    "asset": "0x2222222222222222222222222222222222222222",
                    "master_destination": "0x3333333333333333333333333333333333333333",
                    "minimum_collection_amount": "250"
                }
            ],
            "fees": {
                "max_fee_per_gas": "100",
                "max_priority_fee_per_gas": "10",
                "max_gas_limit": 200000,
                "max_total_fee": "20000000"
            },
            "gas_funder": {
                "address": "0x4444444444444444444444444444444444444444",
                "key_locator": "kms:gas-funder",
                "maximum_funding_amount": "5000000"
            }
        }"#
        .to_vec()
    }

    #[test]
    fn validates_and_materializes_versioned_policy() {
        let bytes = valid_policy();
        let policy = PaymentPolicy::from_json(&bytes).expect("valid policy must parse");

        assert_eq!(policy.version, 1);
        assert_eq!(policy.scope.network, "test");
        assert_eq!(policy.ethereum_chain_id, 1);
        assert_eq!(policy.assets.len(), 2);
        assert_eq!(policy.deposit_ttl, Duration::from_secs(3600));
        assert_eq!(policy.digest, <[u8; 32]>::from(Sha256::digest(&bytes)));
        assert_eq!(
            policy.gas_funder.key,
            KeyLocator::Identifier("kms:gas-funder".to_owned())
        );
    }

    #[test]
    fn rejects_duplicate_assets_and_noncanonical_addresses() {
        let duplicate = String::from_utf8(valid_policy())
            .expect("fixture must be UTF-8")
            .replace("0x2222222222222222222222222222222222222222", "native");
        assert_eq!(
            PaymentPolicy::from_json(duplicate.as_bytes())
                .expect_err("duplicate must fail")
                .kind,
            PolicyErrorKind::Invalid
        );

        let uppercase = String::from_utf8(valid_policy())
            .expect("fixture must be UTF-8")
            .replace(
                "0x1111111111111111111111111111111111111111",
                "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            );
        assert!(PaymentPolicy::from_json(uppercase.as_bytes()).is_err());
    }

    #[test]
    fn rejects_invalid_thresholds_scope_and_unknown_fields() {
        let zero_ttl = String::from_utf8(valid_policy())
            .expect("fixture must be UTF-8")
            .replace(
                "\"deposit_ttl_seconds\": 3600",
                "\"deposit_ttl_seconds\": 0",
            );
        assert!(PaymentPolicy::from_json(zero_ttl.as_bytes()).is_err());

        let wrong_scope = String::from_utf8(valid_policy())
            .expect("fixture must be UTF-8")
            .replace("\"chain\": \"ethereum\"", "\"chain\": \"bitcoin\"");
        assert!(PaymentPolicy::from_json(wrong_scope.as_bytes()).is_err());

        let zero_chain_id = String::from_utf8(valid_policy())
            .expect("fixture must be UTF-8")
            .replace("\"chain_id\": 1", "\"chain_id\": 0");
        assert!(PaymentPolicy::from_json(zero_chain_id.as_bytes()).is_err());

        let unknown = String::from_utf8(valid_policy())
            .expect("fixture must be UTF-8")
            .replace("\"version\": 1", "\"version\": 1, \"unexpected\": true");
        assert_eq!(
            PaymentPolicy::from_json(unknown.as_bytes())
                .expect_err("unknown field must fail")
                .kind,
            PolicyErrorKind::InvalidJson
        );
    }

    #[test]
    fn rejects_priority_fee_above_total_per_gas_ceiling() {
        let invalid_fees = String::from_utf8(valid_policy())
            .expect("fixture must be UTF-8")
            .replace(
                "\"max_priority_fee_per_gas\": \"10\"",
                "\"max_priority_fee_per_gas\": \"101\"",
            );
        assert!(PaymentPolicy::from_json(invalid_fees.as_bytes()).is_err());
    }

    #[test]
    fn policy_debug_output_does_not_expose_the_gas_funder_locator() {
        let policy = PaymentPolicy::from_json(&valid_policy()).expect("policy must parse");
        let output = format!("{policy:?}");
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("kms:gas-funder"));
    }
}
