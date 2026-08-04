use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use deposits::{
    AccountingCommand, ApplyResult, AwaitingWatchPageRequest, ConsumerCheckpointName,
    CreateDeposit, CreateDepositWithLedger, DepositBalances, DepositErrorKind, DepositId,
    DepositLedger, DepositState, DepositStore, IdempotencyKey, LedgerObservationKind,
    MirrorObservation, MirrorOutcome, MirroredObservation, ObservationConsumerCheckpoints,
    PersistentPaymentRepository, ProjectObservation, ProjectionId, ReconciliationCase,
    ReconciliationCaseId, ReconciliationReason, ReconciliationState, ReconciliationStore,
    RecordObservationBalance, UserId,
};
use indexing::{
    BlockHash, BlockHeight, BlockRef, ConfirmationProof, EventCursor, IndexScope, MovementId,
    ObservationEvent, ObservationEventId, ObservationRevision, ObservedTransaction,
    TransactionStatus, WatchId,
};
use signer::KeyLocator;
use storage_rocksdb::RocksDbStorage;
use tempfile::TempDir;

fn amount(value: u64) -> AtomicAmount {
    let mut bytes = [0_u8; 32];
    bytes[24..].copy_from_slice(&value.to_be_bytes());
    AtomicAmount(bytes)
}

fn chain() -> ChainId {
    ChainId("ethereum".to_owned())
}

fn asset() -> AssetId {
    AssetId {
        chain: chain(),
        asset: "native".to_owned(),
    }
}

fn address(value: &str) -> CanonicalAddress {
    CanonicalAddress {
        chain: chain(),
        value: value.to_owned(),
    }
}

fn create_deposit() -> CreateDepositWithLedger {
    CreateDepositWithLedger {
        deposit: CreateDeposit {
            id: DepositId("deposit-1".to_owned()),
            idempotency_key: IdempotencyKey("create-1".to_owned()),
            user_id: UserId("user-1".to_owned()),
            asset: asset(),
            address: address("0x1111111111111111111111111111111111111111"),
            key: KeyLocator::Identifier("deposit-key-1".to_owned()),
            expected: amount(100),
            birthday: BlockHeight(10),
            expires_at: 10_000,
            created_at: 1_000,
        },
        ledger_recorded_at: 1_000,
    }
}

fn block(height: u64, hash: u8) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(vec![hash; 32]),
        parent_hash: Some(BlockHash(vec![hash.saturating_sub(1); 32])),
        timestamp: Some(1_000 + height),
    }
}

fn reorg_observation() -> MirroredObservation {
    let previous_block = block(20, 20);
    MirroredObservation {
        event: ObservationEvent {
            id: ObservationEventId("event-reorg-1".to_owned()),
            cursor: EventCursor(1),
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
                transaction_id: CanonicalTransactionId {
                    chain: chain(),
                    value: "0xtransaction".to_owned(),
                },
                revision: ObservationRevision(2),
                status: TransactionStatus::Reorged { previous_block },
                movements: Vec::new(),
                fee: None,
                first_seen_at: 1_020,
                observed_at: 1_030,
            },
        },
        received_at: 1_031,
    }
}

