#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use support::TestDatabase;

const BLOCK_POSITIONS: &str = include_str!("../migrations/0004_block_positions.sql");
const BLOCK_POSITIONS_SHA256: &str =
    "5019860075ddc36d4aca97de660968c92b77f42efaabe70fe226b74f978696c7";
const VERIFIED_SCOPES: &str = r#"[
    {"chain":"bitcoin","network":"regtest"},
    {"chain":"ethereum","network":"mainnet"}
]"#;
const BASELINE_TABLES: [&str; 7] = [
    "checkpoint",
    "history",
    "movement",
    "output",
    "journal",
    "journal_output",
    "payment_wallets",
];

#[derive(Debug, Eq, PartialEq)]
struct TableSignature {
    rows: usize,
    sha256: String,
}

#[test]
fn block_position_migration_changes_only_approved_relations_and_columns() {
    assert_eq!(
        format!("{:x}", Sha256::digest(BLOCK_POSITIONS.as_bytes())),
        BLOCK_POSITIONS_SHA256,
        "finalized migration checksum changed"
    );
    let alters = BLOCK_POSITIONS
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("ALTER TABLE") && line.contains("ADD COLUMN"))
        .collect::<Vec<_>>();

    assert_eq!(
        alters,
        [
            "ALTER TABLE checkpoint ADD COLUMN position bigint;",
            "ALTER TABLE checkpoint ADD COLUMN parent_position bigint;",
            "ALTER TABLE history ADD COLUMN block_position bigint;",
            "ALTER TABLE history ADD COLUMN block_parent_position bigint;",
            "ALTER TABLE journal ADD COLUMN block_position bigint;",
            "ALTER TABLE journal ADD COLUMN block_parent_position bigint;",
            "ALTER TABLE journal ADD COLUMN previous_checkpoint_position bigint;",
            "ALTER TABLE journal ADD COLUMN previous_checkpoint_parent_position bigint;",
        ]
    );

    let executable = BLOCK_POSITIONS
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!executable.contains("ALTER TABLE movement"));
    assert!(!executable.contains("ALTER TABLE output"));
    assert!(!executable.contains("ALTER TABLE journal_output"));
    assert!(!executable.contains("ALTER TABLE payment_wallets"));
    assert!(!executable.contains("UPDATE movement"));
    assert!(!executable.contains("UPDATE output"));
    assert!(!executable.contains("UPDATE journal_output"));
    assert!(!executable.contains("UPDATE payment_wallets"));
    for constraint in [
        "checkpoint_position_nonnegative",
        "checkpoint_parent_complete",
        "history_block_position_nonnegative",
        "history_block_parent_complete",
        "journal_block_position_nonnegative",
        "journal_block_parent_complete",
        "journal_previous_checkpoint_complete",
    ] {
        assert!(
            executable.contains(&format!("ADD CONSTRAINT {constraint}")),
            "missing final constraint {constraint}"
        );
        assert!(
            executable.contains(&format!("VALIDATE CONSTRAINT {constraint}")),
            "missing validation for {constraint}"
        );
    }
    for required in [
        "checkpoint ALTER COLUMN position SET NOT NULL",
        "history ALTER COLUMN block_position SET NOT NULL",
        "journal ALTER COLUMN block_position SET NOT NULL",
    ] {
        assert!(executable.contains(required), "missing {required}");
    }
    assert!(executable.contains("payment_sdk.verified_dense_scopes"));
    assert!(executable.contains("unverified populated scope"));
}

#[tokio::test]
async fn fresh_schema_receives_final_coordinate_constraints() {
    let database = TestDatabase::start_baseline().await;
    let pool = database.pool();
    let client = pool.get().await.expect("fresh migration connection");

    client
        .batch_execute(BLOCK_POSITIONS)
        .await
        .expect("apply finalized migration to fresh schema");

    assert_coordinate_schema(&client).await;
    assert_final_constraints_reject_invalid_writes(&client).await;
    assert!(database.registry_sentinel_unchanged().await);
}

#[tokio::test]
async fn dense_backfill_preserves_baseline_rows_and_application_bytes() {
    let database = TestDatabase::start_baseline().await;
    let pool = database.pool();
    let client = pool.get().await.expect("dense backfill connection");
    insert_dense_fixtures(&client).await;
    let before = baseline_signatures(&client).await;

    client
        .query_one(
            "SELECT set_config('payment_sdk.verified_dense_scopes', $1, false)",
            &[&VERIFIED_SCOPES],
        )
        .await
        .expect("set verified dense scopes");
    client
        .batch_execute(BLOCK_POSITIONS)
        .await
        .expect("rehearse dense coordinate backfill");

    assert_eq!(baseline_signatures(&client).await, before);
    assert_dense_coordinates(&client).await;
    assert_coordinate_schema(&client).await;
    assert!(database.registry_sentinel_unchanged().await);
}

