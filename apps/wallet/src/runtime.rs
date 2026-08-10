use std::{error::Error, fmt, sync::Arc};

use chain_bitcoin::BitcoinCoreClient;
use chain_ethereum::{Ethereum, EthereumHttpRpc, EthereumWallet};
use http_support::{HealthState, HttpTransport};
use json_rpc::TransportJsonRpcClient;
use signer::{Curve, KeyTweakKind, SignatureScheme, Signer, SignerCapabilities, SignerStatus};
use signer_remote::RemoteSignerClient;
use tokio::sync::watch;
use tracing::info;
use wallet_worker::{
    BitcoinIxClient, BitcoinOperations, EthereumOperations, WalletService, api, bitcoin_api,
};

use crate::config::{BitcoinServeOptions, ServeOptions};

pub type AppError = Box<dyn Error + Send + Sync>;
pub type AppResult<T> = Result<T, AppError>;

pub async fn serve(options: ServeOptions) -> AppResult<()> {
    options.validate()?;
    let rpc = EthereumHttpRpc::new(options.rpc_configuration()?)?;
    rpc.verify_chain_id().await.map_err(|_| {
        Box::new(RuntimeError(
            "Ethereum RPC readiness probe failed or returned the wrong chain".to_owned(),
        )) as AppError
    })?;

    let custody = RemoteSignerClient::connect(options.custody_configuration()?)
        .await
        .map_err(|_| {
            Box::new(RuntimeError(
                "remote custody capability discovery failed".to_owned(),
            )) as AppError
        })?;
    validate_ethereum_custody_capabilities(&custody.capabilities())?;
    match custody.status().await.map_err(|_| {
        Box::new(RuntimeError(
            "remote custody readiness probe failed".to_owned(),
        )) as AppError
    })? {
        SignerStatus::Available => {}
        SignerStatus::InteractionRequired | SignerStatus::Unavailable { .. } => {
            return Err(Box::new(RuntimeError(
                "remote custody is not available for unattended signing".to_owned(),
            )));
        }
    }

    let wallet = EthereumWallet::new(options.chain_id, rpc);
    let service = WalletService::<Ethereum, _, _, _>::new(wallet, custody.clone(), custody);
    let operations: Arc<dyn api::EthereumWalletOperations> =
        Arc::new(EthereumOperations::new(service));

    let health = HealthState::new(false);
    let server_config = options.server_configuration()?;
    let router =
        http_support::service_router(api::router(operations), &server_config, health.clone())?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = http_support::serve(router, &server_config, shutdown_signal(shutdown_rx));
    tokio::pin!(server);

    health.set_ready(true);
    info!(
        chain_id = options.chain_id,
        bind = %options.http_bind,
        "stateless Ethereum Wallet Service is ready"
    );

    tokio::select! {
        result = &mut server => return result.map_err(|error| Box::new(error) as AppError),
        result = termination_signal() => result?,
    }

    // Readiness flips before graceful drain begins, so callers stop sending
    // new signing or broadcast requests during shutdown.
    health.set_ready(false);
    let _ = shutdown_tx.send(true);
    match tokio::time::timeout(options.shutdown_grace(), &mut server).await {
        Ok(result) => result.map_err(|error| Box::new(error) as AppError),
        Err(_) => Err(Box::new(RuntimeError(
            "Wallet Service graceful shutdown deadline expired".to_owned(),
        ))),
    }
}

