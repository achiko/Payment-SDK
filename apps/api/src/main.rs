mod config;
mod readiness;

use std::{collections::BTreeMap, env, future::IntoFuture, sync::Arc};

use chain_bitcoin::{AddressType, FeeRate, IndexUtxos};
use chain_ethereum::AssetKind;
use config::{AnyError, Config};
use indexing::BlockHeight;
use payment_api::{State, WalletAsset};
use tokio::sync::watch;

const USDC_DECIMALS: u8 = 6;

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
        let usdc = config.usdc.as_ref().map(config::UsdcConfig::contract);
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
        let accounts = Arc::new(chain_ethereum::AccountClient::new(
            client.clone(),
            config.chain_id,
        )?);
        if let Some(contract) = &usdc {
            accounts.validate_token(contract, USDC_DECIMALS).await?;
        }
        let accounts: Arc<dyn chain_ethereum::Accounts> = accounts;
        let transactions: Arc<dyn chain_ethereum::Transactions> = Arc::new(
            chain_ethereum::TransactionClient::new(client, config.chain_id, config.limits()?)?,
        );
        Some((scope, config.chain_id, usdc, accounts, transactions))
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
        wallets.register(WalletAsset::Btc, scope, provider, sender, None)?;
    }
    if let Some((scope, chain_id, usdc, accounts, transactions)) = ethereum {
        let eth_provider = chain_ethereum::WalletProvider::new(
            chain_ethereum::WalletConfig {
                scope: scope.clone(),
                chain_id,
                asset: AssetKind::Native,
                decimals: chain_ethereum::ETH.decimals,
            },
            accounts.clone(),
            transactions.clone(),
            history.clone(),
        );
        let eth_sender = eth_provider.transactions();
        wallets.register(
            WalletAsset::Eth,
            scope.clone(),
            eth_provider,
            eth_sender,
            None,
        )?;

        if let Some(contract) = usdc {
            let usdc_provider = chain_ethereum::WalletProvider::new(
                chain_ethereum::WalletConfig {
                    scope: scope.clone(),
                    chain_id,
                    asset: AssetKind::Erc20(contract),
                    decimals: u32::from(USDC_DECIMALS),
                },
                accounts,
                transactions,
                history,
            );
            let usdc_sender = usdc_provider.transactions();
            wallets.register(WalletAsset::Usdc, scope, usdc_provider, usdc_sender, None)?;
        }
    }
    let mut imported_ethereum_assets = BTreeMap::new();
    for configured in config.wallets {
        let encoded = env::var(configured.secret_env)?;
        let secret = hex::decode(encoded).map_err(|_| "wallet secret must be hexadecimal")?;
        if secret.len() != 32 {
            return Err("wallet secret must contain exactly 32 bytes".into());
        }
        let asset = configured.asset;
        let wallet = wallets
            .import(
                configured.id,
                &asset,
                wallets::SecretBytes::new(secret),
                BlockHeight(configured.start_height),
            )
            .await?;
        if matches!(asset, WalletAsset::Eth | WalletAsset::Usdc) {
            remember_imported_ethereum_asset(
                &mut imported_ethereum_assets,
                wallet.address.text,
                asset,
            )?;
        }
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

fn remember_imported_ethereum_asset(
    addresses: &mut BTreeMap<String, WalletAsset>,
    address: String,
    asset: WalletAsset,
) -> Result<(), AnyError> {
    if let Some(existing) = addresses.get(&address) {
        if *existing != asset {
            return Err(
                "one imported Ethereum address cannot be registered for multiple assets".into(),
            );
        }
    } else {
        addresses.insert(address, asset);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_ethereum_address_has_one_asset() {
        let mut addresses = BTreeMap::new();
        remember_imported_ethereum_asset(
            &mut addresses,
            "0x1111111111111111111111111111111111111111".to_owned(),
            WalletAsset::Eth,
        )
        .expect("first asset");
        remember_imported_ethereum_asset(
            &mut addresses,
            "0x1111111111111111111111111111111111111111".to_owned(),
            WalletAsset::Eth,
        )
        .expect("same asset may share an imported address");
        let error = remember_imported_ethereum_asset(
            &mut addresses,
            "0x1111111111111111111111111111111111111111".to_owned(),
            WalletAsset::Usdc,
        )
        .expect_err("one imported address cannot represent two assets");
        assert_eq!(
            error.to_string(),
            "one imported Ethereum address cannot be registered for multiple assets"
        );
    }
}
