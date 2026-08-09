//! Ethereum Indexer Service composition root.
//!
//! The library facade and the `indexer-worker` binary run the same bounded
//! Ethereum v1 runtime. Embedding changes process placement only: each service
//! instance still exclusively owns one index scope and one RocksDB path.

mod api;
mod config;
mod runtime;
mod service;

use clap::Parser;

pub use service::{
    IndexerService, IndexerServiceConfig, IndexerServiceConfigError, IndexerServiceError,
};
pub use telemetry::PrometheusTelemetry;

/// Parses the `indexer-worker` CLI and executes the selected operation.
///
/// This is public only so the package's thin binary target can delegate to the
/// library composition root. Programmatic callers should use
/// [`IndexerService`] instead.
#[doc(hidden)]
pub async fn run_cli() -> Result<(), IndexerServiceError> {
    match config::Cli::parse().command {
        config::Command::Serve(options) => {
            let service = IndexerService::from_serve_options(options)?;
            let telemetry = PrometheusTelemetry::install()?;
            service.run(telemetry).await
        }
        config::Command::Backup(options) => runtime::backup(options).await,
        config::Command::Migrate(options) => runtime::migrate(options).await,
        config::Command::Rebuild(options) => runtime::rebuild(options).await,
        config::Command::RebuildAbort(options) => runtime::abort_rebuild(options).await,
        config::Command::Cleanup(options) => runtime::cleanup(options).await,
    }
}
