use std::{collections::BTreeMap, sync::Arc, time::Duration};

use chain_bitcoin::{
    AddressType as BitcoinAddressType, CoreConfig, FeeRate, IndexUtxos, Network,
    RpcClient as BitcoinClient, WalletConfig as BitcoinWallet, WalletProvider as BitcoinProvider,
    parse_bitcoin_block_hash,
};
use chain_ethereum::{
    AssetKind, HttpConfig as EthereumHttp, Limits, WalletConfig as EthereumWallet,
    WalletProvider as EthereumProvider, Wei,
};
use http_kit::client::{Config as HttpConfig, Reqwest, Retry};
use indexing::{History, IndexScope, Indexer, OutputQuery};
use indexing_http::{Config as IndexerHttp, Remote};
use json_rpc::{Failover, TransportClient};
use storage_rocksdb::RocksDb;
use wallets::{Provider, SecretBytes, Wallet, Wallets};

use crate::{
    BitcoinConfig, CollectionPolicy, CompositionError, DepositObserver, Deposits, EthereumAsset,
    EthereumConfig, Payments, Planner, RuntimeConfig, Secrets, Service, StorageRepository, Sweeps,
    SystemClock, WalletConfig,
    resolver::{DepositKey, DepositResolver, GasResolver},
};

/// Fully owned Payment Service runtime assembled by the application layer.
pub struct Runtime {
    service: Service,
}

/// Finite application-owned selection of configured wallet providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WalletKey {
    Bitcoin(usize),
    Ethereum(usize),
}

