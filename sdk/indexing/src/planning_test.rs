use std::collections::BTreeSet;

use base::Decimal;

use super::{addresses, commit, confirmation, observation, validate_draft};
use crate::{
    AssetId, BlockHash, BlockHeight, BlockRef, CanonicalAddress, ChainId, CommitBlock,
    CommitContext, ConfirmationPolicy, ConfirmationProof, IndexChanges, IndexErrorKind, IndexScope,
    IndexUndo, InterpretedBlock, MovementId, ObservationDraft, ObservationDraftStatus,
    ObservationRevision, ObservedTransaction, TransactionRef, TransactionStatus, ValueMovement,
    WatchId, WatchVersion,
};

fn test_scope(network: &str) -> IndexScope {
    IndexScope {
        chain: ChainId("ethereum".into()),
        network: network.into(),
    }
}

fn address(scope: &IndexScope, value: &str) -> CanonicalAddress {
    CanonicalAddress {
        scope: scope.clone(),
        value: value.into(),
    }
}

fn transaction(scope: &IndexScope) -> TransactionRef {
    TransactionRef {
        scope: scope.clone(),
        value: "transaction".into(),
    }
}

fn movement(scope: &IndexScope, id: &str) -> ValueMovement {
    ValueMovement::Transfer {
        id: MovementId(id.into()),
        asset: AssetId {
            chain: scope.chain.clone(),
            asset: "native".into(),
        },
        amount: Decimal::zero(),
        from: address(scope, "from"),
        to: address(scope, "to"),
    }
}

fn draft(scope: &IndexScope) -> ObservationDraft {
    ObservationDraft {
        scope: scope.clone(),
        transaction_id: transaction(scope),
        status: ObservationDraftStatus::Included,
        movements: vec![movement(scope, "movement")],
        fee: None,
        watch_ids: vec![WatchId("watch-a".into())],
        first_seen_at: 10,
        observed_at: 11,
    }
}

fn block(height: u64) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(vec![height as u8]),
        parent_hash: None,
        timestamp: None,
    }
}

#[test]
fn draft_validation_accepts_active_watches_and_rejects_invalid_facts() {
    let scope = test_scope("mainnet");
    let active = BTreeSet::from([WatchId("watch-a".into())]);
    assert_eq!(validate_draft(&draft(&scope), &scope, &active), Ok(()));

    let cases = [
        (test_scope("sepolia"), IndexErrorKind::ScopeMismatch),
        (scope.clone(), IndexErrorKind::InvalidWatch),
    ];
    for (expected_scope, kind) in cases {
        let result = validate_draft(&draft(&scope), &expected_scope, &BTreeSet::new());
        assert_eq!(result.unwrap_err().kind, kind);
    }

    let mut failed = draft(&scope);
    failed.status = ObservationDraftStatus::Failed { reason: None };
    assert_eq!(
        validate_draft(&failed, &scope, &active).unwrap_err().kind,
        IndexErrorKind::InvalidBlock
    );

    let mut duplicate = draft(&scope);
    duplicate.movements.push(movement(&scope, "movement"));
    assert_eq!(
        validate_draft(&duplicate, &scope, &active)
            .unwrap_err()
            .kind,
        IndexErrorKind::InvalidBlock
    );
}

#[test]
fn observation_advances_revision_and_unions_watches() {
    let scope = test_scope("mainnet");
    let first = draft(&scope);
    let (prior, _) = observation(
        None,
        &[],
        &scope,
        &first.transaction_id,
        TransactionStatus::Pending,
        Some(&first),
        20,
    )
    .unwrap();
    let mut next = draft(&scope);
    next.watch_ids = vec![WatchId("watch-b".into()), WatchId("watch-a".into())];
    let (observed, watches) = observation(
        Some(&prior),
        &[WatchId("watch-a".into())],
        &scope,
        &next.transaction_id,
        TransactionStatus::Dropped,
        Some(&next),
        30,
    )
    .unwrap();

    assert_eq!(observed.revision, ObservationRevision(2));
    assert_eq!(observed.first_seen_at, 10);
    assert_eq!(observed.observed_at, 30);
    assert_eq!(
        watches,
        vec![WatchId("watch-a".into()), WatchId("watch-b".into())]
    );
}

