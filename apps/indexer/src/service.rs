use std::{error::Error, future::Future, net::SocketAddr, path::PathBuf};

use crate::{
    config::{
        BitcoinRepositoryOptions, BitcoinServeOptions, BitcoinSourceOptions, ConfigError,
        DatabaseOptions, RepositoryOptions, ServeOptions, SourceOptions,
    },
    runtime,
};
use chain_bitcoin::BitcoinNetwork;
use http::AuthenticationMode;
use telemetry::PrometheusTelemetry;

const DEFAULT_CONFIRMATION_DEPTH: u64 = 12;
const DEFAULT_REORG_RETENTION: u64 = 50;
const DEFAULT_RPC_TIMEOUT_SECONDS: u64 = 15;
const DEFAULT_BITCOIN_RPC_MAX_RESPONSE_BYTES: usize = 268_435_456;
const DEFAULT_HTTP_PORT: u16 = 8080;
const DEFAULT_METRICS_PORT: u16 = 9090;
const DEFAULT_POLL_SECONDS: u64 = 5;
const DEFAULT_READY_MAX_LAG: u64 = 2;
const DEFAULT_READY_MAX_AGE_SECONDS: u64 = 30;

/// Error returned when an [`IndexerServiceConfig`] is invalid.
pub type IndexerServiceConfigError = ConfigError;

/// Error returned while starting, running, or stopping an [`IndexerService`].
pub type IndexerServiceError = Box<dyn Error + Send + Sync>;

/// Programmatic configuration for one embedded Ethereum Indexer Service.
///
/// [`Self::new`] selects the documented Ethereum v1 defaults. Fields remain
/// public so a composition root can intentionally override policy, network,
/// listener, and readiness settings before constructing the service. This type
/// deliberately does not implement `Debug`, preventing RPC credentials or a
/// bearer token from being printed accidentally.
pub struct IndexerServiceConfig {
    pub database_path: PathBuf,
    pub network: String,
    pub bootstrap_height: u64,
    pub confirmation_depth: u64,
    pub reorg_retention: u64,
    pub expected_chain_id: u64,
    pub expected_genesis_hash: String,
    pub rpc_http_url: String,
    pub rpc_ws_url: Option<String>,
    pub rpc_timeout_seconds: u64,
    pub http_bind: SocketAddr,
    pub metrics_bind: SocketAddr,
    pub authentication_mode: AuthenticationMode,
    pub bearer_token: Option<String>,
    pub upstream_tls_terminated: bool,
    pub poll_seconds: u64,
    pub ready_max_lag: u64,
    pub ready_max_age_seconds: u64,
}

impl IndexerServiceConfig {
    /// Creates an Ethereum v1 configuration with loopback listeners and the
    /// documented confirmation, retention, polling, and readiness defaults.
    #[must_use]
    pub fn new(
        database_path: impl Into<PathBuf>,
        network: impl Into<String>,
        bootstrap_height: u64,
        expected_chain_id: u64,
        expected_genesis_hash: impl Into<String>,
        rpc_http_url: impl Into<String>,
        authentication_mode: AuthenticationMode,
    ) -> Self {
        Self {
            database_path: database_path.into(),
            network: network.into(),
            bootstrap_height,
            confirmation_depth: DEFAULT_CONFIRMATION_DEPTH,
            reorg_retention: DEFAULT_REORG_RETENTION,
            expected_chain_id,
            expected_genesis_hash: expected_genesis_hash.into(),
            rpc_http_url: rpc_http_url.into(),
            rpc_ws_url: None,
            rpc_timeout_seconds: DEFAULT_RPC_TIMEOUT_SECONDS,
            http_bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_HTTP_PORT)),
            metrics_bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_METRICS_PORT)),
            authentication_mode,
            bearer_token: None,
            upstream_tls_terminated: false,
            poll_seconds: DEFAULT_POLL_SECONDS,
            ready_max_lag: DEFAULT_READY_MAX_LAG,
            ready_max_age_seconds: DEFAULT_READY_MAX_AGE_SECONDS,
        }
    }

    fn into_serve_options(self) -> ServeOptions {
        ServeOptions {
            repository: RepositoryOptions {
                database: DatabaseOptions {
                    database_path: self.database_path,
                },
                network: self.network,
                bootstrap_height: self.bootstrap_height,
                confirmation_depth: self.confirmation_depth,
                reorg_retention: self.reorg_retention,
            },
            source: SourceOptions {
                expected_chain_id: self.expected_chain_id,
                expected_genesis_hash: self.expected_genesis_hash,
                rpc_http_url: self.rpc_http_url,
                rpc_ws_url: self.rpc_ws_url,
                rpc_timeout_seconds: self.rpc_timeout_seconds,
            },
            http_bind: self.http_bind,
            metrics_bind: self.metrics_bind,
            authentication_mode: self.authentication_mode,
            bearer_token: self.bearer_token,
            upstream_tls_terminated: self.upstream_tls_terminated,
            poll_seconds: self.poll_seconds,
            ready_max_lag: self.ready_max_lag,
            ready_max_age_seconds: self.ready_max_age_seconds,
        }
    }
}