impl Runtime {
    /// Opens storage, constructs remote adapters, verifies Bitcoin node
    /// identity, and creates every configured wallet without exposing keys.
    pub async fn build(config: RuntimeConfig, secrets: Secrets) -> Result<Self, CompositionError> {
        config.validate().map_err(CompositionError::configuration)?;

        let mut indexer_config = IndexerHttp::with_endpoints(config.indexer.endpoints.clone());
        indexer_config.request_timeout = Duration::from_secs(config.indexer.timeout_seconds);
        indexer_config.max_response_bytes = config.indexer.max_response_bytes;
        indexer_config.bearer_token = config
            .indexer
            .bearer_token_env
            .as_deref()
            .map(|name| secrets.read(name))
            .transpose()?;
        let remote = Arc::new(
            Remote::connect(indexer_config)
                .map_err(|error| CompositionError::adapter("Indexer", error))?,
        );
        let indexer: Arc<dyn Indexer> = remote.clone();

        let database = Arc::new(
            RocksDb::open(&config.database)
                .map_err(|error| CompositionError::adapter("payment storage", error))?,
        );
        let store = Arc::new(StorageRepository::new(database.clone()));
        let mut payments = Payments::new(store, indexer);
        let mut deposit_keys = Vec::new();
        let mut wallet_instances = BTreeMap::new();

        for (index, wallet) in config.wallets.iter().enumerate() {
            let keys = config
                .deposits
                .as_ref()
                .filter(|deposits| deposits.wallet == wallet.identity().0)
                .map(|deposits| deposits.keys.as_slice())
                .unwrap_or_default();
            let (id, scope, instance, mut resolved) = match wallet {
                WalletConfig::Bitcoin(value) => {
                    let scope = bitcoin_scope(value);
                    let (wallet, resolved) = bitcoin_wallet(
                        WalletKey::Bitcoin(index),
                        value,
                        scope.clone(),
                        remote.clone(),
                        &secrets,
                        keys,
                    )
                    .await?;
                    (value.id.clone(), scope, wallet, resolved)
                }
                WalletConfig::Ethereum(value) => {
                    let scope = ethereum_scope(value);
                    let (wallet, resolved) = ethereum_wallet(
                        WalletKey::Ethereum(index),
                        value,
                        scope.clone(),
                        remote.clone(),
                        &secrets,
                        keys,
                    )
                    .await?;
                    (value.id.clone(), scope, wallet, resolved)
                }
            };
            deposit_keys.append(&mut resolved);
            wallet_instances.insert(id.clone(), instance.clone());
            payments = payments.with(id, scope, instance);
        }

        let token =
            http_kit::server::BearerToken::new(secrets.read(&config.server.bearer_token_env)?)
                .map_err(|error| CompositionError::adapter("HTTP authentication", error))?;
        let limits = http_kit::server::RequestLimits::new(config.server.max_request_body_bytes)
            .map_err(|error| CompositionError::adapter("HTTP request limits", error))?;
        let transport = if config.server.tls_terminated_upstream {
            http_kit::server::TransportSecurity::TlsTerminatedUpstream
        } else {
            http_kit::server::TransportSecurity::PlaintextLoopback
        };
        let server = http_kit::server::Config::new(config.bind, transport, Some(token), limits);
        let mut service = Service::new(config.service(), Arc::new(payments), server)
            .map_err(CompositionError::configuration)?;
        if let Some(deposit_config) = &config.deposits {
            let wallet = config
                .wallets
                .iter()
                .find(|wallet| wallet.identity().0 == deposit_config.wallet)
                .ok_or_else(|| CompositionError::invalid("deposit wallet is not configured"))?;
            let (_, scope, _) = wallet.identity();
            let asset = indexing::AssetId {
                chain: scope.chain.clone(),
                asset: wallet.asset(),
            };
            let mode = match wallet {
                WalletConfig::Bitcoin(_) => deposits::CollectionMode::UtxoBatch,
                WalletConfig::Ethereum(value) => match value.asset {
                    EthereumAsset::Native => deposits::CollectionMode::AccountTransfer,
                    EthereumAsset::Erc20 { .. } => deposits::CollectionMode::TokenWithGas,
                },
            };
            let resolver = Arc::new(
                DepositResolver::new(deposit_keys)
                    .map_err(|error| CompositionError::adapter("deposit keys", error))?,
            );
            let deposit_store = Arc::new(deposits::PaymentStore::new(database.as_ref().clone()));
            let observer = Arc::new(DepositObserver::new(
                scope.clone(),
                remote.clone(),
                deposit_store.clone(),
            ));
            let deposits = Arc::new(Deposits::new(
                deposit_store.clone(),
                remote.clone(),
                resolver.clone(),
                scope.clone(),
            ));
            let master = wallet_instances
                .get(&deposit_config.wallet)
                .ok_or_else(|| CompositionError::invalid("collection master wallet is missing"))?;
            let destination = master
                .address_text(&master.address())
                .map_err(|error| CompositionError::adapter("collection destination", error))?;
            let policy = CollectionPolicy::configured(deposit_config)?;
            let planner = Arc::new(Planner::new(
                deposit_store.clone(),
                remote.clone(),
                scope.clone(),
                asset.clone(),
                indexing::CanonicalAddress {
                    scope: scope.clone(),
                    value: destination.text,
                },
                mode,
                policy,
            ));
            let mut sweeps =
                Sweeps::new(deposit_store, resolver, remote, scope.clone()).for_asset(asset, mode);
            if let Some(gas_id) = &deposit_config.gas_wallet {
                let gas = wallet_instances.get(gas_id).cloned().ok_or_else(|| {
                    CompositionError::invalid("deposit gas wallet is not configured")
                })?;
                sweeps = sweeps.with_gas_wallet(Arc::new(GasResolver::new(gas, scope)));
            }
            service = service
                .with_observer(observer)
                .with_deposits(deposits)
                .with_planner(planner)
                .with_sweeps(Arc::new(sweeps), Arc::new(SystemClock::default()));
        }
        Ok(Self { service })
    }

    pub async fn run(self) -> Result<(), CompositionError> {
        self.service
            .run()
            .await
            .map_err(|error| CompositionError::adapter("Payment Service", error))
    }

    pub async fn run_until<F>(self, shutdown: F) -> Result<(), CompositionError>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.service
            .run_until(shutdown)
            .await
            .map_err(|error| CompositionError::adapter("Payment Service", error))
    }
}

