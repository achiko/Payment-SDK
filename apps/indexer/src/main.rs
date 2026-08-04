//! IX composition root: Ethereum source, durable repository, workers, and API.

mod api;
mod config;
mod runtime;

use clap::Parser;
use config::{Cli, Command};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .try_init()?;

    match Cli::parse().command {
        Command::Serve(options) => runtime::serve(options).await,
        Command::Backup(options) => runtime::backup(options).await,
        Command::Migrate(options) => runtime::migrate(options).await,
        Command::Rebuild(options) => runtime::rebuild(options).await,
        Command::RebuildAbort(options) => runtime::abort_rebuild(options).await,
        Command::Cleanup(options) => runtime::cleanup(options).await,
    }
}
