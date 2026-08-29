use std::{
    collections::BTreeSet,
    env,
    error::Error,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use chain_bitcoin::Network;
use indexing::BlockHash;
use payment_api::WalletAsset;
use serde::{Deserialize, de};

pub(crate) type AnyError = Box<dyn Error + Send + Sync>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) bind: SocketAddr,
    bearer_token_env: String,
    #[serde(default)]
    tls_terminated_upstream: bool,
    pub(crate) indexes: IndexConfig,
    #[serde(default)]
    pub(crate) wallets: Vec<ConfiguredWallet>,
}

impl Config {
    pub(crate) async fn read(path: impl AsRef<Path>) -> Result<Self, AnyError> {
        let config: Self = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), AnyError> {
        if self.bearer_token_env.trim().is_empty() {
            return Err("bearer-token environment name must not be empty".into());
        }
        if self.indexes.bitcoin.is_none() && self.indexes.ethereum.is_none() {
            return Err("at least one chain must be configured".into());
        }
        if let Some(config) = &self.indexes.bitcoin {
            config.validate()?;
        }
        if let Some(config) = &self.indexes.ethereum {
            config.validate()?;
        }
        let mut ids = BTreeSet::new();
        for wallet in &self.wallets {
            if wallet.id.trim().is_empty() || wallet.secret_env.trim().is_empty() {
                return Err("wallet ID and secret environment name must not be empty".into());
            }
            if !ids.insert(&wallet.id) {
                return Err("configured wallet IDs must be unique".into());
            }
            let configured = match wallet.asset {
                WalletAsset::Btc => self.indexes.bitcoin.is_some(),
                WalletAsset::Eth => self.indexes.ethereum.is_some(),
                WalletAsset::Usdc => self
                    .indexes
                    .ethereum
                    .as_ref()
                    .is_some_and(|ethereum| ethereum.usdc.is_some()),
            };
            if !configured {
                return Err("configured wallet references a disabled asset".into());
            }
        }
        Ok(())
    }