async fn bitcoin_wallet(
    key: WalletKey,
    config: &BitcoinConfig,
    scope: IndexScope,
    indexer: Arc<Remote<Reqwest>>,
    secrets: &Secrets,
    deposit_keys: &[crate::KeyConfig],
) -> Result<(Arc<dyn Wallet>, Vec<DepositKey>), CompositionError> {
    let network = bitcoin_network(&config.network)?;
    let genesis = parse_bitcoin_block_hash(&config.genesis_hash)
        .map_err(|error| CompositionError::adapter("Bitcoin genesis hash", error))?;
    if config.timeout_seconds == 0 || config.max_response_bytes == 0 || config.max_fee_rate == 0 {
        return Err(CompositionError::invalid(
            "Bitcoin timeout, response limit, and maximum fee rate must be positive",
        ));
    }
    let rpc = BitcoinClient::connect(
        bitcoin_transport(config)?,
        CoreConfig {
            expected_network: network,
            expected_genesis_hash: genesis,
        },
    )
    .await
    .map_err(|error| CompositionError::adapter("Bitcoin RPC", error))?;
    let outputs: Arc<dyn OutputQuery> = indexer.clone();
    let utxos = Arc::new(
        IndexUtxos::new(scope.clone(), network, outputs)
            .map_err(|error| CompositionError::adapter("Bitcoin outputs", error))?,
    );
    let history: Arc<dyn History> = indexer;
    let provider: Arc<dyn Provider> = Arc::new(BitcoinProvider::new(
        BitcoinWallet {
            scope: scope.clone(),
            network,
            address_type: if config.taproot {
                BitcoinAddressType::Taproot
            } else {
                BitcoinAddressType::SegwitV0
            },
            fee_target_blocks: config.fee_target_blocks,
            max_fee_rate: FeeRate::new(config.max_fee_rate),
        },
        utxos,
        Arc::new(rpc.fees()),
        Arc::new(rpc.transactions()),
        history,
    ));
    let wallet = create_wallet(key, provider.clone(), secrets.read(&config.secret_env)?).await?;
    let deposits = create_deposit_keys(
        provider.as_ref(),
        config.id.as_str(),
        scope,
        "native",
        deposit_keys,
        secrets,
    )
    .await?;
    Ok((wallet, deposits))
}

fn bitcoin_transport(
    config: &BitcoinConfig,
) -> Result<Failover<TransportClient<Reqwest>>, CompositionError> {
    let first = config
        .rpc_urls
        .first()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .ok_or_else(|| CompositionError::invalid("Bitcoin RPC endpoints must not be empty"))?;
    if config
        .rpc_urls
        .iter()
        .any(|endpoint| endpoint.trim().is_empty())
    {
        return Err(CompositionError::invalid(
            "Bitcoin RPC endpoints must not be empty",
        ));
    }
    let mut transport_config =
        HttpConfig::new(first.clone(), Duration::from_secs(config.timeout_seconds));
    transport_config.max_response_bytes = config.max_response_bytes;
    transport_config.default_headers = config.rpc_headers.clone();
    let transport = Reqwest::new(transport_config)
        .map_err(|error| CompositionError::adapter("Bitcoin HTTP client", error))?;
    let endpoints = config
        .rpc_urls
        .iter()
        .cloned()
        .map(|endpoint| TransportClient::new(transport.clone(), endpoint))
        .collect();
    Failover::new(endpoints)
        .map_err(|error| CompositionError::adapter("Bitcoin RPC failover", error))
}

async fn ethereum_wallet(
    key: WalletKey,
    config: &EthereumConfig,
    scope: IndexScope,
    indexer: Arc<Remote<Reqwest>>,
    secrets: &Secrets,
    deposit_keys: &[crate::KeyConfig],
) -> Result<(Arc<dyn Wallet>, Vec<DepositKey>), CompositionError> {
    if config.rpc_urls.is_empty()
        || config
            .rpc_urls
            .iter()
            .any(|endpoint| endpoint.trim().is_empty())
        || config.timeout_seconds == 0
        || config.max_response_bytes == 0
    {
        return Err(CompositionError::invalid(
            "Ethereum RPC endpoints, timeout, and response limit must be configured",
        ));
    }
    let limits = Limits::new(
        config.max_input_bytes,
        config.gas_margin_basis_points,
        config.max_gas_limit,
        Wei::from_u128(config.max_fee_per_gas),
        Wei::from_u128(config.max_priority_fee_per_gas),
        Wei::from_u128(config.max_total_fee),
    )
    .map_err(|error| CompositionError::adapter("Ethereum limits", error))?;
    let mut rpc_config = EthereumHttp::new(
        config.rpc_urls[0].clone(),
        config.chain_id,
        Duration::from_secs(config.timeout_seconds),
        config.max_response_bytes,
        Retry::default(),
        limits,
    )
    .map_err(|error| CompositionError::adapter("Ethereum RPC", error))?;
    rpc_config = rpc_config
        .with_endpoints(config.rpc_urls.clone())
        .map_err(|error| CompositionError::adapter("Ethereum RPC", error))?;
    for (name, value) in &config.rpc_headers {
        rpc_config = rpc_config.with_header(name.clone(), value.clone());
    }
    let (accounts, transactions) = rpc_config
        .connect()
        .map_err(|error| CompositionError::adapter("Ethereum RPC", error))?;
    let history: Arc<dyn History> = indexer;
    let provider: Arc<dyn Provider> = Arc::new(EthereumProvider::new(
        EthereumWallet {
            scope: scope.clone(),
            asset: ethereum_asset(&config.asset)?,
            decimals: ethereum_decimals(&config.asset),
        },
        Arc::new(accounts),
        Arc::new(transactions),
        history,
    ));
    let wallet = create_wallet(key, provider.clone(), secrets.read(&config.secret_env)?).await?;
    let asset = match &config.asset {
        EthereumAsset::Native => "native".to_owned(),
        EthereumAsset::Erc20 { contract, .. } => contract.to_ascii_lowercase(),
    };
    let deposits = create_deposit_keys(
        provider.as_ref(),
        config.id.as_str(),
        scope,
        &asset,
        deposit_keys,
        secrets,
    )
    .await?;
    Ok((wallet, deposits))
}