#[tokio::test]
async fn deposit_creation_is_atomic_idempotent_and_activates_exactly_one_watch()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
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
        .awaiting_watch(AwaitingWatchPageRequest {
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
            .awaiting_watch(AwaitingWatchPageRequest {
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
async fn mirror_cursor_and_duplicate_semantics_survive_restart()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let observation = reorg_observation();
    let command = MirrorObservation {
        expected_cursor: None,
        observation: observation.clone(),
    };

    {
        let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
        assert_eq!(
            repository.mirror_and_advance(command.clone()).await?,
            MirrorOutcome::Appended {
                cursor: EventCursor(1)
            }
        );
    }

    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
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
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    let created = repository.create_with_ledger(create_deposit()).await?;

    let confirmed = repository
        .record_observation(RecordObservationBalance {
            projection_id: ProjectionId("initial-confirmation".to_owned()),
            event_id: ObservationEventId("event-confirmed-1".to_owned()),
            observation_revision: ObservationRevision(1),
            status: TransactionStatus::Confirmed {
                block: block(20, 20),
                proof: ConfirmationProof::Depth {
                    required: 12,
                    observed: 12,
                },
            },
            kind: LedgerObservationKind::Incoming,
            movement_ids: vec![MovementId("movement-1".to_owned())],
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(created.ledger.id),
            next_balances: DepositBalances {
                received: amount(100),
                confirmed: amount(100),
                balance: amount(100),
                collected: amount(0),
                accounted: amount(0),
            },
            recorded_at: 1_020,
        })
        .await?;
    let confirmed = match confirmed {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => {
            panic!("the first confirmation projection must append")
        }
    };
    let accounted = repository
        .record_accounting(AccountingCommand {
            idempotency_key: IdempotencyKey("account-1".to_owned()),
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(confirmed.id),
            next_accounted: amount(100),
            recorded_at: 1_021,
        })
        .await?;
    let accounted = match accounted {
        ApplyResult::Appended { entry } => entry,
        ApplyResult::AlreadyPresent { .. } => panic!("the first accounting command must append"),
    };

    let observation = reorg_observation();
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: None,
            observation: observation.clone(),
        })
        .await?;
    let reconciliation = ReconciliationCase {
        id: ReconciliationCaseId("reconciliation-1".to_owned()),
        deposit_id: created.deposit.id.clone(),
        triggering_event_id: observation.event.id.clone(),
        reason: ReconciliationReason::PostCreditReorg {
            accounted: amount(100),
            corrected_confirmed: amount(40),
        },
        state: ReconciliationState::Open,
        created_at: 1_032,
    };
    let projection = ProjectObservation {
        expected_cursor: None,
        through: EventCursor(1),
        ledger_updates: vec![RecordObservationBalance {
            projection_id: ProjectionId("reorg-projection-1".to_owned()),
            event_id: observation.event.id,
            observation_revision: observation.event.transaction.revision,
            status: observation.event.transaction.status,
            kind: LedgerObservationKind::Reorg,
            movement_ids: Vec::new(),
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(accounted.id),
            next_balances: DepositBalances {
                received: amount(40),
                confirmed: amount(40),
                balance: amount(40),
                collected: amount(0),
                accounted: amount(100),
            },
            recorded_at: 1_032,
        }],
        reconciliation_cases: vec![reconciliation.clone()],
    };

    let result = repository.project_and_advance(projection.clone()).await?;
    assert_eq!(result.checkpoint.cursor, Some(EventCursor(1)));
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
            received: amount(40),
            confirmed: amount(40),
            balance: amount(40),
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
            idempotency_key: IdempotencyKey("account-while-blocked".to_owned()),
            deposit_id: created.deposit.id.clone(),
            expected_head: Some(current.id),
            next_accounted: amount(40),
            recorded_at: 1_033,
        })
        .await
        .expect_err("automatic accounting must stop during reconciliation");
    assert_eq!(blocked.kind, DepositErrorKind::InvalidState);

    let resolved = repository
        .resolve_case(
            &reconciliation.id,
            "operator accepted liability".to_owned(),
            1_040,
        )
        .await?;
    assert_eq!(
        resolved.state,
        ReconciliationState::Resolved {
            resolution: "operator accepted liability".to_owned(),
            resolved_at: 1_040,
        }
    );
    assert_eq!(
        repository
            .resolve_case(
                &reconciliation.id,
                "operator accepted liability".to_owned(),
                1_040
            )
            .await?,
        resolved
    );
    assert!(
        !repository
            .automatic_actions_blocked(&created.deposit.id)
            .await?
    );
    Ok(())
}
