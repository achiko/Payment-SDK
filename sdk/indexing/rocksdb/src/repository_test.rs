use std::ops::Deref;

use base::Decimal;
use futures_executor::block_on;
use tempfile::TempDir;

use super::Repository;
use crate::{
    AssetId, BlockHash, BlockHeight, BlockOutcome, BlockRef, BlockStore, CanonicalAddress,
    CanonicalStore, ChainId, CommitBlock, ConfirmationPolicy, HistoryQuery, HistoryStore,
    IndexChanges, IndexScope, IndexUndo, InterpretedBlock, MovementId, ObservationDraft,
    ObservationDraftStatus, RegisterWatch, RevertTip, TransactionQuery, TransactionRef,
    TransactionStatus, ValueMovement, WatchReceipt, WatchRequest, WatchStore, WatchVersion,
};

struct TestRepository {
    repository: Repository,
    _directory: TempDir,
}

impl Deref for TestRepository {
    type Target = Repository;

    fn deref(&self) -> &Self::Target {
        &self.repository
    }
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("chain".to_owned()),
        network: "test".to_owned(),
    }
}

fn policy() -> ConfirmationPolicy {
    ConfirmationPolicy {
        minimum_confirmations: 2,
        require_chain_finality: false,
    }
}

fn repository() -> TestRepository {
    let directory = TempDir::new().expect("temporary database directory");
    let storage = storage_rocksdb::RocksDb::open(directory.path()).expect("temporary database");
    let repository = Repository::new(storage, scope()).expect("test repository");
    TestRepository {
        repository,
        _directory: directory,
    }
}

fn block(height: u64, hash: u8, parent: u8) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(vec![hash]),
        parent_hash: Some(BlockHash(vec![parent])),
        timestamp: Some(1_000 + height),
    }
}

fn address(value: &str) -> CanonicalAddress {
    CanonicalAddress {
        scope: scope(),
        value: value.to_owned(),
    }
}

fn transaction(value: &str) -> TransactionRef {
    TransactionRef {
        scope: scope(),
        value: value.to_owned(),
    }
}

fn draft(id: &str, watch: crate::WatchId) -> ObservationDraft {
    ObservationDraft {
        scope: scope(),
        transaction_id: transaction(id),
        status: ObservationDraftStatus::Included,
        movements: vec![ValueMovement::Transfer {
            id: MovementId(format!("move-{id}")),
            asset: AssetId {
                chain: scope().chain,
                asset: "native".to_owned(),
            },
            amount: Decimal::from(1_u64),
            from: address("sender"),
            to: address("receiver"),
        }],
        fee: None,
        watch_ids: vec![watch],
        first_seen_at: 1_001,
        observed_at: 1_001,
    }
}

fn commit(
    repository: &TestRepository,
    block: BlockRef,
    checkpoint: Option<BlockRef>,
    watch_version: u64,
    drafts: Vec<ObservationDraft>,
) -> BlockOutcome {
    let command = CommitBlock {
        scope: scope(),
        expected_checkpoint: checkpoint,
        expected_watch_version: WatchVersion(watch_version),
        confirmation_policy: policy(),
        reorg_retention: 4,
        block: InterpretedBlock {
            block,
            drafts,
            effect: IndexChanges::default(),
            undo: IndexUndo::default(),
        },
    };
    let context = block_on(repository.load_commit(&command)).expect("context loads");
    let plan = indexing::plan_commit(&command, &context).expect("commit plans");
    block_on(repository.commit_block(plan)).expect("commit succeeds")
}

fn register(repository: &TestRepository, request: WatchRequest) -> WatchReceipt {
    let command = RegisterWatch {
        target: request.selector.clone(),
        request,
        registered_at: None,
    };
    let context = block_on(repository.load_watch(&command)).expect("watch context");
    let decision = indexing::plan_watch(&command, &context).expect("watch plans");
    if let Some(plan) = decision.plan {
        block_on(repository.save_watch(plan)).expect("watch saves");
    }
    decision.receipt
}

fn revert(repository: &TestRepository, expected_tip: BlockRef) -> Option<BlockRef> {
    let command = RevertTip {
        scope: scope(),
        expected_tip,
    };
    let context = block_on(repository.load_revert(&command)).expect("revert context");
    let decision = indexing::plan_revert(&command, &context).expect("revert plans");
    if let Some(plan) = decision.plan {
        block_on(repository.save_revert(plan)).expect("revert saves");
    }
    decision.checkpoint
}

#[test]
fn watch_is_idempotent_and_starts_at_requested_height() {
    let repository = repository();
    let request = WatchRequest {
        scope: scope(),
        selector: address("receiver"),
        start_height: BlockHeight(2),
        idempotency_key: "wallet-1".to_owned(),
    };
    let first = register(&repository, request.clone());
    let second = register(&repository, request);
    assert_eq!(first.id, second.id);
    assert!(
        block_on(repository.watches_at(&scope(), BlockHeight(1)))
            .expect("watches")
            .watches
            .is_empty()
    );
    assert_eq!(
        block_on(repository.watches_at(&scope(), BlockHeight(2)))
            .expect("watches")
            .watches
            .len(),
        1
    );
}

#[test]
fn history_survives_confirmation_and_records_reorg_revision() {
    let repository = repository();
    let request = WatchRequest {
        scope: scope(),
        selector: address("receiver"),
        start_height: BlockHeight(1),
        idempotency_key: "wallet-1".to_owned(),
    };
    let watch = register(&repository, request).id;

    let first = block(1, 1, 0);
    assert_eq!(
        commit(
            &repository,
            first.clone(),
            None,
            1,
            vec![draft("tx", watch)]
        ),
        BlockOutcome::Applied
    );
    let second = block(2, 2, 1);
    commit(&repository, second.clone(), Some(first.clone()), 1, vec![]);

    let observed = block_on(repository.transaction(TransactionQuery {
        scope: scope(),
        transaction_id: transaction("tx"),
    }))
    .expect("query")
    .expect("transaction");
    assert!(matches!(
        observed.status,
        TransactionStatus::Confirmed { .. }
    ));
    let history = block_on(repository.transactions_by_address(HistoryQuery {
        scope: scope(),
        address: address("receiver"),
        after: None,
        limit: 10,
    }))
    .expect("history");
    assert_eq!(history.transactions.len(), 1);

    assert_eq!(revert(&repository, second), Some(first.clone()));
    assert_eq!(revert(&repository, first), None);
    let reorged = block_on(repository.transaction(TransactionQuery {
        scope: scope(),
        transaction_id: transaction("tx"),
    }))
    .expect("query")
    .expect("transaction revision retained");
    assert!(matches!(reorged.status, TransactionStatus::Reorged { .. }));
    assert_eq!(reorged.revision.0, 4);
    assert_eq!(
        block_on(repository.checkpoint(&scope())).expect("checkpoint"),
        None
    );
}
