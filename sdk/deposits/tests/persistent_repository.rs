use std::{
    future::poll_fn,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Poll, Waker},
};

use base::Decimal;
use deposits::KeyId;
use deposits::{
    AccountingCommand, ActionGuard, AppendObservation, ApplyResult, AwaitingQuery, CaseId,
    CaseOpener, CaseReader, CaseResolver, CloseDeposit, CollectionId, CommandIdentity,
    CommandOperation, CommandPrincipal, ConsumerCheckpointName, DepositBalances, DepositCreator,
    DepositErrorKind, DepositFilter, DepositId, DepositLifecycle, DepositPlan, DepositQuery,
    DepositReader, DepositState, DepositStateKind, EntryId, EventProjector, EventReader,
    EventWriter, IdempotencyKey, IndexRebuilder, LedgerEffect, LedgerEntryCause, LedgerReader,
    LedgerWriter, MirrorObservation, MirrorOutcome, MirroredObservation, OpenDeposit, PaymentStore,
    ProgressReader, ProjectObservation, ProjectionFeeTreatment, ProjectionId, RebuildRequest,
    ReconciliationCase, ReconciliationDecision, ReconciliationReason, ReconciliationState,
    RecordObservation, RequestHash, ResolveReconciliation, UserId, WatchQueue,
};
use indexing::{AssetId, CanonicalAddress, ChainId, TransactionRef};
use indexing::{
    BlockHash, BlockHeight, BlockRef, ConfirmationProof, EventCursor, EventId, IndexScope,
    MovementId, NetworkFee, ObservationEvent, ObservationRevision, ObservedTransaction,
    TransactionStatus, ValueMovement, WatchId,
};
use storage::{
    BoxFuture as StorageFuture, CommitResult, Error, Key, Namespace, ScanPage, ScanRequest, Store,
    StoredValue, WriteBatch,
};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;

#[derive(Clone)]
struct CommitBarrierStorage<S> {
    inner: S,
    synchronize_commits: Arc<AtomicBool>,
    barrier: Arc<TwoPartyBarrier>,
}

impl<S> CommitBarrierStorage<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            synchronize_commits: Arc::new(AtomicBool::new(false)),
            barrier: Arc::new(TwoPartyBarrier::default()),
        }
    }

    fn enable(&self) {
        self.synchronize_commits.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct TwoPartyBarrier {
    state: Mutex<BarrierState>,
}

#[derive(Default)]
struct BarrierState {
    arrivals: usize,
    open: bool,
    waiters: Vec<Waker>,
}

impl TwoPartyBarrier {
    async fn wait(&self) {
        let mut registered = false;
        poll_fn(|context| {
            let mut state = self
                .state
                .lock()
                .expect("test barrier mutex must not be poisoned");
            if state.open {
                return Poll::Ready(());
            }
            if !registered {
                registered = true;
                state.arrivals += 1;
            }
            if state.arrivals == 2 {
                state.open = true;
                let waiters = std::mem::take(&mut state.waiters);
                drop(state);
                for waiter in waiters {
                    waiter.wake();
                }
                Poll::Ready(())
            } else {
                state.waiters.push(context.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }
}

impl<S> Store for CommitBarrierStorage<S>
where
    S: Store,
{
    fn get<'a>(
        &'a self,
        namespace: &'a Namespace,
        key: &'a Key,
    ) -> StorageFuture<'a, Result<Option<StoredValue>, Error>> {
        self.inner.get(namespace, key)
    }

    fn scan<'a>(&'a self, request: ScanRequest) -> StorageFuture<'a, Result<ScanPage, Error>> {
        self.inner.scan(request)
    }

    fn commit<'a>(&'a self, batch: WriteBatch) -> StorageFuture<'a, Result<CommitResult, Error>> {
        Box::pin(async move {
            if self.synchronize_commits.load(Ordering::Acquire) {
                self.barrier.wait().await;
            }
            self.inner.commit(batch).await
        })
    }
}

fn amount(value: u64) -> Decimal {
    Decimal::from(value)
}

fn chain() -> ChainId {
    ChainId("chain-a".to_owned())
}

fn asset() -> AssetId {
    AssetId {
        chain: chain(),
        asset: "native".to_owned(),
    }
}

fn address(value: &str) -> CanonicalAddress {
    CanonicalAddress {
        scope: IndexScope {
            chain: chain(),
            network: "test".to_owned(),
        },
        value: value.to_owned(),
    }
}

fn create_deposit() -> OpenDeposit {
    OpenDeposit {
        deposit: DepositPlan {
            id: DepositId("deposit-1".to_owned()),
            idempotency_key: IdempotencyKey("create-1".to_owned()),
            user_id: UserId("user-1".to_owned()),
            asset: asset(),
            address: address("0x1111111111111111111111111111111111111111"),
            key: KeyId::Identifier("deposit-key-1".to_owned()),
            key_purpose: "payment-service-deposit-address-v1".to_owned(),
            expected: amount(100),
            birthday: BlockHeight(10),
            expires_at: 10_000,
            created_at: 1_000,
        },
        ledger_recorded_at: 1_000,
    }
}

fn create_deposit_named(id: &str, user_id: &str, address_suffix: u8) -> OpenDeposit {
    let mut command = create_deposit();
    command.deposit.id = DepositId(id.to_owned());
    command.deposit.idempotency_key = IdempotencyKey(format!("create-{id}"));
    command.deposit.user_id = UserId(user_id.to_owned());
    command.deposit.address = address(&format!("0x{address_suffix:040x}"));
    command.deposit.key = KeyId::Identifier(format!("key-{id}"));
    command
}

#[tokio::test]
async fn identical_address_text_on_distinct_networks_has_distinct_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let first = create_deposit_named("deposit-network-a", "user-1", 7);
    let mut second = create_deposit_named("deposit-network-b", "user-1", 7);
    second.deposit.address.scope.network = "other-testnet".to_owned();

    let first_created = repository.create_with_ledger(first.clone()).await?;
    let second_created = repository.create_with_ledger(second.clone()).await?;

    assert_eq!(
        repository.by_address(&first.deposit.address).await?,
        Some(first_created.deposit)
    );
    assert_eq!(
        repository.by_address(&second.deposit.address).await?,
        Some(second_created.deposit)
    );
    Ok(())
}

fn block(height: u64, hash: u8) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(vec![hash; 32]),
        parent_hash: Some(BlockHash(vec![hash.saturating_sub(1); 32])),
        timestamp: Some(1_000 + height),
    }
}

fn movement(id: &str, value: u64) -> ValueMovement {
    ValueMovement::Transfer {
        id: MovementId(id.to_owned()),
        asset: asset(),
        amount: amount(value),
        from: address("0x2222222222222222222222222222222222222222"),
        to: address("0x1111111111111111111111111111111111111111"),
    }
}

fn outgoing_movement(id: &str, value: u64) -> ValueMovement {
    ValueMovement::Transfer {
        id: MovementId(id.to_owned()),
        asset: asset(),
        amount: amount(value),
        from: address("0x1111111111111111111111111111111111111111"),
        to: address("0x2222222222222222222222222222222222222222"),
    }
}

fn network_fee(value: u64, payer: CanonicalAddress, fee_asset: AssetId) -> NetworkFee {
    NetworkFee {
        asset: fee_asset,
        amount: amount(value),
        payer: Some(payer),
    }
}

fn confirmed_observation() -> MirroredObservation {
    MirroredObservation {
        event: ObservationEvent {
            id: EventId("event-confirmed-1".to_owned()),
            cursor: EventCursor(1),
            watch_ids: vec![WatchId("watch-1".to_owned())],
            previous_status: None,
            transaction: ObservedTransaction {
                scope: IndexScope {
                    chain: chain(),
                    network: "test".to_owned(),
                },
                transaction_id: TransactionRef {
                    scope: IndexScope {
                        chain: chain(),
                        network: "test".to_owned(),
                    },
                    value: "0xtransaction".to_owned(),
                },
                revision: ObservationRevision(1),
                status: TransactionStatus::Confirmed {
                    block: block(20, 20),
                    proof: ConfirmationProof::Depth {
                        required: 12,
                        observed: 12,
                    },
                },
                movements: vec![movement("movement-1", 100)],
                fee: None,
                first_seen_at: 1_020,
                observed_at: 1_025,
            },
        },
        received_at: 1_026,
    }
}

