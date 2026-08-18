use std::{error, fmt, net::SocketAddr, path::PathBuf, time::Duration};

use indexing::IndexScope;

const DEFAULT_RECONCILE_SECONDS: u64 = 5;
const DEFAULT_RECONCILE_LIMIT: usize = 256;

/// Operational settings for one Payment Service process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub bind: SocketAddr,
    pub reconcile_interval: Duration,
    pub reconcile_limit: usize,
    pub scopes: Vec<IndexScope>,
}

impl Config {
    #[must_use]
    pub fn new(bind: SocketAddr, scopes: Vec<IndexScope>) -> Self {
        Self {
            bind,
            reconcile_interval: Duration::from_secs(DEFAULT_RECONCILE_SECONDS),
            reconcile_limit: DEFAULT_RECONCILE_LIMIT,
            scopes,
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.reconcile_interval.is_zero() {
            return Err(ConfigError::new("reconcile interval must be positive"));
        }
        if self.reconcile_limit == 0 {
            return Err(ConfigError::new("reconcile limit must be positive"));
        }
        if self.scopes.is_empty() {
            return Err(ConfigError::new(
                "at least one reconciliation scope must be configured",
            ));
        }
        for (index, scope) in self.scopes.iter().enumerate() {
            if scope.chain.0.trim().is_empty() || scope.network.trim().is_empty() {
                return Err(ConfigError::new(
                    "reconciliation scopes require a chain and network",
                ));
            }
            if self.scopes[..index].contains(scope) {
                return Err(ConfigError::new(
                    "reconciliation scopes must not contain duplicates",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for ConfigError {}

/// Complete process configuration loaded by the `payment-api` binary.
///
/// Private keys are referenced by environment-variable name. They are never
/// accepted in this serializable value, which makes configuration diagnostics
/// and accidental JSON logging safe.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub bind: SocketAddr,
    pub server: ServerConfig,
    pub database: PathBuf,
    pub indexer: IndexerConfig,
    pub wallets: Vec<WalletConfig>,
    #[serde(default)]
    pub deposits: Option<DepositConfig>,
    #[serde(default = "default_reconcile_seconds")]
    pub reconcile_seconds: u64,
    #[serde(default = "default_reconcile_limit")]
    pub reconcile_limit: usize,
}

/// A finite app-owned set of local deposit keys for one configured wallet.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepositConfig {
    pub wallet: String,
    #[serde(default = "native_asset")]
    pub asset: String,
    #[serde(default)]
    pub gas_wallet: Option<String>,
    pub policy_version: String,
    pub policy_digest: String,
    pub minimum_collection: String,
    pub minimum_confirmations: u64,
    pub coinbase_maturity: u64,
    pub max_participants: usize,
    pub max_inputs: usize,
    #[serde(default)]
    pub gas_amount: Option<String>,
    pub keys: Vec<KeyConfig>,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    pub purpose: String,
    pub secret_env: String,
}

/// Public HTTP boundary settings. Secret values are referenced, never stored.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bearer_token_env: String,
    #[serde(default = "default_request_bytes")]
    pub max_request_body_bytes: usize,
    #[serde(default)]
    pub tls_terminated_upstream: bool,
}

impl RuntimeConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.bearer_token_env.trim().is_empty() {
            return Err(ConfigError::new(
                "server bearer-token environment name must not be empty",
            ));
        }
        if self.server.max_request_body_bytes == 0 {
            return Err(ConfigError::new(
                "server maximum request body size must be positive",
            ));
        }
        if self.database.as_os_str().is_empty() {
            return Err(ConfigError::new("payment database path must not be empty"));
        }
        if self.indexer.endpoints.is_empty()
            || self
                .indexer
                .endpoints
                .iter()
                .any(|endpoint| endpoint.trim().is_empty())
        {
            return Err(ConfigError::new(
                "at least one non-empty Indexer endpoint is required",
            ));
        }
        if self.wallets.is_empty() {
            return Err(ConfigError::new("at least one wallet must be configured"));
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut scopes = std::collections::BTreeSet::new();
        for wallet in &self.wallets {
            let (id, scope, secret_env) = wallet.identity();
            if id.trim().is_empty() || secret_env.trim().is_empty() {
                return Err(ConfigError::new(
                    "wallet ID and private-key environment name must not be empty",
                ));
            }
            if let WalletConfig::Bitcoin(bitcoin) = wallet
                && (bitcoin.rpc_urls.is_empty()
                    || bitcoin
                        .rpc_urls
                        .iter()
                        .any(|endpoint| endpoint.trim().is_empty()))
            {
                return Err(ConfigError::new(
                    "Bitcoin requires at least one non-empty RPC endpoint",
                ));
            }
            if let WalletConfig::Ethereum(ethereum) = wallet {
                if ethereum.rpc_urls.is_empty()
                    || ethereum
                        .rpc_urls
                        .iter()
                        .any(|endpoint| endpoint.trim().is_empty())
                {
                    return Err(ConfigError::new(
                        "Ethereum requires at least one non-empty RPC endpoint",
                    ));
                }
                if let EthereumAsset::Erc20 { contract, decimals } = &ethereum.asset
                    && (contract.parse::<chain_ethereum::Address>().is_err() || *decimals == 0)
                {
                    return Err(ConfigError::new(
                        "Ethereum ERC-20 asset requires a canonical contract and positive decimals",
                    ));
                }
                if !matches!(
                    (ethereum.network.as_str(), ethereum.chain_id),
                    ("mainnet", 1) | ("sepolia", 11_155_111)
                ) {
                    return Err(ConfigError::new(
                        "Ethereum network and chain ID must match a supported wallet network",
                    ));
                }
            }
            if !ids.insert(id) {
                return Err(ConfigError::new("wallet IDs must be unique"));
            }
            scopes.insert(scope);
        }
        if let Some(deposits) = &self.deposits {
            if deposits.wallet.trim().is_empty()
                || deposits.asset.trim().is_empty()
                || deposits.policy_version.trim().is_empty()
                || hex::decode(&deposits.policy_digest).is_err()
                || deposits.policy_digest.len() != 64
                || deposits
                    .minimum_collection
                    .parse::<base::Decimal>()
                    .map_or(true, |amount| amount <= base::Decimal::zero())
                || deposits.max_participants == 0
                || deposits.max_inputs == 0
                || deposits.keys.is_empty()
            {
                return Err(ConfigError::new(
                    "deposit wallet, asset, and at least one key are required",
                ));
            }
            let Some(wallet) = self
                .wallets
                .iter()
                .find(|wallet| wallet.identity().0 == deposits.wallet)
            else {
                return Err(ConfigError::new(
                    "deposit configuration references an unknown wallet",
                ));
            };
            if wallet.asset() != deposits.asset.to_ascii_lowercase() {
                return Err(ConfigError::new(
                    "deposit asset must match the configured wallet asset",
                ));
            }
            if let Some(gas_id) = &deposits.gas_wallet {
                let gas = self
                    .wallets
                    .iter()
                    .find(|candidate| candidate.identity().0 == *gas_id)
                    .ok_or_else(|| {
                        ConfigError::new("deposit gas wallet references an unknown wallet")
                    })?;
                if gas.identity().1 != wallet.identity().1 || gas.asset() != "native" {
                    return Err(ConfigError::new(
                        "deposit gas wallet must be native and share the deposit scope",
                    ));
                }
                if deposits.asset == "native" {
                    return Err(ConfigError::new(
                        "a native deposit asset must not configure a gas wallet",
                    ));
                }
                if deposits
                    .gas_amount
                    .as_deref()
                    .and_then(|value| value.parse::<base::Decimal>().ok())
                    .is_none_or(|amount| amount <= base::Decimal::zero())
                {
                    return Err(ConfigError::new(
                        "token collection requires a positive configured gas amount",
                    ));
                }
            } else if deposits.gas_amount.is_some() {
                return Err(ConfigError::new(
                    "gas amount is only valid for a token collection",
                ));
            }
            let wallet_secret = wallet.identity().2;
            let mut purposes = std::collections::BTreeSet::new();
            let mut secret_names = std::collections::BTreeSet::new();
            for key in &deposits.keys {
                if key.purpose.trim().is_empty() || key.secret_env.trim().is_empty() {
                    return Err(ConfigError::new(
                        "deposit key purpose and private-key environment name must not be empty",
                    ));
                }
                if !purposes.insert(&key.purpose) {
                    return Err(ConfigError::new(
                        "deposit key purposes must not contain duplicates",
                    ));
                }
                if !secret_names.insert(&key.secret_env) {
                    return Err(ConfigError::new(
                        "deposit private-key environment names must not contain duplicates",
                    ));
                }
                if key.secret_env == wallet_secret {
                    return Err(ConfigError::new(
                        "a deposit key must not reuse the configured payment wallet key",
                    ));
                }
            }
        }
        Config {
            bind: self.bind,
            reconcile_interval: Duration::from_secs(self.reconcile_seconds),
            reconcile_limit: self.reconcile_limit,
            scopes: scopes.into_iter().collect(),
        }
        .validate()
    }

    pub(crate) fn service(&self) -> Config {
        let scopes = self
            .wallets
            .iter()
            .map(WalletConfig::identity)
            .map(|(_, scope, _)| scope)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        Config {
            bind: self.bind,
            reconcile_interval: Duration::from_secs(self.reconcile_seconds),
            reconcile_limit: self.reconcile_limit,
            scopes,
        }
    }
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerConfig {
    pub endpoints: Vec<String>,
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_response_bytes")]
    pub max_response_bytes: usize,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "chain", rename_all = "lowercase", deny_unknown_fields)]
pub enum WalletConfig {
    Bitcoin(BitcoinConfig),
    Ethereum(EthereumConfig),
}

impl WalletConfig {
    pub(crate) fn identity(&self) -> (String, IndexScope, &str) {
        match self {
            Self::Bitcoin(value) => (
                value.id.clone(),
                IndexScope {
                    chain: indexing::ChainId(chain_bitcoin::CHAIN.to_owned()),
                    network: value.network.clone(),
                },
                &value.secret_env,
            ),
            Self::Ethereum(value) => (
                value.id.clone(),
                IndexScope {
                    chain: indexing::ChainId(chain_ethereum::CHAIN.to_owned()),
                    network: value.network.clone(),
                },
                &value.secret_env,
            ),
        }
    }

    pub(crate) fn asset(&self) -> String {
        match self {
            Self::Bitcoin(_) => native_asset(),
            Self::Ethereum(value) => match &value.asset {
                EthereumAsset::Native => native_asset(),
                EthereumAsset::Erc20 { contract, .. } => contract.to_ascii_lowercase(),
            },
        }
    }
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BitcoinConfig {
    pub id: String,
    pub network: String,
    pub rpc_urls: Vec<String>,
    #[serde(default)]
    pub rpc_headers: Vec<(String, String)>,
    pub genesis_hash: String,
    pub secret_env: String,
    #[serde(default)]
    pub taproot: bool,
    #[serde(default = "default_fee_blocks")]
    pub fee_target_blocks: u16,
    pub max_fee_rate: u64,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_response_bytes")]
    pub max_response_bytes: usize,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EthereumConfig {
    pub id: String,
    pub network: String,
    pub rpc_urls: Vec<String>,
    #[serde(default)]
    pub rpc_headers: Vec<(String, String)>,
    pub chain_id: u64,
    #[serde(default)]
    pub asset: EthereumAsset,
    pub secret_env: String,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_response_bytes")]
    pub max_response_bytes: usize,
    #[serde(default = "default_input_bytes")]
    pub max_input_bytes: usize,
    #[serde(default = "default_gas_margin")]
    pub gas_margin_basis_points: u32,
    #[serde(default = "default_gas_limit")]
    pub max_gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub max_total_fee: u128,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EthereumAsset {
    #[default]
    Native,
    Erc20 {
        contract: String,
        decimals: u32,
    },
}

const fn default_reconcile_seconds() -> u64 {
    DEFAULT_RECONCILE_SECONDS
}
const fn default_reconcile_limit() -> usize {
    DEFAULT_RECONCILE_LIMIT
}
const fn default_timeout_seconds() -> u64 {
    15
}
const fn default_response_bytes() -> usize {
    16 * 1024 * 1024
}
const fn default_request_bytes() -> usize {
    1024 * 1024
}
const fn default_fee_blocks() -> u16 {
    6
}
const fn default_input_bytes() -> usize {
    128 * 1024
}
const fn default_gas_margin() -> u32 {
    2_000
}
const fn default_gas_limit() -> u64 {
    30_000_000
}

fn native_asset() -> String {
    "native".to_owned()
}

impl fmt::Debug for RuntimeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeConfig")
            .field("bind", &self.bind)
            .field("server", &self.server)
            .field("database", &self.database)
            .field("indexer", &self.indexer)
            .field("wallet_count", &self.wallets.len())
            .field("deposits", &self.deposits.as_ref().map(|_| "[CONFIGURED]"))
            .field("reconcile_seconds", &self.reconcile_seconds)
            .field("reconcile_limit", &self.reconcile_limit)
            .finish()
    }
}

impl fmt::Debug for DepositConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DepositConfig")
            .field("wallet", &self.wallet)
            .field("asset", &self.asset)
            .field("gas_wallet", &self.gas_wallet)
            .field("policy_version", &self.policy_version)
            .field("minimum_collection", &self.minimum_collection)
            .field("minimum_confirmations", &self.minimum_confirmations)
            .field("coinbase_maturity", &self.coinbase_maturity)
            .field("max_participants", &self.max_participants)
            .field("max_inputs", &self.max_inputs)
            .field("gas_amount", &self.gas_amount)
            .field("key_count", &self.keys.len())
            .finish()
    }
}

impl fmt::Debug for KeyConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyConfig")
            .field("purpose", &self.purpose)
            .field("secret_env", &"[CONFIGURED]")
            .finish()
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("bearer_token_env", &"[CONFIGURED]")
            .field("max_request_body_bytes", &self.max_request_body_bytes)
            .field("tls_terminated_upstream", &self.tls_terminated_upstream)
            .finish()
    }
}

