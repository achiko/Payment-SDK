use std::collections::BTreeSet;

use deadpool_postgres::Pool;
use indexing::IndexError;
use tokio_postgres::IsolationLevel;

use crate::{row, store, unavailable};

const COLUMNS: &str = r#"
SELECT STRING_AGG(
    table_name || '.' || column_name || ':' || udt_name || ':' || is_nullable,
    E'\n' ORDER BY table_name, ordinal_position
)
FROM information_schema.columns
WHERE table_schema = current_schema()
  AND table_name IN (
      'payments_checkpoint', 'payments_history', 'payments_journal', 'payments_journal_output',
      'payments_movement', 'payments_output', 'payments_wallet'
  )"#;

const CONSTRAINTS: &str = r#"
SELECT STRING_AGG(
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
      AND relation.relname IN (
          'payments_checkpoint', 'payments_history', 'payments_journal', 'payments_journal_output',
          'payments_movement', 'payments_output', 'payments_wallet'
      )
      AND constraint_record.contype IN ('p', 'u', 'f', 'c')
    GROUP BY relation.relname, constraint_record.contype
) baseline_constraints"#;

const INDEXES: &str = r#"
SELECT table_relation.relname,
       index_relation.relname,
       definition.indisprimary,
       definition.indisunique,
       (
           SELECT STRING_AGG(indexed_attribute.attname, ',' ORDER BY index_key.ordinality)
           FROM UNNEST(definition.indkey)
                WITH ORDINALITY AS index_key(attnum, ordinality)
           JOIN pg_attribute indexed_attribute
             ON indexed_attribute.attrelid = table_relation.oid
            AND indexed_attribute.attnum = index_key.attnum
       )
FROM pg_index definition
JOIN pg_class table_relation ON table_relation.oid = definition.indrelid
JOIN pg_class index_relation ON index_relation.oid = definition.indexrelid
JOIN pg_namespace namespace ON namespace.oid = table_relation.relnamespace
WHERE namespace.nspname = current_schema()
  AND table_relation.relname IN (
      'payments_checkpoint', 'payments_history', 'payments_journal', 'payments_journal_output',
      'payments_movement', 'payments_output', 'payments_wallet'
  )"#;

const JOURNAL_CASCADE: &str = r#"
SELECT confdeltype = 'c'
FROM pg_constraint
WHERE conrelid = 'payments_journal_output'::regclass AND contype = 'f'"#;

const EXPECTED_COLUMNS: &str = "\
payments_checkpoint.chain:text:NO
payments_checkpoint.network:text:NO
payments_checkpoint.height:int8:NO
payments_checkpoint.hash:bytea:NO
payments_checkpoint.parent_hash:bytea:YES
payments_checkpoint.block_timestamp:int8:YES
payments_checkpoint.position:int8:NO
payments_checkpoint.parent_position:int8:YES
payments_history.chain:text:NO
payments_history.network:text:NO
payments_history.address:text:NO
payments_history.height:int8:NO
payments_history.transaction_id:text:NO
payments_history.status:text:NO
payments_history.failure_reason:text:YES
payments_history.block_hash:bytea:NO
payments_history.block_parent:bytea:YES
payments_history.block_timestamp:int8:YES
payments_history.fee_asset:text:YES
payments_history.fee_amount:numeric:YES
payments_history.fee_payer:text:YES
payments_history.block_position:int8:NO
payments_history.block_parent_position:int8:YES
payments_journal.chain:text:NO
payments_journal.network:text:NO
payments_journal.height:int8:NO
payments_journal.block_hash:bytea:NO
payments_journal.block_parent:bytea:YES
payments_journal.block_timestamp:int8:YES
payments_journal.previous_checkpoint_height:int8:YES
payments_journal.previous_checkpoint_hash:bytea:YES
payments_journal.previous_checkpoint_parent:bytea:YES
payments_journal.previous_checkpoint_time:int8:YES
payments_journal.block_position:int8:NO
payments_journal.block_parent_position:int8:YES
payments_journal.previous_checkpoint_position:int8:YES
payments_journal.previous_checkpoint_parent_position:int8:YES
payments_journal_output.chain:text:NO
payments_journal_output.network:text:NO
payments_journal_output.height:int8:NO
payments_journal_output.transaction_id:text:NO
payments_journal_output.output_index:int4:NO
payments_journal_output.address:text:NO
payments_journal_output.asset_chain:text:NO
payments_journal_output.asset:text:NO
payments_journal_output.amount:numeric:NO
payments_journal_output.evidence:bytea:NO
payments_journal_output.created_at:int8:NO
payments_journal_output.coinbase:bool:NO
payments_movement.chain:text:NO
payments_movement.network:text:NO
payments_movement.address:text:NO
payments_movement.height:int8:NO
payments_movement.transaction_id:text:NO
payments_movement.ordinal:int4:NO
payments_movement.kind:text:NO
payments_movement.movement_id:text:NO
payments_movement.asset_chain:text:NO
payments_movement.asset:text:NO
payments_movement.amount:numeric:NO
payments_movement.from_address:text:YES
payments_movement.to_address:text:YES
payments_output.chain:text:NO
payments_output.network:text:NO
payments_output.transaction_id:text:NO
payments_output.output_index:int4:NO
payments_output.address:text:NO
payments_output.asset_chain:text:NO
payments_output.asset:text:NO
payments_output.amount:numeric:NO
payments_output.evidence:bytea:NO
payments_output.created_at:int8:NO
payments_output.coinbase:bool:NO
payments_wallet.id:text:NO
payments_wallet.chain:text:NO
payments_wallet.network:text:NO
payments_wallet.address:text:NO
payments_wallet.start_height:int8:NO
payments_wallet.secret:bytea:NO
payments_wallet.created_at:timestamptz:NO";

