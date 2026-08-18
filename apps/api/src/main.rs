use std::future::IntoFuture;
use std::{env, error::Error, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use chain_bitcoin::{AddressType, FeeRate, IndexUtxos, Network};
use chain_ethereum::AssetKind;
use indexing::{
    BlockHash, BlockHeight, ChainId, ConfirmationPolicy, IndexScope, OutputQuery, StatusStore,
    SyncPhase,
};
use json_rpc::{Config as RpcTransportConfig, Http as RpcClient, Retry};
use payment_api::{Chain, Gateway, WalletFamily};
use tokio::{sync::watch, task::JoinSet};

type AnyError = Box<dyn Error + Send + Sync>;

mod sync_task;
mod validation;

#[derive(Clone, serde::Deserialize)]
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

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredWallet {
    id: String,
    chain: Chain,
    secret_env: String,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexConfig {
    #[serde(default)]
    bitcoin: Option<BitcoinConfig>,
    #[serde(default)]
    ethereum: Option<EthereumConfig>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BitcoinConfig {
    database: PathBuf,
    network: Network,
    genesis_hash: String,
    rpc: RpcConfig,
    #[serde(flatten)]
    sync: SyncConfig,
}

#[derive(Clone, serde::Deserialize)]
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

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EthereumLimits {
    max_input_bytes: usize,
    gas_margin_basis_points: u32,
    max_gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    max_total_fee: u128,
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

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncConfig {
    #[serde(default)]
    bootstrap_height: u64,
    confirmation_depth: u64,
    reorg_retention: u64,
    #[serde(default = "default_poll_millis")]
    poll_millis: u64,
    #[serde(default = "default_batch_size")]
    batch_size: usize,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcConfig {
    endpoints: Vec<String>,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: u64,
    #[serde(default = "default_response_bytes")]
    max_response_bytes: usize,
}

struct IndexProcess {
    bitcoin: Option<BitcoinIndex>,
    ethereum: Option<EthereumIndex>,
    shutdown: watch::Sender<bool>,
    tasks: JoinSet<Result<(), AnyError>>,
}

struct BitcoinIndex {
    scope: IndexScope,
    index: Arc<indexing::Index<indexing_rocksdb::Repository>>,
    outputs: Arc<dyn OutputQuery>,
    fees: Arc<dyn chain_bitcoin::Fees>,
    transactions: Arc<dyn chain_bitcoin::Transactions>,
    network: Network,
}

struct EthereumIndex {
    scope: IndexScope,
    index: Arc<indexing::Index<indexing_rocksdb::Repository>>,
    accounts: Arc<dyn chain_ethereum::Accounts>,
    transactions: Arc<dyn chain_ethereum::Transactions>,
    network: String,
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let path = env::args_os()
        .nth(1)
        .ok_or("usage: payment-api <config.json>")?;
    let config: Config = serde_json::from_slice(&tokio::fs::read(path).await?)?;
    validation::validate(&config)?;

    let bearer_token = env::var(&config.bearer_token_env)?;
    let token = http_support::server::BearerToken::new(bearer_token)?;
    let transport = if config.tls_terminated_upstream {
        http_support::server::TransportSecurity::TlsTerminatedUpstream
    } else {
        http_support::server::TransportSecurity::PlaintextLoopback
    };
    let server = http_support::server::Config::new(
        config.bind,
        transport,
        Some(token),
        http_support::server::RequestLimits::default(),
    );
    server.validate()?;

    let mut indexes = start_indexes(config.indexes.clone()).await?;
    wait_ready(&mut indexes).await?;

    let mut providers = wallets::Providers::new();
    let mut families = Vec::new();
    if let Some(index) = &indexes.bitcoin {
        let provider = chain_bitcoin::WalletProvider::new(
            chain_bitcoin::WalletConfig {
                scope: index.scope.clone(),
                network: index.network,
                address_type: AddressType::SegwitV0,
                fee_target_blocks: 6,
                max_fee_rate: FeeRate::new(10_000_000),
            },
            Arc::new(IndexUtxos::new(
                index.scope.clone(),
                index.network,
                Arc::clone(&index.outputs),
            )?),
            Arc::clone(&index.fees),
            Arc::clone(&index.transactions),
            index.index.clone(),
        );
        let transactions = provider.transactions();
        providers.register(Chain::Bitcoin, provider)?;
        families.push(WalletFamily {
            chain: Chain::Bitcoin,
            network: index.scope.network.clone(),
            scope: index.scope.clone(),
            watcher: index.index.clone(),
            checkpoint: index.index.clone(),
            transactions,
        });
    }
    if let Some(index) = &indexes.ethereum {
        let provider = chain_ethereum::WalletProvider::new(
            chain_ethereum::WalletConfig {
                scope: index.scope.clone(),
                asset: AssetKind::Native,
                decimals: chain_ethereum::ETH.decimals,
            },
            Arc::clone(&index.accounts),
            Arc::clone(&index.transactions),
            index.index.clone(),
        );
        let transactions = provider.transactions();
        providers.register(Chain::Ethereum, provider)?;
        families.push(WalletFamily {
            chain: Chain::Ethereum,
            network: index.network.clone(),
            scope: index.scope.clone(),
            watcher: index.index.clone(),
            checkpoint: index.index.clone(),
            transactions,
        });
    }
    let mut gateway = Gateway::new(providers);
    for family in families {
        gateway.register(family)?;
    }
    for wallet in config.wallets {
        let encoded = env::var(wallet.secret_env)?;
        let secret = hex::decode(encoded).map_err(|_| "wallet secret must be hexadecimal")?;
        if secret.len() != 32 {
            return Err("wallet secret must contain exactly 32 bytes".into());
        }
        gateway
            .initialize(wallet.id, wallet.chain, wallets::SecretBytes::new(secret))
            .await?;
    }

    let router = gateway.router(&server)?;
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let serve = axum::serve(listener, router).into_future();
    tokio::pin!(serve);
    let result: Result<(), AnyError> = tokio::select! {
        result = &mut serve => result.map_err(Into::into),
        result = indexes.tasks.join_next() => sync_task::stopped(result),
        result = tokio::signal::ctrl_c() => result.map_err(Into::into),
    };
    let _ = indexes.shutdown.send(true);
    while let Some(synchronizer) = indexes.tasks.join_next().await {
        synchronizer??;
    }
    result
}

async fn start_indexes(config: IndexConfig) -> Result<IndexProcess, AnyError> {
    let (shutdown, _) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let bitcoin = match config.bitcoin {
        Some(config) => Some(start_bitcoin(config, shutdown.subscribe(), &mut tasks).await?),
        None => None,
    };
    let ethereum = match config.ethereum {
        Some(config) => Some(start_ethereum(config, shutdown.subscribe(), &mut tasks).await?),
        None => None,
    };
    Ok(IndexProcess {
        bitcoin,
        ethereum,
        shutdown,
        tasks,
    })
}

async fn start_bitcoin(
    config: BitcoinConfig,
    shutdown: watch::Receiver<bool>,
    tasks: &mut JoinSet<Result<(), AnyError>>,
) -> Result<BitcoinIndex, AnyError> {
    let scope = IndexScope {
        chain: ChainId(chain_bitcoin::CHAIN.to_owned()),
        network: config.network.canonical_name().to_owned(),
    };
    let genesis = chain_bitcoin::parse_bitcoin_block_hash(&config.genesis_hash)?;
    let rpc = chain_bitcoin::RpcClient::connect(
        rpc_client(&config.rpc)?,
        chain_bitcoin::CoreConfig {
            expected_network: config.network,
            expected_genesis_hash: genesis.clone(),
        },
    )
    .await?;
    let source = chain_bitcoin::Source::from_client(
        rpc.clone(),
        chain_bitcoin::SourceConfig {
            scope: scope.clone(),
            network: config.network,
            expected_genesis_hash: genesis,
        },
    )?;
    let interpreter = chain_bitcoin::BlockInterpreter::new(scope.clone(), config.network)?;
    let repository = indexing_rocksdb::Repository::new(
        storage_rocksdb::RocksDb::open(&config.database)?,
        scope.clone(),
    )?;
    let synchronizer = indexing::Synchronizer::new(
        source,
        interpreter,
        repository.clone(),
        sync_config(&config.sync, scope.clone())?,
    );
    let index = Arc::new(indexing::Index::new(repository.clone()));
    let outputs = Arc::new(indexing_rocksdb::OutputReader::new(repository));
    tasks.spawn(sync_task::run(
        synchronizer,
        Duration::from_millis(config.sync.poll_millis),
        config.sync.batch_size,
        shutdown,
    ));
    Ok(BitcoinIndex {
        scope,
        index,
        outputs,
        fees: Arc::new(rpc.fees()),
        transactions: Arc::new(rpc.transactions()),
        network: config.network,
    })
}

async fn start_ethereum(
    config: EthereumConfig,
    shutdown: watch::Receiver<bool>,
    tasks: &mut JoinSet<Result<(), AnyError>>,
) -> Result<EthereumIndex, AnyError> {
    let scope = IndexScope {
        chain: ChainId(chain_ethereum::CHAIN.to_owned()),
        network: config.network.clone(),
    };
    let rpc = chain_ethereum::RpcClient::new(rpc_client(&config.rpc)?);
    let source = chain_ethereum::Source::from_rpc(
        rpc.clone(),
        chain_ethereum::SourceConfig {
            scope: scope.clone(),
            expected_chain_id: config.chain_id,
            expected_genesis_hash: ethereum_hash(&config.genesis_hash)?,
        },
    )
    .await?;
    let accounts = chain_ethereum::AccountClient::new(rpc.clone(), config.chain_id)?;
    let transactions = chain_ethereum::TransactionClient::new(
        rpc,
        config.chain_id,
        ethereum_limits(&config.limits)?,
    )?;
    let interpreter = chain_ethereum::BlockInterpreter::new(scope.clone())?;
    let repository = indexing_rocksdb::Repository::new(
        storage_rocksdb::RocksDb::open(&config.database)?,
        scope.clone(),
    )?;
    let synchronizer = indexing::Synchronizer::new(
        source,
        interpreter,
        repository.clone(),
        sync_config(&config.sync, scope.clone())?,
    );
    let index = Arc::new(indexing::Index::new(repository));
    tasks.spawn(sync_task::run(
        synchronizer,
        Duration::from_millis(config.sync.poll_millis),
        config.sync.batch_size,
        shutdown,
    ));
    Ok(EthereumIndex {
        scope,
        index,
        accounts: Arc::new(accounts),
        transactions: Arc::new(transactions),
        network: config.network,
    })
}

async fn wait_ready(indexes: &mut IndexProcess) -> Result<(), AnyError> {
    loop {
        let bitcoin = match &indexes.bitcoin {
            Some(index) => ready(&index.index, &index.scope).await?,
            None => true,
        };
        let ethereum = match &indexes.ethereum {
            Some(index) => ready(&index.index, &index.scope).await?,
            None => true,
        };
        if bitcoin && ethereum {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(25)) => {}
            result = indexes.tasks.join_next() => return sync_task::stopped(result),
        }
    }
}

async fn ready(
    index: &indexing::Index<indexing_rocksdb::Repository>,
    scope: &IndexScope,
) -> Result<bool, AnyError> {
    let Some(status) = StatusStore::status(index.repository(), scope).await? else {
        return Ok(false);
    };
    Ok(status.phase == SyncPhase::Ready
        && indexing::Checkpoint::checkpoint(index, scope)
            .await?
            .is_some())
}

fn rpc_client(config: &RpcConfig) -> Result<RpcClient, AnyError> {
    let attempts = std::num::NonZeroU32::new(3).ok_or("invalid retry count")?;
    let mut transport = RpcTransportConfig::new(
        &config.endpoints[0],
        Duration::from_secs(config.timeout_seconds),
    );
    transport.endpoints = config.endpoints.clone();
    transport.max_response_bytes = config.max_response_bytes;
    transport.retry = Retry::new(attempts, Duration::from_millis(250), Duration::from_secs(2))?;
    transport.headers = config.headers.clone();
    Ok(RpcClient::new(transport)?)
}

fn sync_config(config: &SyncConfig, scope: IndexScope) -> Result<indexing::SyncConfig, AnyError> {
    Ok(indexing::SyncConfig::new(
        scope,
        BlockHeight(config.bootstrap_height),
        ConfirmationPolicy {
            minimum_confirmations: config.confirmation_depth,
            require_chain_finality: false,
        },
        config.reorg_retention,
    )?)
}

fn ethereum_hash(value: &str) -> Result<BlockHash, AnyError> {
    let hex = value
        .strip_prefix("0x")
        .ok_or("Ethereum genesis hash must start with 0x")?;
    if hex.len() != 64 {
        return Err("Ethereum genesis hash must contain exactly 32 bytes".into());
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BlockHash(bytes))
}

fn ethereum_limits(value: &EthereumLimits) -> Result<chain_ethereum::Limits, AnyError> {
    Ok(chain_ethereum::Limits::new(
        value.max_input_bytes,
        value.gas_margin_basis_points,
        value.max_gas_limit,
        chain_ethereum::Wei::from_u128(value.max_fee_per_gas),
        chain_ethereum::Wei::from_u128(value.max_priority_fee_per_gas),
        chain_ethereum::Wei::from_u128(value.max_total_fee),
    )?)
}

const fn default_poll_millis() -> u64 {
    1_000
}
const fn default_batch_size() -> usize {
    100
}
const fn default_timeout_seconds() -> u64 {
    15
}
const fn default_response_bytes() -> usize {
    64 * 1024 * 1024
}
