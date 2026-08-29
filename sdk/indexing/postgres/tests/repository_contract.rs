//! Contract coverage for the PostgreSQL repository.
//!
//! Mirrors `indexing-redb`'s repository contract so both backends are held to
//! the same behaviour, and adds the cases the batched write path could plausibly
//! get wrong: per-address duplication, movement ordering inside a transaction,
//! spends that must exist versus spends that may not, and cursor pagination.
//!
//! Every test owns an isolated schema in a disposable container created from
//! the pinned official PostgreSQL image. Missing Docker, image, migration, or
//! database access is therefore a test failure rather than a skipped pass.

mod support;

#[path = "../examples/bench/cleanup.rs"]
mod bench_cleanup;

use std::sync::atomic::{AtomicU64, Ordering};

use indexing::{
    AssetId, BlockAddition, BlockHash, BlockHeight, BlockOutcome, BlockParent, BlockPosition,
    BlockRef, BlockSelector, Blocks, CanonicalAddress, ChainId, Decimal, HistoryQuery,
    IndexErrorKind, IndexScope, IndexedOutput, InterpretedBlock, MovementId, ObservationDraft,
    ObservationDraftStatus, OutputChanges, OutputId, OutputKey, OutputRequest, Outputs,
    RegisteredAddress, Registry, TransactionRef, Transactions, ValueMovement,
};
use indexing_postgres::Repository;

use support::TestDatabase;

static NEXT: AtomicU64 = AtomicU64::new(0);

const INDEXING_TABLES: &[&str] = &[
    "checkpoint",
    "history",
    "journal",
    "journal_output",
    "movement",
    "output",
];
const REGISTRY_TABLES: &[&str] = &["payment_wallets"];

const EXPECTED_COLUMNS: &str = "\
checkpoint.chain:text:NO
checkpoint.network:text:NO
checkpoint.height:int8:NO
checkpoint.hash:bytea:NO
checkpoint.parent_hash:bytea:YES
checkpoint.block_timestamp:int8:YES
checkpoint.position:int8:NO
checkpoint.parent_position:int8:YES
history.chain:text:NO
history.network:text:NO
history.address:text:NO
history.height:int8:NO
history.transaction_id:text:NO
history.status:text:NO
history.failure_reason:text:YES
history.block_hash:bytea:NO
history.block_parent:bytea:YES
history.block_timestamp:int8:YES
history.fee_asset:text:YES
history.fee_amount:numeric:YES
history.fee_payer:text:YES
history.block_position:int8:NO
history.block_parent_position:int8:YES
journal.chain:text:NO
journal.network:text:NO
journal.height:int8:NO
journal.block_hash:bytea:NO
journal.block_parent:bytea:YES
journal.block_timestamp:int8:YES
journal.previous_checkpoint_height:int8:YES
journal.previous_checkpoint_hash:bytea:YES
journal.previous_checkpoint_parent:bytea:YES
journal.previous_checkpoint_time:int8:YES
journal.block_position:int8:NO
journal.block_parent_position:int8:YES
journal.previous_checkpoint_position:int8:YES
journal.previous_checkpoint_parent_position:int8:YES
journal_output.chain:text:NO
journal_output.network:text:NO
journal_output.height:int8:NO
journal_output.transaction_id:text:NO
journal_output.output_index:int4:NO
journal_output.address:text:NO
journal_output.asset_chain:text:NO
journal_output.asset:text:NO
journal_output.amount:numeric:NO
journal_output.evidence:bytea:NO
journal_output.created_at:int8:NO
journal_output.coinbase:bool:NO
movement.chain:text:NO
movement.network:text:NO
movement.address:text:NO
movement.height:int8:NO
movement.transaction_id:text:NO
movement.ordinal:int4:NO
movement.kind:text:NO
movement.movement_id:text:NO
movement.asset_chain:text:NO
movement.asset:text:NO
movement.amount:numeric:NO
movement.from_address:text:YES
movement.to_address:text:YES
output.chain:text:NO
output.network:text:NO
output.transaction_id:text:NO
output.output_index:int4:NO
output.address:text:NO
output.asset_chain:text:NO
output.asset:text:NO
output.amount:numeric:NO
output.evidence:bytea:NO
output.created_at:int8:NO
output.coinbase:bool:NO
payment_wallets.id:text:NO
payment_wallets.chain:text:NO
payment_wallets.network:text:NO
payment_wallets.address:text:NO
payment_wallets.start_height:int8:NO
payment_wallets.secret:bytea:NO
payment_wallets.created_at:timestamptz:NO";