    pub(crate) fn server(&self) -> Result<http_support::server::Config, AnyError> {
        let token = http_support::server::BearerToken::new(env::var(&self.bearer_token_env)?)?;
        let security = if self.tls_terminated_upstream {
            http_support::server::TransportSecurity::TlsTerminatedUpstream
        } else {
            http_support::server::TransportSecurity::PlaintextLoopback
        };
        let config = http_support::server::Config::new(
            self.bind,
            security,
            Some(token),
            http_support::server::RequestLimits::default(),
        );
        config.validate()?;
        Ok(config)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfiguredWallet {
    pub(crate) id: String,
    pub(crate) asset: WalletAsset,
    pub(crate) secret_env: String,
    pub(crate) start_position: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IndexConfig {
    #[serde(default)]
    pub(crate) bitcoin: Option<BitcoinConfig>,
    #[serde(default)]
    pub(crate) ethereum: Option<EthereumConfig>,
}

impl IndexConfig {
    pub(crate) fn interval(&self) -> Duration {
        let millis = self
            .bitcoin
            .iter()
            .map(|config| config.sync.poll_millis)
            .chain(self.ethereum.iter().map(|config| config.sync.poll_millis))
            .min()
            .unwrap_or(1_000);
        Duration::from_millis(millis)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BitcoinConfig {
    pub(crate) database: PathBuf,
    pub(crate) network: Network,
    genesis_hash: String,
    rpc: RpcConfig,
    #[serde(flatten)]
    sync: SyncConfig,
}

impl BitcoinConfig {
    pub(crate) fn settings(&self) -> Result<chain_bitcoin::IndexerSettings, AnyError> {
        let endpoint = self
            .rpc
            .endpoints
            .first()
            .ok_or("RPC endpoints must not be empty")?;
        let mut settings = chain_bitcoin::IndexerSettings::new(
            endpoint.clone(),
            self.network,
            self.genesis_hash.as_str(),
        );
        self.rpc.apply_to(
            &mut settings.endpoints,
            &mut settings.headers,
            &mut settings.request_timeout,
            &mut settings.max_response_bytes,
        );
        settings.confirmations = self.sync.confirmation_depth;
        settings.reorg_retention = self.sync.reorg_retention;
        settings.batch_size = self.sync.batch_size;
        Ok(settings)
    }

    fn validate(&self) -> Result<(), AnyError> {
        self.rpc.validate(&self.database)?;
        self.sync.validate()?;
        let hash = chain_bitcoin::parse_bitcoin_block_hash(&self.genesis_hash)?;
        if chain_bitcoin::format_bitcoin_block_hash(&hash)? != self.genesis_hash {
            return Err("Bitcoin genesis hash must use canonical lowercase encoding".into());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EthereumConfig {
    pub(crate) database: PathBuf,
    network: String,
    pub(crate) chain_id: u64,
    genesis_hash: String,
    rpc: RpcConfig,
    #[serde(default)]
    limits: EthereumLimits,
    #[serde(default)]
    pub(crate) usdc: Option<UsdcConfig>,
    #[serde(flatten)]
    sync: SyncConfig,
}

impl EthereumConfig {
    pub(crate) fn settings(&self) -> Result<chain_ethereum::IndexerSettings, AnyError> {
        let endpoint = self
            .rpc
            .endpoints
            .first()
            .ok_or("RPC endpoints must not be empty")?;
        let mut settings = chain_ethereum::IndexerSettings::new(
            endpoint.clone(),
            chain_ethereum::Network::new(self.chain_id, self.network.as_str()),
            self.genesis_hash.as_str(),
        );
        self.rpc.apply_to(
            &mut settings.endpoints,
            &mut settings.headers,
            &mut settings.request_timeout,
            &mut settings.max_response_bytes,
        );
        settings.confirmations = self.sync.confirmation_depth;
        settings.reorg_retention = self.sync.reorg_retention;
        settings.batch_size = self.sync.batch_size;
        Ok(settings)
    }

    fn validate(&self) -> Result<(), AnyError> {
        self.rpc.validate(&self.database)?;
        self.sync.validate()?;
        if self.network.trim().is_empty() || self.chain_id == 0 {
            return Err("Ethereum network and chain ID must be configured".into());
        }
        if self.usdc.is_some() && self.rpc.endpoints.len() != 1 {
            return Err(
                "USDC configuration currently requires exactly one Ethereum RPC endpoint".into(),
            );
        }
        self.genesis()?;
        self.limits.build()?;
        Ok(())
    }

    fn genesis(&self) -> Result<BlockHash, AnyError> {
        let value = self
            .genesis_hash
            .strip_prefix("0x")
            .ok_or("Ethereum genesis hash must start with 0x")?;
        let bytes = hex::decode(value)?;
        if bytes.len() != 32 {
            return Err("Ethereum genesis hash must contain exactly 32 bytes".into());
        }
        Ok(BlockHash(bytes))
    }

    pub(crate) fn limits(&self) -> Result<chain_ethereum::Limits, AnyError> {
        self.limits.build()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsdcConfig {
    #[serde(deserialize_with = "deserialize_contract")]
    contract: chain_ethereum::Address,
}

impl UsdcConfig {
    pub(crate) fn contract(&self) -> chain_ethereum::Address {
        self.contract.clone()
    }
}

fn deserialize_contract<'de, D>(deserializer: D) -> Result<chain_ethereum::Address, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let encoded = String::deserialize(deserializer)?;
    let contract = encoded
        .parse::<chain_ethereum::Address>()
        .map_err(de::Error::custom)?;
    if contract.to_string() != encoded {
        return Err(de::Error::custom(
            "USDC contract must use canonical lowercase encoding",
        ));
    }
    if contract.is_zero() {
        return Err(de::Error::custom(
            "USDC contract must not be the zero address",
        ));
    }
    Ok(contract)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncConfig {
    confirmation_depth: u64,
    reorg_retention: u64,
    #[serde(default = "SyncConfig::default_poll_millis")]
    poll_millis: u64,
    #[serde(default = "SyncConfig::default_batch_size")]
    batch_size: usize,
}

impl SyncConfig {
    fn validate(&self) -> Result<(), AnyError> {
        if self.confirmation_depth == 0
            || self.reorg_retention == 0
            || self.poll_millis == 0
            || self.batch_size == 0
        {
            return Err(
                "index confirmation, retention, polling, and batch values must be positive".into(),
            );
        }
        Ok(())
    }

    const fn default_poll_millis() -> u64 {
        1_000
    }

    const fn default_batch_size() -> usize {
        100
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcConfig {
    endpoints: Vec<String>,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default = "RpcConfig::default_timeout_seconds")]
    timeout_seconds: u64,
    #[serde(default = "RpcConfig::default_response_bytes")]
    max_response_bytes: usize,
}

impl RpcConfig {
    fn validate(&self, database: &Path) -> Result<(), AnyError> {
        if database.as_os_str().is_empty()
            || self.endpoints.is_empty()
            || self.endpoints.iter().any(|value| value.trim().is_empty())
            || self.timeout_seconds == 0
            || self.max_response_bytes == 0
            || self.headers.iter().any(|(name, _)| name.trim().is_empty())
        {
            return Err("invalid database or RPC configuration".into());
        }
        Ok(())
    }

    fn apply_to(
        &self,
        endpoints: &mut Vec<String>,
        headers: &mut Vec<(String, String)>,
        request_timeout: &mut Duration,
        max_response_bytes: &mut usize,
    ) {
        endpoints.clone_from(&self.endpoints);
        headers.clone_from(&self.headers);
        *request_timeout = Duration::from_secs(self.timeout_seconds);
        *max_response_bytes = self.max_response_bytes;
    }

    const fn default_timeout_seconds() -> u64 {
        15
    }

    const fn default_response_bytes() -> usize {
        64 * 1024 * 1024
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EthereumLimits {
    max_input_bytes: usize,
    gas_margin_basis_points: u32,
    max_gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    max_total_fee: u128,
}

impl EthereumLimits {
    fn build(&self) -> Result<chain_ethereum::Limits, AnyError> {
        Ok(chain_ethereum::Limits::new(
            self.max_input_bytes,
            self.gas_margin_basis_points,
            self.max_gas_limit,
            chain_ethereum::Wei::from_u128(self.max_fee_per_gas),
            chain_ethereum::Wei::from_u128(self.max_priority_fee_per_gas),
            chain_ethereum::Wei::from_u128(self.max_total_fee),
        )?)
    }
}

impl Default for EthereumLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 128 * 1024,
            gas_margin_basis_points: 2_000,
            max_gas_limit: 30_000_000,
            max_fee_per_gas: 1_000_000_000_000,
            max_priority_fee_per_gas: 100_000_000_000,
            max_total_fee: 10_000_000_000_000_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const CONTRACT: &str = "0x1111111111111111111111111111111111111111";

    #[test]
    fn accepts_canonical_nonzero_usdc_contract() {
        let config = parse(ethereum(Some(CONTRACT)), json!([])).expect("configuration JSON");
        config.validate().expect("valid USDC configuration");
        assert_eq!(
            config
                .indexes
                .ethereum
                .expect("Ethereum configuration")
                .usdc
                .expect("USDC configuration")
                .contract()
                .to_string(),
            CONTRACT
        );
    }

    #[test]
    fn rejects_malformed_noncanonical_and_zero_usdc_contracts() {
        for contract in [
            "0x1234",
            "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "0x0000000000000000000000000000000000000000",
        ] {
            assert!(
                parse(ethereum(Some(contract)), json!([])).is_err(),
                "contract {contract} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_imported_usdc_wallet_without_usdc_configuration() {
        let wallets = json!([{
            "id": "treasury-usdc",
            "asset": "usdc",
            "secret_env": "USDC_SECRET",
            "start_position": 1
        }]);
        let error = parse(ethereum(None), wallets)
            .expect("configuration JSON")
            .validate()
            .expect_err("USDC wallet needs USDC configuration");
        assert_eq!(
            error.to_string(),
            "configured wallet references a disabled asset"
        );
    }

    #[test]
    fn rejects_height_only_wallet_birthday_configuration() {
        let wallets = json!([{
            "id": "treasury",
            "asset": "eth",
            "secret_env": "ETH_SECRET",
            "start_height": 1
        }]);

        assert!(
            parse(ethereum(None), wallets).is_err(),
            "the pre-release height-only birthday spelling must be rejected"
        );
    }

    #[test]
    fn requires_one_endpoint_for_endpoint_affine_usdc_validation() {
        let mut indexes = ethereum(Some(CONTRACT));
        indexes["ethereum"]["rpc"]["endpoints"] =
            json!(["http://127.0.0.1:8545", "http://127.0.0.1:9545"]);
        let error = parse(indexes, json!([]))
            .expect("configuration JSON")
            .validate()
            .expect_err("USDC validation must not span failover endpoints");
        assert_eq!(
            error.to_string(),
            "USDC configuration currently requires exactly one Ethereum RPC endpoint"
        );

        let mut native_only = ethereum(None);
        native_only["ethereum"]["rpc"]["endpoints"] =
            json!(["http://127.0.0.1:8545", "http://127.0.0.1:9545"]);
        parse(native_only, json!([]))
            .expect("configuration JSON")
            .validate()
            .expect("native Ethereum keeps ordered RPC failover");
    }

    fn parse(indexes: Value, wallets: Value) -> Result<Config, serde_json::Error> {
        serde_json::from_value(json!({
            "bind": "127.0.0.1:3000",
            "bearer_token_env": "API_TOKEN",
            "indexes": indexes,
            "wallets": wallets
        }))
    }

    fn ethereum(usdc: Option<&str>) -> Value {
        let mut config = json!({
            "database": "/tmp/ethereum.redb",
            "network": "mainnet",
            "chain_id": 1,
            "genesis_hash": format!("0x{}", "11".repeat(32)),
            "rpc": {
                "endpoints": ["http://127.0.0.1:8545"]
            },
            "confirmation_depth": 1,
            "reorg_retention": 10
        });
        if let Some(contract) = usdc {
            config["usdc"] = json!({"contract": contract});
        }
        json!({"ethereum": config})
    }
}
