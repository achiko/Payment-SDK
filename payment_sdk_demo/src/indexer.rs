use std::path::PathBuf;

use indexer_worker::{IndexerService, IndexerServiceConfig, PrometheusTelemetry};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Indexer test started");

    let expected_genesis_hash = std::env::var("IX_EXPECTED_GENESIS_HASH")?;
    let mut config = IndexerServiceConfig::new(
        PathBuf::from("./tmp/indexer-db"),
        "anvil",
        0,
        31_337,
        expected_genesis_hash,
        "http://127.0.0.1:8545",
    );
    config.confirmation_depth = 3;
    config.reorg_retention = 50;

    let service = IndexerService::new(config)?;
    let telemetry = PrometheusTelemetry::install()?;
    service.run(telemetry).await
}