#[tokio::test]
async fn unknown_populated_scope_aborts_the_complete_migration() {
    let database = TestDatabase::start_baseline().await;
    let pool = database.pool();
    let client = pool.get().await.expect("unknown-scope connection");
    client
        .execute(
            "INSERT INTO checkpoint (chain, network, height, hash, parent_hash) \
             VALUES ('solana', 'mainnet', 1, decode('01', 'hex'), decode('00', 'hex'))",
            &[],
        )
        .await
        .expect("insert unknown populated scope");
    let before = baseline_signatures(&client).await;
    client
        .query_one(
            "SELECT set_config('payment_sdk.verified_dense_scopes', $1, false)",
            &[&VERIFIED_SCOPES],
        )
        .await
        .expect("set verified dense scopes");

    let error = client
        .batch_execute(BLOCK_POSITIONS)
        .await
        .expect_err("unknown populated scope must abort");
    let message = database_error_message(&error);
    assert!(
        message.contains("unverified populated scope"),
        "unexpected migration error: {message}"
    );
    client
        .batch_execute("ROLLBACK")
        .await
        .expect("close aborted migration transaction");

    assert_eq!(coordinate_column_count(&client).await, 0);
    assert_eq!(baseline_signatures(&client).await, before);
    assert!(database.registry_sentinel_unchanged().await);
}

#[tokio::test]
async fn invalid_retained_parent_rolls_back_the_complete_migration() {
    let database = TestDatabase::start_baseline().await;
    let pool = database.pool();
    let client = pool.get().await.expect("invalid-parent connection");
    client
        .execute(
            "INSERT INTO checkpoint (chain, network, height, hash, parent_hash) \
             VALUES ('bitcoin', 'regtest', 1, decode('01', 'hex'), NULL)",
            &[],
        )
        .await
        .expect("insert invalid retained parent");
    let before = baseline_signatures(&client).await;
    client
        .query_one(
            "SELECT set_config('payment_sdk.verified_dense_scopes', $1, false)",
            &[&VERIFIED_SCOPES],
        )
        .await
        .expect("set verified dense scopes");

    let error = client
        .batch_execute(BLOCK_POSITIONS)
        .await
        .expect_err("invalid retained parent must abort");
    assert!(
        database_error_message(&error).contains("invalid dense parent relationship"),
        "unexpected migration error: {}",
        database_error_message(&error)
    );
    client
        .batch_execute("ROLLBACK")
        .await
        .expect("close invalid-row migration transaction");

    assert_eq!(coordinate_column_count(&client).await, 0);
    assert_eq!(baseline_signatures(&client).await, before);
    assert!(database.registry_sentinel_unchanged().await);
}

async fn insert_dense_fixtures(client: &tokio_postgres::Client) {
    client
        .batch_execute(
            "
            INSERT INTO checkpoint
                (chain, network, height, hash, parent_hash, block_timestamp)
            VALUES
                ('bitcoin', 'regtest', 2, decode('02', 'hex'), decode('01', 'hex'), 20),
                ('ethereum', 'mainnet', 7, decode('07', 'hex'), decode('06', 'hex'), 70);

            INSERT INTO history
                (chain, network, address, height, transaction_id, status,
                 block_hash, block_parent, block_timestamp, fee_asset, fee_amount, fee_payer)
            VALUES
                ('bitcoin', 'regtest', 'btc-address', 2, 'btc-transaction', 'included',
                 decode('02', 'hex'), decode('01', 'hex'), 20, 'btc', 2, 'btc-address'),
                ('ethereum', 'mainnet', 'eth-address', 7, 'eth-transaction', 'included',
                 decode('07', 'hex'), decode('06', 'hex'), 70, 'eth', 7, 'eth-address');

            INSERT INTO movement
                (chain, network, address, height, transaction_id, ordinal, kind,
                 movement_id, asset_chain, asset, amount, from_address, to_address)
            VALUES
                ('bitcoin', 'regtest', 'btc-address', 2, 'btc-transaction', 0, 'output',
                 'btc-movement', 'bitcoin', 'btc', 200, NULL, 'btc-address'),
                ('ethereum', 'mainnet', 'eth-address', 7, 'eth-transaction', 0, 'transfer',
                 'eth-movement', 'ethereum', 'eth', 700, 'eth-from', 'eth-address');

            INSERT INTO output
                (chain, network, transaction_id, output_index, address, asset_chain,
                 asset, amount, evidence, created_at, coinbase)
            VALUES
                ('bitcoin', 'regtest', 'btc-output', 0, 'btc-address', 'bitcoin',
                 'btc', 200, decode('51', 'hex'), 2, false);

            INSERT INTO journal
                (chain, network, height, block_hash, block_parent, block_timestamp,
                 previous_checkpoint_height, previous_checkpoint_hash,
                 previous_checkpoint_parent, previous_checkpoint_time)
            VALUES
                ('bitcoin', 'regtest', 2, decode('02', 'hex'), decode('01', 'hex'), 20,
                 1, decode('01', 'hex'), decode('00', 'hex'), 10),
                ('ethereum', 'mainnet', 7, decode('07', 'hex'), decode('06', 'hex'), 70,
                 6, decode('06', 'hex'), decode('05', 'hex'), 60);

            INSERT INTO journal_output
                (chain, network, height, transaction_id, output_index, address,
                 asset_chain, asset, amount, evidence, created_at, coinbase)
            VALUES
                ('bitcoin', 'regtest', 2, 'btc-spent', 0, 'btc-address',
                 'bitcoin', 'btc', 100, decode('51', 'hex'), 1, false);
            ",
        )
        .await
        .expect("insert dense Bitcoin and Ethereum fixtures");
}