/// One fully composed Ethereum Indexer Service runtime.
///
/// Construction validates configuration without opening RocksDB, connecting to
/// RPC, or binding listeners. Runtime effects begin only when [`Self::run`] or
/// [`Self::run_until`] is awaited.
pub struct IndexerService {
    options: ServeOptions,
}

/// Programmatic configuration for one embedded Bitcoin Indexer Service.
///
/// Confirmation depth and reorg retention are constructor arguments rather
/// than defaults. This makes the deployment's settlement and rollback policy
/// explicit at every composition root. This type deliberately does not
/// implement `Debug`, preventing Bitcoin Core credentials or a bearer token
/// from being printed accidentally.
pub struct BitcoinIndexerServiceConfig {
    pub database_path: PathBuf,
    pub network: BitcoinNetwork,
    pub bootstrap_height: u64,
    pub confirmation_depth: u64,
    pub reorg_retention: u64,
    pub expected_genesis_hash: String,
    pub rpc_http_url: String,
    pub rpc_headers: Vec<String>,
    pub rpc_timeout_seconds: u64,
    pub rpc_max_response_bytes: usize,
    pub http_bind: SocketAddr,
    pub metrics_bind: SocketAddr,
    pub authentication_mode: AuthenticationMode,
    pub bearer_token: Option<String>,
    pub upstream_tls_terminated: bool,
    pub poll_seconds: u64,
    pub ready_max_lag: u64,
    pub ready_max_age_seconds: u64,
}

impl BitcoinIndexerServiceConfig {
    /// Creates a Bitcoin configuration with explicit chain policy and
    /// loopback operational defaults.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database_path: impl Into<PathBuf>,
        network: BitcoinNetwork,
        bootstrap_height: u64,
        confirmation_depth: u64,
        reorg_retention: u64,
        expected_genesis_hash: impl Into<String>,
        rpc_http_url: impl Into<String>,
        authentication_mode: AuthenticationMode,
    ) -> Self {
        Self {
            database_path: database_path.into(),
            network,
            bootstrap_height,
            confirmation_depth,
            reorg_retention,
            expected_genesis_hash: expected_genesis_hash.into(),
            rpc_http_url: rpc_http_url.into(),
            rpc_headers: Vec::new(),
            rpc_timeout_seconds: DEFAULT_RPC_TIMEOUT_SECONDS,
            rpc_max_response_bytes: DEFAULT_BITCOIN_RPC_MAX_RESPONSE_BYTES,
            http_bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_HTTP_PORT)),
            metrics_bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_METRICS_PORT)),
            authentication_mode,
            bearer_token: None,
            upstream_tls_terminated: false,
            poll_seconds: DEFAULT_POLL_SECONDS,
            ready_max_lag: DEFAULT_READY_MAX_LAG,
            ready_max_age_seconds: DEFAULT_READY_MAX_AGE_SECONDS,
        }
    }

    fn into_serve_options(self) -> BitcoinServeOptions {
        BitcoinServeOptions {
            repository: BitcoinRepositoryOptions {
                database: DatabaseOptions {
                    database_path: self.database_path,
                },
                network: self.network.canonical_name().to_owned(),
                bootstrap_height: self.bootstrap_height,
                confirmation_depth: self.confirmation_depth,
                reorg_retention: self.reorg_retention,
            },
            source: BitcoinSourceOptions {
                expected_genesis_hash: self.expected_genesis_hash,
                rpc_http_url: self.rpc_http_url,
                rpc_headers: self.rpc_headers,
                rpc_timeout_seconds: self.rpc_timeout_seconds,
                rpc_max_response_bytes: self.rpc_max_response_bytes,
            },
            http_bind: self.http_bind,
            metrics_bind: self.metrics_bind,
            authentication_mode: self.authentication_mode,
            bearer_token: self.bearer_token,
            upstream_tls_terminated: self.upstream_tls_terminated,
            poll_seconds: self.poll_seconds,
            ready_max_lag: self.ready_max_lag,
            ready_max_age_seconds: self.ready_max_age_seconds,
        }
    }
}