fn reorg_observation_at(cursor: u64) -> MirroredObservation {
    let previous_block = block(20, 20);
    MirroredObservation {
        event: ObservationEvent {
            id: EventId("event-reorg-1".to_owned()),
            cursor: EventCursor(cursor),
            watch_ids: vec![WatchId("watch-1".to_owned())],
            previous_status: Some(TransactionStatus::Confirmed {
                block: previous_block.clone(),
                proof: ConfirmationProof::Depth {
                    required: 12,
                    observed: 12,
                },
            }),
            transaction: ObservedTransaction {
                scope: IndexScope {
                    chain: chain(),
                    network: "test".to_owned(),
                },
                transaction_id: TransactionRef {
                    scope: IndexScope {
                        chain: chain(),
                        network: "test".to_owned(),
                    },
                    value: "0xtransaction".to_owned(),
                },
                revision: ObservationRevision(2),
                status: TransactionStatus::Reorged { previous_block },
                movements: vec![movement("movement-1", 100)],
                fee: None,
                first_seen_at: 1_020,
                observed_at: 1_030,
            },
        },
        received_at: 1_031,
    }
}

fn reorg_observation() -> MirroredObservation {
    reorg_observation_at(1)
}

fn accounting_identity(key: &str, hash: u8) -> CommandIdentity {
    CommandIdentity {
        principal: CommandPrincipal("administrator".to_owned()),
        operation: CommandOperation::Accounting,
        client_key: IdempotencyKey(key.to_owned()),
        request_hash: RequestHash([hash; 32]),
    }
}

fn reconciliation_identity(key: &str, hash: u8) -> CommandIdentity {
    CommandIdentity {
        principal: CommandPrincipal("administrator".to_owned()),
        operation: CommandOperation::ResolveReconciliation,
        client_key: IdempotencyKey(key.to_owned()),
        request_hash: RequestHash([hash; 32]),
    }
}

fn reconciliation_case(
    id: &str,
    deposit_id: DepositId,
    triggering_event_id: &str,
) -> ReconciliationCase {
    ReconciliationCase {
        id: CaseId(id.to_owned()),
        deposit_id,
        triggering_event_id: EventId(triggering_event_id.to_owned()),
        reason: ReconciliationReason::PostCreditReorg {
            accounted: amount(1),
            corrected_confirmed: amount(0),
        },
        state: ReconciliationState::Open,
        created_at: 1_030,
    }
}

fn observation_revision(
    event_id: &str,
    cursor: u64,
    transaction_id: &str,
    revision: u64,
    status: TransactionStatus,
    previous_status: Option<TransactionStatus>,
    movements: Vec<ValueMovement>,
) -> MirroredObservation {
    observation_revision_with_fee(
        event_id,
        cursor,
        transaction_id,
        revision,
        status,
        previous_status,
        movements,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn observation_revision_with_fee(
    event_id: &str,
    cursor: u64,
    transaction_id: &str,
    revision: u64,
    status: TransactionStatus,
    previous_status: Option<TransactionStatus>,
    movements: Vec<ValueMovement>,
    fee: Option<NetworkFee>,
) -> MirroredObservation {
    MirroredObservation {
        event: ObservationEvent {
            id: EventId(event_id.to_owned()),
            cursor: EventCursor(cursor),
            watch_ids: vec![WatchId("watch-1".to_owned())],
            previous_status,
            transaction: ObservedTransaction {
                scope: IndexScope {
                    chain: chain(),
                    network: "test".to_owned(),
                },
                transaction_id: TransactionRef {
                    scope: IndexScope {
                        chain: chain(),
                        network: "test".to_owned(),
                    },
                    value: transaction_id.to_owned(),
                },
                revision: ObservationRevision(revision),
                status,
                movements,
                fee,
                first_seen_at: 2_000,
                observed_at: 2_000 + revision,
            },
        },
        received_at: 2_010 + revision,
    }
}

fn included_status() -> TransactionStatus {
    TransactionStatus::Included {
        block: block(30, 30),
        confirmations: 1,
    }
}

fn confirmed_status() -> TransactionStatus {
    TransactionStatus::Confirmed {
        block: block(30, 30),
        proof: ConfirmationProof::Depth {
            required: 12,
            observed: 12,
        },
    }
}

#[tokio::test]
async fn deposit_creation_is_atomic_idempotent_and_activates_exactly_one_watch()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let command = create_deposit();

    let created = repository.create_with_ledger(command.clone()).await?;
    assert_eq!(created.deposit.state, DepositState::AwaitingWatch);
    assert_eq!(created.ledger.balances, DepositBalances::default());
    assert_eq!(
        repository.current(&created.deposit.id).await?,
        Some(created.ledger.clone())
    );

    let retry = repository.create_with_ledger(command.clone()).await?;
    assert_eq!(retry, created);
    let awaiting = repository
        .awaiting_watch(AwaitingQuery {
            after: None,
            limit: 10,
        })
        .await?;
    assert_eq!(awaiting.deposits, vec![created.deposit.clone()]);
    assert_eq!(awaiting.next, None);

    let mut conflicting = command.clone();
    conflicting.deposit.id = DepositId("deposit-2".to_owned());
    conflicting.deposit.address = address("0x2222222222222222222222222222222222222222");
    let error = repository
        .create_with_ledger(conflicting)
        .await
        .expect_err("an idempotency key cannot create a different deposit");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    assert_eq!(
        repository
            .deposit(&DepositId("deposit-2".to_owned()))
            .await?,
        None
    );
    assert_eq!(
        repository
            .current(&DepositId("deposit-2".to_owned()))
            .await?,
        None
    );

    let watch_id = WatchId("watch-1".to_owned());
    let active = repository
        .activate_watch(
            &created.deposit.id,
            &command.deposit.idempotency_key,
            watch_id.clone(),
        )
        .await?;
    assert_eq!(
        active.state,
        DepositState::Active {
            watch_id: watch_id.clone()
        }
    );
    assert_eq!(
        repository
            .activate_watch(
                &created.deposit.id,
                &command.deposit.idempotency_key,
                watch_id,
            )
            .await?,
        active
    );
    let different_watch = repository
        .activate_watch(
            &created.deposit.id,
            &command.deposit.idempotency_key,
            WatchId("watch-2".to_owned()),
        )
        .await
        .expect_err("an active deposit cannot be rebound to another IX watch");
    assert_eq!(different_watch.kind, DepositErrorKind::Conflict);
    assert!(
        repository
            .awaiting_watch(AwaitingQuery {
                after: None,
                limit: 10,
            })
            .await?
            .deposits
            .is_empty()
    );
    assert_eq!(
        repository.current(&created.deposit.id).await?,
        Some(created.ledger)
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_watch_activation_converges_on_the_same_acknowledgement()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let storage = CommitBarrierStorage::new(RocksDb::open(directory.path())?);
    let repository = PaymentStore::new(storage.clone());
    let command = create_deposit();
    let created = repository.create_with_ledger(command.clone()).await?;
    let watch_id = WatchId("watch-race".to_owned());

    storage.enable();
    let first = repository.activate_watch(
        &created.deposit.id,
        &command.deposit.idempotency_key,
        watch_id.clone(),
    );
    let second = repository.activate_watch(
        &created.deposit.id,
        &command.deposit.idempotency_key,
        watch_id.clone(),
    );
    let (first, second) = tokio::join!(first, second);

    let expected = DepositState::Active {
        watch_id: watch_id.clone(),
    };
    assert_eq!(first?.state, expected);
    assert_eq!(second?.state, expected);
    assert_eq!(
        repository
            .deposit(&created.deposit.id)
            .await?
            .expect("concurrent activation must preserve the deposit")
            .state,
        DepositState::Active { watch_id }
    );
    Ok(())
}

#[tokio::test]
async fn close_and_incoming_projection_cannot_both_commit_from_the_same_zero_head()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let storage = CommitBarrierStorage::new(RocksDb::open(directory.path())?);
    let repository = PaymentStore::new(storage.clone());
    let command = create_deposit();
    let created = repository.create_with_ledger(command.clone()).await?;
    let active = repository
        .activate_watch(
            &created.deposit.id,
            &command.deposit.idempotency_key,
            WatchId("watch-close-projection-race".to_owned()),
        )
        .await?;
    let bypass = repository
        .set_state(&active.id, DepositState::Closed)
        .await
        .expect_err("generic lifecycle changes must not bypass guarded close");
    assert_eq!(bypass.kind, DepositErrorKind::InvalidState);
    let observation = observation_revision(
        "event-close-projection-race",
        24,
        "0xcloseprojectionrace",
        1,
        included_status(),
        None,
        vec![movement("close-projection-race", 10)],
    );
    repository
        .append(AppendObservation {
            observation: observation.clone(),
        })
        .await?;

    storage.enable();
    let close = repository.close(CloseDeposit {
        deposit_id: active.id.clone(),
        expected_state: active.state,
        expected_ledger_head: created.ledger.id.clone(),
    });
    let projection = repository.record_observation(RecordObservation {
        event_id: observation.event.id,
        effect: LedgerEffect::Incoming {
            movements: vec![MovementId("close-projection-race".to_owned())],
        },
        deposit_id: active.id.clone(),
        expected_head: Some(created.ledger.id),
        recorded_at: 4_100,
    });
    let (close, projection) = tokio::join!(close, projection);

    assert_ne!(close.is_ok(), projection.is_ok());
    let durable_deposit = repository
        .deposit(&active.id)
        .await?
        .expect("racing close must preserve the deposit");
    let durable_head = repository
        .current(&active.id)
        .await?
        .expect("racing close must preserve the ledger head");
    if close.is_ok() {
        assert_eq!(durable_deposit.state, DepositState::Closed);
        assert_eq!(durable_head.balances.balance, Decimal::zero());
        assert_eq!(
            projection
                .expect_err("projection must lose when close commits")
                .kind,
            DepositErrorKind::Conflict
        );
    } else {
        assert!(matches!(durable_deposit.state, DepositState::Active { .. }));
        assert_eq!(durable_head.balances.balance, amount(10));
        assert_eq!(
            close
                .expect_err("close must lose when projection commits")
                .kind,
            DepositErrorKind::Conflict
        );
    }
    Ok(())
}

#[tokio::test]
async fn closed_deposit_keeps_projecting_late_payments() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let command = create_deposit();
    let created = repository.create_with_ledger(command.clone()).await?;
    let active = repository
        .activate_watch(
            &created.deposit.id,
            &command.deposit.idempotency_key,
            WatchId("watch-retained-after-close".to_owned()),
        )
        .await?;
    repository
        .close(CloseDeposit {
            deposit_id: active.id.clone(),
            expected_state: active.state,
            expected_ledger_head: created.ledger.id.clone(),
        })
        .await?;

    let observation = observation_revision(
        "event-late-after-close",
        25,
        "0xlateafterclose",
        1,
        included_status(),
        None,
        vec![movement("late-after-close", 10)],
    );
    repository
        .append(AppendObservation {
            observation: observation.clone(),
        })
        .await?;
    repository
        .record_observation(RecordObservation {
            event_id: observation.event.id,
            effect: LedgerEffect::Incoming {
                movements: vec![MovementId("late-after-close".to_owned())],
            },
            deposit_id: active.id.clone(),
            expected_head: Some(created.ledger.id),
            recorded_at: 4_200,
        })
        .await?;

    assert_eq!(
        repository
            .deposit(&active.id)
            .await?
            .expect("closed deposit must remain durable")
            .state,
        DepositState::Closed
    );
    assert_eq!(
        repository
            .current(&active.id)
            .await?
            .expect("late payment must append a ledger row")
            .balances
            .balance,
        amount(10)
    );
    Ok(())
}

