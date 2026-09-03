#[allow(dead_code)]
mod support;

use sha2::{Digest, Sha256};
use support::TestDatabase;

const INITIALIZER: &str = include_str!("../migrations/0001_init.sql");
const INITIALIZER_SHA256: &str = "4d45ff45eab2c718ab3eb554a818a11391fde4ca8806ff26be782d9f40676b7c";

#[test]
fn initializer_defines_the_final_schema_without_upgrade_steps() {
    assert_eq!(
        format!("{:x}", Sha256::digest(INITIALIZER.as_bytes())),
        INITIALIZER_SHA256,
        "canonical schema initializer checksum changed"
    );

    let executable = INITIALIZER
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(!executable.contains("ALTER TABLE"));
    assert!(!executable.contains("DROP INDEX"));
    assert!(!executable.contains("UPDATE "));
    assert!(!executable.contains("payment_sdk.verified_dense_scopes"));

    for table in [
        "checkpoint",
        "history",
        "movement",
        "output",
        "journal",
        "journal_output",
        "payment_wallets",
    ] {
        assert!(
            executable.contains(&format!("CREATE TABLE {table} (")),
            "initializer must create {table}"
        );
    }

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
            executable.contains(&format!("CONSTRAINT {constraint}")),
            "initializer must define {constraint}"
        );
    }

    for index in [
        "history_by_height",
        "movement_by_height",
        "output_by_address_identity",
        "output_by_height",
        "payment_wallets_by_scope",
    ] {
        assert!(
            executable.contains(&format!("CREATE INDEX {index}")),
            "initializer must define {index}"
        );
    }

    assert!(!executable.contains("REFERENCES history"));
    assert!(executable.contains("REFERENCES journal"));
    assert!(executable.contains("ON DELETE CASCADE"));
}

#[tokio::test]
async fn initializer_creates_the_final_coordinate_schema() {
    let database = TestDatabase::start().await;
    let pool = database.pool();
    let client = pool.get().await.expect("fresh initializer connection");

    assert_coordinate_schema(&client).await;
    assert_final_constraints_reject_invalid_writes(&client).await;
    assert!(database.registry_sentinel_unchanged().await);
}

#[tokio::test]
async fn initializer_refuses_to_replay_over_an_existing_schema() {
    let database = TestDatabase::start().await;
    let pool = database.pool();
    let client = pool.get().await.expect("initializer replay connection");

    client
        .batch_execute(INITIALIZER)
        .await
        .expect_err("the fresh-schema initializer must not replay");
    client
        .batch_execute("ROLLBACK")
        .await
        .expect("close the aborted initializer transaction");

    assert_coordinate_schema(&client).await;
    assert!(database.registry_sentinel_unchanged().await);
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