/// One fully composed block-only Bitcoin Indexer Service runtime.
///
/// Construction validates configuration without opening RocksDB, connecting
/// to Bitcoin Core, or binding listeners. Runtime effects begin only when
/// [`Self::run`] or [`Self::run_until`] is awaited.
pub struct BitcoinIndexerService {
    options: BitcoinServeOptions,
}

impl BitcoinIndexerService {
    /// Validates a programmatic Bitcoin configuration and prepares one service.
    pub fn new(config: BitcoinIndexerServiceConfig) -> Result<Self, IndexerServiceConfigError> {
        Self::from_serve_options(config.into_serve_options())
    }

    pub(crate) fn from_serve_options(
        options: BitcoinServeOptions,
    ) -> Result<Self, IndexerServiceConfigError> {
        options.validate()?;
        Ok(Self { options })
    }

    /// Runs until Ctrl+C or a supervised task failure.
    pub async fn run(self, telemetry: PrometheusTelemetry) -> Result<(), IndexerServiceError> {
        runtime::serve_bitcoin(self.options, telemetry).await
    }

    /// Runs until the caller-provided shutdown future completes or a
    /// supervised task fails.
    pub async fn run_until<F>(
        self,
        telemetry: PrometheusTelemetry,
        shutdown: F,
    ) -> Result<(), IndexerServiceError>
    where
        F: Future<Output = ()> + Send,
    {
        runtime::serve_bitcoin_until(self.options, telemetry, async move {
            shutdown.await;
            Ok(())
        })
        .await
    }
}

impl IndexerService {
    /// Validates a programmatic configuration and prepares one service.
    pub fn new(config: IndexerServiceConfig) -> Result<Self, IndexerServiceConfigError> {
        Self::from_serve_options(config.into_serve_options())
    }

    pub(crate) fn from_serve_options(
        options: ServeOptions,
    ) -> Result<Self, IndexerServiceConfigError> {
        options.validate()?;
        Ok(Self { options })
    }

    /// Runs until Ctrl+C or a supervised task failure.
    ///
    /// The caller supplies the process Prometheus adapter explicitly because
    /// installing a metrics recorder is a process-global side effect.
    pub async fn run(self, telemetry: PrometheusTelemetry) -> Result<(), IndexerServiceError> {
        runtime::serve(self.options, telemetry).await
    }