#[tokio::test]
async fn deposit_listing_rebuilds_indexes_and_expiration_preserves_the_watch()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    for command in [
        create_deposit_named("deposit-1", "user-1", 1),
        create_deposit_named("deposit-2", "user-1", 2),
        create_deposit_named("deposit-3", "user-2", 3),
        create_deposit_named("deposit-4", "user-2", 4),
    ] {
        repository.create_with_ledger(command).await?;
    }

    // Before the completion marker exists, filtered reads use authoritative
    // deposit rows and therefore cannot hide records missing derived indexes.
    let pre_rebuild = repository
        .deposits(DepositQuery {
            after: None,
            limit: 4,
            user_id: Some(UserId("user-1".to_owned())),
            state: None,
        })
        .await?;
    assert_eq!(
        pre_rebuild
            .deposits
            .iter()
            .map(|deposit| deposit.id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["deposit-1", "deposit-2"]
    );

    let first_rebuild = repository
        .rebuild_deposit_indexes(RebuildRequest {
            after: None,
            limit: 2,
        })
        .await?;
    assert_eq!(first_rebuild.scanned, 2);
    assert_eq!(first_rebuild.next, Some(DepositId("deposit-2".to_owned())));
    assert!(!first_rebuild.complete);
    let completed = repository
        .rebuild_deposit_indexes(RebuildRequest {
            after: first_rebuild.next,
            limit: 2,
        })
        .await?;
    assert_eq!(completed.scanned, 2);
    assert_eq!(completed.next, None);
    assert!(completed.complete);

    let first_user_page = repository
        .deposits(DepositQuery {
            after: None,
            limit: 1,
            user_id: Some(UserId("user-1".to_owned())),
            state: None,
        })
        .await?;
    assert_eq!(first_user_page.deposits.len(), 1);
    assert_eq!(
        first_user_page.next,
        Some(DepositId("deposit-1".to_owned()))
    );
    let second_user_page = repository
        .deposits(DepositQuery {
            after: first_user_page.next,
            limit: 1,
            user_id: Some(UserId("user-1".to_owned())),
            state: None,
        })
        .await?;
    assert_eq!(second_user_page.deposits[0].id.0, "deposit-2");
    assert_eq!(second_user_page.next, None);

    let deposit_id = DepositId("deposit-2".to_owned());
    let watch_id = WatchId("watch-expiring".to_owned());
    repository
        .activate_watch(
            &deposit_id,
            &IdempotencyKey("create-deposit-2".to_owned()),
            watch_id.clone(),
        )
        .await?;
    let wrong_watch = repository
        .set_state(
            &deposit_id,
            DepositState::Expired {
                watch_id: WatchId("different-watch".to_owned()),
            },
        )
        .await
        .expect_err("expiration cannot replace the durable IX watch");
    assert_eq!(wrong_watch.kind, DepositErrorKind::InvalidState);
    repository
        .set_state(
            &deposit_id,
            DepositState::Expired {
                watch_id: watch_id.clone(),
            },
        )
        .await?;
    let expired = repository
        .deposit(&deposit_id)
        .await?
        .expect("expired deposit remains durable");
    assert_eq!(expired.state.watch_id(), Some(&watch_id));

    let expired_for_user = repository
        .deposits(DepositQuery {
            after: None,
            limit: 10,
            user_id: Some(UserId("user-1".to_owned())),
            state: Some(DepositStateKind::Expired),
        })
        .await?;
    assert_eq!(expired_for_user.deposits, vec![expired]);
    assert_eq!(expired_for_user.next, None);

    let awaiting_expiration = repository
        .set_state(
            &DepositId("deposit-1".to_owned()),
            DepositState::Expired {
                watch_id: WatchId("unregistered-watch".to_owned()),
            },
        )
        .await
        .expect_err("AwaitingWatch cannot expire because it has no acknowledged watch");
    assert_eq!(awaiting_expiration.kind, DepositErrorKind::InvalidState);
    Ok(())
}

#[tokio::test]
async fn mirror_cursor_and_duplicate_semantics_survive_restart()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let observation = reorg_observation();
    let command = MirrorObservation {
        expected_cursor: None,
        observation: observation.clone(),
    };

    {
        let repository = PaymentStore::new(RocksDb::open(directory.path())?);
        assert_eq!(
            repository.mirror_and_advance(command.clone()).await?,
            MirrorOutcome::Appended {
                cursor: EventCursor(1)
            }
        );
    }

    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    assert_eq!(
        repository
            .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
            .await?
            .cursor,
        Some(EventCursor(1))
    );
    assert_eq!(
        repository.mirror_and_advance(command).await?,
        MirrorOutcome::AlreadyPresent {
            cursor: EventCursor(1)
        }
    );

    let mut conflicting = observation;
    conflicting.received_at += 1;
    let error = repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(1)),
            observation: conflicting,
        })
        .await
        .expect_err("a duplicate event ID cannot carry a different payload");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    assert_eq!(
        repository
            .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
            .await?
            .cursor,
        Some(EventCursor(1))
    );
    Ok(())
}

