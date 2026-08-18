use base::Decimal;
use deposits::KeyId;
use deposits::{
    AcceptBroadcast, ApplyResult, AttachWatch, BatchJob, BatchParticipant, CloseDeposit,
    CollectionAllocation, CollectionCreator, CollectionError, CollectionHistory, CollectionId,
    CollectionLegKind, CollectionLegState, CollectionQuery, CollectionReader,
    CollectionReservationState, CollectionRetry, CollectionState, CommandIdentity,
    CommandOperation, CommandPrincipal, CreateBatch, CreateLeg, DatabaseInitializer,
    DepositCreator, DepositId, DepositLifecycle, DepositPlan, DepositReader, DepositState, EntryId,
    EventProjector, FailLeg, IdempotencyKey, InitializeDatabase, JobAssociations, JobCommands,
    JobId, JobPayload, JobPlan, JobQuery, LedgerEffect, LedgerReader, LegId, LegOutcome,
    MirrorObservation, OpenDeposit, PaymentStore, PolicyIdentity, ProgressReader, ProjectBatch,
    ProjectObservation, ProjectionFeeTreatment, RecordObservation, RecordSignature,
    ReleaseReservation, RequestHash, ResourceId, ResourceProof, RetryLeg, SignedBytes,
    SpendResource, SubmissionWriter, TransitionGuard, User, UserId, UserStore,
    UtxoBatchProjectionTransition, WatchQueue,
};
use indexing::{AssetId, CanonicalAddress, ChainId, TransactionRef};
use indexing::{
    BlockHash, BlockHeight, BlockRef, ConfirmationProof, EventCursor, EventId, IndexScope,
    MovementId, NetworkFee, ObservationEvent, ObservationRevision, ObservedTransaction,
    TransactionStatus, ValueMovement, WatchId,
};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;

type Repository = PaymentStore<RocksDb>;

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
            network: "regtest".to_owned(),
        },
        value: value.to_owned(),
    }
}