    /// Runs until the caller-provided shutdown future completes or a
    /// supervised task fails.
    ///
    /// This is the preferred lifecycle when the Indexer is embedded in a
    /// larger process whose composition root already owns shutdown handling.
    /// Configuration validation and the synchronous RocksDB open complete
    /// before `shutdown` is polled. The subsequent asynchronous RPC preflight
    /// and the supervised runtime are cancellation-aware.
    pub async fn run_until<F>(
        self,
        telemetry: PrometheusTelemetry,
        shutdown: F,
    ) -> Result<(), IndexerServiceError>
    where
        F: Future<Output = ()> + Send,
    {
        runtime::serve_until(self.options, telemetry, async move {
            shutdown.await;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> IndexerServiceConfig {
        IndexerServiceConfig::new(
            "indexer.db",
            "anvil",
            0,
            31_337,
            format!("0x{}", "11".repeat(32)),
            "http://127.0.0.1:8545",
            AuthenticationMode::GlobalTrusted,
        )
    }

    #[test]
    fn programmatic_config_uses_documented_v1_defaults() {
        let config = config();

        assert_eq!(config.confirmation_depth, 12);
        assert_eq!(config.reorg_retention, 50);
        assert_eq!(config.rpc_timeout_seconds, 15);
        assert_eq!(config.http_bind, SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert_eq!(
            config.metrics_bind,
            SocketAddr::from(([127, 0, 0, 1], 9090))
        );
        assert_eq!(config.poll_seconds, 5);
        assert_eq!(config.ready_max_lag, 2);
        assert_eq!(config.ready_max_age_seconds, 30);
    }

    #[test]
    fn service_rejects_invalid_config_before_runtime_side_effects() {
        let mut config = config();
        config.confirmation_depth = 0;

        let error = match IndexerService::new(config) {
            Ok(_) => panic!("zero confirmation depth must be rejected"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "confirmation depth must be greater than zero"
        );
    }

    fn bitcoin_config() -> BitcoinIndexerServiceConfig {
        let mut config = BitcoinIndexerServiceConfig::new(
            "bitcoin-indexer.db",
            BitcoinNetwork::Regtest,
            0,
            2,
            100,
            "22".repeat(32),
            "http://127.0.0.1:18443",
            AuthenticationMode::Strict,
        );
        config.rpc_headers = vec!["authorization=Basic hidden".to_owned()];
        config.bearer_token = Some("indexer-hidden".to_owned());
        config
    }

    #[test]
    fn bitcoin_programmatic_config_requires_explicit_policy() {
        let config = bitcoin_config();

        assert_eq!(config.confirmation_depth, 2);
        assert_eq!(config.reorg_retention, 100);
        assert_eq!(config.network, BitcoinNetwork::Regtest);
        assert_eq!(config.rpc_timeout_seconds, 15);
        assert_eq!(config.rpc_max_response_bytes, 268_435_456);
        assert_eq!(config.http_bind, SocketAddr::from(([127, 0, 0, 1], 8080)));
    }

    #[test]
    fn bitcoin_service_rejects_invalid_policy_before_runtime_side_effects() {
        let mut config = bitcoin_config();
        config.reorg_retention = 0;

        let error = match BitcoinIndexerService::new(config) {
            Ok(_) => panic!("zero Bitcoin reorg retention must be rejected"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "reorg retention must be greater than zero"
        );
    }

    #[test]
    fn strict_services_require_api_authentication_while_core_auth_is_always_required() {
        let mut strict_ethereum = config();
        strict_ethereum.authentication_mode = AuthenticationMode::Strict;
        let error = match IndexerService::new(strict_ethereum) {
            Ok(_) => panic!("strict Ethereum IX without API authentication must be rejected"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "Indexer Service bearer token is required in strict authentication mode"
        );

        let mut missing_api_auth = bitcoin_config();
        missing_api_auth.bearer_token = None;
        let error = match BitcoinIndexerService::new(missing_api_auth) {
            Ok(_) => panic!("Bitcoin IX without API authentication must be rejected"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "Indexer Service bearer token is required in strict authentication mode"
        );

        let mut missing_core_auth = bitcoin_config();
        missing_core_auth.authentication_mode = AuthenticationMode::GlobalTrusted;
        missing_core_auth.bearer_token = None;
        missing_core_auth.rpc_headers.clear();
        let error = match BitcoinIndexerService::new(missing_core_auth) {
            Ok(_) => panic!("Bitcoin IX without Core authentication must be rejected"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "Bitcoin Core RPC requires an authorization header"
        );
    }

    #[test]
    fn global_trusted_services_accept_missing_api_bearers() {
        let mut ethereum = config();
        ethereum.bearer_token = Some("ignored bearer with whitespace".to_owned());
        IndexerService::new(ethereum)
            .expect("global-trusted Ethereum IX must not require an API bearer");

        let mut bitcoin = bitcoin_config();
        bitcoin.authentication_mode = AuthenticationMode::GlobalTrusted;
        bitcoin.bearer_token = None;
        BitcoinIndexerService::new(bitcoin)
            .expect("global-trusted Bitcoin IX must not require an API bearer");
    }

    #[test]
    fn global_trusted_non_loopback_service_still_requires_upstream_tls() {
        let mut config = config();
        config.http_bind = SocketAddr::from(([0, 0, 0, 0], 8080));

        let error = match IndexerService::new(config) {
            Ok(_) => panic!("non-loopback global-trusted IX must require upstream TLS"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "a non-loopback API bind requires trusted upstream TLS"
        );
    }

    #[test]
    fn strict_bearer_validation_does_not_expose_the_configured_value() {
        let secret = "do-not-print this bearer";
        let mut strict = config();
        strict.authentication_mode = AuthenticationMode::Strict;
        strict.bearer_token = Some(secret.to_owned());

        let error = match IndexerService::new(strict) {
            Ok(_) => panic!("strict IX must reject a malformed bearer"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains(secret));
    }
}
