use std::path::PathBuf;

use indexer_worker::{IndexerService, IndexerServiceConfig, PrometheusTelemetry};
use payment_http::AuthenticationMode;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Indexer test started");

    let expected_genesis_hash = std::env::var("IX_EXPECTED_GENESIS_HASH")?;
    let authentication_mode =
        std::env::var("STRICT_AUTHENTICATION_MODE")?.parse::<AuthenticationMode>()?;
    let mut config = IndexerServiceConfig::new(
        PathBuf::from("./tmp/indexer-db"),
        "anvil",
        0,
        31_337,
        expected_genesis_hash,
        "http://127.0.0.1:8545",
        authentication_mode,
    );
    if authentication_mode.is_strict() {
        config.bearer_token = Some(std::env::var("IX_BEARER_TOKEN")?);
    }
    config.confirmation_depth = 3;
    config.reorg_retention = 50;

    let service = IndexerService::new(config)?;
    let telemetry = PrometheusTelemetry::install()?;
    service.run(telemetry).await
}
