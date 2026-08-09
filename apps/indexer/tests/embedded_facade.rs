use indexer_worker::{IndexerService, IndexerServiceConfig};

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
fn external_application_can_import_and_configure_the_facade() {
    let mut config = config();
    config.confirmation_depth = 1;

    let service = IndexerService::new(config).expect("test configuration must be valid");
    assert_send(service);
}

fn assert_send<T: Send>(_value: T) {}