const EXPECTED_INDEXES: &str = "\
checkpoint.checkpoint_pkey:true:true:chain,network
history.history_by_height:false:false:chain,network,height
history.history_pkey:true:true:chain,network,address,height,transaction_id
journal.journal_pkey:true:true:chain,network,height
journal_output.journal_output_pkey:true:true:chain,network,height,transaction_id,output_index
movement.movement_by_height:false:false:chain,network,height
movement.movement_pkey:true:true:chain,network,address,height,transaction_id,ordinal
output.output_by_address_identity:false:false:chain,network,address,transaction_id,output_index
output.output_by_height:false:false:chain,network,created_at
output.output_pkey:true:true:chain,network,transaction_id,output_index
payment_wallets.payment_wallets_by_scope:false:false:chain,network
payment_wallets.payment_wallets_chain_network_address_key:false:true:chain,network,address
payment_wallets.payment_wallets_pkey:true:true:id";

const EXPECTED_CONSTRAINT_TYPES: &str = "\
checkpoint:c:2
checkpoint:p:1
history:c:3
history:p:1
journal:c:3
journal:p:1
journal_output:f:1
journal_output:p:1
movement:c:1
movement:p:1
output:p:1
payment_wallets:p:1
payment_wallets:u:1";

const HOLD_SCOPE_LOCK: &str = "\
SELECT pg_advisory_xact_lock(hashtextextended(
    octet_length($1::text)::text || ':' || $1::text ||
    octet_length($2::text)::text || ':' || $2::text,
    5787213827046134867
))";

#[tokio::test]
async fn wrong_database_credentials_fail_instead_of_skipping() {
    let database = TestDatabase::start().await;
    let pool = indexing_postgres::pool(&database.url_with_password("intentionally-wrong"), 1)
        .expect("wrong credentials still form a valid pool configuration");

    assert!(
        pool.get().await.is_err(),
        "wrong PostgreSQL credentials must fail instead of skipping the test"
    );
}

