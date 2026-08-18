//! Bitcoin and Ethereum Indexer Service composition root.
//!
//! The library facades and the `indexer-worker` binary run the same bounded,
//! chain-specific runtime. Embedding changes process placement only: each
//! service instance still exclusively owns one index scope and one RocksDB
//! path.

mod api;
mod config;
mod runtime;
mod service;

use clap::Parser;

pub use http::server::AuthenticationMode;
pub use service::{
    BitcoinConfig, BitcoinService, ConfigError, EthereumConfig, EthereumService, ServiceError,
};

/// Parses the `indexer-worker` CLI and executes the selected operation.
///
/// This is public only so the package's thin binary target can delegate to the
/// library composition root. Programmatic callers should use
/// [`EthereumService`] or [`BitcoinService`] instead.
#[doc(hidden)]
pub async fn run_cli() -> Result<(), ServiceError> {
    match config::Cli::parse().command {
        config::Command::Serve(options) => {
            let service = EthereumService::from_serve_options(options)?;
            service.run().await
        }
        config::Command::Backup(options) => runtime::backup(options).await,
        config::Command::Rebuild(options) => runtime::rebuild(options).await,
        config::Command::RebuildAbort(options) => runtime::abort_rebuild(options).await,
        config::Command::Cleanup(options) => runtime::cleanup(options).await,
        config::Command::Bitcoin(options) => match options.command {
            config::BitcoinCommand::Serve(options) => {
                let service = BitcoinService::from_serve_options(options)?;
                service.run().await
            }
            config::BitcoinCommand::Backup(options) => runtime::backup(options).await,
            config::BitcoinCommand::Rebuild(options) => runtime::rebuild_bitcoin(options).await,
            config::BitcoinCommand::RebuildAbort(options) => {
                runtime::abort_bitcoin_rebuild(options).await
            }
            config::BitcoinCommand::Cleanup(options) => runtime::cleanup_bitcoin(options).await,
        },
    }
}