async fn create_wallet(
    key: WalletKey,
    provider: Arc<dyn Provider>,
    secret: String,
) -> Result<Arc<dyn Wallet>, CompositionError> {
    let secret = read_key(&secret)?;
    let mut wallets = Wallets::new();
    wallets
        .register(key, provider)
        .map_err(|error| CompositionError::adapter("wallet registry", error))?;
    wallets
        .new_wallet(&key, secret)
        .await
        .map_err(|error| CompositionError::adapter("wallet", error))
}

async fn create_deposit_keys(
    provider: &dyn Provider,
    wallet_id: &str,
    scope: IndexScope,
    asset: &str,
    keys: &[crate::KeyConfig],
    secrets: &Secrets,
) -> Result<Vec<DepositKey>, CompositionError> {
    let mut resolved = Vec::with_capacity(keys.len());
    for key in keys {
        let wallet = provider
            .create(read_key(&secrets.read(&key.secret_env)?)?)
            .await
            .map_err(|error| CompositionError::adapter("deposit wallet", error))?;
        resolved.push(DepositKey {
            purpose: key.purpose.clone(),
            wallet_id: wallet_id.to_owned(),
            scope: scope.clone(),
            asset: indexing::AssetId {
                chain: scope.chain.clone(),
                asset: asset.to_owned(),
            },
            wallet,
        });
    }
    Ok(resolved)
}

fn ethereum_asset(asset: &EthereumAsset) -> Result<AssetKind, CompositionError> {
    match asset {
        EthereumAsset::Native => Ok(AssetKind::Native),
        EthereumAsset::Erc20 { contract, .. } => contract
            .parse()
            .map(AssetKind::Erc20)
            .map_err(|error| CompositionError::adapter("Ethereum token contract", error)),
    }
}

const fn ethereum_decimals(asset: &EthereumAsset) -> u32 {
    match asset {
        EthereumAsset::Native => chain_ethereum::ETH.decimals,
        EthereumAsset::Erc20 { decimals, .. } => *decimals,
    }
}

fn read_key(encoded: &str) -> Result<SecretBytes, CompositionError> {
    let bytes = hex::decode(encoded.trim())
        .map_err(|_| CompositionError::invalid("private key must be hexadecimal"))?;
    if bytes.len() != 32 {
        return Err(CompositionError::invalid(
            "private key must contain exactly 32 bytes",
        ));
    }
    Ok(SecretBytes::new(bytes))
}

fn bitcoin_scope(config: &BitcoinConfig) -> IndexScope {
    IndexScope {
        chain: indexing::ChainId(chain_bitcoin::CHAIN.to_owned()),
        network: config.network.clone(),
    }
}

fn ethereum_scope(config: &EthereumConfig) -> IndexScope {
    IndexScope {
        chain: indexing::ChainId(chain_ethereum::CHAIN.to_owned()),
        network: config.network.clone(),
    }
}

fn bitcoin_network(value: &str) -> Result<Network, CompositionError> {
    match value {
        "mainnet" => Ok(Network::Mainnet),
        "testnet3" => Ok(Network::Testnet3),
        "testnet4" => Ok(Network::Testnet4),
        "signet" => Ok(Network::Signet),
        "regtest" => Ok(Network::Regtest),
        _ => Err(CompositionError::invalid("unsupported Bitcoin network")),
    }
}

#[cfg(test)]
#[path = "composition_test.rs"]
mod tests;
