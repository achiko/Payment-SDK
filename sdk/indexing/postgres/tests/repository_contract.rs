//! Contract coverage for the PostgreSQL repository.
//!
//! Mirrors `indexing-redb`'s repository contract so both backends are held to
//! the same behaviour, and adds the cases the batched write path could plausibly
//! get wrong: per-address duplication, movement ordering inside a transaction,
//! spends that must exist versus spends that may not, and cursor pagination.
//!
//! Requires a database with `migrations/0001_init.sql` applied:
//!
//!   POSTGRES_TEST_URL=postgres://user@127.0.0.1:5433/db cargo test -p indexing-postgres
//!
//! Without that variable the tests report themselves skipped rather than fail,
//! so a checkout with no database still runs a green suite. Every test picks a
//! unique chain name, and scope is a column in every key, so tests share one
//! schema without colliding.

use std::sync::atomic::{AtomicU64, Ordering};

use indexing::{
    AssetId, BlockAddition, BlockHash, BlockHeight, BlockOutcome, BlockRef, BlockSelector, Blocks,
    CanonicalAddress, ChainId, Decimal, HistoryQuery, IndexErrorKind, IndexScope, IndexedOutput,
    InterpretedBlock, MovementId, ObservationDraft, ObservationDraftStatus, OutputChanges,
    OutputId, OutputKey, OutputRequest, Outputs, RegisteredAddress, Registry, TransactionRef,
    Transactions, ValueMovement,
};
use indexing_postgres::Repository;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A scope no other test uses, so one schema serves the whole suite.
fn unique_scope() -> IndexScope {
    let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    IndexScope {
        chain: ChainId(format!("test-{stamp}-{ordinal}")),
        network: "testnet".into(),
    }
}

/// `None` when no database is configured, which the tests report as a skip.
fn repository(scope: &IndexScope) -> Option<Repository> {
    let url = std::env::var("POSTGRES_TEST_URL").ok()?;
    let pool = indexing_postgres::pool(&url, 4).expect("connection pool");
    Some(Repository::new(pool, scope.clone()).expect("repository"))
}

macro_rules! repository {
    ($scope:expr) => {
        match repository($scope) {
            Some(repository) => repository,
            None => {
                eprintln!("skipped: POSTGRES_TEST_URL is not set");
                return;
            }
        }
    };
}

fn block(height: u64, hash: u8, parent: u8) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(vec![hash]),
        parent_hash: Some(BlockHash(vec![parent])),
        timestamp: Some(1_000 + height),
    }
}

fn address(scope: &IndexScope, value: &str) -> CanonicalAddress {
    CanonicalAddress {
        scope: scope.clone(),
        value: value.into(),
    }
}

fn transaction(scope: &IndexScope, value: &str) -> TransactionRef {
    TransactionRef {
        scope: scope.clone(),
        value: value.into(),
    }
}

fn asset(scope: &IndexScope) -> AssetId {
    AssetId {
        chain: scope.chain.clone(),
        asset: "native".into(),
    }
}

fn transfer(scope: &IndexScope, id: &str, amount: u64) -> ValueMovement {
    ValueMovement::Transfer {
        id: MovementId(id.into()),
        asset: asset(scope),
        amount: Decimal::from(amount),
        from: address(scope, "sender"),
        to: address(scope, "receiver"),
    }
}

fn draft(scope: &IndexScope, id: &str) -> ObservationDraft {
    ObservationDraft {
        scope: scope.clone(),
        transaction_id: transaction(scope, id),
        status: ObservationDraftStatus::Included,
        movements: vec![transfer(scope, &format!("movement-{id}"), 1)],
        fee: None,
    }
}

fn output(scope: &IndexScope, id: &str, index: u32, height: u64) -> IndexedOutput {
    IndexedOutput {
        id: OutputId {
            transaction: transaction(scope, id),
            index,
        },
        address: address(scope, "receiver"),
        asset: asset(scope),
        amount: Decimal::from(5_u64),
        evidence: vec![0xab, 0xcd],
        created_at: BlockHeight(height),
        coinbase: false,
    }
}

fn addition(
    scope: &IndexScope,
    block: BlockRef,
    expected: Option<BlockRef>,
    drafts: Vec<ObservationDraft>,
    outputs: OutputChanges,
) -> BlockAddition {
    BlockAddition::new(
        scope.clone(),
        expected,
        4,
        InterpretedBlock {
            block,
            transactions: drafts,
            outputs,
        },
    )
    .expect("valid block facts")
}

