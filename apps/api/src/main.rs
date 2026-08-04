//! Payment Service composition root for durable IX handshakes and fact intake.
//!
//! Business projection is intentionally not guessed here. The runtime mirrors
//! IX facts durably, reports the independent projection cursor, and leaves
//! deposit/collection/accounting classification to a configured PS workflow.

mod config;
mod indexer_client;
mod runtime;

use clap::Parser;
use config::{Cli, Command};

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("payment-api failed: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), runtime::RuntimeError> {
    match cli.command {
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
                "ingestion_cursor={} projection_cursor={} pending_sample={} more_pending={} classification_configured=false",
                cursor_text(report.ingestion_cursor),
                cursor_text(report.projection_cursor),
                report.pending_sample,
                report.more_pending
            );
        }
    }
    Ok(())
}

fn cursor_text(cursor: Option<indexing::EventCursor>) -> String {
    cursor.map_or_else(|| "none".to_owned(), |cursor| cursor.0.to_string())
}
