use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, ChainId};
use deposits::{
    BoxFuture, DepositAddressRequest, DepositAddressSource, DepositIndexerClient, DepositState,
    DepositStore, DepositWatchCoordinator, GeneratedDepositAddress, IdempotencyKey,
    PersistentPaymentRepository, RegisterDeposit, UserId,
};
use indexing::{
    BlockHash, BlockHeight, BlockRef, ConfirmationPolicy, IndexError, IndexErrorKind, IndexScope,
    SyncPhase, SyncStatus, WatchId, WatchReceipt, WatchRequest,
};
use signer::KeyLocator;
use storage_rocksdb::RocksDbStorage;
use tempfile::TempDir;

#[derive(Clone)]
struct Addresses {
    calls: Arc<AtomicUsize>,
}

impl DepositAddressSource for Addresses {
    fn address<'a>(
        &'a self,
        request: DepositAddressRequest,
    ) -> BoxFuture<'a, Result<GeneratedDepositAddress, deposits::DepositError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(GeneratedDepositAddress {
                address: CanonicalAddress {
                    chain: request.scope.chain,
                    value: "0x1111111111111111111111111111111111111111".to_owned(),
                },
                key: KeyLocator::Identifier("deposit-key-1".to_owned()),
            })
        })
    }
}

#[derive(Clone)]
struct Indexer {
    scope: IndexScope,
    watch_attempts: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<WatchRequest>>>,
}

impl DepositIndexerClient for Indexer {
    fn status<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
        Box::pin(async move {
            if scope != &self.scope {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "test scope mismatch",
                    false,
                ));
            }
            Ok(SyncStatus {
                scope: self.scope.clone(),
                checkpoint: Some(BlockRef {
                    height: BlockHeight(42),
                    hash: BlockHash(vec![42; 32]),
                    parent_hash: Some(BlockHash(vec![41; 32])),
                    timestamp: Some(1_000),
                }),
                observed_tip: None,
                confirmation_policy: ConfirmationPolicy {
                    minimum_confirmations: 12,
                    require_chain_finality: false,
                },
                phase: SyncPhase::Ready,
                rebuild_reason: None,
                halted_reason: None,
            })
        })
    }

    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("test request mutex must not be poisoned")
                .push(request.clone());
            let attempt = self.watch_attempts.fetch_add(1, Ordering::AcqRel);
            if attempt == 0 {
                return Err(IndexError::new(
                    IndexErrorKind::Source,
                    "simulated response loss",
                    true,
                ));
            }
            Ok(WatchReceipt {
                id: WatchId("watch-1".to_owned()),
                scope: request.scope,
                selector: request.selector,
                start_height: request.start_height,
                registered_at: None,
                inactive_from: None,
                confirmation_policy: ConfirmationPolicy {
                    minimum_confirmations: 12,
                    require_chain_finality: false,
                },
            })
        })
    }
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("ethereum".to_owned()),
        network: "test".to_owned(),
    }
}

fn command(scope: IndexScope) -> RegisterDeposit {
    RegisterDeposit {
        scope,
        id: deposits::DepositId("deposit-1".to_owned()),
        idempotency_key: IdempotencyKey("request-1".to_owned()),
        user_id: UserId("user-1".to_owned()),
        asset: AssetId {
            chain: ChainId("ethereum".to_owned()),
            asset: "native".to_owned(),
        },
        expected: AtomicAmount([0; 32]),
        expires_at: 2_000,
        created_at: 1_000,
    }
}

#[tokio::test]
async fn response_loss_reuses_durable_address_and_birthday_until_watch_acknowledgement()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let store = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    let configured_scope = scope();
    let indexer = Indexer {
        scope: configured_scope.clone(),
        watch_attempts: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let addresses = Addresses {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let coordinator =
        DepositWatchCoordinator::new(&store, &indexer, &addresses, configured_scope.clone());
    let request = command(configured_scope);

    coordinator
        .register(request.clone())
        .await
        .expect_err("the first IX response is deliberately lost");
    let awaiting = store
        .deposit(&request.id)
        .await?
        .expect("deposit must be durable before IX acknowledgement");
    assert_eq!(awaiting.state, DepositState::AwaitingWatch);
    assert_eq!(awaiting.birthday, BlockHeight(42));

    let active = coordinator.register(request).await?;
    assert_eq!(
        active.state,
        DepositState::Active {
            watch_id: WatchId("watch-1".to_owned())
        }
    );
    assert_eq!(addresses.calls.load(Ordering::Acquire), 1);
    let requests = indexer
        .requests
        .lock()
        .expect("test request mutex must not be poisoned");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].start_height, BlockHeight(42));
    Ok(())
}