#[tokio::test]
async fn post_credit_reorg_projection_opens_and_resolves_a_blocking_case()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let created = repository.create_with_ledger(create_deposit()).await?;

    let confirmation = confirmed_observation();
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: None,
            observation: confirmation.clone(),
        })
        .await?;
    let confirmed = repository
        .project_and_advance(ProjectObservation {
            expected_cursor: None,
            through: EventCursor(1),
            affected_deposits: vec![created.deposit.id.clone()],
            ledger_updates: vec![RecordObservation {
                event_id: confirmation.event.id.clone(),
                effect: LedgerEffect::Incoming {
                    movements: vec![MovementId("movement-1".to_owned())],
                },
                deposit_id: created.deposit.id.clone(),
                expected_head: Some(created.ledger.id),
                recorded_at: 1_020,
            }],
            reconciliation_cases: Vec::new(),
            fee_treatment: ProjectionFeeTreatment::Separate,
            utxo_batch_transition: None,
        })
        .await?
        .ledger_results
        .into_iter()
        .next()
        .expect("confirmation projects one deposit");
    let confirmed = match confirmed {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => {
            panic!("the first confirmation projection must append")
        }
    };
    let observations = repository
        .observations_for_deposit(DepositFilter {
            deposit_id: created.deposit.id.clone(),
            after: None,
            limit: 10,
        })
        .await?;
    assert_eq!(observations.observations, vec![confirmation]);
    assert_eq!(observations.next, None);
    let accounting = AccountingCommand {
        command: accounting_identity("account-1", 1),
        deposit_id: created.deposit.id.clone(),
        expected_head: Some(confirmed.id),
        next_accounted: amount(100),
        reason: "credit confirmed exchange deposit".to_owned(),
        recorded_at: 1_021,
    };
    let accounted = repository.record_accounting(accounting.clone()).await?;
    let accounted = match accounted {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("the first accounting command must append"),
    };
    assert!(matches!(
        &accounted.cause,
        deposits::LedgerEntryCause::Accounting {
            idempotency_key,
            reason
        } if idempotency_key == &accounting.command.client_key
            && reason == "credit confirmed exchange deposit"
    ));
    let mut replay = accounting.clone();
    replay.recorded_at = 1_999;
    assert!(matches!(
        repository.record_accounting(replay).await?,
        ApplyResult::AlreadyPresent { .. }
    ));
    let mut changed_reason = accounting.clone();
    changed_reason.reason = "different business justification".to_owned();
    changed_reason.command.request_hash = RequestHash([8; 32]);
    let changed_reason_error = repository
        .record_accounting(changed_reason)
        .await
        .expect_err("same scoped identity cannot change the persisted accounting reason");
    assert_eq!(changed_reason_error.kind, DepositErrorKind::Conflict);
    let mut changed_hash = accounting;
    changed_hash.command.request_hash = RequestHash([9; 32]);
    let changed_hash_error = repository
        .record_accounting(changed_hash)
        .await
        .expect_err("same scoped client key cannot identify changed request content");
    assert_eq!(changed_hash_error.kind, DepositErrorKind::Conflict);

    let observation = reorg_observation_at(2);
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(1)),
            observation: observation.clone(),
        })
        .await?;
    let reconciliation = ReconciliationCase {
        id: CaseId("reconciliation-1".to_owned()),
        deposit_id: created.deposit.id.clone(),
        triggering_event_id: observation.event.id.clone(),
        reason: ReconciliationReason::PostCreditReorg {
            accounted: amount(100),
            corrected_confirmed: amount(0),
        },
        state: ReconciliationState::Open,
        created_at: 1_032,
    };
    let projection = ProjectObservation {
        expected_cursor: Some(EventCursor(1)),
        through: EventCursor(2),
        affected_deposits: vec![created.deposit.id.clone()],
        ledger_updates: vec![RecordObservation {
            event_id: observation.event.id,
            effect: LedgerEffect::Incoming {
                movements: vec![MovementId("movement-1".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(accounted.id),
            recorded_at: 1_032,
        }],
        reconciliation_cases: vec![reconciliation.clone()],
        fee_treatment: ProjectionFeeTreatment::Separate,
        utxo_batch_transition: None,
    };

    let result = repository.project_and_advance(projection.clone()).await?;
    assert_eq!(result.checkpoint.cursor, Some(EventCursor(2)));
    assert!(matches!(
        result.ledger_results.as_slice(),
        [ApplyResult::Appended { .. }]
    ));
    assert_eq!(result.reconciliation_cases, vec![reconciliation.clone()]);
    assert_eq!(
        repository
            .current(&created.deposit.id)
            .await?
            .map(|entry| entry.balances),
        Some(DepositBalances {
            received: amount(0),
            confirmed: amount(0),
            balance: amount(0),
            collected: amount(0),
            accounted: amount(100),
        })
    );
    assert!(
        repository
            .automatic_actions_blocked(&created.deposit.id)
            .await?
    );

    let retry = repository.project_and_advance(projection).await?;
    assert!(matches!(
        retry.ledger_results.as_slice(),
        [ApplyResult::AlreadyPresent { .. }]
    ));
    assert_eq!(retry.reconciliation_cases, vec![reconciliation.clone()]);

    let current = repository
        .current(&created.deposit.id)
        .await?
        .expect("the projected ledger must have a current head");
    let blocked = repository
        .record_accounting(AccountingCommand {
            command: accounting_identity("account-while-blocked", 2),
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(current.id.clone()),
            next_accounted: amount(0),
            reason: "reverse credit after reorg".to_owned(),
            recorded_at: 1_033,
        })
        .await
        .expect_err("automatic accounting must stop during reconciliation");
    assert_eq!(blocked.kind, DepositErrorKind::InvalidState);

    let resolution = ResolveReconciliation {
        command: reconciliation_identity("resolve-reconciliation-1", 3),
        case_id: reconciliation.id.clone(),
        decision: ReconciliationDecision::ReverseCredit {
            expected_head: current.id.clone(),
            reason: "reverse credit after reorg".to_owned(),
        },
        resolved_at: 1_040,
    };
    let resolved = repository.resolve_case(resolution.clone()).await?;
    let reversed = repository
        .current(&created.deposit.id)
        .await?
        .expect("reverse credit must append a corrected absolute ledger row");
    assert_eq!(reversed.previous, Some(current.id));
    assert_eq!(
        reversed.balances,
        DepositBalances {
            received: amount(0),
            confirmed: amount(0),
            balance: amount(0),
            collected: amount(0),
            accounted: amount(0),
        }
    );
    assert!(matches!(
        &reversed.cause,
        LedgerEntryCause::ReconciliationResolution {
            case_id,
            idempotency_key,
            reason,
        } if case_id == &reconciliation.id
            && idempotency_key == &resolution.command.client_key
            && reason == "reverse credit after reorg"
    ));
    assert!(matches!(
        &resolved.state,
        ReconciliationState::Resolved {
            resolution: stored,
            resolved_at: 1_040,
        } if stored.command == resolution.command
            && stored.decision == resolution.decision
            && stored.ledger_entry_id.as_ref() == Some(&reversed.id)
    ));

    let mut replay = resolution.clone();
    replay.resolved_at = 1_999;
    assert_eq!(repository.resolve_case(replay).await?, resolved);

    let mut changed = resolution;
    changed.command.request_hash = RequestHash([4; 32]);
    changed.decision = ReconciliationDecision::AcceptLiability {
        reason: "accept the liability instead".to_owned(),
    };
    let error = repository
        .resolve_case(changed)
        .await
        .expect_err("a scoped reconciliation key cannot identify a changed request");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    assert!(
        !repository
            .automatic_actions_blocked(&created.deposit.id)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn non_ledger_reconciliation_decisions_preserve_the_absolute_ledger()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);

    let liability_deposit = repository
        .create_with_ledger(create_deposit_named(
            "deposit-liability",
            "user-liability",
            41,
        ))
        .await?;
    let liability_case = reconciliation_case(
        "reconciliation-liability",
        liability_deposit.deposit.id.clone(),
        "event-liability",
    );
    repository.open_case(liability_case.clone()).await?;
    let liability_command = ResolveReconciliation {
        command: reconciliation_identity("accept-liability", 21),
        case_id: liability_case.id,
        decision: ReconciliationDecision::AcceptLiability {
            reason: "merchant accepts the business liability".to_owned(),
        },
        resolved_at: 2_000,
    };
    let liability_result = repository.resolve_case(liability_command.clone()).await?;
    assert_eq!(
        repository.current(&liability_deposit.deposit.id).await?,
        Some(liability_deposit.ledger)
    );
    assert!(matches!(
        &liability_result.state,
        ReconciliationState::Resolved {
            resolution,
            resolved_at: 2_000,
        } if resolution.command == liability_command.command
            && resolution.decision == liability_command.decision
            && resolution.ledger_entry_id.is_none()
    ));
    let mut liability_replay = liability_command;
    liability_replay.resolved_at = 9_999;
    assert_eq!(
        repository.resolve_case(liability_replay).await?,
        liability_result
    );

    let debt_deposit = repository
        .create_with_ledger(create_deposit_named(
            "deposit-external-debt",
            "user-external-debt",
            42,
        ))
        .await?;
    let debt_case = reconciliation_case(
        "reconciliation-external-debt",
        debt_deposit.deposit.id.clone(),
        "event-external-debt",
    );
    repository.open_case(debt_case.clone()).await?;
    let debt_command = ResolveReconciliation {
        command: reconciliation_identity("record-external-debt", 22),
        case_id: debt_case.id,
        decision: ReconciliationDecision::ExternalDebtRecorded {
            external_reference: "debt-system/case-414".to_owned(),
            reason: "the liability is tracked by the external debt system".to_owned(),
        },
        resolved_at: 2_100,
    };
    let debt_result = repository.resolve_case(debt_command.clone()).await?;
    assert_eq!(
        repository.current(&debt_deposit.deposit.id).await?,
        Some(debt_deposit.ledger)
    );
    assert!(matches!(
        &debt_result.state,
        ReconciliationState::Resolved {
            resolution,
            resolved_at: 2_100,
        } if resolution.command == debt_command.command
            && resolution.decision == debt_command.decision
            && resolution.ledger_entry_id.is_none()
    ));

    let mut changed_debt = debt_command;
    changed_debt.decision = ReconciliationDecision::ExternalDebtRecorded {
        external_reference: "debt-system/case-999".to_owned(),
        reason: "changed request body".to_owned(),
    };
    let conflict = repository
        .resolve_case(changed_debt)
        .await
        .expect_err("a reconciliation key cannot be reused for another request body");
    assert_eq!(conflict.kind, DepositErrorKind::Conflict);
    Ok(())
}

#[tokio::test]
async fn reverse_credit_head_conflict_leaves_case_and_ledger_unchanged()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let created = repository
        .create_with_ledger(create_deposit_named(
            "deposit-stale-head",
            "user-stale-head",
            43,
        ))
        .await?;
    let case = reconciliation_case(
        "reconciliation-stale-head",
        created.deposit.id.clone(),
        "event-stale-head",
    );
    repository.open_case(case.clone()).await?;

    let error = repository
        .resolve_case(ResolveReconciliation {
            command: reconciliation_identity("stale-reverse-credit", 24),
            case_id: case.id.clone(),
            decision: ReconciliationDecision::ReverseCredit {
                expected_head: EntryId("not-the-current-head".to_owned()),
                reason: "reverse the credited value".to_owned(),
            },
            resolved_at: 2_200,
        })
        .await
        .expect_err("a stale expected ledger head must reject the whole resolution");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    assert_eq!(repository.case(&case.id).await?, Some(case));
    assert_eq!(
        repository.current(&created.deposit.id).await?,
        Some(created.ledger)
    );
    Ok(())
}

#[tokio::test]
async fn rocksdb_projects_included_confirmed_reorged_and_reincluded_revisions_once()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let created = repository.create_with_ledger(create_deposit()).await?;
    let movement_ids = vec![
        MovementId("part-a".to_owned()),
        MovementId("part-b".to_owned()),
    ];
    let movements = || vec![movement("part-a", 40), movement("part-b", 110)];

    let included = observation_revision(
        "event-lifecycle-1",
        1,
        "0xlifecycle",
        1,
        included_status(),
        None,
        movements(),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: None,
            observation: included.clone(),
        })
        .await?;
    let included_result = repository
        .project_and_advance(ProjectObservation {
            expected_cursor: None,
            through: EventCursor(1),
            affected_deposits: vec![created.deposit.id.clone()],
            ledger_updates: vec![RecordObservation {
                event_id: included.event.id.clone(),
                effect: LedgerEffect::Incoming {
                    movements: movement_ids.clone(),
                },
                deposit_id: created.deposit.id.clone(),
                expected_head: Some(created.ledger.id.clone()),
                recorded_at: 2_001,
            }],
            reconciliation_cases: Vec::new(),
            fee_treatment: ProjectionFeeTreatment::Separate,
            utxo_batch_transition: None,
        })
        .await?;
    let included_entry = match &included_result.ledger_results[0] {
        ApplyResult::Appended { entry } => entry.clone(),
        ApplyResult::AlreadyPresent { .. } => panic!("first inclusion must append"),
    };
    assert_eq!(
        included_entry.balances,
        DepositBalances {
            received: amount(150),
            confirmed: amount(0),
            balance: amount(150),
            collected: amount(0),
            accounted: amount(0),
        }
    );
    assert!(matches!(
        &included_entry.cause,
        LedgerEntryCause::Observation { projection_id, .. }
            if projection_id == &ProjectionId::for_observation(
                &included.event.id,
                included.event.transaction.revision,
                &created.deposit.id,
            )
    ));

    let confirmed = observation_revision(
        "event-lifecycle-2",
        2,
        "0xlifecycle",
        2,
        confirmed_status(),
        Some(included_status()),
        movements(),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(1)),
            observation: confirmed.clone(),
        })
        .await?;
    let confirmed_result = repository
        .project_and_advance(ProjectObservation {
            expected_cursor: Some(EventCursor(1)),
            through: EventCursor(2),
            affected_deposits: vec![created.deposit.id.clone()],
            ledger_updates: vec![RecordObservation {
                event_id: confirmed.event.id.clone(),
                effect: LedgerEffect::Incoming {
                    movements: movement_ids.clone(),
                },
                deposit_id: created.deposit.id.clone(),
                expected_head: Some(included_entry.id),
                recorded_at: 2_002,
            }],
            reconciliation_cases: Vec::new(),
            fee_treatment: ProjectionFeeTreatment::Separate,
            utxo_batch_transition: None,
        })
        .await?;
    let confirmed_entry = match &confirmed_result.ledger_results[0] {
        ApplyResult::Appended { entry } => entry.clone(),
        ApplyResult::AlreadyPresent { .. } => panic!("first confirmation must append"),
    };
    assert_eq!(confirmed_entry.balances.received, amount(150));
    assert_eq!(confirmed_entry.balances.confirmed, amount(150));
    assert_eq!(confirmed_entry.balances.balance, amount(150));

    let accounted = repository
        .record_accounting(AccountingCommand {
            command: accounting_identity("lifecycle-credit", 3),
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(confirmed_entry.id),
            next_accounted: amount(80),
            reason: "credit part of confirmed overpayment".to_owned(),
            recorded_at: 2_003,
        })
        .await?;
    let accounted = match accounted {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("accounting must append"),
    };

    let reorged = observation_revision(
        "event-lifecycle-3",
        3,
        "0xlifecycle",
        3,
        TransactionStatus::Reorged {
            previous_block: block(30, 30),
        },
        Some(confirmed_status()),
        movements(),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(2)),
            observation: reorged.clone(),
        })
        .await?;
    let reorged_result = repository
        .project_and_advance(ProjectObservation {
            expected_cursor: Some(EventCursor(2)),
            through: EventCursor(3),
            affected_deposits: vec![created.deposit.id.clone()],
            ledger_updates: vec![RecordObservation {
                event_id: reorged.event.id,
                effect: LedgerEffect::Incoming {
                    movements: movement_ids.clone(),
                },
                deposit_id: created.deposit.id.clone(),
                expected_head: Some(accounted.id),
                recorded_at: 2_004,
            }],
            reconciliation_cases: Vec::new(),
            fee_treatment: ProjectionFeeTreatment::Separate,
            utxo_batch_transition: None,
        })
        .await?;
    let reorged_entry = match &reorged_result.ledger_results[0] {
        ApplyResult::Appended { entry } => entry.clone(),
        ApplyResult::AlreadyPresent { .. } => panic!("first reorg must append"),
    };
    assert_eq!(
        reorged_entry.balances,
        DepositBalances {
            received: amount(0),
            confirmed: amount(0),
            balance: amount(0),
            collected: amount(0),
            accounted: amount(80),
        }
    );

    let reincluded = observation_revision(
        "event-lifecycle-4",
        4,
        "0xlifecycle",
        4,
        included_status(),
        Some(TransactionStatus::Reorged {
            previous_block: block(30, 30),
        }),
        movements(),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(3)),
            observation: reincluded.clone(),
        })
        .await?;
    let reincluded_command = ProjectObservation {
        expected_cursor: Some(EventCursor(3)),
        through: EventCursor(4),
        affected_deposits: vec![created.deposit.id.clone()],
        ledger_updates: vec![RecordObservation {
            event_id: reincluded.event.id,
            effect: LedgerEffect::Incoming {
                movements: movement_ids,
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(reorged_entry.id),
            recorded_at: 2_005,
        }],
        reconciliation_cases: Vec::new(),
        fee_treatment: ProjectionFeeTreatment::Separate,
        utxo_batch_transition: None,
    };
    let reincluded_result = repository
        .project_and_advance(reincluded_command.clone())
        .await?;
    let reincluded_entry = match &reincluded_result.ledger_results[0] {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("first re-inclusion must append"),
    };
    assert_eq!(reincluded_entry.balances.received, amount(150));
    assert_eq!(reincluded_entry.balances.confirmed, amount(0));
    assert_eq!(reincluded_entry.balances.balance, amount(150));
    assert_eq!(reincluded_entry.balances.accounted, amount(80));
    assert!(matches!(
        repository
            .project_and_advance(reincluded_command)
            .await?
            .ledger_results
            .as_slice(),
        [ApplyResult::AlreadyPresent { .. }]
    ));
    Ok(())
}

