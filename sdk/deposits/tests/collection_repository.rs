use base::Decimal;
use deposits::{
    AcceptBroadcast, AttachWatch, CollectionAllocation, CollectionCreator, CollectionError,
    CollectionHistory, CollectionId, CollectionLegKind, CollectionLegState, CollectionMode,
    CollectionPlan, CollectionQuery, CollectionReservationState, CollectionRetry, CollectionState,
    ConfirmLeg, CreateCollectionOutcome, CreateLeg, DepositErrorKind, DepositId, FailLeg, JobId,
    LegId, LegOutcome, LegRef, PaymentStore, PolicyIdentity, RecordSignature, ReleaseReservation,
    ReorgLeg, ReservationReleaseReason, SignedBytes, SubmissionWriter, TransitionGuard, UserId,
};
use indexing::WatchId;
use indexing::{AssetId, CanonicalAddress, ChainId, TransactionRef};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;

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

fn create_collection(collection_id: &str, job_id: &str, deposit_id: &str) -> CollectionPlan {
    CollectionPlan {
        id: CollectionId(collection_id.to_owned()),
        job_id: JobId(job_id.to_owned()),
        user_id: UserId("user-1".to_owned()),
        deposit_id: DepositId(deposit_id.to_owned()),
        mode: CollectionMode::AccountTransfer,
        asset: asset(),
        destination: CanonicalAddress {
            scope: indexing::IndexScope {
                chain: chain(),
                network: "test".to_owned(),
            },
            value: "0x2222222222222222222222222222222222222222".to_owned(),
        },
        policy: PolicyIdentity {
            version: "policy-v1".to_owned(),
            digest: [7; 32],
        },
        reservation_amount: amount(100),
        legs: vec![CreateLeg {
            id: LegId("sweep".to_owned()),
            kind: CollectionLegKind::Sweep,
            planned_amount: None,
        }],
        created_at: 100,
    }
}

fn transaction(value: &str) -> TransactionRef {
    TransactionRef {
        scope: indexing::IndexScope {
            chain: chain(),
            network: "test".to_owned(),
        },
        value: value.to_owned(),
    }
}

fn guard(collection: &deposits::Collection, position: usize) -> TransitionGuard {
    TransitionGuard {
        collection_state: collection.state,
        leg_state: collection.legs[position].state.clone(),
    }
}

fn safe_error(code: &str) -> CollectionError {
    CollectionError {
        code: code.to_owned(),
        message: "safe diagnostic".to_owned(),
        retryable: true,
    }
}

async fn create_and_broadcast(
    repository: &PaymentStore<RocksDb>,
    command: CollectionPlan,
    transaction_id: TransactionRef,
) -> Result<deposits::Collection, Box<dyn std::error::Error>> {
    let created = repository
        .create_or_replay_collection(command)
        .await?
        .collection()
        .clone();
    let signed = repository
        .record_signed(RecordSignature {
            collection_id: created.id.clone(),
            leg_id: created.legs[0].id.clone(),
            expected: guard(&created, 0),
            expected_transaction_id: transaction_id.clone(),
            envelope: SignedBytes::new(vec![1, 2, 3, 4])?,
            allocations: Vec::new(),
            fee_limit: Some(amount(10)),
            signed_at: 110,
            expires_at: 1_000,
        })
        .await?;
    let broadcast = repository
        .accept_broadcast(AcceptBroadcast {
            collection_id: signed.id.clone(),
            leg_id: signed.legs[0].id.clone(),
            expected: guard(&signed, 0),
            transaction_id,
            accepted_at: 120,
        })
        .await?;
    Ok(broadcast)
}

async fn attach_watch(
    repository: &PaymentStore<RocksDb>,
    collection: deposits::Collection,
) -> Result<deposits::Collection, Box<dyn std::error::Error>> {
    Ok(repository
        .attach_watch(AttachWatch {
            collection_id: collection.id.clone(),
            leg_id: collection.legs[0].id.clone(),
            expected: guard(&collection, 0),
            watch_id: WatchId(format!("watch-{}", collection.id.0)),
            updated_at: 130,
        })
        .await?)
}

fn allocation(deposit_id: &DepositId) -> CollectionAllocation {
    CollectionAllocation {
        deposit_id: deposit_id.clone(),
        asset: asset(),
        gross_debit: amount(100),
        master_credit: amount(99),
        allocated_fee_asset: asset(),
        allocated_fee: amount(1),
    }
}

