mod readiness;

use std::{
    collections::BTreeSet,
    env,
    error::Error,
    future::IntoFuture,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chain_bitcoin::{AddressType, FeeRate, IndexUtxos, Network};
use chain_ethereum::AssetKind;
use indexing::{BlockHash, BlockHeight};
use payment_api::{Chain, State};
use serde::Deserialize;
use tokio::sync::watch;

type AnyError = Box<dyn Error + Send + Sync>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bind: SocketAddr,
    bearer_token_env: String,
    #[serde(default)]
    tls_terminated_upstream: bool,
    indexes: IndexConfig,
    #[serde(default)]
    wallets: Vec<ConfiguredWallet>,
}
impl Config {
    async fn read(path: impl AsRef<Path>) -> Result<Self, AnyError> {
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
            let configured = match wallet.chain {
                Chain::Bitcoin => self.indexes.bitcoin.is_some(),
                Chain::Ethereum => self.indexes.ethereum.is_some(),
            };
            if !configured {
                return Err("configured wallet references a disabled chain".into());
            }
        }
        Ok(())
    }

    fn server(&self) -> Result<http_support::server::Config, AnyError> {
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
struct ConfiguredWallet {
    id: String,
    chain: Chain,
    secret_env: String,
    start_height: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexConfig {
    #[serde(default)]
    bitcoin: Option<BitcoinConfig>,
    #[serde(default)]
    ethereum: Option<EthereumConfig>,
}
impl IndexConfig {
    fn interval(&self) -> Duration {
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
struct BitcoinConfig {
    database: PathBuf,
    network: Network,
    genesis_hash: String,
    rpc: RpcConfig,
    #[serde(flatten)]
    sync: SyncConfig,
}
impl BitcoinConfig {
    fn settings(&self) -> Result<chain_bitcoin::IndexerSettings, AnyError> {
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
struct EthereumConfig {
    database: PathBuf,
    network: String,
    chain_id: u64,
    genesis_hash: String,
    rpc: RpcConfig,
    #[serde(default)]
    limits: EthereumLimits,
    #[serde(flatten)]
    sync: SyncConfig,
}
impl EthereumConfig {
    fn settings(&self) -> Result<chain_ethereum::IndexerSettings, AnyError> {
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

    /// Copies the configured transport tuning onto a chain's indexer settings.
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
#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let path = env::args_os()
        .nth(1)
        .ok_or("usage: payment-api <config.json>")?;
    let config = Config::read(path).await?;
    let server = config.server()?;
    let bind = config.bind;
    let interval = config.indexes.interval();
    let mut indexers = Vec::<Arc<dyn indexing::Indexer>>::new();

    let bitcoin = if let Some(config) = config.indexes.bitcoin {
        let settings = config.settings()?;
        let scope = settings.scope();
        // One long-lived client per chain: the indexer gets a clone, the wallet
        // provider below keeps the original.
        let client = settings.client().await?;
        let repository = Arc::new(indexing_redb::Repository::new(
            storage_redb::Redb::open(&config.database)?,
            scope.clone(),
        )?);
        indexers.push(Arc::new(settings.build(
            client.clone(),
            repository.as_ref().clone(),
            None,
        )?));
        Some((scope, config.network, repository, client))
    } else {
        None
    };
    let ethereum = if let Some(config) = config.indexes.ethereum {
        let settings = config.settings()?;
        let scope = settings.scope();
        let client = settings.client()?;
        let repository = indexing_redb::Repository::new(
            storage_redb::Redb::open(&config.database)?,
            scope.clone(),
        )?;
        indexers.push(Arc::new(
            settings.build(client.clone(), repository, None).await?,
        ));
        let accounts: Arc<dyn chain_ethereum::Accounts> = Arc::new(
            chain_ethereum::AccountClient::new(client.clone(), config.chain_id)?,
        );
        let transactions: Arc<dyn chain_ethereum::Transactions> =
            Arc::new(chain_ethereum::TransactionClient::new(
                client,
                config.chain_id,
                config.limits.build()?,
            )?);
        Some((scope, config.chain_id, accounts, transactions))
    } else {
        None
    };
    let composer = Arc::new(indexing::Composer::new(indexers)?);
    let indexer: Arc<dyn indexing::Indexer> = composer.clone();
    let checkpoint: Arc<dyn indexing::Checkpoint> = composer.clone();
    let history: Arc<dyn indexing::History> = composer;
    let mut wallets = wallets::Wallets::new(checkpoint);

    if let Some((scope, network, repository, rpc)) = bitcoin {
        let outputs: Arc<dyn indexing::Outputs> = repository;
        let utxos = Arc::new(IndexUtxos::new(scope.clone(), network, outputs)?);
        let provider = chain_bitcoin::WalletProvider::new(
            chain_bitcoin::WalletConfig {
                scope: scope.clone(),
                network,
                address_type: AddressType::SegwitV0,
                fee_target_blocks: 6,
                max_fee_rate: FeeRate::new(10_000_000),
            },
            utxos,
            Arc::new(rpc.fees()),
            Arc::new(rpc.transactions()),
            history.clone(),
        );
        let sender = provider.transactions();
        wallets.register(Chain::Bitcoin, scope, provider, sender, None)?;
    }
    if let Some((scope, chain_id, accounts, transactions)) = ethereum {
        let provider = chain_ethereum::WalletProvider::new(
            chain_ethereum::WalletConfig {
                scope: scope.clone(),
                chain_id,
                asset: AssetKind::Native,
                decimals: chain_ethereum::ETH.decimals,
            },
            accounts,
            transactions,
            history,
        );
        let sender = provider.transactions();
        wallets.register(Chain::Ethereum, scope, provider, sender, None)?;
    }
    for configured in config.wallets {
        let encoded = env::var(configured.secret_env)?;
        let secret = hex::decode(encoded).map_err(|_| "wallet secret must be hexadecimal")?;
        if secret.len() != 32 {
            return Err("wallet secret must contain exactly 32 bytes".into());
        }
        wallets
            .import(
                configured.id,
                &configured.chain,
                wallets::SecretBytes::new(secret),
                BlockHeight(configured.start_height),
            )
            .await?;
    }
    let wallets = Arc::new(wallets);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (sync_state, sync_state_rx) = watch::channel(indexing_runtime::SyncState::CatchingUp);
    let (readiness, mut readiness_rx) = watch::channel(false);
    let filters = wallets.clone();
    let mut synchronization = tokio::spawn(indexing_runtime::run(
        indexer,
        move || filters.filters(),
        interval,
        shutdown_rx,
        sync_state,
    ));

    readiness::publish(sync_state_rx, readiness);

    while !*readiness_rx.borrow() {
        tokio::select! {
            changed = readiness_rx.changed() => {
                changed.map_err(|_| "index readiness channel closed")?;
            }
            result = &mut synchronization => {
                return match result {
                    Ok(Ok(())) => Err("index synchronization stopped during startup".into()),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.into()),
                };
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                let _ = shutdown.send(true);
                synchronization.await??;
                return Ok(());
            }
        }
    }
    let state = State::new(wallets, readiness_rx);
    let router = payment_api::router(state, &server)?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let serving = axum::serve(listener, router).into_future();
    tokio::pin!(serving);
    let result = tokio::select! {
        result = &mut serving => result.map_err(Into::into),
        result = &mut synchronization => {
            return match result {
                Ok(Ok(())) => Err("index synchronization stopped unexpectedly".into()),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.into()),
            };
        }
        signal = tokio::signal::ctrl_c() => signal.map_err(Into::into),
    };
    let _ = shutdown.send(true);
    synchronization.await??;
    result
}
