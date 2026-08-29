use std::ops::Deref;

use base::Decimal;
use futures_executor::block_on;
use indexing::{
    AssetId, BlockAddition, BlockHash, BlockHeight, BlockOutcome, BlockParent, BlockPosition,
    BlockRef, BlockSelector, Blocks, CanonicalAddress, ChainId, HistoryQuery, IndexScope,
    IndexedOutput, InterpretedBlock, MovementId, ObservationDraft, ObservationDraftStatus,
    OutputChanges, OutputId, OutputRequest, Outputs, TransactionRef, Transactions, ValueMovement,
};
use tempfile::TempDir;

use super::Repository;

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

fn repository() -> TestRepository {
    let directory = TempDir::new().expect("temporary database directory");
    let database = directory.path().join("index.redb");
    let storage = storage_redb::Redb::open(&database).expect("temporary database");
    TestRepository {
        repository: Repository::new(storage, scope()).expect("repository scope"),
        _directory: directory,
    }
}

fn block(height: u64, hash: u8, parent: u8) -> BlockRef {
    BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(vec![hash]),
        parent: Some(BlockParent {
            position: BlockPosition(height - 1),
            hash: BlockHash(vec![parent]),
        }),
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

fn draft(id: &str) -> ObservationDraft {
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
    }
}

fn output(id: &str) -> IndexedOutput {
    IndexedOutput {
        id: OutputId {
            transaction: transaction(id),
            index: 0,
        },
        address: address("receiver"),
        asset: AssetId {
            chain: scope().chain,
            asset: "native".to_owned(),
        },
        amount: Decimal::from(1_u64),
        evidence: vec![1, 2, 3],
        created_at: BlockHeight(1),
        coinbase: false,
    }
}

fn commit(
    repository: &TestRepository,
    block: BlockRef,
    checkpoint: Option<BlockRef>,
    drafts: Vec<ObservationDraft>,
    outputs: OutputChanges,
) -> BlockOutcome {
    let addition = BlockAddition::new(
        scope(),
        checkpoint,
        4,
        InterpretedBlock {
            block,
            transactions: drafts,
            outputs,
        },
    )
    .expect("valid block addition");
    block_on(repository.add(addition)).expect("block commit")
}

#[test]
fn commit_stores_only_checkpoint_history_journal_and_live_outputs() {
    let repository = repository();
    let first = block(1, 1, 0);
    let created = output("tx");
    assert_eq!(
        commit(
            &repository,
            first.clone(),
            None,
            vec![draft("tx")],
            OutputChanges {
                created: vec![created.clone()],
                ..OutputChanges::default()
            },
        ),
        BlockOutcome::Applied
    );

    assert_eq!(
        block_on(repository.get(BlockSelector::Tip(scope()))).expect("checkpoint"),
        Some(first.clone())
    );
    assert_eq!(
        block_on(repository.get(BlockSelector::Height {
            scope: scope(),
            height: first.height,
        }))
        .expect("journal block"),
        Some(first)
    );
    let history = block_on(Transactions::list(
        &*repository,
        HistoryQuery {
            scope: scope(),
            address: address("receiver"),
            after: None,
            limit: 10,
        },
    ))
    .expect("history");
    assert_eq!(history.transactions.len(), 1);
    let outputs = block_on(Outputs::list(
        &*repository,
        OutputRequest {
            scope: scope(),
            address: address("receiver"),
            after: None,
            limit: 10,
        },
    ))
    .expect("outputs");
    assert_eq!(outputs.outputs, vec![created]);
}

#[test]
fn revert_removes_canonical_history_and_restores_outputs() {
    let repository = repository();
    let first = block(1, 1, 0);
    let created = output("tx");
    commit(
        &repository,
        first.clone(),
        None,
        vec![draft("tx")],
        OutputChanges {
            created: vec![created],
            ..OutputChanges::default()
        },
    );
    block_on(repository.remove(scope(), first)).expect("revert");

    assert_eq!(
        block_on(repository.get(BlockSelector::Tip(scope()))).expect("checkpoint"),
        None
    );
    assert!(
        block_on(Transactions::list(
            &*repository,
            HistoryQuery {
                scope: scope(),
                address: address("receiver"),
                after: None,
                limit: 10,
            }
        ))
        .expect("history")
        .transactions
        .is_empty()
    );
}

#[test]
fn reverting_a_spend_restores_the_live_output() {
    let repository = repository();
    let first = block(1, 1, 0);
    let created = output("funding");
    commit(
        &repository,
        first.clone(),
        None,
        vec![draft("funding")],
        OutputChanges {
            created: vec![created.clone()],
            ..OutputChanges::default()
        },
    );
    let second = block(2, 2, 1);
    commit(
        &repository,
        second.clone(),
        Some(first.clone()),
        vec![draft("spend")],
        OutputChanges {
            spent: vec![created.key()],
            ..OutputChanges::default()
        },
    );
    assert!(
        block_on(Outputs::list(
            &*repository,
            OutputRequest {
                scope: scope(),
                address: address("receiver"),
                after: None,
                limit: 10,
            }
        ))
        .expect("spent output page")
        .outputs
        .is_empty()
    );

    block_on(repository.remove(scope(), second)).expect("revert spend");

    let outputs = block_on(Outputs::list(
        &*repository,
        OutputRequest {
            scope: scope(),
            address: address("receiver"),
            after: None,
            limit: 10,
        },
    ))
    .expect("restored output page");
    assert_eq!(outputs.outputs, vec![created]);
    assert_eq!(outputs.checkpoint, Some(first));
}