const EXPECTED_CONSTRAINTS: &str = "\
payments_checkpoint:c:2
payments_checkpoint:p:1
payments_history:c:3
payments_history:p:1
payments_journal:c:3
payments_journal:p:1
payments_journal_output:f:1
payments_journal_output:p:1
payments_movement:c:1
payments_movement:p:1
payments_output:p:1
payments_wallet:p:1
payments_wallet:u:1";

const EXPECTED_INDEXES: &str = "\
payments_checkpoint.payments_checkpoint_pkey:true:true:chain,network
payments_history.payments_history_by_height:false:false:chain,network,height
payments_history.payments_history_pkey:true:true:chain,network,address,height,transaction_id
payments_journal.payments_journal_pkey:true:true:chain,network,height
payments_journal_output.payments_journal_output_pkey:true:true:chain,network,height,transaction_id,output_index
payments_movement.payments_movement_by_height:false:false:chain,network,height
payments_movement.payments_movement_pkey:true:true:chain,network,address,height,transaction_id,ordinal
payments_output.payments_output_by_address_identity:false:false:chain,network,address,transaction_id,output_index
payments_output.payments_output_by_height:false:false:chain,network,created_at
payments_output.payments_output_pkey:true:true:chain,network,transaction_id,output_index
payments_wallet.payments_wallet_by_scope:false:false:chain,network
payments_wallet.payments_wallet_chain_network_address_key:false:true:chain,network,address
payments_wallet.payments_wallet_pkey:true:true:id";

// design-lint: allow unclassified-free-function -- public startup algorithm validates the deployment-owned shared schema through one read-only repeatable-read transaction on an injected foreign pool independently of scope-bound repositories
/// Checks that a pool resolves to the configured compatible schema.
///
/// Validation uses one read-only repeatable-read transaction and never creates,
/// alters, repairs, or migrates database objects.
pub async fn validate_schema(pool: &Pool, expected_schema: &str) -> Result<(), IndexError> {
    let mut client = pool.get().await.map_err(unavailable)?;
    let transaction = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .map_err(store)?;

    let schema: Option<String> = transaction
        .query_one("SELECT current_schema()", &[])
        .await
        .map_err(store)?
        .try_get(0)
        .map_err(store)?;
    let schema = schema.ok_or_else(|| row::store("PostgreSQL search path has no schema"))?;
    if schema != expected_schema {
        return Err(row::store(format!(
            "PostgreSQL pool resolved schema {schema}, expected {expected_schema}"
        )));
    }

    let read_only: String = transaction
        .query_one("SHOW transaction_read_only", &[])
        .await
        .map_err(store)?
        .try_get(0)
        .map_err(store)?;
    if read_only != "on" {
        return Err(row::store("PostgreSQL schema validation is not read-only"));
    }

    require_signature(&transaction, COLUMNS, EXPECTED_COLUMNS, "columns").await?;
    require_signature(
        &transaction,
        CONSTRAINTS,
        EXPECTED_CONSTRAINTS,
        "constraints",
    )
    .await?;

    let mut actual_indexes = BTreeSet::new();
    for row in transaction.query(INDEXES, &[]).await.map_err(store)? {
        let table: String = row.try_get(0).map_err(store)?;
        let index: String = row.try_get(1).map_err(store)?;
        let primary: bool = row.try_get(2).map_err(store)?;
        let unique: bool = row.try_get(3).map_err(store)?;
        let columns: String = row.try_get(4).map_err(store)?;
        actual_indexes.insert(format!("{table}.{index}:{primary}:{unique}:{columns}"));
    }
    if !EXPECTED_INDEXES
        .lines()
        .all(|required| actual_indexes.contains(required))
    {
        return Err(row::store(format!(
            "PostgreSQL schema {schema} has incompatible indexes"
        )));
    }

    let cascade: Option<bool> = transaction
        .query_opt(JOURNAL_CASCADE, &[])
        .await
        .map_err(store)?
        .map(|row| row.try_get(0))
        .transpose()
        .map_err(store)?;
    if cascade != Some(true) {
        return Err(row::store(format!(
            "PostgreSQL schema {schema} has incompatible journal cascade"
        )));
    }

    transaction.commit().await.map_err(store)
}

async fn require_signature(
    transaction: &tokio_postgres::Transaction<'_>,
    query: &str,
    expected: &str,
    component: &str,
) -> Result<(), IndexError> {
    let actual: Option<String> = transaction
        .query_one(query, &[])
        .await
        .map_err(store)?
        .try_get(0)
        .map_err(store)?;
    if actual.as_deref() == Some(expected) {
        return Ok(());
    }
    Err(row::store(format!(
        "PostgreSQL schema has incompatible {component}"
    )))
}