#[tokio::test]
async fn baseline_migrations_match_catalog_keys_and_ownership() {
    let database = TestDatabase::start().await;
    let client = database
        .pool()
        .get()
        .await
        .expect("baseline catalog connection");

    let tables: Vec<String> = client
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = current_schema() ORDER BY table_name",
            &[],
        )
        .await
        .expect("read baseline tables")
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    let expected_tables: Vec<String> = INDEXING_TABLES
        .iter()
        .chain(REGISTRY_TABLES)
        .map(|table| (*table).to_owned())
        .collect();
    assert_eq!(tables, expected_tables, "every table must have one owner");

    let columns: String = client
        .query_one(
            "SELECT STRING_AGG(\
                 table_name || '.' || column_name || ':' || udt_name || ':' || is_nullable,\
                 E'\\n' ORDER BY table_name, ordinal_position\
             ) FROM information_schema.columns WHERE table_schema = current_schema()",
            &[],
        )
        .await
        .expect("read baseline columns")
        .get(0);
    assert_eq!(columns, EXPECTED_COLUMNS, "effective columns drifted");

    let indexes: String = client
        .query_one(
            r#"SELECT STRING_AGG(
                   table_name || '.' || index_name || ':' || is_primary || ':' || is_unique
                   || ':' || columns, E'\n' ORDER BY table_name, index_name
               )
               FROM (
                   SELECT table_relation.relname AS table_name,
                          index_relation.relname AS index_name,
                          definition.indisprimary::text AS is_primary,
                          definition.indisunique::text AS is_unique,
                          (
                              SELECT STRING_AGG(indexed_attribute.attname, ',' ORDER BY index_key.ordinality)
                              FROM UNNEST(definition.indkey)
                                   WITH ORDINALITY AS index_key(attnum, ordinality)
                              JOIN pg_attribute indexed_attribute
                                ON indexed_attribute.attrelid = table_relation.oid
                               AND indexed_attribute.attnum = index_key.attnum
                          ) AS columns
                   FROM pg_index definition
                   JOIN pg_class table_relation ON table_relation.oid = definition.indrelid
                   JOIN pg_class index_relation ON index_relation.oid = definition.indexrelid
                   JOIN pg_namespace namespace ON namespace.oid = table_relation.relnamespace
                   WHERE namespace.nspname = current_schema()
               ) baseline_indexes"#,
            &[],
        )
        .await
        .expect("read baseline indexes")
        .get(0);
    assert_eq!(
        indexes, EXPECTED_INDEXES,
        "scope or pagination keys drifted"
    );

    let constraints: String = client
        .query_one(
            r#"SELECT STRING_AGG(
                   table_name || ':' || constraint_type || ':' || constraint_count,
                   E'\n' ORDER BY table_name, constraint_type
               )
               FROM (
                   SELECT relation.relname AS table_name,
                          constraint_record.contype::text AS constraint_type,
                          COUNT(*)::text AS constraint_count
                   FROM pg_constraint constraint_record
                   JOIN pg_class relation ON relation.oid = constraint_record.conrelid
                   JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
                   WHERE namespace.nspname = current_schema()
                     AND constraint_record.contype IN ('p', 'u', 'f', 'c')
                   GROUP BY relation.relname, constraint_record.contype
               ) baseline_constraints"#,
            &[],
        )
        .await
        .expect("read baseline constraints")
        .get(0);
    assert_eq!(
        constraints, EXPECTED_CONSTRAINT_TYPES,
        "effective constraints drifted"
    );

    let journal_delete_cascades: bool = client
        .query_one(
            "SELECT confdeltype = 'c' FROM pg_constraint \
             WHERE conrelid = 'journal_output'::regclass AND contype = 'f'",
            &[],
        )
        .await
        .expect("read journal-output foreign key")
        .get(0);
    assert!(
        journal_delete_cascades,
        "journal-output deletion must cascade"
    );

    let movement_foreign_keys: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM pg_constraint \
             WHERE conrelid = 'movement'::regclass AND contype = 'f'",
            &[],
        )
        .await
        .expect("read movement foreign keys")
        .get(0);
    assert_eq!(
        movement_foreign_keys, 0,
        "migration 0003 must remove the movement foreign key"
    );
}

#[tokio::test]
async fn startup_schema_validation_is_read_only_and_compatible() {
    let database = TestDatabase::start().await;

    indexing_postgres::validate_schema(&database.pool(), database.schema())
        .await
        .expect("reviewed baseline schema is compatible");

    assert!(
        database.registry_sentinel_unchanged().await,
        "read-only startup validation must preserve payment_wallets"
    );
}

#[tokio::test]
async fn startup_schema_validation_rejects_a_missing_relation() {
    let database = TestDatabase::start().await;
    database
        .pool()
        .get()
        .await
        .expect("missing-relation setup connection")
        .batch_execute("DROP TABLE checkpoint")
        .await
        .expect("remove required relation in owned schema");

    assert_schema_incompatible(
        indexing_postgres::validate_schema(&database.pool(), database.schema()).await,
        "incompatible columns",
    );
}

#[tokio::test]
async fn startup_schema_validation_rejects_a_wrong_column() {
    let database = TestDatabase::start().await;
    database
        .pool()
        .get()
        .await
        .expect("wrong-column setup connection")
        .batch_execute("ALTER TABLE checkpoint ALTER COLUMN height TYPE integer")
        .await
        .expect("change required column in owned schema");

    assert_schema_incompatible(
        indexing_postgres::validate_schema(&database.pool(), database.schema()).await,
        "incompatible columns",
    );
}

#[tokio::test]
async fn startup_schema_validation_rejects_the_wrong_schema() {
    let database = TestDatabase::start().await;
    database
        .pool()
        .get()
        .await
        .expect("wrong-schema setup connection")
        .batch_execute("CREATE SCHEMA startup_wrong")
        .await
        .expect("create wrong owned schema");
    let wrong_pool = database.pool_for_schema("startup_wrong");

    assert_schema_incompatible(
        indexing_postgres::validate_schema(&wrong_pool, database.schema()).await,
        "resolved schema startup_wrong",
    );
}

