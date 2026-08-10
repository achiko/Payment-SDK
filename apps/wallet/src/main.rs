//! WS composition root: authenticated stateless Ethereum or Bitcoin operations.

mod config;
mod runtime;

use clap::Parser;
use config::{BitcoinCommand, Cli, Command};
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
        Command::Bitcoin(options) => match options.command {
            BitcoinCommand::Serve(options) => runtime::serve_bitcoin(options).await,
        },
    }
}