fn transaction(value: &str) -> TransactionRef {
    TransactionRef {
        scope: IndexScope {
            chain: chain(),
            network: "regtest".to_owned(),
        },
        value: value.to_owned(),
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

fn policy() -> PolicyIdentity {
    PolicyIdentity {
        version: "utxo-policy-v1".to_owned(),
        digest: [11; 32],
    }
}

fn guard(collection: &deposits::Collection) -> TransitionGuard {
    TransitionGuard {
        collection_state: collection.state,
        leg_state: collection.legs[0].state.clone(),
    }
}

fn resource(txid: &str, output_index: u32, value: u64) -> SpendResource {
    SpendResource {
        id: ResourceId {
            transaction_id: transaction(txid),
            output_index,
        },
        amount: amount(value),
        evidence: ResourceProof::new(
            format!("utxo-evidence-v1:{txid}:{output_index}:{value}").into_bytes(),
        )
        .expect("test evidence is bounded"),
    }
}

fn allocation(deposit_id: &str, gross: u64, fee: u64) -> CollectionAllocation {
    CollectionAllocation {
        deposit_id: DepositId(deposit_id.to_owned()),
        asset: asset(),
        gross_debit: amount(gross),
        master_credit: amount(gross - fee),
        allocated_fee_asset: asset(),
        allocated_fee: amount(fee),
    }
}

fn batch_command(
    collection_id: &str,
    job_id: &str,
    participants: Vec<(&str, &str, EntryId, Vec<SpendResource>)>,
) -> CreateBatch {
    CreateBatch {
        id: CollectionId(collection_id.to_owned()),
        job_id: JobId(job_id.to_owned()),
        asset: asset(),
        destination: address("bcrt1qmaster0000000000000000000000000000000"),
        policy: policy(),
        participants: participants
            .into_iter()
            .map(
                |(user_id, deposit_id, expected_ledger_head, spend_resources)| BatchParticipant {
                    user_id: UserId(user_id.to_owned()),
                    deposit_id: DepositId(deposit_id.to_owned()),
                    expected_ledger_head,
                    reservation_amount: spend_resources.iter().fold(
                        Decimal::zero(),
                        |total, resource| {
                            total
                                .checked_add(&resource.amount)
                                .expect("test resource sum fits")
                        },
                    ),
                    spend_resources,
                },
            )
            .collect(),
        leg: CreateLeg {
            id: LegId("sweep".to_owned()),
            kind: CollectionLegKind::Sweep,
            planned_amount: None,
        },
        created_at: 20,
    }
}

fn safe_reorg() -> CollectionError {
    CollectionError {
        code: "chain_reorg".to_owned(),
        message: "canonical UTXO transaction was reorged".to_owned(),
        retryable: true,
    }
}

fn deposit_command(id: &str, user: &str, deposit_address: &str, expected: u64) -> OpenDeposit {
    OpenDeposit {
        deposit: DepositPlan {
            id: DepositId(id.to_owned()),
            idempotency_key: IdempotencyKey(format!("create-{id}")),
            user_id: UserId(user.to_owned()),
            asset: asset(),
            address: address(deposit_address),
            key: KeyId::Identifier(format!("key-{id}")),
            key_purpose: "utxo-payment-deposit-v1".to_owned(),
            expected: amount(expected),
            birthday: BlockHeight(1),
            expires_at: 10_000,
            created_at: 10,
        },
        ledger_recorded_at: 10,
    }
}

#[allow(clippy::too_many_arguments)]
fn observation(
    id: &str,
    cursor: u64,
    txid: &str,
    revision: u64,
    status: TransactionStatus,
    previous_status: Option<TransactionStatus>,
    movements: Vec<ValueMovement>,
    fee: Option<NetworkFee>,
) -> deposits::MirroredObservation {
    deposits::MirroredObservation {
        event: ObservationEvent {
            id: EventId(id.to_owned()),
            cursor: EventCursor(cursor),
            watch_ids: vec![WatchId("collection-watch".to_owned())],
            previous_status,
            transaction: ObservedTransaction {
                scope: IndexScope {
                    chain: chain(),
                    network: "regtest".to_owned(),
                },
                transaction_id: transaction(txid),
                revision: ObservationRevision(revision),
                status,
                movements,
                fee,
                first_seen_at: 100,
                observed_at: 100 + revision,
            },
        },
        received_at: 200 + revision,
    }
}

fn input_movements() -> Vec<ValueMovement> {
    vec![
        ValueMovement::Input {
            id: MovementId("input-a".to_owned()),
            asset: asset(),
            amount: amount(1_000),
            owner: Some(address("bcrt1qdeposit-a")),
        },
        ValueMovement::Input {
            id: MovementId("input-b".to_owned()),
            asset: asset(),
            amount: amount(2_000),
            owner: Some(address("bcrt1qdeposit-b")),
        },
    ]
}

fn collection_network_fee() -> Option<NetworkFee> {
    Some(NetworkFee {
        asset: asset(),
        amount: amount(30),
        payer: None,
    })
}

fn collection_projection(
    event: &deposits::MirroredObservation,
    expected_cursor: Option<EventCursor>,
    heads: &[deposits::EntryId],
    transition: UtxoBatchProjectionTransition,
    expected: TransitionGuard,
) -> ProjectBatch {
    ProjectBatch {
        projection: ProjectObservation {
            expected_cursor,
            through: event.event.cursor,
            affected_deposits: vec![
                DepositId("deposit-a".to_owned()),
                DepositId("deposit-b".to_owned()),
            ],
            ledger_updates: vec![
                RecordObservation {
                    event_id: event.event.id.clone(),
                    effect: LedgerEffect::Collection {
                        movements: vec![MovementId("input-a".to_owned())],
                    },
                    deposit_id: DepositId("deposit-a".to_owned()),
                    expected_head: Some(heads[0].clone()),
                    recorded_at: 300 + event.event.cursor.0,
                },
                RecordObservation {
                    event_id: event.event.id.clone(),
                    effect: LedgerEffect::Collection {
                        movements: vec![MovementId("input-b".to_owned())],
                    },
                    deposit_id: DepositId("deposit-b".to_owned()),
                    expected_head: Some(heads[1].clone()),
                    recorded_at: 300 + event.event.cursor.0,
                },
            ],
            reconciliation_cases: Vec::new(),
            fee_treatment: ProjectionFeeTreatment::IncludedInMovementEffect,
            utxo_batch_transition: None,
        },
        collection_id: CollectionId("batch-1".to_owned()),
        leg_id: LegId("sweep".to_owned()),
        expected,
        transaction_id: transaction("collection-tx"),
        transition,
    }
}

fn appended_heads(outcome: &deposits::BatchOutcome) -> Vec<deposits::EntryId> {
    outcome
        .projection
        .ledger_results
        .iter()
        .map(|result| match result {
            ApplyResult::Appended { entry } | ApplyResult::AlreadyPresent { entry } => {
                entry.id.clone()
            }
        })
        .collect()
}

#[tokio::test]
async fn batch_creation_is_atomic_canonical_and_indexed_for_every_participant()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = Repository::new(RocksDb::open(directory.path())?);
    let owner = CommandPrincipal("exchange-principal".to_owned());
    let mut initial_heads = std::collections::BTreeMap::new();
    for (user_id, deposit_id, deposit_address, expected) in [
        ("user-a", "deposit-a", "bcrt1qdeposit-a", 50),
        ("user-b", "deposit-b", "bcrt1qdeposit-b", 60),
        ("user-c", "deposit-c", "bcrt1qdeposit-c", 50),
        ("user-d", "deposit-d", "bcrt1qdeposit-d", 70),
    ] {
        repository
            .ensure_user(User {
                id: UserId(user_id.to_owned()),
                owner: owner.clone(),
                first_seen_at: 1,
            })
            .await?;
        let created = repository
            .create_with_ledger(deposit_command(
                deposit_id,
                user_id,
                deposit_address,
                expected,
            ))
            .await?;
        initial_heads.insert(deposit_id, created.ledger.id);
    }
    for (job_id, collection_id, deposit_ids) in [
        (
            "job-left",
            "batch-left",
            vec![
                DepositId("deposit-a".to_owned()),
                DepositId("deposit-b".to_owned()),
            ],
        ),
        (
            "job-right",
            "batch-right",
            vec![
                DepositId("deposit-c".to_owned()),
                DepositId("deposit-d".to_owned()),
            ],
        ),
    ] {
        repository
            .create_or_replay(JobPlan {
                id: JobId(job_id.to_owned()),
                command: CommandIdentity {
                    principal: owner.clone(),
                    operation: CommandOperation::CollectionPlan,
                    client_key: IdempotencyKey(format!("command-{job_id}")),
                    request_hash: RequestHash([job_id.as_bytes()[4]; 32]),
                },
                payload: JobPayload::CreateBatch(BatchJob {
                    collection_id: CollectionId(collection_id.to_owned()),
                    deposit_ids,
                }),
                user_owner: owner.clone(),
                policy: policy(),
                created_at: 19,
            })
            .await?;
    }
    let shared = resource("funding-shared", 0, 50);
    let left = batch_command(
        "batch-left",
        "job-left",
        vec![
            (
                "user-a",
                "deposit-a",
                initial_heads["deposit-a"].clone(),
                vec![shared.clone()],
            ),
            (
                "user-b",
                "deposit-b",
                initial_heads["deposit-b"].clone(),
                vec![resource("funding-b", 0, 60)],
            ),
        ],
    );
    let right = batch_command(
        "batch-right",
        "job-right",
        vec![
            (
                "user-c",
                "deposit-c",
                initial_heads["deposit-c"].clone(),
                vec![shared],
            ),
            (
                "user-d",
                "deposit-d",
                initial_heads["deposit-d"].clone(),
                vec![resource("funding-d", 0, 70)],
            ),
        ],
    );

    let (left_result, right_result) = tokio::join!(
        repository.create_or_replay_utxo_batch(left.clone()),
        repository.create_or_replay_utxo_batch(right.clone())
    );
    assert_ne!(left_result.is_ok(), right_result.is_ok());
    let (winner, loser) = if left_result.is_ok() {
        (left, right)
    } else {
        (right, left)
    };
    let winner_collection = repository
        .collection(&winner.id)
        .await?
        .expect("one whole batch wins");
    for participant in &winner_collection.participants {
        let indexed = repository
            .collections_for_deposit(
                &participant.reservation.deposit_id,
                CollectionQuery {
                    after: None,
                    limit: 10,
                },
            )
            .await?;
        assert_eq!(indexed.collections, vec![winner_collection.clone()]);
    }
    assert!(repository.collection(&loser.id).await?.is_none());
    for participant in loser.participants {
        assert!(
            repository
                .collections_for_deposit(
                    &participant.deposit_id,
                    CollectionQuery {
                        after: None,
                        limit: 10,
                    },
                )
                .await?
                .collections
                .is_empty()
        );
    }
    drop(repository);
    let reopened = Repository::new(RocksDb::open(directory.path())?);
    assert_eq!(
        reopened.collection(&winner.id).await?,
        Some(winner_collection)
    );
    Ok(())
}

#[tokio::test]
async fn intervening_ledger_projection_rejects_stale_utxo_reservation_without_partial_state()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = Repository::new(RocksDb::open(directory.path())?);
    let owner = CommandPrincipal("exchange-principal".to_owned());
    repository
        .ensure_user(User {
            id: UserId("user-a".to_owned()),
            owner: owner.clone(),
            first_seen_at: 1,
        })
        .await?;
    let created = repository
        .create_with_ledger(deposit_command(
            "deposit-a",
            "user-a",
            "bcrt1qdeposit-a",
            50,
        ))
        .await?;
    repository
        .create_or_replay(JobPlan {
            id: JobId("job-stale".to_owned()),
            command: CommandIdentity {
                principal: owner.clone(),
                operation: CommandOperation::CollectionPlan,
                client_key: IdempotencyKey("command-stale".to_owned()),
                request_hash: RequestHash([9; 32]),
            },
            payload: JobPayload::CreateBatch(BatchJob {
                collection_id: CollectionId("batch-stale".to_owned()),
                deposit_ids: vec![created.deposit.id.clone()],
            }),
            user_owner: owner,
            policy: policy(),
            created_at: 19,
        })
        .await?;
    let selected_head = created.ledger.id;
    let funding = observation(
        "funding-after-selection",
        1,
        "funding-after-selection-tx",
        1,
        TransactionStatus::Included {
            block: block(10, 10),
            confirmations: 1,
        },
        None,
        vec![ValueMovement::Output {
            id: MovementId("funding-after-selection-output".to_owned()),
            asset: asset(),
            amount: amount(50),
            owner: Some(created.deposit.address.clone()),
        }],
        None,
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: None,
            observation: funding.clone(),
        })
        .await?;
    repository
        .project_and_advance(ProjectObservation {
            expected_cursor: None,
            through: EventCursor(1),
            affected_deposits: vec![created.deposit.id.clone()],
            ledger_updates: vec![RecordObservation {
                event_id: funding.event.id,
                effect: LedgerEffect::Incoming {
                    movements: vec![MovementId("funding-after-selection-output".to_owned())],
                },
                deposit_id: created.deposit.id.clone(),
                expected_head: Some(selected_head.clone()),
                recorded_at: 30,
            }],
            reconciliation_cases: Vec::new(),
            fee_treatment: ProjectionFeeTreatment::Separate,
            utxo_batch_transition: None,
        })
        .await?;

    let command = batch_command(
        "batch-stale",
        "job-stale",
        vec![(
            "user-a",
            "deposit-a",
            selected_head,
            vec![resource("funding-after-selection-tx", 0, 50)],
        )],
    );
    let error = repository
        .create_or_replay_utxo_batch(command)
        .await
        .expect_err("ledger movement after selection must stale the reservation fence");
    assert_eq!(error.kind, deposits::DepositErrorKind::Conflict);
    assert!(
        repository
            .collection(&CollectionId("batch-stale".to_owned()))
            .await?
            .is_none()
    );
    assert!(
        repository
            .collections_for_deposit(
                &created.deposit.id,
                CollectionQuery {
                    after: None,
                    limit: 10,
                },
            )
            .await?
            .collections
            .is_empty()
    );
    assert!(
        repository
            .active_collection_for(&created.deposit.id, &asset())
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn signed_utxo_batch_rejects_terminal_failure_and_exact_resource_release()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = Repository::new(RocksDb::open(directory.path())?);
    let owner = CommandPrincipal("exchange-principal".to_owned());
    let mut deposits = Vec::new();
    for (user_id, deposit_id, deposit_address) in [
        ("user-a", "deposit-a", "bcrt1qdeposit-a"),
        ("user-b", "deposit-b", "bcrt1qdeposit-b"),
    ] {
        repository
            .ensure_user(User {
                id: UserId(user_id.to_owned()),
                owner: owner.clone(),
                first_seen_at: 1,
            })
            .await?;
        deposits.push(
            repository
                .create_with_ledger(deposit_command(deposit_id, user_id, deposit_address, 1_000))
                .await?,
        );
    }
    let active = repository
        .activate_watch(
            &deposits[0].deposit.id,
            &deposits[0].deposit.idempotency_key,
            WatchId("deposit-watch-a".to_owned()),
        )
        .await?;
    for (job_id, collection_id, deposit_id) in [
        ("job-a", "batch-a", "deposit-a"),
        ("job-b", "batch-b", "deposit-b"),
    ] {
        repository
            .create_or_replay(JobPlan {
                id: JobId(job_id.to_owned()),
                command: CommandIdentity {
                    principal: owner.clone(),
                    operation: CommandOperation::CollectionPlan,
                    client_key: IdempotencyKey(format!("command-{job_id}")),
                    request_hash: RequestHash([job_id.as_bytes()[4]; 32]),
                },
                payload: JobPayload::CreateBatch(BatchJob {
                    collection_id: CollectionId(collection_id.to_owned()),
                    deposit_ids: vec![DepositId(deposit_id.to_owned())],
                }),
                user_owner: owner.clone(),
                policy: policy(),
                created_at: 19,
            })
            .await?;
    }

    let exact_resource = resource("shared-funding", 0, 1_000);
    let created = repository
        .create_or_replay_utxo_batch(batch_command(
            "batch-a",
            "job-a",
            vec![(
                "user-a",
                "deposit-a",
                deposits[0].ledger.id.clone(),
                vec![exact_resource.clone()],
            )],
        ))
        .await?
        .collection()
        .clone();
    let signed = repository
        .record_signed(RecordSignature {
            collection_id: created.id.clone(),
            leg_id: created.legs[0].id.clone(),
            expected: guard(&created),
            expected_transaction_id: transaction("collection-tx"),
            envelope: SignedBytes::new(vec![1, 2, 3, 4])?,
            allocations: vec![allocation("deposit-a", 1_000, 10)],
            fee_limit: None,
            signed_at: 40,
            expires_at: u64::MAX,
        })
        .await?;
    let broadcast = repository
        .accept_broadcast(AcceptBroadcast {
            collection_id: signed.id.clone(),
            leg_id: signed.legs[0].id.clone(),
            expected: guard(&signed),
            transaction_id: transaction("collection-tx"),
            accepted_at: 41,
        })
        .await?;

    let active_close = repository
        .close(CloseDeposit {
            deposit_id: active.id,
            expected_state: active.state,
            expected_ledger_head: deposits[0].ledger.id.clone(),
        })
        .await
        .expect_err("an active UTXO reservation must still block deposit closure");
    assert_eq!(active_close.kind, deposits::DepositErrorKind::InvalidState);

    let terminal_failure = repository
        .fail_leg(FailLeg {
            collection_id: broadcast.id.clone(),
            leg_id: broadcast.legs[0].id.clone(),
            expected: guard(&broadcast),
            transaction_id: transaction("collection-tx"),
            error: CollectionError {
                code: "terminal_failure".to_owned(),
                message: "test terminal failure".to_owned(),
                retryable: false,
            },
            failed_at: 42,
        })
        .await
        .expect_err("signed UTXO batches must not enter a releasable failure state");
    assert_eq!(
        terminal_failure.kind,
        deposits::DepositErrorKind::InvalidState
    );
    let release = repository
        .release_reservation(ReleaseReservation {
            collection_id: broadcast.id.clone(),
            expected_collection_state: broadcast.state,
            expected_reservation_state: CollectionReservationState::Active,
            reason: deposits::ReservationReleaseReason::TerminalFailure,
            released_at: 43,
        })
        .await
        .expect_err("UTXO exact-resource ownership must never use generic release");
    assert_eq!(release.kind, deposits::DepositErrorKind::InvalidState);
    assert_eq!(
        repository.collection(&broadcast.id).await?,
        Some(broadcast.clone())
    );
    assert!(
        repository
            .signed_envelope(&broadcast.id, &broadcast.legs[0].id)
            .await?
            .is_some()
    );
    assert_eq!(
        repository
            .retained_collection_for(&deposits[0].deposit.id, &asset())
            .await?,
        Some(broadcast)
    );

    let overlap = repository
        .create_or_replay_utxo_batch(batch_command(
            "batch-b",
            "job-b",
            vec![(
                "user-b",
                "deposit-b",
                deposits[1].ledger.id.clone(),
                vec![exact_resource],
            )],
        ))
        .await
        .expect_err("the retained exact outpoint must block another deposit's batch");
    assert_eq!(overlap.kind, deposits::DepositErrorKind::Conflict);
    Ok(())
}

