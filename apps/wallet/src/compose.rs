use std::{error::Error, fmt, num::NonZeroU32, sync::Arc, time::Duration};

use chain_bitcoin::{
    CoreConfig, IndexUtxos, RpcClient, WalletConfig as BitcoinConfig,
    WalletProvider as BitcoinProvider, parse_bitcoin_block_hash,
};
use chain_ethereum::{
    AssetKind, HttpConfig, Limits, WalletConfig as EthereumConfig,
    WalletProvider as EthereumProvider, Wei,
};
use http_support::client::{Config as ClientConfig, Reqwest, Retry};
use indexing::{ChainId, IndexScope};
use indexing_http::{Config as IndexConfig, Remote};
use json_rpc::{Failover, TransportClient};
use wallets::{Wallet, Wallets};

use crate::{Config, Service, config};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ProviderKey {
    Bitcoin,
    Ethereum,
}

pub async fn compose(
    config: Config,
) -> Result<(http_support::server::Config, Service), ComposeError> {
    let Config {
        bind,
        bearer_token,
        tls_terminated_upstream,
        bitcoin: bitcoin_config,
        ethereum: ethereum_config,
    } = config;
    let transport = if tls_terminated_upstream {
        http_support::server::TransportSecurity::TlsTerminatedUpstream
    } else {
        http_support::server::TransportSecurity::PlaintextLoopback
    };
    let server = http_support::server::Config::new(
        bind,
        transport,
        Some(bearer_token),
        http_support::server::RequestLimits::default(),
    );
    server.validate().map_err(ComposeError::source)?;
    let mut service = Service::new();
    if let Some(options) = bitcoin_config {
        let (id, wallet) = bitcoin(options).await?;
        service = service.with(id, wallet);
    }
    if let Some(options) = ethereum_config {
        let (id, wallet) = ethereum(options).await?;
        service = service.with(id, wallet);
    }
    Ok((server, service))
}

async fn bitcoin(options: config::Bitcoin) -> Result<(String, Arc<dyn Wallet>), ComposeError> {
    let scope = IndexScope {
        chain: ChainId(chain_bitcoin::CHAIN.to_owned()),
        network: options.network.canonical_name().to_owned(),
    };
    let index = indexer(options.indexer_urls, options.indexer_token, options.timeout)?;
    let utxos = Arc::new(
        IndexUtxos::new(scope.clone(), options.network, Arc::new(index.clone()))
            .map_err(ComposeError::source)?,
    );
    let clients = options
        .rpc_urls
        .iter()
        .map(|endpoint| {
            let mut config = ClientConfig::new(endpoint, options.timeout);
            config.max_response_bytes = 64 * 1024 * 1024;
            config.retry_policy = retry()?;
            Ok(TransportClient::new(
                Reqwest::new(config).map_err(ComposeError::source)?,
                endpoint,
            ))
        })
        .collect::<Result<Vec<_>, ComposeError>>()?;
    let mut transport = Failover::new(clients).map_err(ComposeError::source)?;
    if let Some(value) = options.rpc_authorization {
        transport = transport.with_header("authorization", value);
    }
    let rpc = RpcClient::connect(
        transport,
        CoreConfig {
            expected_network: options.network,
            expected_genesis_hash: parse_bitcoin_block_hash(&options.genesis_hash)
                .map_err(ComposeError::source)?,
        },
    )
    .await
    .map_err(ComposeError::source)?;
    let provider = BitcoinProvider::new(
        BitcoinConfig {
            scope,
            network: options.network,
            address_type: options.address_type,
            fee_target_blocks: options.fee_target_blocks,
            max_fee_rate: options.max_fee_rate,
        },
        utxos,
        Arc::new(rpc.fees()),
        Arc::new(rpc.transactions()),
        Arc::new(index),
    );
    let mut wallets = Wallets::new();
    wallets
        .register(ProviderKey::Bitcoin, provider)
        .map_err(ComposeError::source)?;
    let wallet = wallets
        .new_wallet(&ProviderKey::Bitcoin, options.secret)
        .await
        .map_err(ComposeError::source)?;
    Ok((options.id, wallet))
}

async fn ethereum(options: config::Ethereum) -> Result<(String, Arc<dyn Wallet>), ComposeError> {
    let index = indexer(options.indexer_urls, options.indexer_token, options.timeout)?;
    let limits = Limits::new(
        128 * 1024,
        2_000,
        30_000_000,
        Wei::from_u128(1_000_000_000_000),
        Wei::from_u128(100_000_000_000),
        Wei::from_u128(10_000_000_000_000_000_000),
    )
    .map_err(ComposeError::source)?;
    let mut rpc = HttpConfig::new(
        options.rpc_urls[0].clone(),
        options.chain_id,
        options.timeout,
        64 * 1024 * 1024,
        retry()?,
        limits,
    )
    .map_err(ComposeError::source)?
    .with_endpoints(options.rpc_urls)
    .map_err(ComposeError::source)?;
    if let Some(value) = options.rpc_authorization {
        rpc = rpc.with_header("authorization", value);
    }
    let (accounts, transactions) = rpc.connect().map_err(ComposeError::source)?;
    let scope = IndexScope {
        chain: ChainId(chain_ethereum::CHAIN.to_owned()),
        network: options.network,
    };
    let provider = EthereumProvider::new(
        EthereumConfig {
            scope,
            asset: AssetKind::Native,
            decimals: chain_ethereum::ETH.decimals,
        },
        Arc::new(accounts),
        Arc::new(transactions),
        Arc::new(index),
    );
    let mut wallets = Wallets::new();
    wallets
        .register(ProviderKey::Ethereum, provider)
        .map_err(ComposeError::source)?;
    let wallet = wallets
        .new_wallet(&ProviderKey::Ethereum, options.secret)
        .await
        .map_err(ComposeError::source)?;
    Ok((options.id, wallet))
}

fn indexer(
    endpoints: Vec<String>,
    token: Option<String>,
    timeout: Duration,
) -> Result<Remote<Reqwest>, ComposeError> {
    let mut config = IndexConfig::with_endpoints(endpoints);
    config.bearer_token = token;
    config.request_timeout = timeout;
    config.retry_policy = retry()?;
    Remote::connect(config).map_err(ComposeError::source)
}

fn retry() -> Result<Retry, ComposeError> {
    Retry::new(
        NonZeroU32::new(3).ok_or_else(|| ComposeError::new("invalid retry count"))?,
        Duration::from_millis(250),
        Duration::from_secs(2),
    )
    .map_err(ComposeError::source)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposeError {
    message: String,
}
impl ComposeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
    fn source(error: impl Error) -> Self {
        Self::new(error.to_string())
    }
}
impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for ComposeError {}