#[tokio::test]
async fn create_replays_exactly_rejects_conflicts_and_reserves_deposit_asset()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let command = create_collection("collection-1", "job-1", "deposit-1");

    let created = repository
        .create_or_replay_collection(command.clone())
        .await?;
    assert!(matches!(created, CreateCollectionOutcome::Created { .. }));
    let replayed = repository
        .create_or_replay_collection(command.clone())
        .await?;
    assert!(matches!(replayed, CreateCollectionOutcome::Replayed { .. }));
    assert_eq!(created.collection(), replayed.collection());

    let mut changed = command.clone();
    changed.destination.value = "0x3333333333333333333333333333333333333333".to_owned();
    let error = repository
        .create_or_replay_collection(changed)
        .await
        .expect_err("a collection ID cannot replay different content");
    assert_eq!(error.kind, DepositErrorKind::Conflict);

    let exclusive = create_collection("collection-2", "job-2", "deposit-1");
    let error = repository
        .create_or_replay_collection(exclusive)
        .await
        .expect_err("one deposit and asset can only have one active reservation");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    Ok(())
}

#[tokio::test]
async fn signed_envelope_and_transaction_attribution_survive_restart_and_broadcast()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let command = create_collection("collection-restart", "job-restart", "deposit-restart");
    let tx = transaction("0xaaa");
    let signed;
    {
        let repository = PaymentStore::new(RocksDb::open(directory.path())?);
        let created = repository
            .create_or_replay_collection(command)
            .await?
            .collection()
            .clone();
        signed = repository
            .record_signed(RecordSignature {
                collection_id: created.id.clone(),
                leg_id: created.legs[0].id.clone(),
                expected: guard(&created, 0),
                expected_transaction_id: tx.clone(),
                envelope: SignedBytes::new(vec![9, 8, 7])?,
                allocations: Vec::new(),
                fee_limit: Some(amount(10)),
                signed_at: 110,
                expires_at: 1_000,
            })
            .await?;
    }

    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let page = repository
        .collections_for_deposit(
            &DepositId("deposit-restart".to_owned()),
            CollectionQuery {
                after: None,
                limit: 10,
            },
        )
        .await?;
    assert_eq!(page.collections, vec![signed.clone()]);
    assert_eq!(page.next, None);
    let envelope = repository
        .signed_envelope(&signed.id, &signed.legs[0].id)
        .await?
        .expect("signed envelope must survive repository restart");
    assert_eq!(envelope.bytes.as_bytes(), &[9, 8, 7]);
    assert_eq!(envelope.expected_transaction_id, tx);
    assert_eq!(
        repository.leg_for_transaction(&tx).await?,
        Some(LegRef {
            collection_id: signed.id.clone(),
            leg_id: signed.legs[0].id.clone(),
        })
    );

    let mismatch = repository
        .accept_broadcast(AcceptBroadcast {
            collection_id: signed.id.clone(),
            leg_id: signed.legs[0].id.clone(),
            expected: guard(&signed, 0),
            transaction_id: transaction("0xbbb"),
            accepted_at: 120,
        })
        .await
        .expect_err("broadcast response must match the signed envelope hash");
    assert_eq!(mismatch.kind, DepositErrorKind::Conflict);
    assert!(
        repository
            .signed_envelope(&signed.id, &signed.legs[0].id)
            .await?
            .is_some()
    );

    let broadcast = repository
        .accept_broadcast(AcceptBroadcast {
            collection_id: signed.id.clone(),
            leg_id: signed.legs[0].id.clone(),
            expected: guard(&signed, 0),
            transaction_id: tx.clone(),
            accepted_at: 120,
        })
        .await?;
    assert!(matches!(
        broadcast.legs[0].state,
        CollectionLegState::Broadcast { .. }
    ));
    assert!(
        repository
            .signed_envelope(&signed.id, &signed.legs[0].id)
            .await?
            .is_some()
    );
    assert_eq!(
        repository.leg_for_transaction(&tx).await?,
        Some(LegRef {
            collection_id: signed.id,
            leg_id: signed.legs[0].id.clone(),
        })
    );
    Ok(())
}

