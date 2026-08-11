use std::{error::Error, fmt, future::Future, sync::Arc, time::Duration};

use axum::{Router, extract::State, response::IntoResponse, routing::get};
use chain_bitcoin::BitcoinCoreClient;
use chain_ethereum::{Ethereum, EthereumHttpRpc, EthereumWallet};
use http_support::{AuthenticationMode, HealthState, HttpServerConfig, HttpTransport};
use json_rpc::TransportJsonRpcClient;
use signer::{Curve, KeyTweakKind, SignatureScheme, Signer, SignerCapabilities, SignerStatus};
use signer_remote::RemoteSignerClient;
use telemetry::{Attribute, PrometheusTelemetry, Telemetry};
use tokio::sync::watch;
use tracing::{info, warn};
use wallet_worker::{
    BitcoinIxClient, BitcoinOperations, EthereumOperations, WalletService, api, bitcoin_api,
};

use crate::config::{BitcoinServeOptions, CustodyAuthenticationPolicy, ServeOptions};

pub type AppError = Box<dyn Error + Send + Sync>;
pub type AppResult<T> = Result<T, AppError>;

const DEPENDENCY_READINESS_INTERVAL: Duration = Duration::from_secs(5);

pub async fn serve(options: ServeOptions) -> AppResult<()> {
    options.validate()?;
    let telemetry = install_telemetry(options.authentication_mode)?;
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

    let readiness_monitor = DependencyReadinessMonitor::Ethereum {
        custody: custody.clone(),
    };
    let wallet = EthereumWallet::new(options.chain_id, rpc);
    let service = WalletService::<Ethereum, _, _, _>::new(wallet, custody.clone(), custody);
    let operations: Arc<dyn api::EthereumWalletOperations> =
        Arc::new(EthereumOperations::new(service));

    let health = HealthState::new(false);
    let server_config = options.server_configuration()?;
    let metrics_server_config = options.metrics_server_configuration()?;
    let router = http_support::service_router(
        api::router(operations, options.authentication_mode),
        &server_config,
        health.clone(),
    )?;

    health.set_ready(true);
    warn_global_trusted(
        options.authentication_mode,
        options.custody_authentication_policy,
        false,
    );
    info!(
        chain_id = options.chain_id,
        bind = %options.http_bind,
        metrics_bind = %options.metrics_bind,
        authentication_mode = %options.authentication_mode,
        custody_authentication_policy = %options.custody_authentication_policy,
        "stateless Ethereum Wallet Service is ready"
    );
    serve_http_pair(
        router,
        server_config,
        metrics_router(telemetry),
        metrics_server_config,
        health,
        readiness_monitor,
        options.shutdown_grace(),
    )
    .await
}

pub async fn serve_bitcoin(options: BitcoinServeOptions) -> AppResult<()> {
    options.validate()?;
    let telemetry = install_telemetry(options.authentication_mode)?;
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

    let readiness_monitor = DependencyReadinessMonitor::Bitcoin {
        custody: custody.clone(),
        ix: Arc::clone(&ix),
    };
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
    let metrics_server_config = options.metrics_server_configuration()?;
    let router = http_support::service_router(
        bitcoin_api::router(network, operations, options.authentication_mode),
        &server_config,
        health.clone(),
    )?;

    health.set_ready(true);
    warn_global_trusted(
        options.authentication_mode,
        options.custody_authentication_policy,
        true,
    );
    info!(
        network = network.canonical_name(),
        ix_checkpoint_height = ix_status.checkpoint.height.0,
        bind = %options.http_bind,
        metrics_bind = %options.metrics_bind,
        authentication_mode = %options.authentication_mode,
        custody_authentication_policy = %options.custody_authentication_policy,
        "stateless Bitcoin Wallet Service is ready"
    );
    serve_http_pair(
        router,
        server_config,
        metrics_router(telemetry),
        metrics_server_config,
        health,
        readiness_monitor,
        options.shutdown_grace(),
    )
    .await
}

fn install_telemetry(authentication_mode: AuthenticationMode) -> AppResult<PrometheusTelemetry> {
    let telemetry = PrometheusTelemetry::install()?;
    telemetry.gauge(
        "payment_sdk_strict_authentication_mode",
        if authentication_mode.is_strict() {
            1.0
        } else {
            0.0
        },
        &[Attribute {
            key: "service".to_owned(),
            value: "wallet".to_owned(),
        }],
    );
    Ok(telemetry)
}

fn metrics_router(telemetry: PrometheusTelemetry) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .with_state(telemetry)
}

async fn metrics(State(telemetry): State<PrometheusTelemetry>) -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        telemetry.render(),
    )
}

fn warn_global_trusted(
    authentication_mode: AuthenticationMode,
    custody_authentication_policy: CustodyAuthenticationPolicy,
    bitcoin: bool,
) {
    if authentication_mode != AuthenticationMode::GlobalTrusted {
        return;
    }
    let ignored_credentials = match (bitcoin, custody_authentication_policy) {
        (false, CustodyAuthenticationPolicy::RepositoryModeMatched) => {
            "WS_BEARER_TOKEN and WS_CUSTODY_BEARER_TOKEN"
        }
        (false, CustodyAuthenticationPolicy::IndependentStrict) => "WS_BEARER_TOKEN",
        (true, CustodyAuthenticationPolicy::RepositoryModeMatched) => {
            "WS_BEARER_TOKEN, WS_CUSTODY_BEARER_TOKEN, WS_BITCOIN_IX_BEARER_TOKEN, and IX Authorization headers"
        }
        (true, CustodyAuthenticationPolicy::IndependentStrict) => {
            "WS_BEARER_TOKEN, WS_BITCOIN_IX_BEARER_TOKEN, and IX Authorization headers"
        }
    };
    warn!(
        ignored_credentials,
        custody_authentication_policy = %custody_authentication_policy,
        "STRICT AUTHENTICATION IS DISABLED: every reachable caller is globally trusted; listed service credentials are ignored"
    );
}

