//! Canonical history, movement, and live-output writes for one block commit.

use deadpool_postgres::Transaction;
use indexing::{BlockAddition, IndexError, IndexErrorKind, IndexScope};
use tokio_postgres::types::ToSql;

use crate::{
    columns::{self, SpendKeys},
    prepare_in, row,
};

const WRITE_HISTORY: &str = "\
INSERT INTO history (chain, network, address, height, transaction_id, status, failure_reason,
                     block_position, block_hash, block_parent_position, block_parent,
                     block_timestamp, fee_asset, fee_amount, fee_payer)
SELECT $1, $2, entry.address, $3, entry.transaction_id, entry.status, entry.failure_reason,
       $4, $5, $6, $7, $8, entry.fee_asset, entry.fee_amount::numeric, entry.fee_payer
FROM UNNEST($9::text[], $10::text[], $11::text[], $12::text[], $13::text[], $14::text[],
            $15::text[])
     AS entry(address, transaction_id, status, failure_reason, fee_asset, fee_amount, fee_payer)";

const WRITE_MOVEMENT: &str = "\
INSERT INTO movement (chain, network, address, height, transaction_id, ordinal, kind, movement_id,
                      asset_chain, asset, amount, from_address, to_address)
SELECT $1, $2, entry.address, $3, entry.transaction_id, entry.ordinal, entry.kind,
       entry.movement_id, entry.asset_chain, entry.asset, entry.amount::numeric,
       entry.from_address, entry.to_address
FROM UNNEST($4::text[], $5::text[], $6::int4[], $7::text[], $8::text[], $9::text[], $10::text[],
            $11::text[], $12::text[], $13::text[])
     AS entry(address, transaction_id, ordinal, kind, movement_id, asset_chain, asset, amount,
              from_address, to_address)";

const WRITE_CREATED: &str = "\
INSERT INTO output (chain, network, transaction_id, output_index, address, asset_chain, asset,
                    amount, evidence, created_at, coinbase)
SELECT $1, $2, entry.transaction_id, entry.output_index, entry.address, entry.asset_chain,
       entry.asset, entry.amount::numeric, entry.evidence, $3, entry.coinbase
FROM UNNEST($4::text[], $5::int4[], $6::text[], $7::text[], $8::text[], $9::text[], $10::bytea[],
            $11::bool[])
     AS entry(transaction_id, output_index, address, asset_chain, asset, amount, evidence,
              coinbase)";

/// Moves spent outputs out of the live set and into the rollback journal.
const SPEND_OUTPUTS: &str = "\
WITH target AS (
    SELECT * FROM UNNEST($3::text[], $4::text[], $5::int4[])
        AS t(address, transaction_id, output_index)
), removed AS (
    DELETE FROM output USING target
    WHERE output.chain = $1 AND output.network = $2
      AND output.address = target.address
      AND output.transaction_id = target.transaction_id
      AND output.output_index = target.output_index
    RETURNING output.*
)
INSERT INTO journal_output (chain, network, height, transaction_id, output_index, address,
                            asset_chain, asset, amount, evidence, created_at, coinbase)
SELECT chain, network, $6, transaction_id, output_index, address, asset_chain, asset, amount,
       evidence, created_at, coinbase
FROM removed";

/// Writes the block's canonical transactions and their movements.
pub(crate) async fn write_history(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
    addition: &BlockAddition,
) -> Result<(), IndexError> {
    let (history, movements) = columns::canonical(addition)?;

    if history.is_empty() {
        return Ok(());
    }

    let block = addition.block();
    let block_position = row::as_i64(block.position.0, "block position")?;
    let block_parent_position = block
        .parent
        .as_ref()
        .map(|parent| row::as_i64(parent.position.0, "parent block position"))
        .transpose()?;
    let block_parent = block.parent.as_ref().map(|parent| parent.hash.0.clone());
    let block_timestamp = block
        .timestamp
        .map(|value| row::as_i64(value, "block timestamp"))
        .transpose()?;
    let statement = prepare_in(transaction, WRITE_HISTORY).await?;
    transaction
        .execute(
            &statement,
            &[
                &scope.chain.0,
                &scope.network,
                &height,
                &block_position,
                &block.hash.0,
                &block_parent_position,
                &block_parent,
                &block_timestamp,
                &history.address,
                &history.transaction_id,
                &history.status,
                &history.failure_reason,
                &history.fee_asset,
                &history.fee_amount,
                &history.fee_payer,
            ],
        )
        .await
        .map_err(conflict_aware)?;

    if movements.is_empty() {
        return Ok(());
    }
    let statement = prepare_in(transaction, WRITE_MOVEMENT).await?;
    transaction
        .execute(
            &statement,
            &[
                &scope.chain.0,
                &scope.network,
                &height,
                &movements.address,
                &movements.transaction_id,
                &movements.ordinal,
                &movements.kind,
                &movements.movement_id,
                &movements.asset_chain,
                &movements.asset,
                &movements.amount,
                &movements.from_address,
                &movements.to_address,
            ],
        )
        .await
        .map_err(conflict_aware)?;
    Ok(())
}

pub(crate) async fn write_created(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
    addition: &BlockAddition,
) -> Result<(), IndexError> {
    let rows = columns::created(&addition.outputs().created)?;
    if rows.is_empty() {
        return Ok(());
    }

    let statement = prepare_in(transaction, WRITE_CREATED).await?;
    transaction
        .execute(
            &statement,
            &[
                &scope.chain.0,
                &scope.network,
                &height,
                &rows.transaction_id,
                &rows.output_index,
                &rows.address,
                &rows.asset_chain,
                &rows.asset,
                &rows.amount,
                &rows.evidence,
                &rows.coinbase,
            ],
        )
        .await
        .map_err(conflict_aware)?;
    Ok(())
}

/// Applies required and tracked spends under their distinct miss policies.
pub(crate) async fn write_spent(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
    addition: &BlockAddition,
) -> Result<(), IndexError> {
    let required = columns::spends(&addition.outputs().spent)?;
    let tracked = columns::spends(&addition.outputs().tracked_spends)?;

    if !required.is_empty() {
        let moved = spend(transaction, scope, height, &required).await?;
        if moved != required.len() as u64 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block spends an unknown indexed output",
                false,
            ));
        }
    }
    if !tracked.is_empty() {
        spend(transaction, scope, height, &tracked).await?;
    }
    Ok(())
}

async fn spend(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
    keys: &SpendKeys,
) -> Result<u64, IndexError> {
    let statement = prepare_in(transaction, SPEND_OUTPUTS).await?;
    let parameters: [&(dyn ToSql + Sync); 6] = [
        &scope.chain.0,
        &scope.network,
        &keys.address,
        &keys.transaction_id,
        &keys.output_index,
        &height,
    ];
    transaction
        .execute(&statement, &parameters)
        .await
        .map_err(crate::store)
}

fn conflict_aware(error: tokio_postgres::Error) -> IndexError {
    let unique = error
        .code()
        .is_some_and(|code| code == &tokio_postgres::error::SqlState::UNIQUE_VIOLATION);
    if unique {
        return IndexError::new(
            IndexErrorKind::Conflict,
            "canonical record already exists",
            true,
        );
    }
    crate::store(error)
}