#[tokio::test]
async fn input_output_net_projection_handles_debit_credit_zero_and_atomic_conflict_case()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let created = repository.create_with_ledger(create_deposit()).await?;
    let deposit_address = created.deposit.address.clone();

    let funding = observation_revision(
        "net-funding",
        1,
        "net-funding-tx",
        1,
        included_status(),
        None,
        vec![ValueMovement::Output {
            id: MovementId("net-funding-output".to_owned()),
            asset: asset(),
            amount: amount(100),
            owner: Some(deposit_address.clone()),
        }],
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: None,
            observation: funding.clone(),
        })
        .await?;
    let funded = repository
        .project_and_advance(ProjectObservation {
            expected_cursor: None,
            through: EventCursor(1),
            affected_deposits: vec![created.deposit.id.clone()],
            ledger_updates: vec![RecordObservation {
                event_id: funding.event.id,
                effect: LedgerEffect::Incoming {
                    movements: vec![MovementId("net-funding-output".to_owned())],
                },
                deposit_id: created.deposit.id.clone(),
                expected_head: Some(created.ledger.id),
                recorded_at: 3_001,
            }],
            reconciliation_cases: Vec::new(),
            fee_treatment: ProjectionFeeTreatment::Separate,
            utxo_batch_transition: None,
        })
        .await?;
    let mut head = match &funded.ledger_results[0] {
        ApplyResult::Appended { entry } => entry.clone(),
        ApplyResult::AlreadyPresent { .. } => panic!("funding must append"),
    };
    assert_eq!(head.balances.balance, amount(100));

    let collection_included = observation_revision_with_fee(
        "input-collection",
        2,
        "input-collection-tx",
        1,
        included_status(),
        None,
        vec![ValueMovement::Input {
            id: MovementId("input-collection-debit".to_owned()),
            asset: asset(),
            amount: amount(20),
            owner: Some(deposit_address.clone()),
        }],
        Some(network_fee(5, deposit_address.clone(), asset())),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(1)),
            observation: collection_included.clone(),
        })
        .await?;
    let collection_outcome = repository
        .project_and_advance(ProjectObservation {
            expected_cursor: Some(EventCursor(1)),
            through: EventCursor(2),
            affected_deposits: vec![created.deposit.id.clone()],
            ledger_updates: vec![RecordObservation {
                event_id: collection_included.event.id,
                effect: LedgerEffect::Collection {
                    movements: vec![MovementId("input-collection-debit".to_owned())],
                },
                deposit_id: created.deposit.id.clone(),
                expected_head: Some(head.id),
                recorded_at: 3_002,
            }],
            reconciliation_cases: Vec::new(),
            fee_treatment: ProjectionFeeTreatment::IncludedInMovementEffect,
            utxo_batch_transition: None,
        })
        .await?;
    head = match &collection_outcome.ledger_results[0] {
        ApplyResult::Appended { entry } => entry.clone(),
        ApplyResult::AlreadyPresent { .. } => panic!("input collection must append"),
    };
    assert_eq!(head.balances.balance, amount(80));
    assert!(matches!(
        &head.cause,
        LedgerEntryCause::Observation {
            network_fee: None,
            ..
        }
    ));

    let debit = observation_revision_with_fee(
        "net-debit",
        3,
        "conflicting-spend",
        1,
        included_status(),
        None,
        vec![
            ValueMovement::Input {
                id: MovementId("net-debit-input".to_owned()),
                asset: asset(),
                amount: amount(100),
                owner: Some(deposit_address.clone()),
            },
            ValueMovement::Output {
                id: MovementId("net-debit-return".to_owned()),
                asset: asset(),
                amount: amount(40),
                owner: Some(deposit_address.clone()),
            },
        ],
        Some(network_fee(10, deposit_address.clone(), asset())),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(2)),
            observation: debit.clone(),
        })
        .await?;
    let conflict_case = ReconciliationCase {
        id: CaseId("reserved-spend-conflict".to_owned()),
        deposit_id: created.deposit.id.clone(),
        triggering_event_id: debit.event.id.clone(),
        reason: ReconciliationReason::ReservedSpendConflict {
            collection_id: CollectionId("retained-collection".to_owned()),
            transaction_id: debit.event.transaction.transaction_id.clone(),
        },
        state: ReconciliationState::Open,
        created_at: 3_002,
    };
    let debit_outcome = repository
        .project_and_advance(ProjectObservation {
            expected_cursor: Some(EventCursor(2)),
            through: EventCursor(3),
            affected_deposits: vec![created.deposit.id.clone()],
            ledger_updates: vec![RecordObservation {
                event_id: debit.event.id,
                effect: LedgerEffect::NetBalanceChange {
                    debit_movements: vec![MovementId("net-debit-input".to_owned())],
                    credit_movements: vec![MovementId("net-debit-return".to_owned())],
                },
                deposit_id: created.deposit.id.clone(),
                expected_head: Some(head.id),
                recorded_at: 3_002,
            }],
            reconciliation_cases: vec![conflict_case.clone()],
            fee_treatment: ProjectionFeeTreatment::IncludedInMovementEffect,
            utxo_batch_transition: None,
        })
        .await?;
    assert_eq!(debit_outcome.checkpoint.cursor, Some(EventCursor(3)));
    assert_eq!(
        debit_outcome.reconciliation_cases,
        vec![conflict_case.clone()]
    );
    assert_eq!(
        repository.case(&conflict_case.id).await?,
        Some(conflict_case)
    );
    head = match &debit_outcome.ledger_results[0] {
        ApplyResult::Appended { entry } => entry.clone(),
        ApplyResult::AlreadyPresent { .. } => panic!("net debit must append"),
    };
    assert_eq!(head.balances.balance, amount(20));
    assert!(matches!(
        &head.cause,
        LedgerEntryCause::Observation {
            movement_ids,
            network_fee: None,
            ..
        } if movement_ids == &vec![
            MovementId("net-debit-input".to_owned()),
            MovementId("net-debit-return".to_owned()),
        ]
    ));

    for (cursor, event_id, txid, input, output, expected_balance) in [
        (4, "net-credit", "net-credit-tx", 10, 50, 60),
        (5, "net-zero", "net-zero-tx", 25, 25, 60),
    ] {
        let input_id = MovementId(format!("{event_id}-input"));
        let output_id = MovementId(format!("{event_id}-return"));
        let event = observation_revision_with_fee(
            event_id,
            cursor,
            txid,
            1,
            included_status(),
            None,
            vec![
                ValueMovement::Input {
                    id: input_id.clone(),
                    asset: asset(),
                    amount: amount(input),
                    owner: Some(deposit_address.clone()),
                },
                ValueMovement::Output {
                    id: output_id.clone(),
                    asset: asset(),
                    amount: amount(output),
                    owner: Some(deposit_address.clone()),
                },
            ],
            Some(network_fee(1, deposit_address.clone(), asset())),
        );
        repository
            .mirror_and_advance(MirrorObservation {
                expected_cursor: Some(EventCursor(cursor - 1)),
                observation: event.clone(),
            })
            .await?;
        let outcome = repository
            .project_and_advance(ProjectObservation {
                expected_cursor: Some(EventCursor(cursor - 1)),
                through: EventCursor(cursor),
                affected_deposits: vec![created.deposit.id.clone()],
                ledger_updates: vec![RecordObservation {
                    event_id: event.event.id,
                    effect: LedgerEffect::NetBalanceChange {
                        debit_movements: vec![input_id],
                        credit_movements: vec![output_id],
                    },
                    deposit_id: created.deposit.id.clone(),
                    expected_head: Some(head.id),
                    recorded_at: 3_000 + cursor,
                }],
                reconciliation_cases: Vec::new(),
                fee_treatment: ProjectionFeeTreatment::IncludedInMovementEffect,
                utxo_batch_transition: None,
            })
            .await?;
        head = match &outcome.ledger_results[0] {
            ApplyResult::Appended { entry } => entry.clone(),
            ApplyResult::AlreadyPresent { .. } => panic!("net change must append"),
        };
        assert_eq!(head.balances.balance, amount(expected_balance));
        assert_eq!(outcome.checkpoint.cursor, Some(EventCursor(cursor)));
    }
    Ok(())
}

