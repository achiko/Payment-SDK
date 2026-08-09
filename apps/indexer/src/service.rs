use std::{error::Error, future::Future, net::SocketAddr, path::PathBuf};

use crate::{
    config::{ConfigError, DatabaseOptions, RepositoryOptions, ServeOptions, SourceOptions},
    runtime,
};
use telemetry::PrometheusTelemetry;

const DEFAULT_CONFIRMATION_DEPTH: u64 = 12;
const DEFAULT_REORG_RETENTION: u64 = 50;
const DEFAULT_RPC_TIMEOUT_SECONDS: u64 = 15;
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
}
