use chain_bitcoin::{Block, BlockInterpreter, Network};
use indexer_worker::{
    AuthenticationMode, BitcoinConfig, BitcoinService, EthereumConfig, EthereumService,
};
use indexing::{
    BlockHash, BlockHeight, BlockRef, BlockSource, CanonicalAddress, ChainId, Checkpoint,
    ConfirmationPolicy, IndexScope, SourceError, WatchRequest, WatchSelector, Watcher,
};

fn config() -> EthereumConfig {
    EthereumConfig::new(
        "indexer.db",
        "anvil",
        0,
        31_337,
        format!("0x{}", "11".repeat(32)),
        "http://127.0.0.1:8545",
        AuthenticationMode::GlobalTrusted,
    )
}

#[test]
fn bitcoin_facade_is_constructible_without_runtime_side_effects() {
    let mut config = BitcoinConfig::new(
        "bitcoin-indexer.db",
        Network::Regtest,
        0,
        2,
        100,
        "22".repeat(32),
        "http://127.0.0.1:18443",
        AuthenticationMode::Strict,
    );
    config.rpc_headers = vec!["authorization=Basic hidden".to_owned()];
    config.bearer_token = Some("indexer-hidden".to_owned());

    let _service = BitcoinService::new(config)
        .expect("valid Bitcoin facade config must construct without runtime effects");
}

#[test]
fn external_application_can_import_and_configure_the_facade() {
    let mut config = config();
    config.confirmation_depth = 1;

    let service = EthereumService::new(config).expect("test configuration must be valid");
    assert_send(service);
}

fn assert_send<T: Send>(_value: T) {}

#[derive(Clone)]
enum OfflineSource {
    Fixture,
}

impl BlockSource for OfflineSource {
    type Block = Block;

    fn tip<'a>(&'a self) -> indexing::BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async { Err(offline()) })
    }

    fn block_at<'a>(
        &'a self,
        _height: BlockHeight,
    ) -> indexing::BoxFuture<'a, Result<Block, SourceError>> {
        Box::pin(async { Err(offline()) })
    }

    fn canonical_hash<'a>(
        &'a self,
        _height: BlockHeight,
    ) -> indexing::BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
        Box::pin(async { Err(offline()) })
    }
}

fn offline() -> SourceError {
    SourceError {
        message: "offline fixture must not be queried".to_owned(),
        retryable: false,
    }
}

#[tokio::test]
async fn embedded_bitcoin_exposes_only_the_consumer_handle() {
    let directory = tempfile::tempdir().expect("temporary index directory must be created");
    let scope = IndexScope {
        chain: ChainId(chain_bitcoin::CHAIN.to_owned()),
        network: "regtest".to_owned(),
    };
    let config = indexing_rocksdb::Config::new(
        scope.clone(),
        BlockHeight(0),
        ConfirmationPolicy {
            minimum_confirmations: 1,
            require_chain_finality: false,
        },
        100,
    )
    .expect("embedded index configuration must be valid");
    let interpreter = BlockInterpreter::new(scope.clone(), Network::Regtest)
        .expect("Bitcoin interpreter must accept its canonical scope");
    let runtime = indexing_rocksdb::Runtime::open(
        directory.path(),
        config,
        OfflineSource::Fixture,
        interpreter,
    )
    .expect("embedded index storage must open");
    let indexer = runtime.handle();

    let receipt = indexer
        .watch(WatchRequest {
            scope: scope.clone(),
            selector: WatchSelector::Address(CanonicalAddress {
                scope: scope.clone(),
                value: "bcrt1qembeddedfixture".to_owned(),
            }),
            start_height: BlockHeight(0),
            idempotency_key: "embedded-watch".to_owned(),
        })
        .await
        .expect("embedded handle must persist a watch without accessing RPC");

    assert_eq!(receipt.scope, scope);
    assert_eq!(indexer.checkpoint(&receipt.scope).await, Ok(None));
}
