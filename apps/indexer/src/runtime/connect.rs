use std::{num::NonZeroU32, time::Duration};

use chain_bitcoin::{Source as BitcoinSourceClient, SourceConfig as BitcoinSourceConfig};
use chain_ethereum::{Source as EthereumSourceClient, SourceConfig as EthereumSourceConfig};
use indexing::IndexScope;
use indexing_rocksdb::{Config as RepositoryConfig, RocksRepository};
use json_rpc::TransportClient;
use storage_rocksdb::RocksDb;

use super::{AppError, AppResult, failure};
use crate::config::{
    BitcoinRepository, BitcoinSource, EthereumRepository, EthereumSource, bitcoin_bootstrap_height,
    bootstrap_height,
};

type RpcClient = TransportClient<http::client::Reqwest>;
type EthereumClient = EthereumSourceClient<RpcClient>;
type EthereumStore = RocksRepository;
type BitcoinStore = RocksRepository;

pub(super) fn repository(
    storage: RocksDb,
    options: &EthereumRepository,
) -> AppResult<EthereumStore> {
    Ok(RocksRepository::new(storage, repository_config(options)?))
}

pub(super) fn repository_config(options: &EthereumRepository) -> AppResult<RepositoryConfig> {
    Ok(RepositoryConfig::new(
        options.scope()?,
        bootstrap_height(options),
        options.confirmation_policy()?,
        options.reorg_retention,
    )?)
}

pub(super) fn bitcoin_repository(
    storage: RocksDb,
    options: &BitcoinRepository,
) -> AppResult<BitcoinStore> {
    Ok(RocksRepository::new(
        storage,
        bitcoin_repository_config(options)?,
    ))
}

pub(super) fn bitcoin_repository_config(
    options: &BitcoinRepository,
) -> AppResult<RepositoryConfig> {
    Ok(RepositoryConfig::new(
        options.scope()?,
        bitcoin_bootstrap_height(options),
        options.confirmation_policy()?,
        options.reorg_retention,
    )?)
}

pub(super) async fn connect_source(
    scope: &IndexScope,
    options: &EthereumSource,
) -> AppResult<EthereumClient> {
    let attempts = NonZeroU32::new(3)
        .ok_or_else(|| failure("non-zero RPC retry count could not be constructed"))?;
    let mut transport_config = http::client::Config::new(&options.rpc_http_url, options.timeout());
    transport_config.retry_policy =
        http::client::Retry::new(attempts, Duration::from_millis(250), Duration::from_secs(2))?;
    let transport = http::client::Reqwest::new(transport_config)?;
    let client = TransportClient::new(transport, &options.rpc_http_url);
    EthereumSourceClient::connect(
        client,
        EthereumSourceConfig {
            scope: scope.clone(),
            expected_chain_id: options.expected_chain_id,
            expected_genesis_hash: options.genesis_hash()?,
        },
    )
    .await
    .map_err(|error| Box::new(error) as AppError)
}

pub(super) async fn connect_bitcoin_source(
    scope: &IndexScope,
    network: chain_bitcoin::Network,
    options: &BitcoinSource,
) -> AppResult<BitcoinSourceClient<RpcClient>> {
    let attempts = NonZeroU32::new(3)
        .ok_or_else(|| failure("non-zero RPC retry count could not be constructed"))?;
    let mut transport_config = http::client::Config::new(&options.rpc_http_url, options.timeout());
    transport_config.max_response_bytes = options.rpc_max_response_bytes;
    transport_config.default_headers = options.parsed_rpc_headers()?;
    transport_config.retry_policy =
        http::client::Retry::new(attempts, Duration::from_millis(250), Duration::from_secs(2))?;
    let transport = http::client::Reqwest::new(transport_config)?;
    let client = TransportClient::new(transport, &options.rpc_http_url);
    BitcoinSourceClient::connect(
        client,
        BitcoinSourceConfig {
            scope: scope.clone(),
            network,
            expected_genesis_hash: options.genesis_hash()?,
        },
    )
    .await
    .map_err(|error| Box::new(error) as AppError)
}