impl fmt::Debug for IndexerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IndexerConfig")
            .field("endpoint_count", &self.endpoints.len())
            .field(
                "bearer_token_env",
                &self.bearer_token_env.as_ref().map(|_| "[CONFIGURED]"),
            )
            .field("timeout_seconds", &self.timeout_seconds)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl fmt::Debug for WalletConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bitcoin(value) => formatter.debug_tuple("Bitcoin").field(value).finish(),
            Self::Ethereum(value) => formatter.debug_tuple("Ethereum").field(value).finish(),
        }
    }
}

impl fmt::Debug for BitcoinConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitcoinConfig")
            .field("id", &self.id)
            .field("network", &self.network)
            .field("rpc_endpoint_count", &self.rpc_urls.len())
            .field("rpc_header_count", &self.rpc_headers.len())
            .field("secret_env", &"[CONFIGURED]")
            .field("taproot", &self.taproot)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for EthereumConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EthereumConfig")
            .field("id", &self.id)
            .field("network", &self.network)
            .field("rpc_endpoint_count", &self.rpc_urls.len())
            .field("rpc_header_count", &self.rpc_headers.len())
            .field("chain_id", &self.chain_id)
            .field("secret_env", &"[CONFIGURED]")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indexing::ChainId;

    use super::*;

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("fixture".to_owned()),
            network: "local".to_owned(),
        }
    }

    #[test]
    fn validates_reconciliation_policy() {
        let bind = SocketAddr::from(([127, 0, 0, 1], 8080));
        assert!(Config::new(bind, Vec::new()).validate().is_err());

        let mut config = Config::new(bind, vec![scope(), scope()]);
        assert!(config.validate().is_err());
        config.scopes.pop();
        config.reconcile_limit = 0;
        assert!(config.validate().is_err());
        config.reconcile_limit = 1;
        config.reconcile_interval = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    fn bitcoin() -> WalletConfig {
        WalletConfig::Bitcoin(BitcoinConfig {
            id: "treasury-btc".to_owned(),
            network: "regtest".to_owned(),
            rpc_urls: vec!["http://127.0.0.1:18443".to_owned()],
            rpc_headers: Vec::new(),
            genesis_hash: "0f9188f13cb7b2c9e5c32db7e0c90a34f4f6a2f9c0c7f7891d3c9d3a00000000"
                .to_owned(),
            secret_env: "PAYMENT_BTC_PRIVATE_KEY".to_owned(),
            taproot: false,
            fee_target_blocks: 6,
            max_fee_rate: 100_000,
            timeout_seconds: 1,
            max_response_bytes: 1024,
        })
    }

    fn ethereum(id: &str, asset: EthereumAsset) -> WalletConfig {
        WalletConfig::Ethereum(EthereumConfig {
            id: id.to_owned(),
            network: "sepolia".to_owned(),
            rpc_urls: vec!["http://127.0.0.1:8545".to_owned()],
            rpc_headers: Vec::new(),
            chain_id: 11_155_111,
            asset,
            secret_env: format!("PAYMENT_{}_KEY", id.to_ascii_uppercase()),
            timeout_seconds: 1,
            max_response_bytes: 1024,
            max_input_bytes: 1024,
            gas_margin_basis_points: 100,
            max_gas_limit: 1_000_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            max_total_fee: 1_000_000,
        })
    }

    fn runtime(wallets: Vec<WalletConfig>) -> RuntimeConfig {
        RuntimeConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            server: ServerConfig {
                bearer_token_env: "PAYMENT_API_TOKEN".to_owned(),
                max_request_body_bytes: 1024,
                tls_terminated_upstream: false,
            },
            database: PathBuf::from("payments.db"),
            indexer: IndexerConfig {
                endpoints: vec!["http://127.0.0.1:8081".to_owned()],
                bearer_token_env: Some("PAYMENT_INDEXER_TOKEN".to_owned()),
                timeout_seconds: 1,
                max_response_bytes: 1024,
            },
            wallets,
            deposits: None,
            reconcile_seconds: 1,
            reconcile_limit: 1,
        }
    }

    #[test]
    fn derives_unique_reconciliation_scopes() {
        let config = runtime(vec![bitcoin()]);
        config.validate().expect("runtime config must validate");
        assert_eq!(config.service().scopes.len(), 1);

        let duplicate = runtime(vec![bitcoin(), bitcoin()]);
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn requires_gateway_authentication_and_a_body_limit() {
        let mut config = runtime(vec![bitcoin()]);
        config.server.bearer_token_env.clear();
        assert!(config.validate().is_err());

        config.server.bearer_token_env = "PAYMENT_API_TOKEN".to_owned();
        config.server.max_request_body_bytes = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_bitcoin_rpc_endpoints() {
        let empty = match bitcoin() {
            WalletConfig::Bitcoin(mut config) => {
                config.rpc_urls.clear();
                WalletConfig::Bitcoin(config)
            }
            WalletConfig::Ethereum(_) => unreachable!("fixture is Bitcoin"),
        };
        assert!(runtime(vec![empty]).validate().is_err());

        let blank = match bitcoin() {
            WalletConfig::Bitcoin(mut config) => {
                config.rpc_urls = vec![" ".to_owned()];
                WalletConfig::Bitcoin(config)
            }
            WalletConfig::Ethereum(_) => unreachable!("fixture is Bitcoin"),
        };
        assert!(runtime(vec![blank]).validate().is_err());
    }

    #[test]
    fn deposit_keys_are_finite_and_unambiguous() {
        let mut config = runtime(vec![bitcoin()]);
        config.deposits = Some(DepositConfig {
            wallet: "treasury-btc".to_owned(),
            asset: "native".to_owned(),
            gas_wallet: None,
            policy_version: "v1".to_owned(),
            policy_digest: "09".repeat(32),
            minimum_collection: "1".to_owned(),
            minimum_confirmations: 1,
            coinbase_maturity: 100,
            max_participants: 100,
            max_inputs: 1_000,
            gas_amount: None,
            keys: vec![
                KeyConfig {
                    purpose: "merchant-a".to_owned(),
                    secret_env: "PAYMENT_DEPOSIT_A".to_owned(),
                },
                KeyConfig {
                    purpose: "merchant-b".to_owned(),
                    secret_env: "PAYMENT_DEPOSIT_B".to_owned(),
                },
            ],
        });
        config.validate().expect("finite deposit map is valid");

        let keys = &mut config.deposits.as_mut().expect("deposit config").keys;
        keys[1].purpose = keys[0].purpose.clone();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_partial_deposit_configuration() {
        let mut config = runtime(vec![bitcoin()]);
        config.deposits = Some(DepositConfig {
            wallet: "missing".to_owned(),
            asset: "native".to_owned(),
            gas_wallet: None,
            policy_version: "v1".to_owned(),
            policy_digest: "09".repeat(32),
            minimum_collection: "1".to_owned(),
            minimum_confirmations: 1,
            coinbase_maturity: 100,
            max_participants: 100,
            max_inputs: 1_000,
            gas_amount: None,
            keys: Vec::new(),
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_account_and_token_deposit_wallets() {
        let mut account = runtime(vec![ethereum("eth", EthereumAsset::Native)]);
        account.deposits = Some(DepositConfig {
            wallet: "eth".to_owned(),
            asset: "native".to_owned(),
            gas_wallet: None,
            policy_version: "v1".to_owned(),
            policy_digest: "09".repeat(32),
            minimum_collection: "1".to_owned(),
            minimum_confirmations: 1,
            coinbase_maturity: 0,
            max_participants: 1,
            max_inputs: 1,
            gas_amount: None,
            keys: vec![KeyConfig {
                purpose: "account-1".to_owned(),
                secret_env: "ACCOUNT_DEPOSIT_KEY".to_owned(),
            }],
        });
        account.validate().expect("account deposit config");

        let contract = "0x1111111111111111111111111111111111111111";
        let mut token = runtime(vec![
            ethereum(
                "usdc",
                EthereumAsset::Erc20 {
                    contract: contract.to_owned(),
                    decimals: 6,
                },
            ),
            ethereum("gas", EthereumAsset::Native),
        ]);
        token.deposits = Some(DepositConfig {
            wallet: "usdc".to_owned(),
            asset: contract.to_owned(),
            gas_wallet: Some("gas".to_owned()),
            policy_version: "v1".to_owned(),
            policy_digest: "09".repeat(32),
            minimum_collection: "1".to_owned(),
            minimum_confirmations: 1,
            coinbase_maturity: 0,
            max_participants: 1,
            max_inputs: 1,
            gas_amount: Some("1".to_owned()),
            keys: vec![KeyConfig {
                purpose: "token-1".to_owned(),
                secret_env: "TOKEN_DEPOSIT_KEY".to_owned(),
            }],
        });
        token.validate().expect("token deposit config");
    }

    #[test]
    fn preserves_bitcoin_rpc_preference_order() {
        let preferred = vec![
            "http://preferred.invalid".to_owned(),
            "http://fallback.invalid".to_owned(),
        ];
        let wallet = match bitcoin() {
            WalletConfig::Bitcoin(mut config) => {
                config.rpc_urls = preferred.clone();
                WalletConfig::Bitcoin(config)
            }
            WalletConfig::Ethereum(_) => unreachable!("fixture is Bitcoin"),
        };
        let encoded = serde_json::to_vec(&wallet).expect("wallet config must serialize");
        let decoded: WalletConfig =
            serde_json::from_slice(&encoded).expect("wallet config must deserialize");
        let WalletConfig::Bitcoin(decoded) = decoded else {
            unreachable!("decoded fixture is Bitcoin")
        };
        assert_eq!(decoded.rpc_urls, preferred);
    }

    #[test]
    fn serialized_config_contains_only_secret_references() {
        let config = runtime(vec![bitcoin()]);
        let encoded = serde_json::to_string(&config).expect("runtime config must serialize");
        assert!(encoded.contains("PAYMENT_BTC_PRIVATE_KEY"));
        assert!(!encoded.contains("private_key\":"));
        let diagnostics = format!("{config:?}");
        assert!(!diagnostics.contains("127.0.0.1:18443"));
        assert!(!diagnostics.contains("PAYMENT_BTC_PRIVATE_KEY"));
        assert!(!diagnostics.contains("PAYMENT_API_TOKEN"));
    }
}