async fn baseline_signatures(
    client: &tokio_postgres::Client,
) -> BTreeMap<&'static str, TableSignature> {
    let mut signatures = BTreeMap::new();
    for table in BASELINE_TABLES {
        let removed_keys = match table {
            "checkpoint" => "ARRAY['position', 'parent_position']::text[]",
            "history" => "ARRAY['block_position', 'block_parent_position']::text[]",
            "journal" => {
                "ARRAY['block_position', 'block_parent_position', \
                'previous_checkpoint_position', \
                'previous_checkpoint_parent_position']::text[]"
            }
            _ => "ARRAY[]::text[]",
        };
        let query = format!(
            "SELECT (to_jsonb(row_value) - {removed_keys})::text AS value \
             FROM {table} AS row_value \
             ORDER BY (to_jsonb(row_value) - {removed_keys})::text"
        );
        let rows = client
            .query(&query, &[])
            .await
            .unwrap_or_else(|error| panic!("read {table} signature: {error}"));
        let mut digest = Sha256::new();
        for row in &rows {
            let value: String = row.get("value");
            digest.update(value.len().to_be_bytes());
            digest.update(value.as_bytes());
        }
        signatures.insert(
            table,
            TableSignature {
                rows: rows.len(),
                sha256: format!("{:x}", digest.finalize()),
            },
        );
    }
    signatures
}

async fn assert_dense_coordinates(client: &tokio_postgres::Client) {
    let valid: bool = client
        .query_one(
            "
            SELECT
                (SELECT BOOL_AND(
                    position = height
                    AND (parent_hash IS NULL) = (parent_position IS NULL)
                    AND (parent_position IS NULL OR parent_position = height - 1)
                ) FROM checkpoint)
                AND
                (SELECT BOOL_AND(
                    block_position = height
                    AND (block_parent IS NULL) = (block_parent_position IS NULL)
                    AND (block_parent_position IS NULL OR block_parent_position = height - 1)
                ) FROM history)
                AND
                (SELECT BOOL_AND(
                    block_position = height
                    AND (block_parent IS NULL) = (block_parent_position IS NULL)
                    AND (block_parent_position IS NULL OR block_parent_position = height - 1)
                    AND previous_checkpoint_position = previous_checkpoint_height
                    AND (previous_checkpoint_parent IS NULL)
                        = (previous_checkpoint_parent_position IS NULL)
                    AND (previous_checkpoint_parent_position IS NULL
                        OR previous_checkpoint_parent_position = previous_checkpoint_height - 1)
                ) FROM journal)
            ",
            &[],
        )
        .await
        .expect("validate dense coordinates")
        .get(0);
    assert!(valid, "dense coordinate backfill produced invalid pairs");
}

async fn coordinate_column_count(client: &tokio_postgres::Client) -> i64 {
    client
        .query_one(
            "
            SELECT COUNT(*)
            FROM information_schema.columns
            WHERE table_schema = current_schema()
              AND (
                (table_name = 'checkpoint' AND column_name IN ('position', 'parent_position'))
                OR (table_name = 'history' AND column_name IN
                    ('block_position', 'block_parent_position'))
                OR (table_name = 'journal' AND column_name IN
                    ('block_position', 'block_parent_position',
                     'previous_checkpoint_position',
                     'previous_checkpoint_parent_position'))
              )
            ",
            &[],
        )
        .await
        .expect("count coordinate columns")
        .get(0)
}

