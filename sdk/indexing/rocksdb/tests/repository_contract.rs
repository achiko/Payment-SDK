use std::path::Path;

use base::Decimal;
use futures_executor::block_on;
use indexing::{
    AssetId, BlockAddition, BlockHash, BlockHeight, BlockOutcome, BlockRef, BlockSelector, Blocks,
    CanonicalAddress, ChainId, HistoryQuery, IndexErrorKind, IndexScope, IndexedOutput,
    InterpretedBlock, MovementId, ObservationDraft, ObservationDraftStatus, OutputChanges,
    OutputId, OutputKey, OutputRequest, Outputs, TransactionRef, Transactions, ValueMovement,
};
use indexing_rocksdb::Repository;
use tempfile::TempDir;

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("chain".into()),
        network: "testnet".into(),
    }
}

fn open(path: &Path) -> Repository {
    let storage = storage_rocksdb::RocksDb::open(path).expect("temporary RocksDB");
    Repository::new(storage, scope()).expect("repository")
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
        value: value.into(),
    }
}

fn transaction(value: &str) -> TransactionRef {
    TransactionRef {
        scope: scope(),
        value: value.into(),
    }
}

fn draft(id: &str) -> ObservationDraft {
    ObservationDraft {
        scope: scope(),
        transaction_id: transaction(id),
        status: ObservationDraftStatus::Included,
        movements: vec![ValueMovement::Transfer {
            id: MovementId(format!("movement-{id}")),
            asset: AssetId {
                chain: scope().chain,
                asset: "native".into(),
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
            asset: "native".into(),
        },
        amount: Decimal::from(1_u64),
        evidence: vec![1, 2, 3],
        created_at: BlockHeight(1),
        coinbase: false,
    }
}

fn addition(
    block: BlockRef,
    expected: Option<BlockRef>,
    transaction: &str,
    outputs: OutputChanges,
) -> BlockAddition {
    BlockAddition::new(
        scope(),
        expected,
        4,
        InterpretedBlock {
            block,
            transactions: vec![draft(transaction)],
            outputs,
        },
    )
    .expect("valid block facts")
}

fn tip(repository: &Repository) -> Option<BlockRef> {
    block_on(repository.get(BlockSelector::Tip(scope()))).expect("checkpoint")
}

fn history(repository: &Repository) -> Vec<String> {
    block_on(Transactions::list(
        repository,
        HistoryQuery {
            scope: scope(),
            address: address("receiver"),
            after: None,
            limit: 10,
        },
    ))
    .expect("history")
    .transactions
    .into_iter()
    .map(|transaction| transaction.transaction_id.value)
    .collect()
}

fn outputs(repository: &Repository) -> Vec<IndexedOutput> {
    block_on(Outputs::list(
        repository,
        OutputRequest {
            scope: scope(),
            address: address("receiver"),
            after: None,
            limit: 10,
        },
    ))
    .expect("outputs")
    .outputs
}

#[test]
fn restart_and_reorg_preserve_atomic_canonical_state() {
    let directory = TempDir::new().expect("temporary directory");
    let repository = open(directory.path());
    let first = block(1, 1, 0);
    let funding = output("funding");
    assert_eq!(
        block_on(repository.add(addition(
            first.clone(),
            None,
            "funding",
            OutputChanges {
                created: vec![funding.clone()],
                ..OutputChanges::default()
            },
        )))
        .expect("first block"),
        BlockOutcome::Applied
    );

    let second = block(2, 2, 1);
    let unknown = OutputKey {
        address: address("receiver"),
        output: OutputId {
            transaction: transaction("unknown"),
            index: 0,
        },
    };
    let invalid = block_on(repository.add(addition(
        second.clone(),
        Some(first.clone()),
        "invalid-spend",
        OutputChanges {
            spent: vec![unknown],
            ..OutputChanges::default()
        },
    )))
    .expect_err("unknown required output");
    assert_eq!(invalid.kind, IndexErrorKind::InvalidBlock);
    assert_eq!(tip(&repository), Some(first.clone()));
    assert_eq!(history(&repository), vec!["funding"]);
    assert_eq!(outputs(&repository), vec![funding.clone()]);

    let stale = block_on(repository.add(addition(
        second.clone(),
        None,
        "stale",
        OutputChanges::default(),
    )))
    .expect_err("stale checkpoint");
    assert_eq!(stale.kind, IndexErrorKind::Conflict);
    assert_eq!(tip(&repository), Some(first.clone()));
    assert_eq!(history(&repository), vec!["funding"]);

    block_on(repository.add(addition(
        second.clone(),
        Some(first.clone()),
        "spend",
        OutputChanges {
            spent: vec![funding.key()],
            ..OutputChanges::default()
        },
    )))
    .expect("second block");
    assert_eq!(history(&repository), vec!["funding", "spend"]);
    assert!(outputs(&repository).is_empty());

    let counterfeit = block(2, 99, 1);
    let rejected = block_on(repository.remove(scope(), counterfeit)).expect_err("wrong tip");
    assert_eq!(rejected.kind, IndexErrorKind::Conflict);
    assert_eq!(tip(&repository), Some(second.clone()));
    assert_eq!(history(&repository), vec!["funding", "spend"]);
    assert!(outputs(&repository).is_empty());

    drop(repository);
    let repository = open(directory.path());
    assert_eq!(tip(&repository), Some(second.clone()));
    assert_eq!(history(&repository), vec!["funding", "spend"]);
    assert!(outputs(&repository).is_empty());

    assert_eq!(
        block_on(repository.remove(scope(), second)).expect("stored journal rollback"),
        Some(first.clone())
    );
    assert_eq!(tip(&repository), Some(first));
    assert_eq!(history(&repository), vec!["funding"]);
    assert_eq!(outputs(&repository), vec![funding.clone()]);

    drop(repository);
    let repository = open(directory.path());
    assert_eq!(history(&repository), vec!["funding"]);
    assert_eq!(outputs(&repository), vec![funding]);
}
