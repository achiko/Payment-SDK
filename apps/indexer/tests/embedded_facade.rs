use chain_bitcoin::BitcoinNetwork;
use indexer_worker::{
    BitcoinIndexerService, BitcoinIndexerServiceConfig, IndexerService, IndexerServiceConfig,
};

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
fn bitcoin_facade_is_constructible_without_runtime_side_effects() {
    let mut config = BitcoinIndexerServiceConfig::new(
        "bitcoin-indexer.db",
        BitcoinNetwork::Regtest,
        0,
        2,
        100,
        "22".repeat(32),
        "http://127.0.0.1:18443",
    );
    config.rpc_headers = vec!["authorization=Basic hidden".to_owned()];
    config.bearer_token = Some("indexer-hidden".to_owned());

    let _service = BitcoinIndexerService::new(config)
        .expect("valid Bitcoin facade config must construct without runtime effects");
}

#[test]
fn external_application_can_import_and_configure_the_facade() {
    let mut config = config();
    config.confirmation_depth = 1;

    let service = IndexerService::new(config).expect("test configuration must be valid");
    assert_send(service);
}

fn assert_send<T: Send>(_value: T) {}
