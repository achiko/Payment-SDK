use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use base::Decimal;
use deposits::KeyId;
use deposits::{
    AddressRequest, BoxFuture, DepositAddressSource, DepositReader, DepositRegistration,
    DepositState, IdempotencyKey, PaymentStore, ProvisionedAddress, UserId, WatchCoordinator,
};
use indexing::{AssetId, CanonicalAddress, ChainId};
use indexing::{
    BlockHash, BlockHeight, BlockRef, Checkpoint, ConfirmationPolicy, IndexError, IndexErrorKind,
    IndexScope, UnwatchOutcome, UnwatchRequest, WatchId, WatchReceipt, WatchRequest, Watcher,
};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;

#[derive(Clone)]
struct Addresses {
    calls: Arc<AtomicUsize>,
}

impl DepositAddressSource for Addresses {
    fn address<'a>(
        &'a self,
        request: AddressRequest,
    ) -> BoxFuture<'a, Result<ProvisionedAddress, deposits::DepositError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(ProvisionedAddress {
                address: CanonicalAddress {
                    scope: request.scope,
                    value: format!("address-{}", request.candidate),
                },
                key: KeyId::Identifier(format!("deposit-key-{}", request.candidate)),
                key_purpose: format!("deposit-purpose-{}", request.candidate),
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

impl Checkpoint for Indexer {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            if scope != &self.scope {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "test scope mismatch",
                    false,
                ));
            }
            Ok(Some(BlockRef {
                height: BlockHeight(42),
                hash: BlockHash(vec![42; 32]),
                parent_hash: Some(BlockHash(vec![41; 32])),
                timestamp: Some(1_000),
            }))
        })
    }
}

impl Watcher for Indexer {
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

    fn unwatch<'a>(
        &'a self,
        _: UnwatchRequest,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Box::pin(async { unreachable!("deposit fixture never removes watches") })
    }
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("chain-a".to_owned()),
        network: "test".to_owned(),
    }
}

fn command(scope: IndexScope) -> DepositRegistration {
    DepositRegistration {
        scope,
        id: deposits::DepositId("deposit-1".to_owned()),
        idempotency_key: IdempotencyKey("request-1".to_owned()),
        user_id: UserId("user-1".to_owned()),
        asset: AssetId {
            chain: ChainId("chain-a".to_owned()),
            asset: "native".to_owned(),
        },
        expected: Decimal::zero(),
        expires_at: 2_000,
        created_at: 1_000,
    }
}

#[tokio::test]
async fn response_loss_reuses_durable_address_and_birthday_until_watch_acknowledgement()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let store = PaymentStore::new(RocksDb::open(directory.path())?);
    let configured_scope = scope();
    let indexer = Indexer {
        scope: configured_scope.clone(),
        watch_attempts: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let addresses = Addresses {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let coordinator = WatchCoordinator::new(&store, &indexer, &addresses, configured_scope.clone());
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
    assert_eq!(awaiting.key_purpose, "deposit-purpose-0");

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
    assert_eq!(requests[0].idempotency_key, "ps-deposit:deposit-1");
    assert!(!requests[0].idempotency_key.contains("request-1"));
    Ok(())
}

#[tokio::test]
async fn independent_deposits_receive_distinct_deterministic_candidates()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let store = PaymentStore::new(RocksDb::open(directory.path())?);
    let configured_scope = scope();
    let indexer = Indexer {
        scope: configured_scope.clone(),
        watch_attempts: Arc::new(AtomicUsize::new(1)),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let addresses = Addresses {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let coordinator = WatchCoordinator::new(&store, &indexer, &addresses, configured_scope.clone());

    let first = coordinator
        .register(command(configured_scope.clone()))
        .await?;
    let mut second_command = command(configured_scope);
    second_command.id = deposits::DepositId("deposit-2".to_owned());
    second_command.idempotency_key = IdempotencyKey("request-2".to_owned());
    second_command.user_id = UserId("user-2".to_owned());
    let second = coordinator.register(second_command.clone()).await?;

    assert_eq!(first.address.value, "address-0");
    assert_eq!(second.address.value, "address-1");
    assert_ne!(first.key, second.key);
    assert_eq!(second.key_purpose, "deposit-purpose-1");
    assert_eq!(coordinator.register(second_command).await?, second);
    assert_eq!(addresses.calls.load(Ordering::Acquire), 3);
    Ok(())
}