#[tokio::test]
async fn optimistic_state_and_confirmation_attribution_are_enforced()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);
    let command = create_collection("collection-confirm", "job-confirm", "deposit-confirm");
    let created = repository
        .create_or_replay_collection(command)
        .await?
        .collection()
        .clone();
    let tx = transaction("0xccc");
    let signed = repository
        .record_signed(RecordSignature {
            collection_id: created.id.clone(),
            leg_id: created.legs[0].id.clone(),
            expected: guard(&created, 0),
            expected_transaction_id: tx.clone(),
            envelope: SignedBytes::new(vec![4, 5, 6])?,
            allocations: Vec::new(),
            fee_limit: Some(amount(10)),
            signed_at: 110,
            expires_at: 1_000,
        })
        .await?;

    let stale = repository
        .accept_broadcast(AcceptBroadcast {
            collection_id: signed.id.clone(),
            leg_id: signed.legs[0].id.clone(),
            expected: guard(&created, 0),
            transaction_id: tx.clone(),
            accepted_at: 120,
        })
        .await
        .expect_err("stale aggregate and leg states must not overwrite the winner");
    assert_eq!(stale.kind, DepositErrorKind::Conflict);

    let broadcast = repository
        .accept_broadcast(AcceptBroadcast {
            collection_id: signed.id.clone(),
            leg_id: signed.legs[0].id.clone(),
            expected: guard(&signed, 0),
            transaction_id: tx.clone(),
            accepted_at: 120,
        })
        .await?;
    let watched = attach_watch(&repository, broadcast).await?;
    let attribution = allocation(watched.deposit_id());
    let confirmed = repository
        .confirm_leg(ConfirmLeg {
            collection_id: watched.id.clone(),
            leg_id: watched.legs[0].id.clone(),
            expected: guard(&watched, 0),
            transaction_id: tx,
            allocation: Some(attribution.clone()),
            confirmed_at: 140,
        })
        .await?;
    assert_eq!(confirmed.state, CollectionState::Completed);
    assert_eq!(confirmed.legs[0].allocation, Some(attribution));
    assert!(matches!(
        confirmed.reservation().state,
        CollectionReservationState::Consumed { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn failure_and_reorg_keep_reservations_until_explicit_release()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PaymentStore::new(RocksDb::open(directory.path())?);

    let failed_broadcast = create_and_broadcast(
        &repository,
        create_collection("collection-failed", "job-failed", "deposit-failed"),
        transaction("0xddd"),
    )
    .await?;
    let failed = repository
        .fail_leg(FailLeg {
            collection_id: failed_broadcast.id.clone(),
            leg_id: failed_broadcast.legs[0].id.clone(),
            expected: guard(&failed_broadcast, 0),
            transaction_id: transaction("0xddd"),
            error: safe_error("receipt_failed"),
            failed_at: 130,
        })
        .await?;
    assert_eq!(failed.state, CollectionState::Failed);
    assert_eq!(
        failed.reservation().state,
        CollectionReservationState::Active
    );
    let blocked = repository
        .create_or_replay_collection(create_collection(
            "collection-blocked",
            "job-blocked",
            "deposit-failed",
        ))
        .await
        .expect_err("terminal failure must retain the reservation until explicit release");
    assert_eq!(blocked.kind, DepositErrorKind::Conflict);
    let released = repository
        .release_reservation(ReleaseReservation {
            collection_id: failed.id.clone(),
            expected_collection_state: CollectionState::Failed,
            expected_reservation_state: CollectionReservationState::Active,
            reason: ReservationReleaseReason::TerminalFailure,
            released_at: 140,
        })
        .await?;
    assert!(matches!(
        released.reservation().state,
        CollectionReservationState::Released { .. }
    ));
    repository
        .create_or_replay_collection(create_collection(
            "collection-after-failure",
            "job-after-failure",
            "deposit-failed",
        ))
        .await?;

    let reorg_tx = transaction("0xeee");
    let reorg_broadcast = create_and_broadcast(
        &repository,
        create_collection("collection-reorg", "job-reorg", "deposit-reorg"),
        reorg_tx.clone(),
    )
    .await?;
    let watched = attach_watch(&repository, reorg_broadcast).await?;
    let confirmed = repository
        .confirm_leg(ConfirmLeg {
            collection_id: watched.id.clone(),
            leg_id: watched.legs[0].id.clone(),
            expected: guard(&watched, 0),
            transaction_id: reorg_tx.clone(),
            allocation: Some(allocation(watched.deposit_id())),
            confirmed_at: 140,
        })
        .await?;
    let reorged = repository
        .reorg_leg(ReorgLeg {
            collection_id: confirmed.id.clone(),
            leg_id: confirmed.legs[0].id.clone(),
            expected: guard(&confirmed, 0),
            transaction_id: reorg_tx.clone(),
            error: safe_error("confirmation_reorged"),
            reorged_at: 150,
        })
        .await?;
    assert_eq!(reorged.state, CollectionState::Reorged);
    assert_eq!(
        reorged.reservation().state,
        CollectionReservationState::Active
    );
    assert!(matches!(
        reorged.legs[0].state,
        CollectionLegState::Reorged { .. }
    ));
    assert_eq!(
        repository.leg_for_transaction(&reorg_tx).await?,
        Some(LegRef {
            collection_id: reorged.id.clone(),
            leg_id: reorged.legs[0].id.clone(),
        })
    );
    let released = repository
        .release_reservation(ReleaseReservation {
            collection_id: reorged.id,
            expected_collection_state: CollectionState::Reorged,
            expected_reservation_state: CollectionReservationState::Active,
            reason: ReservationReleaseReason::Reorg,
            released_at: 160,
        })
        .await?;
    assert!(matches!(
        released.reservation().state,
        CollectionReservationState::Released {
            reason: ReservationReleaseReason::Reorg,
            ..
        }
    ));
    Ok(())
}