pub async fn serve_bitcoin(options: BitcoinServeOptions) -> AppResult<()> {
    options.validate()?;
    let network = options.bitcoin_network()?;
    let transport = HttpTransport::new(options.core_transport_configuration()?)?;
    let json_rpc = TransportJsonRpcClient::new(transport, options.core_rpc_url.clone());
    let core = Arc::new(
        BitcoinCoreClient::connect(json_rpc, options.core_configuration()?)
            .await
            .map_err(|_| {
                Box::new(RuntimeError(
                    "Bitcoin Core 31 readiness, identity, or deployment probe failed".to_owned(),
                )) as AppError
            })?,
    );
    let ix = Arc::new(BitcoinIxClient::new(network, options.ix_configuration()?)?);
    let ix_status = ix.readiness().await.map_err(|_| {
        Box::new(RuntimeError(
            "Bitcoin IX readiness or network probe failed".to_owned(),
        )) as AppError
    })?;
    let ix_canonical_hash = core
        .canonical_hash(ix_status.checkpoint.height)
        .await
        .map_err(|_| {
            Box::new(RuntimeError(
                "Bitcoin Core could not verify the IX checkpoint".to_owned(),
            )) as AppError
        })?;
    if ix_canonical_hash.as_ref() != Some(&ix_status.checkpoint.hash) {
        return Err(Box::new(RuntimeError(
            "Bitcoin IX checkpoint does not match Bitcoin Core".to_owned(),
        )));
    }

    let custody = RemoteSignerClient::connect(options.custody_configuration()?)
        .await
        .map_err(|_| {
            Box::new(RuntimeError(
                "remote custody capability discovery failed".to_owned(),
            )) as AppError
        })?;
    validate_bitcoin_custody_capabilities(&custody.capabilities())?;
    match custody.status().await.map_err(|_| {
        Box::new(RuntimeError(
            "remote custody readiness probe failed".to_owned(),
        )) as AppError
    })? {
        SignerStatus::Available => {}
        SignerStatus::InteractionRequired | SignerStatus::Unavailable { .. } => {
            return Err(Box::new(RuntimeError(
                "remote custody is not available for unattended signing".to_owned(),
            )));
        }
    }

    let operations: Arc<dyn bitcoin_api::BitcoinWalletOperations> =
        Arc::new(BitcoinOperations::new(
            network,
            core,
            ix,
            custody.clone(),
            custody,
            options.operation_policy()?,
        )?);
    let health = HealthState::new(false);
    let server_config = options.server_configuration()?;
    let router = http_support::service_router(
        bitcoin_api::router(network, operations),
        &server_config,
        health.clone(),
    )?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = http_support::serve(router, &server_config, shutdown_signal(shutdown_rx));
    tokio::pin!(server);

    health.set_ready(true);
    info!(
        network = network.canonical_name(),
        ix_checkpoint_height = ix_status.checkpoint.height.0,
        bind = %options.http_bind,
        "stateless Bitcoin Wallet Service is ready"
    );

    tokio::select! {
        result = &mut server => return result.map_err(|error| Box::new(error) as AppError),
        result = termination_signal() => result?,
    }

    health.set_ready(false);
    let _ = shutdown_tx.send(true);
    match tokio::time::timeout(options.shutdown_grace(), &mut server).await {
        Ok(result) => result.map_err(|error| Box::new(error) as AppError),
        Err(_) => Err(Box::new(RuntimeError(
            "Bitcoin Wallet Service graceful shutdown deadline expired".to_owned(),
        ))),
    }
}

fn validate_ethereum_custody_capabilities(capabilities: &SignerCapabilities) -> AppResult<()> {
    if !capabilities.curves.contains(&Curve::Secp256k1)
        || !capabilities
            .schemes
            .contains(&SignatureScheme::EcdsaSecp256k1)
        || !capabilities.can_sign_digests
    {
        return Err(Box::new(RuntimeError(
            "remote custody lacks required secp256k1 digest-signing capability".to_owned(),
        )));
    }
    Ok(())
}

fn validate_bitcoin_custody_capabilities(capabilities: &SignerCapabilities) -> AppResult<()> {
    if !capabilities.curves.contains(&Curve::Secp256k1)
        || !capabilities
            .schemes
            .contains(&SignatureScheme::EcdsaSecp256k1)
        || !capabilities
            .schemes
            .contains(&SignatureScheme::SchnorrSecp256k1)
        || !capabilities
            .key_tweaks
            .contains(&KeyTweakKind::Secp256k1Add)
        || !capabilities.can_sign_digests
    {
        return Err(Box::new(RuntimeError(
            "remote custody lacks required Bitcoin P2WPKH/P2TR digest-signing and tweak capabilities"
                .to_owned(),
        )));
    }
    Ok(())
}

async fn termination_signal() -> AppResult<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|error| Box::new(error) as AppError)?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(|error| Box::new(error) as AppError),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| Box::new(error) as AppError)
    }
}

async fn shutdown_signal(mut shutdown: watch::Receiver<bool>) {
    if !*shutdown.borrow() {
        drop(shutdown.changed().await);
    }
}

#[derive(Debug)]
struct RuntimeError(String);

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_contract_requires_ethereum_digest_signing() {
        let capabilities = SignerCapabilities {
            curves: vec![Curve::Secp256k1],
            schemes: vec![SignatureScheme::EcdsaSecp256k1],
            key_tweaks: Vec::new(),
            can_sign_messages: false,
            can_sign_digests: true,
            requires_user_interaction: false,
        };
        assert!(capabilities.curves.contains(&Curve::Secp256k1));
        assert!(capabilities.can_sign_digests);
        validate_ethereum_custody_capabilities(&capabilities)
            .expect("Ethereum capability set must be accepted");
    }

    #[test]
    fn capability_contract_requires_bitcoin_schnorr_and_tweak_support() {
        let mut capabilities = SignerCapabilities {
            curves: vec![Curve::Secp256k1],
            schemes: vec![
                SignatureScheme::EcdsaSecp256k1,
                SignatureScheme::SchnorrSecp256k1,
            ],
            key_tweaks: vec![KeyTweakKind::Secp256k1Add],
            can_sign_messages: false,
            can_sign_digests: true,
            requires_user_interaction: false,
        };
        validate_bitcoin_custody_capabilities(&capabilities)
            .expect("complete Bitcoin capability set must be accepted");
        capabilities.key_tweaks.clear();
        assert!(validate_bitcoin_custody_capabilities(&capabilities).is_err());
    }
}