fn assert_schema_incompatible(result: Result<(), indexing::IndexError>, message: &str) {
    let error = result.expect_err("incompatible schema must fail startup validation");
    assert_eq!(error.kind, IndexErrorKind::Store);
    assert!(!error.retryable);
    assert!(
        error.message.contains(message),
        "unexpected compatibility error: {}",
        error.message
    );
}

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

async fn repository(scope: &IndexScope) -> (TestDatabase, Repository) {
    let database = TestDatabase::start().await;
    let repository = Repository::new(database.pool(), scope.clone()).expect("repository");
    (database, repository)
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

fn sparse_block(
    position: u64,
    height: u64,
    hash: u8,
    parent_position: u64,
    parent_hash: u8,
) -> BlockRef {
    BlockRef {
        position: BlockPosition(position),
        height: BlockHeight(height),
        hash: BlockHash(vec![hash]),
        parent: Some(BlockParent {
            position: BlockPosition(parent_position),
            hash: BlockHash(vec![parent_hash]),
        }),
        timestamp: Some(1_000 + position),
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
    asset_named(scope, "native")
}

fn asset_named(scope: &IndexScope, name: &str) -> AssetId {
    AssetId {
        chain: scope.chain.clone(),
        asset: name.into(),
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
    draft_with_asset(scope, id, "native")
}

fn draft_with_asset(scope: &IndexScope, id: &str, asset_name: &str) -> ObservationDraft {
    ObservationDraft {
        scope: scope.clone(),
        transaction_id: transaction(scope, id),
        status: ObservationDraftStatus::Included,
        movements: vec![ValueMovement::Transfer {
            id: MovementId(format!("movement-{id}")),
            asset: asset_named(scope, asset_name),
            amount: Decimal::from(1_u64),
            from: address(scope, "sender"),
            to: address(scope, "receiver"),
        }],
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

#[tokio::test]
async fn duplicate_output_identity_is_rejected_before_repository_add() {
    let scope = unique_scope();
    let (_database, repository) = repository(&scope).await;
    let first = output(&scope, "duplicate", 0, 1);
    let mut second = first.clone();
    second.address = address(&scope, "another-receiver");

    let error = BlockAddition::new(
        scope.clone(),
        None,
        4,
        InterpretedBlock {
            block: block(1, 1, 0),
            transactions: one(&scope, "duplicate"),
            outputs: OutputChanges {
                created: vec![first, second],
                ..OutputChanges::default()
            },
        },
    )
    .expect_err("duplicate output identity");

    assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    assert_eq!(tip(&repository, &scope).await, None);
}

#[tokio::test]
async fn concurrent_first_commits_are_serialized_by_scope() {
    let scope = unique_scope();
    let (database, repository) = repository(&scope).await;
    let first = block(1, 1, 0);
    let candidate = addition(
        &scope,
        first.clone(),
        None,
        one(&scope, "funding"),
        OutputChanges::default(),
    );

    let mut holder = database
        .pool()
        .get()
        .await
        .expect("scope-lock holder connection");
    let holder = holder
        .transaction()
        .await
        .expect("scope-lock holder transaction");
    holder
        .query_one(HOLD_SCOPE_LOCK, &[&scope.chain.0, &scope.network])
        .await
        .expect("hold exact scope lock");

    let first_repository = repository.clone();
    let first_candidate = candidate.clone();
    let first_commit = tokio::spawn(async move { first_repository.add(first_candidate).await });
    let second_repository = repository.clone();
    let second_commit = tokio::spawn(async move { second_repository.add(candidate).await });

    let mut waiting = 0_i64;
    for _ in 0..200 {
        waiting = holder
            .query_one(
                "SELECT COUNT(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted",
                &[],
            )
            .await
            .expect("count waiting scope locks")
            .get(0);
        if waiting == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(waiting, 2, "both first commits must wait on the scope lock");
    holder.commit().await.expect("release exact scope lock");

    let first_outcome = first_commit.await.expect("first commit task");
    let second_outcome = second_commit.await.expect("second commit task");
    assert!(
        matches!(
            (&first_outcome, &second_outcome),
            (Ok(BlockOutcome::Applied), Ok(BlockOutcome::AlreadyApplied))
                | (Ok(BlockOutcome::AlreadyApplied), Ok(BlockOutcome::Applied))
        ),
        "one writer must apply and the serialized replay must be idempotent: {first_outcome:?}, {second_outcome:?}"
    );
    assert_eq!(tip(&repository, &scope).await, Some(first));
    assert_eq!(history(&repository, &scope, "receiver").await, ["funding"]);
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

async fn scope_signature(database: &TestDatabase, scope: &IndexScope) -> Vec<(String, String)> {
    let client = database
        .pool()
        .get()
        .await
        .expect("scope-signature connection");
    let mut signature = Vec::with_capacity(INDEXING_TABLES.len());
    for table in INDEXING_TABLES {
        let statement = format!(
            "SELECT COALESCE(STRING_AGG(to_jsonb(scoped)::text, E'\\n' \
             ORDER BY to_jsonb(scoped)::text), '') \
             FROM (SELECT * FROM {table} WHERE chain = $1 AND network = $2) scoped"
        );
        let rows: String = client
            .query_one(&statement, &[&scope.chain.0, &scope.network])
            .await
            .expect("read exact scope signature")
            .get(0);
        signature.push(((*table).to_owned(), rows));
    }
    signature
}

/// The redb contract, run against PostgreSQL: an invalid spend and a stale
/// checkpoint both leave the store untouched, a reorg restores what the
/// orphaned block consumed, and the reverted state is what a later read sees.
#[tokio::test]
async fn reorg_preserves_atomic_canonical_state() {
    let scope = unique_scope();
    let (_database, repository) = repository(&scope).await;

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

#[tokio::test]
async fn sparse_coordinates_round_trip_through_new_handle_and_rollback() {
    let scope = unique_scope();
    let (database, repository) = repository(&scope).await;
    let first = sparse_block(100, 50, 1, 97, 0);
    let second = sparse_block(103, 51, 2, 100, 1);

    repository
        .add(addition(
            &scope,
            first.clone(),
            None,
            one(&scope, "sparse-first"),
            OutputChanges::default(),
        ))
        .await
        .expect("first sparse block");
    repository
        .add(addition(
            &scope,
            second.clone(),
            Some(first.clone()),
            one(&scope, "sparse-second"),
            OutputChanges::default(),
        ))
        .await
        .expect("second sparse block");

    drop(repository);
    let repository = Repository::new(database.pool(), scope.clone()).expect("new handle");
    assert_eq!(tip(&repository, &scope).await, Some(second.clone()));
    assert_eq!(
        repository
            .get(BlockSelector::Height {
                scope: scope.clone(),
                height: BlockHeight(50),
            })
            .await
            .expect("first retained block"),
        Some(first.clone())
    );
    assert_eq!(
        repository
            .get(BlockSelector::Height {
                scope: scope.clone(),
                height: BlockHeight(51),
            })
            .await
            .expect("second retained block"),
        Some(second.clone())
    );

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
    .expect("sparse history");
    let blocks: Vec<BlockRef> = page
        .transactions
        .into_iter()
        .map(|transaction| transaction.block().clone())
        .collect();
    assert_eq!(blocks, [first.clone(), second.clone()]);

    assert_eq!(
        repository
            .remove(scope.clone(), second)
            .await
            .expect("sparse rollback"),
        Some(first.clone())
    );
    drop(repository);
    let repository = Repository::new(database.pool(), scope.clone()).expect("reopened handle");
    assert_eq!(tip(&repository, &scope).await, Some(first));
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
    let (_database, repository) = repository(&scope).await;

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
    let (_database, repository) = repository(&scope).await;

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
    let (_database, repository) = repository(&scope).await;

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

#[tokio::test]
async fn required_spend_cannot_remove_another_address_output() {
    let scope = unique_scope();
    let (_database, repository) = repository(&scope).await;

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
        .expect("funding block");

    let wrong_address = OutputKey {
        address: address(&scope, "intruder"),
        output: funding.id.clone(),
    };
    let error = repository
        .add(addition(
            &scope,
            block(2, 2, 1),
            Some(first.clone()),
            one(&scope, "wrong-required-spend"),
            OutputChanges {
                spent: vec![wrong_address],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect_err("wrong-address required spend must miss");

    assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    assert_eq!(tip(&repository, &scope).await, Some(first));
    assert_eq!(outputs(&repository, &scope).await, vec![funding]);
}

#[tokio::test]
async fn tracked_spend_cannot_remove_another_address_output() {
    let scope = unique_scope();
    let (_database, repository) = repository(&scope).await;

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
        .expect("funding block");

    let wrong_address = OutputKey {
        address: address(&scope, "intruder"),
        output: funding.id.clone(),
    };
    let second = block(2, 2, 1);
    repository
        .add(addition(
            &scope,
            second.clone(),
            Some(first),
            one(&scope, "wrong-tracked-spend"),
            OutputChanges {
                tracked_spends: vec![wrong_address],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect("wrong-address tracked spend is an ordinary miss");

    assert_eq!(tip(&repository, &scope).await, Some(second));
    assert_eq!(outputs(&repository, &scope).await, vec![funding]);
}

/// Output pages walk the whole set exactly once, in the order the cursor
/// encodes, including across the double-digit index boundary that a textual
/// ordering would get wrong.
#[tokio::test]
async fn output_pagination_covers_every_output_once() {
    let scope = unique_scope();
    let (_database, repository) = repository(&scope).await;

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

#[tokio::test]
async fn output_page_keeps_one_snapshot_during_projection_drift() {
    let scope = unique_scope();
    let (database, repository) = repository(&scope).await;
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

    let mut writer = database
        .pool()
        .get()
        .await
        .expect("output-drift connection");
    let writer = writer
        .transaction()
        .await
        .expect("output-drift transaction");
    writer
        .batch_execute("LOCK TABLE output IN ACCESS EXCLUSIVE MODE")
        .await
        .expect("hold output projection query");

    let reader = repository.clone();
    let reader_scope = scope.clone();
    let page = tokio::spawn(async move {
        Outputs::list(
            &reader,
            OutputRequest {
                scope: reader_scope.clone(),
                address: address(&reader_scope, "receiver"),
                after: None,
                limit: 10,
            },
        )
        .await
    });

    let mut waiting = 0_i64;
    for _ in 0..200 {
        waiting = writer
            .query_one(
                "SELECT COUNT(*) FROM pg_locks held \
                 JOIN pg_class relation ON relation.oid = held.relation \
                 WHERE held.locktype = 'relation' AND relation.relname = 'output' \
                   AND NOT held.granted",
                &[],
            )
            .await
            .expect("count blocked output readers")
            .get(0);
        if waiting == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(waiting, 1, "output page must pause after its checkpoint");

    writer
        .execute(
            "DELETE FROM output WHERE chain = $1 AND network = $2",
            &[&scope.chain.0, &scope.network],
        )
        .await
        .expect("change live output projection");
    let second = block(2, 2, 1);
    let second_position = i64::try_from(second.position.0).expect("test position");
    let second_height = i64::try_from(second.height.0).expect("test height");
    let second_parent_position = second
        .parent
        .as_ref()
        .map(|parent| i64::try_from(parent.position.0).expect("test parent position"));
    writer
        .execute(
            "UPDATE checkpoint SET position = $3, height = $4, hash = $5, \
             parent_position = $6, parent_hash = $7, block_timestamp = $8 \
             WHERE chain = $1 AND network = $2",
            &[
                &scope.chain.0,
                &scope.network,
                &second_position,
                &second_height,
                &second.hash.0,
                &second_parent_position,
                &second.parent.as_ref().map(|parent| parent.hash.0.clone()),
                &second
                    .timestamp
                    .map(|timestamp| i64::try_from(timestamp).expect("test timestamp")),
            ],
        )
        .await
        .expect("move checkpoint with output projection");
    writer.commit().await.expect("publish output drift");

    let page = page
        .await
        .expect("output reader task")
        .expect("repeatable output page");
    assert_eq!(page.checkpoint, Some(first));
    assert_eq!(page.outputs, [funding]);

    let current = Outputs::list(
        &repository,
        OutputRequest {
            scope: scope.clone(),
            address: address(&scope, "receiver"),
            after: None,
            limit: 10,
        },
    )
    .await
    .expect("current output page");
    assert_eq!(current.checkpoint, Some(second));
    assert!(current.outputs.is_empty());
}

#[tokio::test]
async fn benchmark_cleanup_preserves_other_scopes_and_registry() {
    let target = unique_scope();
    let database = TestDatabase::start().await;
    let target_repository =
        Repository::new(database.pool(), target.clone()).expect("target repository");
    let other = IndexScope {
        chain: ChainId(format!("{}-other", target.chain.0)),
        network: target.network.clone(),
    };
    let other_repository =
        Repository::new(database.pool(), other.clone()).expect("other repository");
    let target_output = output(&target, "target", 0, 1);
    let other_output = output(&other, "other", 0, 1);

    target_repository
        .add(addition(
            &target,
            block(1, 1, 0),
            None,
            one(&target, "target"),
            OutputChanges {
                created: vec![target_output],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect("target block");
    let other_block = block(1, 2, 0);
    other_repository
        .add(addition(
            &other,
            other_block.clone(),
            None,
            one(&other, "other"),
            OutputChanges {
                created: vec![other_output.clone()],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect("other block");

    let mut client = database
        .pool()
        .get()
        .await
        .expect("benchmark cleanup connection");
    bench_cleanup::clear_scope(&mut client, &target)
        .await
        .expect("clean exact benchmark scope");

    assert_eq!(tip(&target_repository, &target).await, None);
    assert!(
        history(&target_repository, &target, "receiver")
            .await
            .is_empty()
    );
    assert!(outputs(&target_repository, &target).await.is_empty());
    assert_eq!(tip(&other_repository, &other).await, Some(other_block));
    assert_eq!(
        history(&other_repository, &other, "receiver").await,
        ["other"]
    );
    assert_eq!(outputs(&other_repository, &other).await, [other_output]);
    assert!(
        database.registry_sentinel_unchanged().await,
        "benchmark cleanup must preserve payment_wallets"
    );
}

#[tokio::test]
async fn one_pool_isolates_scopes_and_preserves_native_token_facts() {
    let native_scope = unique_scope();
    let token_scope = IndexScope {
        chain: ChainId(format!("{}-token", native_scope.chain.0)),
        network: format!("{}-tokennet", native_scope.network),
    };
    let database = TestDatabase::start().await;
    let pool = database.pool();
    let native_repository =
        Repository::new(pool.clone(), native_scope.clone()).expect("native repository");
    let token_repository = Repository::new(pool, token_scope.clone()).expect("token repository");

    let native_first = block(1, 1, 0);
    let native_output = output(&native_scope, "native", 0, 1);
    native_repository
        .add(addition(
            &native_scope,
            native_first.clone(),
            None,
            one(&native_scope, "native"),
            OutputChanges {
                created: vec![native_output.clone()],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect("native scope block");

    let token_first = block(1, 2, 0);
    token_repository
        .add(addition(
            &token_scope,
            token_first.clone(),
            None,
            vec![draft_with_asset(&token_scope, "token", "usdc")],
            OutputChanges::default(),
        ))
        .await
        .expect("token scope block");

    let cross_scope = native_repository
        .get(BlockSelector::Tip(token_scope.clone()))
        .await
        .expect_err("repository handle must reject another scope");
    assert_eq!(cross_scope.kind, IndexErrorKind::ScopeMismatch);
    let cross_scope_write = native_repository
        .add(addition(
            &token_scope,
            block(2, 9, 2),
            Some(token_first.clone()),
            vec![draft_with_asset(&token_scope, "cross-scope", "usdc")],
            OutputChanges::default(),
        ))
        .await
        .expect_err("repository handle must reject another scope's write");
    assert_eq!(cross_scope_write.kind, IndexErrorKind::ScopeMismatch);

    let native_page = Transactions::list(
        &native_repository,
        HistoryQuery {
            scope: native_scope.clone(),
            address: address(&native_scope, "receiver"),
            after: None,
            limit: 10,
        },
    )
    .await
    .expect("native history");
    let token_page = Transactions::list(
        &token_repository,
        HistoryQuery {
            scope: token_scope.clone(),
            address: address(&token_scope, "receiver"),
            after: None,
            limit: 10,
        },
    )
    .await
    .expect("token history");
    assert_eq!(
        native_page.transactions[0].movements[0].asset().asset,
        "native"
    );
    assert_eq!(
        token_page.transactions[0].movements[0].asset().asset,
        "usdc"
    );

    let token_before = scope_signature(&database, &token_scope).await;
    let native_second = block(2, 3, 1);
    native_repository
        .add(addition(
            &native_scope,
            native_second.clone(),
            Some(native_first),
            one(&native_scope, "spend-native"),
            OutputChanges {
                spent: vec![native_output.key()],
                ..OutputChanges::default()
            },
        ))
        .await
        .expect("mutate only native scope");

    assert_eq!(
        tip(&native_repository, &native_scope).await,
        Some(native_second)
    );
    assert!(outputs(&native_repository, &native_scope).await.is_empty());
    assert_eq!(
        tip(&token_repository, &token_scope).await,
        Some(token_first)
    );
    assert_eq!(scope_signature(&database, &token_scope).await, token_before);
    assert!(
        database.registry_sentinel_unchanged().await,
        "shared-pool writes must preserve payment_wallets"
    );
}

/// History pages behave the same way, and a page boundary must not drop the
/// movements of the transaction it lands on.
#[tokio::test]
async fn history_pagination_keeps_movements_with_their_transaction() {
    let scope = unique_scope();
    let (_database, repository) = repository(&scope).await;

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

#[tokio::test]
async fn history_page_keeps_one_snapshot_during_checkpoint_drift() {
    let scope = unique_scope();
    let (database, repository) = repository(&scope).await;
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

    let mut writer = database
        .pool()
        .get()
        .await
        .expect("checkpoint-drift connection");
    let writer = writer
        .transaction()
        .await
        .expect("checkpoint-drift transaction");
    writer
        .batch_execute("LOCK TABLE movement IN ACCESS EXCLUSIVE MODE")
        .await
        .expect("hold movement query between page reads");

    let reader = repository.clone();
    let reader_scope = scope.clone();
    let page = tokio::spawn(async move {
        Transactions::list(
            &reader,
            HistoryQuery {
                scope: reader_scope.clone(),
                address: address(&reader_scope, "receiver"),
                after: None,
                limit: 10,
            },
        )
        .await
    });

    let mut waiting = 0_i64;
    for _ in 0..200 {
        waiting = writer
            .query_one(
                "SELECT COUNT(*) FROM pg_locks held \
                 JOIN pg_class relation ON relation.oid = held.relation \
                 WHERE held.locktype = 'relation' AND relation.relname = 'movement' \
                   AND NOT held.granted",
                &[],
            )
            .await
            .expect("count blocked movement readers")
            .get(0);
        if waiting == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(waiting, 1, "history must pause between its page queries");

    let second = block(2, 2, 1);
    let second_position = i64::try_from(second.position.0).expect("test position");
    let second_height = i64::try_from(second.height.0).expect("test height");
    let second_parent_position = second
        .parent
        .as_ref()
        .map(|parent| i64::try_from(parent.position.0).expect("test parent position"));
    writer
        .execute(
            "UPDATE checkpoint SET position = $3, height = $4, hash = $5, \
             parent_position = $6, parent_hash = $7, block_timestamp = $8 \
             WHERE chain = $1 AND network = $2",
            &[
                &scope.chain.0,
                &scope.network,
                &second_position,
                &second_height,
                &second.hash.0,
                &second_parent_position,
                &second.parent.as_ref().map(|parent| parent.hash.0.clone()),
                &second
                    .timestamp
                    .map(|timestamp| i64::try_from(timestamp).expect("test timestamp")),
            ],
        )
        .await
        .expect("move checkpoint between history page queries");
    writer.commit().await.expect("publish checkpoint drift");

    let page = page
        .await
        .expect("history reader task")
        .expect("repeatable history page");
    assert_eq!(page.checkpoint, Some(first));
    assert_eq!(page.transactions.len(), 1);
    assert_eq!(page.transactions[0].transaction_id.value, "funding");
    assert_eq!(page.transactions[0].movements.len(), 1);
    assert_eq!(tip(&repository, &scope).await, Some(second));
}

/// Replaying the tip after a restart reports `AlreadyApplied` rather than
/// writing the block twice, so an observer does not re-fire.
#[tokio::test]
async fn replaying_the_tip_is_not_a_second_commit() {
    let scope = unique_scope();
    let (_database, repository) = repository(&scope).await;

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
    let (_database, repository) = repository(&scope).await;

    let entry = RegisteredAddress {
        id: format!("{}-deposit", scope.chain.0),
        filter: indexing::AddressFilter {
            address: address(&scope, "receiver"),
            start_position: BlockPosition(4_711),
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