async fn assert_coordinate_schema(client: &tokio_postgres::Client) {
    let columns: String = client
        .query_one(
            "
            SELECT string_agg(
                table_name || '.' || column_name || ':' || is_nullable,
                ',' ORDER BY table_name, ordinal_position
            )
            FROM information_schema.columns
            WHERE table_schema = current_schema()
              AND (
                (table_name = 'checkpoint' AND column_name IN ('position', 'parent_position'))
                OR (table_name = 'history' AND column_name IN
                    ('block_position', 'block_parent_position'))
                OR (table_name = 'journal' AND column_name IN
                    ('block_position', 'block_parent_position',
                     'previous_checkpoint_position',
                     'previous_checkpoint_parent_position'))
              )
            ",
            &[],
        )
        .await
        .expect("read coordinate column nullability")
        .get(0);
    assert_eq!(
        columns,
        "checkpoint.position:NO,checkpoint.parent_position:YES,\
         history.block_position:NO,history.block_parent_position:YES,\
         journal.block_position:NO,journal.block_parent_position:YES,\
         journal.previous_checkpoint_position:YES,\
         journal.previous_checkpoint_parent_position:YES"
    );

    let rows = client
        .query(
            "
            SELECT conname, convalidated
            FROM pg_constraint
            WHERE conrelid IN (
                'checkpoint'::regclass,
                'history'::regclass,
                'journal'::regclass
            )
              AND conname IN (
                'checkpoint_position_nonnegative',
                'checkpoint_parent_complete',
                'history_block_position_nonnegative',
                'history_block_parent_complete',
                'journal_block_position_nonnegative',
                'journal_block_parent_complete',
                'journal_previous_checkpoint_complete'
              )
            ORDER BY conname
            ",
            &[],
        )
        .await
        .expect("read final coordinate constraints");
    let constraints = rows
        .iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, bool>(1)))
        .collect::<Vec<_>>();
    assert_eq!(
        constraints,
        [
            ("checkpoint_parent_complete".to_owned(), true),
            ("checkpoint_position_nonnegative".to_owned(), true),
            ("history_block_parent_complete".to_owned(), true),
            ("history_block_position_nonnegative".to_owned(), true),
            ("journal_block_parent_complete".to_owned(), true),
            ("journal_block_position_nonnegative".to_owned(), true),
            ("journal_previous_checkpoint_complete".to_owned(), true),
        ]
    );
}

async fn assert_final_constraints_reject_invalid_writes(client: &tokio_postgres::Client) {
    assert_insert_rejected(
        client,
        "INSERT INTO checkpoint (chain, network, height, hash, position) \
         VALUES ('invalid', 'missing-position', 0, decode('00', 'hex'), NULL)",
        "null value in column \"position\"",
    )
    .await;
    assert_insert_rejected(
        client,
        "INSERT INTO checkpoint \
            (chain, network, height, hash, parent_hash, position, parent_position) \
         VALUES ('invalid', 'checkpoint-parent', 1, decode('01', 'hex'), \
                 decode('00', 'hex'), 1, NULL)",
        "checkpoint_parent_complete",
    )
    .await;
    assert_insert_rejected(
        client,
        "INSERT INTO history \
            (chain, network, address, height, transaction_id, status, block_hash, \
             block_parent, block_position, block_parent_position) \
         VALUES ('invalid', 'history-parent', 'address', 1, 'transaction', 'included', \
                 decode('01', 'hex'), decode('00', 'hex'), 1, NULL)",
        "history_block_parent_complete",
    )
    .await;
    assert_insert_rejected(
        client,
        "INSERT INTO journal \
            (chain, network, height, block_hash, block_parent, block_position, \
             block_parent_position, previous_checkpoint_height, \
             previous_checkpoint_hash) \
         VALUES ('invalid', 'journal-previous', 1, decode('01', 'hex'), \
                 decode('00', 'hex'), 1, 0, 0, decode('00', 'hex'))",
        "journal_previous_checkpoint_complete",
    )
    .await;
}

async fn assert_insert_rejected(client: &tokio_postgres::Client, statement: &str, expected: &str) {
    let error = client
        .execute(statement, &[])
        .await
        .expect_err("invalid coordinate row must be rejected");
    let message = database_error_message(&error);
    let constraint = error
        .as_db_error()
        .and_then(tokio_postgres::error::DbError::constraint)
        .unwrap_or("");
    assert!(
        message.contains(expected) || constraint == expected,
        "expected {expected}, got message {message:?} and constraint {constraint:?}"
    );
}

fn database_error_message(error: &tokio_postgres::Error) -> &str {
    error
        .as_db_error()
        .map(tokio_postgres::error::DbError::message)
        .unwrap_or("missing PostgreSQL error detail")
}
