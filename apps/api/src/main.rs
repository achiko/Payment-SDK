//! Payment Service composition root for durable IX handshakes and fact intake.
//!
//! Business projection is intentionally not guessed here. The runtime mirrors
//! IX facts durably, reports the independent projection cursor, and leaves
//! deposit/collection/accounting classification to a configured PS workflow.

mod active_policy;
mod api;
mod api_error;
mod auth;
mod bitcoin_collection_executor;
mod bitcoin_fee_allocation;
mod bitcoin_policy;
mod bitcoin_wallet_client;
mod collection_executor;
mod commands;
mod config;
mod ids;
mod indexer_client;
mod policy;
mod runtime;
pub mod wallet_client;

use clap::Parser;
use config::{BitcoinCommand, Cli, Command};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if let Err(error) = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .try_init()
    {
        eprintln!("payment-api telemetry initialization failed: {error}");
        std::process::exit(1);
    }
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("payment-api failed: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), runtime::RuntimeError> {
    match cli.command {
        Command::Serve(options) => runtime::serve(options).await?,
        Command::Backup(options) => runtime::backup(options).await?,
        Command::Migrate(options) => runtime::migrate(options).await?,
        Command::ReconcileWatches(options) => {
            let report = runtime::reconcile_watches(&options).await?;
            println!(
                "reconcile_batches={} activated={} bounded_work_remaining={}",
                report.batches, report.activated, report.exhausted
            );
        }
        Command::IngestEvents(options) => {
            let report = runtime::ingest_events(&options).await?;
            println!(
                "ingestion_pages={} appended={} duplicates={} cursor={} bounded_work_remaining={}",
                report.pages,
                report.appended,
                report.duplicates,
                cursor_text(report.checkpoint),
                report.exhausted
            );
        }
        Command::ProjectionStatus(options) => {
            let report = runtime::projection_status(&options).await?;
            println!(
                "ingestion_cursor={} projection_cursor={} pending_sample={} more_pending={} classification_configured=true",
                cursor_text(report.ingestion_cursor),
                cursor_text(report.projection_cursor),
                report.pending_sample,
                report.more_pending
            );
        }
        Command::Bitcoin(options) => match options.command {
            BitcoinCommand::Serve(options) => runtime::serve_bitcoin(options).await?,
            BitcoinCommand::Backup(options) => runtime::backup(options).await?,
            BitcoinCommand::Migrate(options) => runtime::migrate_bitcoin(options).await?,
            BitcoinCommand::ReconcileWatches(options) => {
                let report = runtime::reconcile_bitcoin_watches(&options).await?;
                println!(
                    "reconcile_batches={} activated={} bounded_work_remaining={}",
                    report.batches, report.activated, report.exhausted
                );
            }
            BitcoinCommand::IngestEvents(options) => {
                let report = runtime::ingest_bitcoin_events(&options).await?;
                println!(
                    "ingestion_pages={} appended={} duplicates={} cursor={} bounded_work_remaining={}",
                    report.pages,
                    report.appended,
                    report.duplicates,
                    cursor_text(report.checkpoint),
                    report.exhausted
                );
            }
            BitcoinCommand::ProjectionStatus(options) => {
                let report = runtime::projection_status(&options).await?;
                println!(
                    "ingestion_cursor={} projection_cursor={} pending_sample={} more_pending={} classification_configured=true",
                    cursor_text(report.ingestion_cursor),
                    cursor_text(report.projection_cursor),
                    report.pending_sample,
                    report.more_pending
                );
            }
        },
    }
    Ok(())
}

fn cursor_text(cursor: Option<indexing::EventCursor>) -> String {
    cursor.map_or_else(|| "none".to_owned(), |cursor| cursor.0.to_string())
}