#[test]
fn observation_rejects_exhausted_revision() {
    let scope = test_scope("mainnet");
    let draft = draft(&scope);
    let prior = ObservedTransaction {
        scope: scope.clone(),
        transaction_id: draft.transaction_id.clone(),
        revision: ObservationRevision(u64::MAX),
        status: TransactionStatus::Pending,
        movements: Vec::new(),
        fee: None,
        first_seen_at: 1,
        observed_at: 1,
    };
    let error = observation(
        Some(&prior),
        &[],
        &scope,
        &draft.transaction_id,
        TransactionStatus::Dropped,
        None,
        2,
    )
    .unwrap_err();
    assert_eq!(error.kind, IndexErrorKind::Store);
}

#[test]
fn addresses_unions_and_deduplicates_movement_endpoints() {
    let scope = test_scope("mainnet");
    let mut draft = draft(&scope);
    draft.movements.push(ValueMovement::Output {
        id: MovementId("output".into()),
        asset: AssetId {
            chain: scope.chain.clone(),
            asset: "native".into(),
        },
        amount: Decimal::zero(),
        owner: Some(address(&scope, "to")),
    });
    let (observed, _) = observation(
        None,
        &[],
        &scope,
        &draft.transaction_id,
        TransactionStatus::Pending,
        Some(&draft),
        1,
    )
    .unwrap();
    assert_eq!(
        addresses(&observed),
        BTreeSet::from([address(&scope, "from"), address(&scope, "to")])
    );
}

#[test]
fn confirmation_obeys_depth_boundaries_and_rejects_an_older_tip() {
    let inclusion = block(10);
    let policy = ConfirmationPolicy {
        minimum_confirmations: 3,
        require_chain_finality: false,
    };

    assert_eq!(
        confirmation(&inclusion, 1, &block(10), policy).unwrap(),
        None
    );
    assert_eq!(
        confirmation(&inclusion, 1, &block(11), policy).unwrap(),
        Some(TransactionStatus::Included {
            block: inclusion.clone(),
            confirmations: 2,
        })
    );
    assert_eq!(
        confirmation(&inclusion, 2, &block(12), policy).unwrap(),
        Some(TransactionStatus::Confirmed {
            block: inclusion.clone(),
            proof: ConfirmationProof::Depth {
                required: 3,
                observed: 3,
            },
        })
    );
    assert_eq!(
        confirmation(&inclusion, 0, &block(9), policy)
            .unwrap_err()
            .kind,
        IndexErrorKind::InvalidBlock
    );
}

#[test]
fn commit_plans_observation_and_retention_without_storage_records() {
    let scope = test_scope("mainnet");
    let parent = block(9);
    let mut next = block(10);
    next.parent_hash = Some(parent.hash.clone());
    let command = CommitBlock {
        scope: scope.clone(),
        expected_checkpoint: Some(parent.clone()),
        expected_watch_version: WatchVersion(4),
        confirmation_policy: ConfirmationPolicy {
            minimum_confirmations: 2,
            require_chain_finality: false,
        },
        reorg_retention: 5,
        block: InterpretedBlock {
            block: next.clone(),
            drafts: vec![draft(&scope)],
            effect: IndexChanges::default(),
            undo: IndexUndo::default(),
        },
    };
    let context = CommitContext {
        checkpoint: Some(parent),
        watch_version: WatchVersion(4),
        active_watches: BTreeSet::from([WatchId("watch-a".into())]),
        observations: Default::default(),
        pending_confirmations: Default::default(),
    };

    let plan = commit(&command, &context).unwrap();
    let transition = plan.transitions.get(&transaction(&scope)).unwrap();
    assert!(transition.included_here);
    assert_eq!(transition.next.transaction.revision, ObservationRevision(1));
    assert_eq!(plan.prune_before, Some(BlockHeight(5)));
}
