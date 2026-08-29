use std::collections::BTreeSet;

use deadpool_postgres::Pool;
use indexing::{IndexError, IndexErrorKind};
use tokio_postgres::IsolationLevel;

use crate::{store, unavailable};

const COLUMNS: &str = r#"
SELECT STRING_AGG(
    table_name || '.' || column_name || ':' || udt_name || ':' || is_nullable,
    E'\n' ORDER BY table_name, ordinal_position
)
FROM information_schema.columns
WHERE table_schema = current_schema()
  AND table_name IN (
      'checkpoint', 'history', 'journal', 'journal_output',
      'movement', 'output', 'payment_wallets'
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
          'checkpoint', 'history', 'journal', 'journal_output',
          'movement', 'output', 'payment_wallets'
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
      'checkpoint', 'history', 'journal', 'journal_output',
      'movement', 'output', 'payment_wallets'
  )"#;

const JOURNAL_CASCADE: &str = r#"
SELECT confdeltype = 'c'
FROM pg_constraint
WHERE conrelid = 'journal_output'::regclass AND contype = 'f'"#;

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

const EXPECTED_CONSTRAINTS: &str = "\
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
    let schema = schema.ok_or_else(|| incompatible("PostgreSQL search path has no schema"))?;
    if schema != expected_schema {
        return Err(incompatible(format!(
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
        return Err(incompatible(
            "PostgreSQL schema validation is not read-only",
        ));
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
        return Err(incompatible(format!(
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
        return Err(incompatible(format!(
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
    Err(incompatible(format!(
        "PostgreSQL schema has incompatible {component}"
    )))
}

fn incompatible(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}