async fn serve_http_pair(
    api_router: Router,
    api_config: HttpServerConfig,
    metrics_router: Router,
    metrics_config: HttpServerConfig,
    health: HealthState,
    readiness_monitor: DependencyReadinessMonitor,
    shutdown_grace: Duration,
) -> AppResult<()> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let timeout_message = readiness_monitor.shutdown_timeout_message();
    let api_server = http_support::serve(
        api_router,
        &api_config,
        shutdown_signal(shutdown_rx.clone()),
    );
    let metrics_server = http_support::serve(
        metrics_router,
        &metrics_config,
        shutdown_signal(shutdown_rx.clone()),
    );
    let readiness_monitor = readiness_monitor.run(health.clone(), shutdown_rx);
    tokio::pin!(api_server);
    tokio::pin!(metrics_server);
    tokio::pin!(readiness_monitor);

    tokio::select! {
        result = &mut api_server => return result.map_err(|error| Box::new(error) as AppError),
        result = &mut metrics_server => return result.map_err(|error| Box::new(error) as AppError),
        () = &mut readiness_monitor => return Err(Box::new(RuntimeError(
            "Wallet Service dependency readiness monitor stopped unexpectedly".to_owned(),
        ))),
        result = termination_signal() => result?,
    }

    // Readiness flips before graceful drain begins, so callers stop sending
    // new signing or broadcast requests during shutdown.
    health.set_ready(false);
    let _ = shutdown_tx.send(true);
    let shutdown = async {
        let (api_result, metrics_result, ()) =
            tokio::join!(&mut api_server, &mut metrics_server, &mut readiness_monitor);
        api_result.map_err(|error| Box::new(error) as AppError)?;
        metrics_result.map_err(|error| Box::new(error) as AppError)
    };
    match tokio::time::timeout(shutdown_grace, shutdown).await {
        Ok(result) => result,
        Err(_) => Err(Box::new(RuntimeError(timeout_message.to_owned()))),
    }
}

enum DependencyReadinessMonitor {
    Ethereum {
        custody: RemoteSignerClient,
    },
    Bitcoin {
        custody: RemoteSignerClient,
        ix: Arc<BitcoinIxClient>,
    },
}

impl DependencyReadinessMonitor {
    fn shutdown_timeout_message(&self) -> &'static str {
        match self {
            Self::Ethereum { .. } => "Wallet Service graceful shutdown deadline expired",
            Self::Bitcoin { .. } => "Bitcoin Wallet Service graceful shutdown deadline expired",
        }
    }

    async fn dependencies_ready(&self) -> bool {
        match self {
            Self::Ethereum { custody } => {
                matches!(custody.status().await, Ok(SignerStatus::Available))
            }
            Self::Bitcoin { custody, ix } => {
                let (custody_status, ix_status) = tokio::join!(custody.status(), ix.readiness());
                matches!(custody_status, Ok(SignerStatus::Available)) && ix_status.is_ok()
            }
        }
    }

    async fn run(self, health: HealthState, shutdown: watch::Receiver<bool>) {
        let monitor = Arc::new(self);
        run_readiness_monitor(health, DEPENDENCY_READINESS_INTERVAL, shutdown, move || {
            let monitor = Arc::clone(&monitor);
            async move { monitor.dependencies_ready().await }
        })
        .await;
    }
}

async fn run_readiness_monitor<P, F>(
    health: HealthState,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    mut probe: P,
) where
    P: FnMut() -> F,
    F: Future<Output = bool>,
{
    let start = tokio::time::Instant::now() + interval;
    let mut ticker = tokio::time::interval_at(start, interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => {
                health.set_ready(false);
                return;
            }
            _ = ticker.tick() => {}
        }

        let ready = tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => {
                health.set_ready(false);
                return;
            }
            ready = probe() => ready,
        };
        health.set_ready(ready);
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
    wait_for_shutdown(&mut shutdown).await;
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            return;
        }
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
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn recurring_monitor_tracks_dependency_recovery_and_shutdown() {
        let health = HealthState::new(true);
        let dependency_ready = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let probe_state = Arc::clone(&dependency_ready);
        let monitor = tokio::spawn(run_readiness_monitor(
            health.clone(),
            DEPENDENCY_READINESS_INTERVAL,
            shutdown_rx,
            move || std::future::ready(probe_state.load(Ordering::Acquire)),
        ));

        tokio::task::yield_now().await;
        tokio::time::advance(DEPENDENCY_READINESS_INTERVAL).await;
        tokio::task::yield_now().await;
        assert!(!health.is_ready());

        dependency_ready.store(true, Ordering::Release);
        tokio::time::advance(DEPENDENCY_READINESS_INTERVAL).await;
        tokio::task::yield_now().await;
        assert!(health.is_ready());

        shutdown_tx
            .send(true)
            .expect("readiness monitor must remain subscribed");
        monitor.await.expect("readiness monitor must not panic");
        assert!(!health.is_ready());
    }

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