fn one(scope: &IndexScope, id: &str) -> Vec<ObservationDraft> {
    vec![draft(scope, id)]
}

async fn tip(repository: &Repository, scope: &IndexScope) -> Option<BlockRef> {
    repository
        .get(BlockSelector::Tip(scope.clone()))
        .await
        .expect("checkpoint")
}

async fn history(repository: &Repository, scope: &IndexScope, who: &str) -> Vec<String> {
    Transactions::list(
        repository,
        HistoryQuery {
            scope: scope.clone(),
            address: address(scope, who),
            after: None,
            limit: 10,
        },
    )
    .await
    .expect("history")
    .transactions
    .into_iter()
    .map(|transaction| transaction.transaction_id.value)
    .collect()
}

async fn outputs(repository: &Repository, scope: &IndexScope) -> Vec<IndexedOutput> {
    Outputs::list(
        repository,
        OutputRequest {
            scope: scope.clone(),
            address: address(scope, "receiver"),
            after: None,
            limit: 10,
        },
    )
    .await
    .expect("outputs")
    .outputs
}

/// The redb contract, run against PostgreSQL: an invalid spend and a stale
/// checkpoint both leave the store untouched, a reorg restores what the
/// orphaned block consumed, and the reverted state is what a later read sees.
#[tokio::test]
async fn reorg_preserves_atomic_canonical_state() {
    let scope = unique_scope();
    let repository = repository!(&scope);

    let first = block(1, 1, 0);
    let funding = output(&scope, "funding", 0, 1);
    assert_eq!(
        repository
            .add(addition(
                &scope,
                first.clone(),
                None,
                one(&scope, "funding"),
                OutputChanges {
                    created: vec![funding.clone()],
                    ..OutputChanges::default()
                },
            ))
            .await
            .expect("first block"),
        BlockOutcome::Applied
    );

    let second = block(2, 2, 1);
    let unknown = OutputKey {
        address: address(&scope, "receiver"),
        output: OutputId {
            transaction: transaction(&scope, "unknown"),
            index: 0,
        },
    };
    let invalid = repository
        .add(addition(
            &scope,
            second.clone(),
            Some(first.clone()),
            one(&scope, "invalid-spend"),
            OutputChanges {
                spent: vec![unknown],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect_err("unknown required output");
    assert_eq!(invalid.kind, IndexErrorKind::InvalidBlock);
    assert_eq!(tip(&repository, &scope).await, Some(first.clone()));
    assert_eq!(history(&repository, &scope, "receiver").await, ["funding"]);
    assert_eq!(outputs(&repository, &scope).await, vec![funding.clone()]);

    let stale = repository
        .add(addition(
            &scope,
            second.clone(),
            None,
            one(&scope, "stale"),
            OutputChanges::default(),
        ))
        .await
        .expect_err("stale checkpoint");
    assert_eq!(stale.kind, IndexErrorKind::Conflict);
    assert_eq!(tip(&repository, &scope).await, Some(first.clone()));
    assert_eq!(history(&repository, &scope, "receiver").await, ["funding"]);

    repository
        .add(addition(
            &scope,
            second.clone(),
            Some(first.clone()),
            one(&scope, "spend"),
            OutputChanges {
                spent: vec![funding.key()],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect("second block");
    assert_eq!(
        history(&repository, &scope, "receiver").await,
        ["funding", "spend"]
    );
    assert!(outputs(&repository, &scope).await.is_empty());

    let counterfeit = block(2, 99, 1);
    let rejected = repository
        .remove(scope.clone(), counterfeit)
        .await
        .expect_err("wrong tip");
    assert_eq!(rejected.kind, IndexErrorKind::Conflict);
    assert_eq!(tip(&repository, &scope).await, Some(second.clone()));

    // The reverted block's spend is restorable only from the journal.
    assert_eq!(
        repository
            .remove(scope.clone(), second)
            .await
            .expect("stored journal rollback"),
        Some(first.clone())
    );
    assert_eq!(tip(&repository, &scope).await, Some(first));
    assert_eq!(history(&repository, &scope, "receiver").await, ["funding"]);
    assert_eq!(outputs(&repository, &scope).await, vec![funding]);
}

/// Reverting a block removes its movements, not just its history rows.
///
/// Movements are deleted by predicate rather than cascaded from history, so an
/// orphan would survive silently — a history query would not show it, because
/// the history row it belonged to is gone. Re-committing a different block at
/// the reverted height with the same transaction makes any survivor visible:
/// the movement primary key would reject the insert.
#[tokio::test]
async fn reverting_removes_movements_not_only_history() {
    let scope = unique_scope();
    let repository = repository!(&scope);

    let first = block(1, 1, 0);
    repository
        .add(addition(
            &scope,
            first.clone(),
            None,
            one(&scope, "funding"),
            OutputChanges::default(),
        ))
        .await
        .expect("first block");

    let orphaned = block(2, 2, 1);
    repository
        .add(addition(
            &scope,
            orphaned.clone(),
            Some(first.clone()),
            one(&scope, "reorged"),
            OutputChanges::default(),
        ))
        .await
        .expect("block that will be reorged away");
    repository
        .remove(scope.clone(), orphaned)
        .await
        .expect("revert");
    assert_eq!(history(&repository, &scope, "receiver").await, ["funding"]);

    // The replacement reuses the transaction id at the same height.
    repository
        .add(addition(
            &scope,
            block(2, 22, 1),
            Some(first),
            one(&scope, "reorged"),
            OutputChanges::default(),
        ))
        .await
        .expect("no movement survived the revert");

    let page = Transactions::list(
        &repository,
        HistoryQuery {
            scope: scope.clone(),
            address: address(&scope, "receiver"),
            after: None,
            limit: 10,
        },
    )
    .await
    .expect("history page");
    let replacement = page
        .transactions
        .iter()
        .find(|entry| entry.transaction_id.value == "reorged")
        .expect("replacement transaction");
    assert_eq!(
        replacement.movements.len(),
        1,
        "the reverted block's movements must not be listed alongside the new ones"
    );
}

/// A block writes one history row per address a transaction touched, and each
/// of those rows carries the transaction's full movement list in order.
///
/// This is what the batched insert has to preserve: the rows for many
/// transactions travel as one statement, so a mix-up would attach a
/// transaction's movements to the wrong address or reorder them.
#[tokio::test]
async fn batched_writes_keep_per_address_rows_and_movement_order() {
    let scope = unique_scope();
    let repository = repository!(&scope);

    let ordered: Vec<ValueMovement> = (0..5)
        .map(|index| transfer(&scope, &format!("movement-{index}"), 100 + index))
        .collect();
    let drafts = vec![
        ObservationDraft {
            scope: scope.clone(),
            transaction_id: transaction(&scope, "alpha"),
            status: ObservationDraftStatus::Included,
            movements: ordered.clone(),
            fee: None,
        },
        ObservationDraft {
            scope: scope.clone(),
            transaction_id: transaction(&scope, "beta"),
            status: ObservationDraftStatus::Failed {
                reason: Some("reverted".into()),
            },
            movements: Vec::new(),
            fee: None,
        },
    ];

    repository
        .add(addition(
            &scope,
            block(1, 1, 0),
            None,
            drafts,
            OutputChanges::default(),
        ))
        .await
        .expect("block with several transactions");

    // Both endpoints of the transfer are watched, so both list both
    // transactions — "beta" through neither endpoint, so only "alpha".
    assert_eq!(history(&repository, &scope, "receiver").await, ["alpha"]);
    assert_eq!(history(&repository, &scope, "sender").await, ["alpha"]);

    let page = Transactions::list(
        &repository,
        HistoryQuery {
            scope: scope.clone(),
            address: address(&scope, "receiver"),
            after: None,
            limit: 10,
        },
    )
    .await
    .expect("history page");
    let alpha = &page.transactions[0];
    assert_eq!(alpha.movements, ordered, "movement order must survive");
}

/// A required spend that matches nothing is an invalid block; a tracked spend
/// that matches nothing is ordinary and leaves the rest of the block intact.
#[tokio::test]
async fn tracked_spends_tolerate_absent_outputs() {
    let scope = unique_scope();
    let repository = repository!(&scope);

    let first = block(1, 1, 0);
    let funding = output(&scope, "funding", 0, 1);
    repository
        .add(addition(
            &scope,
            first.clone(),
            None,
            one(&scope, "funding"),
            OutputChanges {
                created: vec![funding.clone()],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect("first block");

    let absent = OutputKey {
        address: address(&scope, "receiver"),
        output: OutputId {
            transaction: transaction(&scope, "never-indexed"),
            index: 7,
        },
    };
    repository
        .add(addition(
            &scope,
            block(2, 2, 1),
            Some(first),
            one(&scope, "mixed"),
            OutputChanges {
                spent: vec![funding.key()],
                tracked_spends: vec![absent],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect("tracked spend of an unindexed output is not an error");

    assert!(outputs(&repository, &scope).await.is_empty());
    assert_eq!(
        history(&repository, &scope, "receiver").await,
        ["funding", "mixed"]
    );
}

/// Output pages walk the whole set exactly once, in the order the cursor
/// encodes, including across the double-digit index boundary that a textual
/// ordering would get wrong.
#[tokio::test]
async fn output_pagination_covers_every_output_once() {
    let scope = unique_scope();
    let repository = repository!(&scope);

    let created: Vec<IndexedOutput> = (0..12)
        .map(|index| output(&scope, "funding", index, 1))
        .collect();
    repository
        .add(addition(
            &scope,
            block(1, 1, 0),
            None,
            one(&scope, "funding"),
            OutputChanges {
                created: created.clone(),
                ..OutputChanges::default()
            },
        ))
        .await
        .expect("block with many outputs");

    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = Outputs::list(
            &repository,
            OutputRequest {
                scope: scope.clone(),
                address: address(&scope, "receiver"),
                after: after.clone(),
                limit: 5,
            },
        )
        .await
        .expect("output page");
        seen.extend(page.outputs.iter().map(|output| output.id.index));
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }

    // Ordering is by the output identity as a row value, so index 2 precedes
    // index 10. Concatenating the identity into one string would order them
    // textually and put "…:10" before "…:2".
    let expected: Vec<u32> = (0..12).collect();
    assert_eq!(seen, expected, "pages must be ordered and complete");
}

/// History pages behave the same way, and a page boundary must not drop the
/// movements of the transaction it lands on.
#[tokio::test]
async fn history_pagination_keeps_movements_with_their_transaction() {
    let scope = unique_scope();
    let repository = repository!(&scope);

    let drafts: Vec<ObservationDraft> = (0..7)
        .map(|index| ObservationDraft {
            scope: scope.clone(),
            transaction_id: transaction(&scope, &format!("tx-{index:02}")),
            status: ObservationDraftStatus::Included,
            movements: vec![
                transfer(&scope, &format!("m-{index}-a"), 1),
                transfer(&scope, &format!("m-{index}-b"), 2),
            ],
            fee: None,
        })
        .collect();
    repository
        .add(addition(
            &scope,
            block(1, 1, 0),
            None,
            drafts,
            OutputChanges::default(),
        ))
        .await
        .expect("block with several transactions");

    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = Transactions::list(
            &repository,
            HistoryQuery {
                scope: scope.clone(),
                address: address(&scope, "receiver"),
                after: after.clone(),
                limit: 3,
            },
        )
        .await
        .expect("history page");
        for transaction in &page.transactions {
            assert_eq!(
                transaction.movements.len(),
                2,
                "{} lost its movements at a page boundary",
                transaction.transaction_id.value
            );
            seen.push(transaction.transaction_id.value.clone());
        }
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }

    let expected: Vec<String> = (0..7).map(|index| format!("tx-{index:02}")).collect();
    assert_eq!(seen, expected);
}

/// Replaying the tip after a restart reports `AlreadyApplied` rather than
/// writing the block twice, so an observer does not re-fire.
#[tokio::test]
async fn replaying_the_tip_is_not_a_second_commit() {
    let scope = unique_scope();
    let repository = repository!(&scope);

    let first = block(1, 1, 0);
    let commit = || {
        repository.add(addition(
            &scope,
            first.clone(),
            None,
            one(&scope, "funding"),
            OutputChanges::default(),
        ))
    };
    assert_eq!(commit().await.expect("first block"), BlockOutcome::Applied);
    assert_eq!(
        commit().await.expect("replayed block"),
        BlockOutcome::AlreadyApplied
    );
    assert_eq!(history(&repository, &scope, "receiver").await, ["funding"]);
}

/// The registry stores an address once and refuses a second registration of
/// either the identity or the address.
#[tokio::test]
async fn registry_round_trips_and_rejects_duplicates() {
    let scope = unique_scope();
    let repository = repository!(&scope);

    let entry = RegisteredAddress {
        id: format!("{}-deposit", scope.chain.0),
        filter: indexing::AddressFilter {
            address: address(&scope, "receiver"),
            start_height: BlockHeight(4_711),
        },
        material: vec![0xde, 0xad, 0xbe, 0xef],
    };
    repository
        .register(entry.clone())
        .await
        .expect("first registration");

    let duplicate = repository
        .register(entry.clone())
        .await
        .expect_err("same identity twice");
    assert_eq!(duplicate.kind, IndexErrorKind::Conflict);

    let restored = repository.registered(&scope).await.expect("registrations");
    assert_eq!(restored, vec![entry]);
}