#[tokio::test]
async fn rocksdb_record_observation_uses_actual_amounts_and_rejects_arithmetic_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let created = repository.create_with_ledger(create_deposit()).await?;

    let partial = observation_revision(
        "event-partial",
        10,
        "0xpartial",
        1,
        included_status(),
        None,
        vec![movement("partial", 40)],
    );
    repository
        .append(AppendObservation {
            observation: partial.clone(),
        })
        .await?;
    let partial_entry = repository
        .record_observation(RecordObservation {
            event_id: partial.event.id,
            effect: LedgerEffect::Incoming {
                movements: vec![MovementId("partial".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(created.ledger.id),
            recorded_at: 3_001,
        })
        .await?;
    let partial_entry = match partial_entry {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("partial payment must append"),
    };
    assert_eq!(partial_entry.balances.received, amount(40));

    let overpayment = observation_revision(
        "event-overpayment",
        11,
        "0xoverpayment",
        1,
        included_status(),
        None,
        vec![movement("over-a", 50), movement("over-b", 60)],
    );
    repository
        .append(AppendObservation {
            observation: overpayment.clone(),
        })
        .await?;
    let overpayment_entry = repository
        .record_observation(RecordObservation {
            event_id: overpayment.event.id,
            effect: LedgerEffect::Incoming {
                movements: vec![
                    MovementId("over-a".to_owned()),
                    MovementId("over-b".to_owned()),
                ],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(partial_entry.id),
            recorded_at: 3_002,
        })
        .await?;
    let overpayment_entry = match overpayment_entry {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("overpayment must append"),
    };
    assert_eq!(overpayment_entry.balances.received, amount(150));
    assert_eq!(overpayment_entry.balances.balance, amount(150));

    let overflow = observation_revision(
        "event-overflow",
        12,
        "0xoverflow",
        1,
        included_status(),
        None,
        vec![ValueMovement::Transfer {
            id: MovementId("overflow".to_owned()),
            asset: asset(),
            amount:
                "115792089237316195423570985008687907853269984665640564039457584007913129639935"
                    .parse()
                    .expect("maximum u256 is a valid decimal"),
            from: address("0x2222222222222222222222222222222222222222"),
            to: address("0x1111111111111111111111111111111111111111"),
        }],
    );
    repository
        .append(AppendObservation {
            observation: overflow.clone(),
        })
        .await?;
    let overflow_error = repository
        .record_observation(RecordObservation {
            event_id: overflow.event.id,
            effect: LedgerEffect::Incoming {
                movements: vec![MovementId("overflow".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(overpayment_entry.id.clone()),
            recorded_at: 3_003,
        })
        .await
        .expect_err("u256 overflow must reject the immutable row");
    assert_eq!(overflow_error.kind, DepositErrorKind::InvariantViolation);

    let empty_deposit = create_deposit();
    let mut empty_deposit = empty_deposit;
    empty_deposit.deposit.id = DepositId("deposit-empty".to_owned());
    empty_deposit.deposit.idempotency_key = IdempotencyKey("create-empty".to_owned());
    empty_deposit.deposit.address = address("0x3333333333333333333333333333333333333333");
    let empty = repository.create_with_ledger(empty_deposit).await?;
    let underflow = observation_revision(
        "event-underflow",
        13,
        "0xunderflow",
        1,
        included_status(),
        None,
        vec![movement("underflow", 1)],
    );
    repository
        .append(AppendObservation {
            observation: underflow.clone(),
        })
        .await?;
    let underflow_error = repository
        .record_observation(RecordObservation {
            event_id: underflow.event.id,
            effect: LedgerEffect::Collection {
                movements: vec![MovementId("underflow".to_owned())],
            },
            deposit_id: empty.deposit.id,
            expected_head: Some(empty.ledger.id),
            recorded_at: 3_004,
        })
        .await
        .expect_err("collection debit cannot underflow an empty address balance");
    assert_eq!(underflow_error.kind, DepositErrorKind::InvariantViolation);
    assert_eq!(
        repository
            .current(&created.deposit.id)
            .await?
            .map(|entry| entry.id),
        Some(overpayment_entry.id)
    );
    Ok(())
}

#[tokio::test]
async fn rocksdb_collection_changes_collected_only_after_confirmation()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let created = repository.create_with_ledger(create_deposit()).await?;

    let incoming = observation_revision(
        "event-collection-seed",
        20,
        "0xseed",
        1,
        confirmed_status(),
        None,
        vec![movement("seed", 100)],
    );
    repository
        .append(AppendObservation {
            observation: incoming.clone(),
        })
        .await?;
    let seed = repository
        .record_observation(RecordObservation {
            event_id: incoming.event.id,
            effect: LedgerEffect::Incoming {
                movements: vec![MovementId("seed".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(created.ledger.id),
            recorded_at: 4_001,
        })
        .await?;
    let seed = match seed {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("seed must append"),
    };
    let accounted = repository
        .record_accounting(AccountingCommand {
            command: accounting_identity("collection-accounting", 4),
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(seed.id),
            next_accounted: amount(50),
            reason: "credit before collection".to_owned(),
            recorded_at: 4_002,
        })
        .await?;
    let accounted = match accounted {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("accounting must append"),
    };

    let included = observation_revision(
        "event-collection-included",
        21,
        "0xsweep",
        1,
        included_status(),
        None,
        vec![movement("sweep", 100)],
    );
    repository
        .append(AppendObservation {
            observation: included.clone(),
        })
        .await?;
    let swept = repository
        .record_observation(RecordObservation {
            event_id: included.event.id,
            effect: LedgerEffect::Collection {
                movements: vec![MovementId("sweep".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(accounted.id),
            recorded_at: 4_003,
        })
        .await?;
    let swept = match swept {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("included sweep must append"),
    };
    assert_eq!(swept.balances.balance, amount(0));
    assert_eq!(swept.balances.collected, amount(0));
    assert_eq!(swept.balances.accounted, amount(50));

    let confirmed = observation_revision(
        "event-collection-confirmed",
        22,
        "0xsweep",
        2,
        confirmed_status(),
        Some(included_status()),
        vec![movement("sweep", 100)],
    );
    repository
        .append(AppendObservation {
            observation: confirmed.clone(),
        })
        .await?;
    let collected = repository
        .record_observation(RecordObservation {
            event_id: confirmed.event.id,
            effect: LedgerEffect::Collection {
                movements: vec![MovementId("sweep".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(swept.id),
            recorded_at: 4_004,
        })
        .await?;
    let collected = match collected {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("confirmed sweep must append"),
    };
    assert_eq!(collected.balances.balance, amount(0));
    assert_eq!(collected.balances.collected, amount(100));
    assert_eq!(collected.balances.accounted, amount(50));

    let reorged = observation_revision(
        "event-collection-reorged",
        23,
        "0xsweep",
        3,
        TransactionStatus::Reorged {
            previous_block: block(30, 30),
        },
        Some(confirmed_status()),
        vec![movement("sweep", 100)],
    );
    repository
        .append(AppendObservation {
            observation: reorged.clone(),
        })
        .await?;
    let restored = repository
        .record_observation(RecordObservation {
            event_id: reorged.event.id,
            effect: LedgerEffect::Collection {
                movements: vec![MovementId("sweep".to_owned())],
            },
            deposit_id: created.deposit.id,
            expected_head: Some(collected.id),
            recorded_at: 4_005,
        })
        .await?;
    let restored = match restored {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("reorged sweep must append"),
    };
    assert_eq!(restored.balances.balance, amount(100));
    assert_eq!(restored.balances.collected, amount(0));
    assert_eq!(restored.balances.accounted, amount(50));
    Ok(())
}

#[tokio::test]
async fn rocksdb_projects_repository_derived_collection_fee_and_reverses_it_exactly()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let created = repository.create_with_ledger(create_deposit()).await?;

    let seed_observation = observation_revision(
        "event-fee-seed",
        30,
        "0xfeeseed",
        1,
        confirmed_status(),
        None,
        vec![movement("fee-seed", 110)],
    );
    repository
        .append(AppendObservation {
            observation: seed_observation.clone(),
        })
        .await?;
    let seed = repository
        .record_observation(RecordObservation {
            event_id: seed_observation.event.id,
            effect: LedgerEffect::Incoming {
                movements: vec![MovementId("fee-seed".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(created.ledger.id),
            recorded_at: 5_001,
        })
        .await?;
    let seed = match seed {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("fee seed must append"),
    };

    let payer = created.deposit.address.clone();
    let included_observation = observation_revision_with_fee(
        "event-fee-included",
        31,
        "0xfeesweep",
        1,
        included_status(),
        None,
        vec![outgoing_movement("fee-sweep", 100)],
        Some(network_fee(10, payer.clone(), asset())),
    );
    repository
        .append(AppendObservation {
            observation: included_observation.clone(),
        })
        .await?;
    let included = repository
        .record_observation(RecordObservation {
            event_id: included_observation.event.id,
            effect: LedgerEffect::Collection {
                movements: vec![MovementId("fee-sweep".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(seed.id),
            recorded_at: 5_002,
        })
        .await?;
    let included = match included {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("included fee sweep must append"),
    };
    assert_eq!(included.balances.balance, amount(0));
    assert_eq!(included.balances.collected, amount(0));
    let persisted_included = repository
        .current(&created.deposit.id)
        .await?
        .expect("included fee row remains durable");
    assert!(matches!(
        persisted_included.cause,
        LedgerEntryCause::Observation {
            network_fee: Some(fee),
            ..
        } if fee == amount(10)
    ));

    let confirmed_observation = observation_revision_with_fee(
        "event-fee-confirmed",
        32,
        "0xfeesweep",
        2,
        confirmed_status(),
        Some(included_status()),
        vec![outgoing_movement("fee-sweep", 100)],
        Some(network_fee(10, payer.clone(), asset())),
    );
    repository
        .append(AppendObservation {
            observation: confirmed_observation.clone(),
        })
        .await?;
    let confirmed = repository
        .record_observation(RecordObservation {
            event_id: confirmed_observation.event.id,
            effect: LedgerEffect::Collection {
                movements: vec![MovementId("fee-sweep".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(included.id),
            recorded_at: 5_003,
        })
        .await?;
    let confirmed = match confirmed {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("confirmed fee sweep must append"),
    };
    assert_eq!(confirmed.balances.balance, amount(0));
    assert_eq!(confirmed.balances.collected, amount(110));

    let reorged_observation = observation_revision_with_fee(
        "event-fee-reorged",
        33,
        "0xfeesweep",
        3,
        TransactionStatus::Reorged {
            previous_block: block(30, 30),
        },
        Some(confirmed_status()),
        vec![outgoing_movement("fee-sweep", 100)],
        Some(network_fee(10, payer, asset())),
    );
    repository
        .append(AppendObservation {
            observation: reorged_observation.clone(),
        })
        .await?;
    let reorg_command = RecordObservation {
        event_id: reorged_observation.event.id,
        effect: LedgerEffect::Collection {
            movements: vec![MovementId("fee-sweep".to_owned())],
        },
        deposit_id: created.deposit.id.clone(),
        expected_head: Some(confirmed.id),
        recorded_at: 5_004,
    };
    let restored = repository.record_observation(reorg_command.clone()).await?;
    let restored = match restored {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("fee reorg must append"),
    };
    assert_eq!(restored.balances.balance, amount(110));
    assert_eq!(restored.balances.collected, amount(0));
    assert!(matches!(
        repository.record_observation(reorg_command).await?,
        ApplyResult::AlreadyPresent { entry } if entry == restored
    ));
    Ok(())
}

#[tokio::test]
async fn rocksdb_projects_and_reorgs_a_fee_only_block_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let created = repository.create_with_ledger(create_deposit()).await?;
    let seed_observation = observation_revision(
        "event-failed-fee-seed",
        40,
        "0xfailedfeeseed",
        1,
        confirmed_status(),
        None,
        vec![movement("failed-fee-seed", 10)],
    );
    repository
        .append(AppendObservation {
            observation: seed_observation.clone(),
        })
        .await?;
    let seed = repository
        .record_observation(RecordObservation {
            event_id: seed_observation.event.id,
            effect: LedgerEffect::Incoming {
                movements: vec![MovementId("failed-fee-seed".to_owned())],
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(created.ledger.id),
            recorded_at: 6_001,
        })
        .await?;
    let seed = match seed {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("failed fee seed must append"),
    };

    let failed_status = TransactionStatus::Failed {
        block: Some(block(40, 40)),
        reason: Some("execution reverted".to_owned()),
    };
    let failed_observation = observation_revision_with_fee(
        "event-failed-fee",
        41,
        "0xfailedfee",
        1,
        failed_status.clone(),
        None,
        Vec::new(),
        Some(network_fee(10, created.deposit.address.clone(), asset())),
    );
    repository
        .append(AppendObservation {
            observation: failed_observation.clone(),
        })
        .await?;
    let failed = repository
        .record_observation(RecordObservation {
            event_id: failed_observation.event.id,
            effect: LedgerEffect::Collection {
                movements: Vec::new(),
            },
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(seed.id),
            recorded_at: 6_002,
        })
        .await?;
    let failed = match failed {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("fee-only failure must append"),
    };
    assert_eq!(failed.balances.balance, amount(0));
    assert_eq!(failed.balances.collected, amount(0));

    let reorged_observation = observation_revision_with_fee(
        "event-failed-fee-reorged",
        42,
        "0xfailedfee",
        2,
        TransactionStatus::Reorged {
            previous_block: block(40, 40),
        },
        Some(failed_status),
        Vec::new(),
        Some(network_fee(10, created.deposit.address.clone(), asset())),
    );
    repository
        .append(AppendObservation {
            observation: reorged_observation.clone(),
        })
        .await?;
    let restored = repository
        .record_observation(RecordObservation {
            event_id: reorged_observation.event.id,
            effect: LedgerEffect::Collection {
                movements: Vec::new(),
            },
            deposit_id: created.deposit.id,
            expected_head: Some(failed.id),
            recorded_at: 6_003,
        })
        .await?;
    let restored = match restored {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("failed fee reorg must append"),
    };
    assert_eq!(restored.balances.balance, amount(10));
    assert_eq!(restored.balances.collected, amount(0));
    Ok(())
}

#[tokio::test]
async fn rocksdb_ignores_fees_not_paid_by_the_deposit_in_its_asset()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let created = repository.create_with_ledger(create_deposit()).await?;
    let mismatched_fees = [
        network_fee(
            10,
            address("0x9999999999999999999999999999999999999999"),
            asset(),
        ),
        network_fee(
            10,
            created.deposit.address.clone(),
            AssetId {
                chain: chain(),
                asset: "erc20:0xtoken".to_owned(),
            },
        ),
    ];

    for (offset, fee) in mismatched_fees.into_iter().enumerate() {
        let event_id = format!("event-ineligible-fee-{offset}");
        let observation = observation_revision_with_fee(
            &event_id,
            50 + u64::try_from(offset)?,
            &format!("0xineligiblefee{offset}"),
            1,
            TransactionStatus::Failed {
                block: Some(block(50, 50)),
                reason: Some("execution reverted".to_owned()),
            },
            None,
            Vec::new(),
            Some(fee),
        );
        repository
            .append(AppendObservation {
                observation: observation.clone(),
            })
            .await?;
        let error = repository
            .record_observation(RecordObservation {
                event_id: observation.event.id,
                effect: LedgerEffect::Collection {
                    movements: Vec::new(),
                },
                deposit_id: created.deposit.id.clone(),
                expected_head: Some(created.ledger.id.clone()),
                recorded_at: 7_000 + u64::try_from(offset)?,
            })
            .await
            .expect_err("an unrelated IX fee cannot become a deposit ledger debit");
        assert_eq!(error.kind, DepositErrorKind::InvariantViolation);
    }
    assert_eq!(
        repository
            .current(&created.deposit.id)
            .await?
            .expect("open ledger remains current")
            .balances,
        DepositBalances::default()
    );
    Ok(())
}