#[tokio::test]
async fn retained_transaction_survives_confirm_reorg_retry_reinclude_and_replay()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = Repository::new(RocksDb::open(directory.path())?);
    let owner = CommandPrincipal("exchange-principal".to_owned());
    repository
        .initialize_or_validate(InitializeDatabase {
            scope: IndexScope {
                chain: chain(),
                network: "regtest".to_owned(),
            },
            active_policy: policy(),
            initialized_at: 1,
        })
        .await?;
    for user in ["user-a", "user-b"] {
        repository
            .ensure_user(User {
                id: UserId(user.to_owned()),
                owner: owner.clone(),
                first_seen_at: 2,
            })
            .await?;
    }
    let deposit_a = repository
        .create_with_ledger(deposit_command(
            "deposit-a",
            "user-a",
            "bcrt1qdeposit-a",
            1_000,
        ))
        .await?;
    let deposit_b = repository
        .create_with_ledger(deposit_command(
            "deposit-b",
            "user-b",
            "bcrt1qdeposit-b",
            2_000,
        ))
        .await?;
    let active_a = repository
        .activate_watch(
            &deposit_a.deposit.id,
            &deposit_a.deposit.idempotency_key,
            WatchId("deposit-watch-a".to_owned()),
        )
        .await?;
    repository
        .activate_watch(
            &deposit_b.deposit.id,
            &deposit_b.deposit.idempotency_key,
            WatchId("deposit-watch-b".to_owned()),
        )
        .await?;
    let job = repository
        .create_or_replay(JobPlan {
            id: JobId("batch-job".to_owned()),
            command: CommandIdentity {
                principal: owner.clone(),
                operation: CommandOperation::CollectionPlan,
                client_key: IdempotencyKey("batch-command".to_owned()),
                request_hash: RequestHash([3; 32]),
            },
            payload: JobPayload::CreateBatch(BatchJob {
                collection_id: CollectionId("batch-1".to_owned()),
                deposit_ids: vec![deposit_a.deposit.id.clone(), deposit_b.deposit.id.clone()],
            }),
            user_owner: owner,
            policy: policy(),
            created_at: 20,
        })
        .await?
        .job()
        .clone();
    for user_id in ["user-a", "user-b"] {
        assert_eq!(
            repository
                .jobs_for_user(
                    &UserId(user_id.to_owned()),
                    JobQuery {
                        after: None,
                        limit: 10,
                    },
                )
                .await?
                .jobs,
            vec![job.clone()]
        );
    }

    let funding = observation(
        "funding-event",
        1,
        "funding-tx",
        1,
        TransactionStatus::Confirmed {
            block: block(10, 10),
            proof: ConfirmationProof::Depth {
                required: 1,
                observed: 1,
            },
        },
        None,
        vec![
            ValueMovement::Output {
                id: MovementId("fund-a".to_owned()),
                asset: asset(),
                amount: amount(1_000),
                owner: Some(deposit_a.deposit.address.clone()),
            },
            ValueMovement::Output {
                id: MovementId("fund-b".to_owned()),
                asset: asset(),
                amount: amount(2_000),
                owner: Some(deposit_b.deposit.address.clone()),
            },
        ],
        None,
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
            affected_deposits: vec![deposit_a.deposit.id.clone(), deposit_b.deposit.id.clone()],
            ledger_updates: vec![
                RecordObservation {
                    event_id: funding.event.id.clone(),
                    effect: LedgerEffect::Incoming {
                        movements: vec![MovementId("fund-a".to_owned())],
                    },
                    deposit_id: deposit_a.deposit.id.clone(),
                    expected_head: Some(deposit_a.ledger.id),
                    recorded_at: 30,
                },
                RecordObservation {
                    event_id: funding.event.id.clone(),
                    effect: LedgerEffect::Incoming {
                        movements: vec![MovementId("fund-b".to_owned())],
                    },
                    deposit_id: deposit_b.deposit.id.clone(),
                    expected_head: Some(deposit_b.ledger.id),
                    recorded_at: 30,
                },
            ],
            reconciliation_cases: Vec::new(),
            fee_treatment: ProjectionFeeTreatment::Separate,
            utxo_batch_transition: None,
        })
        .await?;
    let mut heads = funded
        .ledger_results
        .into_iter()
        .map(|result| match result {
            ApplyResult::Appended { entry } | ApplyResult::AlreadyPresent { entry } => entry.id,
        })
        .collect::<Vec<_>>();

    let command = batch_command(
        "batch-1",
        &job.id.0,
        vec![
            (
                "user-a",
                "deposit-a",
                heads[0].clone(),
                vec![resource("funding-tx", 0, 1_000)],
            ),
            (
                "user-b",
                "deposit-b",
                heads[1].clone(),
                vec![resource("funding-tx", 1, 2_000)],
            ),
        ],
    );
    let created = repository
        .create_or_replay_utxo_batch(command.clone())
        .await?
        .collection()
        .clone();
    assert_eq!(
        repository
            .active_collection_for(created.deposit_id(), &created.asset)
            .await?,
        Some(created.clone())
    );
    assert_eq!(
        repository
            .create_or_replay_utxo_batch(command)
            .await?
            .collection(),
        &created
    );
    let allocations = vec![
        allocation("deposit-a", 1_000, 10),
        allocation("deposit-b", 2_000, 20),
    ];
    let signed = repository
        .record_signed(RecordSignature {
            collection_id: created.id.clone(),
            leg_id: created.legs[0].id.clone(),
            expected: guard(&created),
            expected_transaction_id: transaction("collection-tx"),
            envelope: SignedBytes::new(vec![1, 2, 3, 4])?,
            allocations: allocations.clone(),
            fee_limit: None,
            signed_at: 40,
            expires_at: 10_000,
        })
        .await?;
    let broadcast = repository
        .accept_broadcast(AcceptBroadcast {
            collection_id: signed.id.clone(),
            leg_id: signed.legs[0].id.clone(),
            expected: guard(&signed),
            transaction_id: transaction("collection-tx"),
            accepted_at: 41,
        })
        .await?;
    assert!(
        repository
            .signed_envelope(&broadcast.id, &broadcast.legs[0].id)
            .await?
            .is_some()
    );
    let mut collection = repository
        .attach_watch(AttachWatch {
            collection_id: broadcast.id.clone(),
            leg_id: broadcast.legs[0].id.clone(),
            expected: guard(&broadcast),
            watch_id: WatchId("collection-watch".to_owned()),
            updated_at: 42,
        })
        .await?;

    let confirmed_status = TransactionStatus::Confirmed {
        block: block(20, 20),
        proof: ConfirmationProof::Depth {
            required: 1,
            observed: 1,
        },
    };
    let confirmed = observation(
        "collection-confirmed-1",
        2,
        "collection-tx",
        1,
        confirmed_status.clone(),
        None,
        input_movements(),
        Some(NetworkFee {
            asset: asset(),
            amount: amount(30),
            payer: Some(deposit_a.deposit.address.clone()),
        }),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(1)),
            observation: confirmed.clone(),
        })
        .await?;
    let mut stale_heads = heads.clone();
    stale_heads[1] = deposits::EntryId("stale-head".to_owned());
    let atomic_error = repository
        .project_utxo_batch_and_advance(collection_projection(
            &confirmed,
            Some(EventCursor(1)),
            &stale_heads,
            UtxoBatchProjectionTransition::Confirmed {
                allocations: allocations.clone(),
                confirmed_at: 50,
            },
            guard(&collection),
        ))
        .await
        .expect_err("one stale participant must reject the entire semantic commit");
    assert!(matches!(
        atomic_error.kind,
        deposits::DepositErrorKind::Conflict | deposits::DepositErrorKind::NotFound
    ));
    assert_eq!(
        repository
            .consumer_checkpoint(deposits::ConsumerCheckpointName::IxProjection)
            .await?
            .cursor,
        Some(EventCursor(1))
    );
    assert_eq!(
        repository.collection(&collection.id).await?,
        Some(collection.clone())
    );
    assert_eq!(
        repository
            .current(&DepositId("deposit-a".to_owned()))
            .await?
            .expect("first ledger remains unchanged")
            .id,
        heads[0]
    );
    let confirmed_outcome = repository
        .project_utxo_batch_and_advance(collection_projection(
            &confirmed,
            Some(EventCursor(1)),
            &heads,
            UtxoBatchProjectionTransition::Confirmed {
                allocations: allocations.clone(),
                confirmed_at: 50,
            },
            guard(&collection),
        ))
        .await?;
    heads = appended_heads(&confirmed_outcome);
    collection = confirmed_outcome.collection;
    assert_eq!(collection.state, CollectionState::Completed);
    assert!(collection.participants.iter().all(|participant| matches!(
        participant.reservation.state,
        CollectionReservationState::Consumed { .. }
    )));
    assert!(
        repository
            .active_collection_for(collection.deposit_id(), &collection.asset)
            .await?
            .is_none()
    );
    assert_eq!(
        repository
            .retained_collection_for(collection.deposit_id(), &collection.asset)
            .await?,
        Some(collection.clone())
    );
    for deposit_id in [
        DepositId("deposit-a".to_owned()),
        DepositId("deposit-b".to_owned()),
    ] {
        let ledger = repository
            .current(&deposit_id)
            .await?
            .expect("participant ledger exists");
        assert_eq!(ledger.balances.balance, Decimal::zero());
        assert_ne!(ledger.balances.collected, Decimal::zero());
        assert_eq!(
            match &ledger.cause {
                deposits::LedgerEntryCause::Observation { network_fee, .. } => network_fee.clone(),
                _ => None,
            },
            None,
            "UTXO fee allocation must not double-debit a participant ledger"
        );
    }
    repository
        .close(CloseDeposit {
            deposit_id: active_a.id.clone(),
            expected_state: active_a.state,
            expected_ledger_head: heads[0].clone(),
        })
        .await?;
    assert_eq!(
        repository
            .deposit(&active_a.id)
            .await?
            .expect("consumed UTXO owner permits guarded closure")
            .state,
        DepositState::Closed
    );
    assert_eq!(
        repository
            .retained_collection_for(&active_a.id, &asset())
            .await?,
        Some(collection.clone())
    );
    assert!(
        repository
            .signed_envelope(&collection.id, &collection.legs[0].id)
            .await?
            .is_some()
    );

    let reorg_status = TransactionStatus::Reorged {
        previous_block: block(20, 20),
    };
    let reorged = observation(
        "collection-reorged-1",
        3,
        "collection-tx",
        2,
        reorg_status.clone(),
        Some(confirmed_status.clone()),
        input_movements(),
        collection_network_fee(),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(2)),
            observation: reorged.clone(),
        })
        .await?;
    let reorged_outcome = repository
        .project_utxo_batch_and_advance(collection_projection(
            &reorged,
            Some(EventCursor(2)),
            &heads,
            UtxoBatchProjectionTransition::Reorged {
                error: safe_reorg(),
                reorged_at: 60,
            },
            guard(&collection),
        ))
        .await?;
    heads = appended_heads(&reorged_outcome);
    collection = reorged_outcome.collection;
    assert_eq!(collection.state, CollectionState::Reorged);
    assert_eq!(
        repository
            .deposit(&active_a.id)
            .await?
            .expect("reorg projection preserves the closed lifecycle state")
            .state,
        DepositState::Closed
    );
    assert_eq!(
        repository
            .current(&active_a.id)
            .await?
            .expect("reorg projection restores the closed deposit balance")
            .balances
            .balance,
        amount(1_000)
    );
    assert_eq!(
        repository
            .active_collection_for(collection.deposit_id(), &collection.asset)
            .await?,
        Some(collection.clone())
    );
    let release_error = repository
        .release_reservation(ReleaseReservation {
            collection_id: collection.id.clone(),
            expected_collection_state: CollectionState::Reorged,
            expected_reservation_state: CollectionReservationState::Active,
            reason: deposits::ReservationReleaseReason::Reorg,
            released_at: 61,
        })
        .await
        .expect_err("reorg must retain exact UTXO ownership");
    assert_eq!(release_error.kind, deposits::DepositErrorKind::InvalidState);

    let retained_before_retry = repository
        .signed_envelope(&collection.id, &collection.legs[0].id)
        .await?
        .expect("reorg retains the exact signed envelope");
    let retry_command = RetryLeg {
        collection_id: collection.id.clone(),
        leg_id: collection.legs[0].id.clone(),
        expected: guard(&collection),
        updated_at: 62,
    };
    collection = repository.retry_leg(retry_command.clone()).await?;
    assert_eq!(
        collection.legs[0].state,
        CollectionLegState::Signed {
            transaction_id: transaction("collection-tx")
        }
    );
    assert_eq!(collection.legs[0].allocations, allocations);
    assert!(
        repository
            .signed_envelope(&collection.id, &collection.legs[0].id)
            .await?
            .expect("retry retains signed bytes")
            .bytes
            == retained_before_retry.bytes
    );
    collection = repository
        .accept_broadcast(AcceptBroadcast {
            collection_id: collection.id.clone(),
            leg_id: collection.legs[0].id.clone(),
            expected: guard(&collection),
            transaction_id: transaction("collection-tx"),
            accepted_at: 63,
        })
        .await?;
    assert!(matches!(
        collection.legs[0].state,
        CollectionLegState::Broadcast { .. }
    ));
    assert_eq!(
        repository.retry_leg(retry_command).await?,
        collection,
        "retry replay must not regress an already-rebroadcast leg"
    );

    let confirmed_again = observation(
        "collection-confirmed-2",
        4,
        "collection-tx",
        3,
        confirmed_status.clone(),
        Some(reorg_status.clone()),
        input_movements(),
        collection_network_fee(),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(3)),
            observation: confirmed_again.clone(),
        })
        .await?;
    let outcome = repository
        .project_utxo_batch_and_advance(collection_projection(
            &confirmed_again,
            Some(EventCursor(3)),
            &heads,
            UtxoBatchProjectionTransition::Confirmed {
                allocations: allocations.clone(),
                confirmed_at: 70,
            },
            guard(&collection),
        ))
        .await?;
    heads = appended_heads(&outcome);
    collection = outcome.collection;

    let reorged_again = observation(
        "collection-reorged-2",
        5,
        "collection-tx",
        4,
        reorg_status.clone(),
        Some(confirmed_status.clone()),
        input_movements(),
        collection_network_fee(),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(4)),
            observation: reorged_again.clone(),
        })
        .await?;
    let outcome = repository
        .project_utxo_batch_and_advance(collection_projection(
            &reorged_again,
            Some(EventCursor(4)),
            &heads,
            UtxoBatchProjectionTransition::Reorged {
                error: safe_reorg(),
                reorged_at: 80,
            },
            guard(&collection),
        ))
        .await?;
    heads = appended_heads(&outcome);
    collection = outcome.collection;

    let included_status = TransactionStatus::Included {
        block: block(21, 21),
        confirmations: 1,
    };
    let reincluded = observation(
        "collection-reincluded",
        6,
        "collection-tx",
        5,
        included_status.clone(),
        Some(reorg_status),
        input_movements(),
        collection_network_fee(),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(5)),
            observation: reincluded.clone(),
        })
        .await?;
    let outcome = repository
        .project_utxo_batch_and_advance(collection_projection(
            &reincluded,
            Some(EventCursor(5)),
            &heads,
            UtxoBatchProjectionTransition::Reincluded { included_at: 90 },
            guard(&collection),
        ))
        .await?;
    heads = appended_heads(&outcome);
    collection = outcome.collection;
    assert!(matches!(
        collection.legs[0].state,
        CollectionLegState::Broadcast { .. }
    ));

    let final_confirmation = observation(
        "collection-confirmed-3",
        7,
        "collection-tx",
        6,
        confirmed_status,
        Some(included_status),
        input_movements(),
        collection_network_fee(),
    );
    repository
        .mirror_and_advance(MirrorObservation {
            expected_cursor: Some(EventCursor(6)),
            observation: final_confirmation.clone(),
        })
        .await?;
    let final_command = collection_projection(
        &final_confirmation,
        Some(EventCursor(6)),
        &heads,
        UtxoBatchProjectionTransition::Confirmed {
            allocations: allocations.clone(),
            confirmed_at: 100,
        },
        guard(&collection),
    );
    let final_outcome = repository
        .project_utxo_batch_and_advance(final_command.clone())
        .await?;
    let replay = repository
        .project_utxo_batch_and_advance(final_command)
        .await?;
    assert!(
        replay
            .projection
            .ledger_results
            .iter()
            .all(|result| matches!(result, ApplyResult::AlreadyPresent { .. }))
    );
    assert_eq!(replay.collection, final_outcome.collection);
    assert!(
        repository
            .signed_envelope(
                &CollectionId("batch-1".to_owned()),
                &LegId("sweep".to_owned())
            )
            .await?
            .is_some()
    );
    drop(repository);
    let reopened = Repository::new(RocksDb::open(directory.path())?);
    assert_eq!(
        reopened
            .collection(&CollectionId("batch-1".to_owned()))
            .await?,
        Some(final_outcome.collection)
    );
    assert!(
        reopened
            .signed_envelope(
                &CollectionId("batch-1".to_owned()),
                &LegId("sweep".to_owned())
            )
            .await?
            .is_some()
    );
    for user_id in ["user-a", "user-b"] {
        assert_eq!(
            reopened
                .jobs_for_user(
                    &UserId(user_id.to_owned()),
                    JobQuery {
                        after: None,
                        limit: 10,
                    },
                )
                .await?
                .jobs,
            vec![job.clone()]
        );
    }
    Ok(())
}
