//! Atomic block commit and reorg reversal.
//!
//! Both run inside one database transaction. The checkpoint row is locked
//! `FOR UPDATE` first, which serialises concurrent writers on the same scope and
//! gives the compare-and-swap the redb implementation gets from conditional
//! batch writes.
//!
//! Every set of rows is written by a single statement over parameter arrays,
//! so a block costs a fixed number of round trips no matter how many
//! transactions, movements, or outputs it carries.

use deadpool_postgres::Transaction;
use indexing::{BlockAddition, BlockOutcome, BlockRef, IndexError, IndexErrorKind, IndexScope};
use tokio_postgres::types::ToSql;

use crate::{
    Repository,
    columns::{self, SpendKeys},
    prepare_in, row,
};

/// Locks the scope's checkpoint for the rest of the transaction.
const LOCK_CHECKPOINT: &str = "SELECT height, hash, parent_hash AS parent, \
                               block_timestamp AS timestamp \
                               FROM checkpoint WHERE chain = $1 AND network = $2 FOR UPDATE";

const JOURNALLED_HASH: &str =
    "SELECT block_hash FROM journal WHERE chain = $1 AND network = $2 AND height = $3";

/// Records the block and drops what has aged out of the retention window in one
/// statement. The two touch disjoint heights, so folding the prune into the
/// insert saves a round trip without changing what either does.
const WRITE_JOURNAL: &str = "\
WITH pruned AS (
    DELETE FROM journal WHERE chain = $1 AND network = $2 AND height <= $11
)
INSERT INTO journal (chain, network, height, block_hash, block_parent, block_timestamp,
                     previous_checkpoint_height, previous_checkpoint_hash,
                     previous_checkpoint_parent, previous_checkpoint_time)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)";

/// One row per address a transaction touched. Chain, network, height, and the
/// block are identical for every row in a block, so they bind once as scalars
/// and only the per-row columns travel as arrays.
const WRITE_HISTORY: &str = "\
INSERT INTO history (chain, network, address, height, transaction_id, status, failure_reason,
                     block_hash, block_parent, block_timestamp, fee_asset, fee_amount, fee_payer)
SELECT $1, $2, entry.address, $3, entry.transaction_id, entry.status, entry.failure_reason,
       $4, $5, $6, entry.fee_asset, entry.fee_amount::numeric, entry.fee_payer
FROM UNNEST($7::text[], $8::text[], $9::text[], $10::text[], $11::text[], $12::text[],
            $13::text[])
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

/// Every output created by a block shares the block's height, which
/// `OutputChanges::validate` has already enforced.
const WRITE_CREATED: &str = "\
INSERT INTO output (chain, network, transaction_id, output_index, address, asset_chain, asset,
                    amount, evidence, created_at, coinbase)
SELECT $1, $2, entry.transaction_id, entry.output_index, entry.address, entry.asset_chain,
       entry.asset, entry.amount::numeric, entry.evidence, $3, entry.coinbase
FROM UNNEST($4::text[], $5::int4[], $6::text[], $7::text[], $8::text[], $9::text[], $10::bytea[],
            $11::bool[])
     AS entry(transaction_id, output_index, address, asset_chain, asset, amount, evidence,
              coinbase)";

/// Moves spent outputs out of the live set and into the journal in one pass.
///
/// The copy has to see the row before it is gone — the amount and script exist
/// nowhere else — so the delete streams its own removed rows into the journal
/// through a CTE rather than reading them back first. The row count is how many
/// outputs actually existed, which is what tells a required spend from a
/// tracked one.
const SPEND_OUTPUTS: &str = "\
WITH target AS (
    SELECT * FROM UNNEST($3::text[], $4::int4[]) AS t(transaction_id, output_index)
), removed AS (
    DELETE FROM output USING target
    WHERE output.chain = $1 AND output.network = $2
      AND output.transaction_id = target.transaction_id
      AND output.output_index = target.output_index
    RETURNING output.*
)
INSERT INTO journal_output (chain, network, height, transaction_id, output_index, address,
                            asset_chain, asset, amount, evidence, created_at, coinbase)
SELECT chain, network, $5, transaction_id, output_index, address, asset_chain, asset, amount,
       evidence, created_at, coinbase
FROM removed";

const MOVE_CHECKPOINT: &str = "\
INSERT INTO checkpoint (chain, network, height, hash, parent_hash, block_timestamp)
VALUES ($1, $2, $3, $4, $5, $6)
ON CONFLICT (chain, network) DO UPDATE SET height = EXCLUDED.height, hash = EXCLUDED.hash,
    parent_hash = EXCLUDED.parent_hash, block_timestamp = EXCLUDED.block_timestamp";

impl Repository {
    pub(crate) async fn write_block(
        &self,
        addition: BlockAddition,
    ) -> Result<BlockOutcome, IndexError> {
        self.check_scope(addition.scope())?;
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(crate::store)?;

        let current = locked_checkpoint(&transaction, &self.scope).await?;
        let height = row::as_i64(addition.block().height.0, "block height")?;
        let journalled = journalled_block(&transaction, &self.scope, height).await?;

        // Re-presenting the block that is already the checkpoint is not an
        // error: a restart replays the tip, and the caller must be able to tell
        // that apart from a real commit so an observer does not re-fire.
        if current.as_ref() == Some(addition.block()) {
            return if journalled.as_deref() == Some(addition.block().hash.0.as_slice()) {
                Ok(BlockOutcome::AlreadyApplied)
            } else {
                Err(row::store(
                    "canonical checkpoint is missing its rollback journal",
                ))
            };
        }
        if journalled.is_some() {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "another retained block exists at this height",
                true,
            ));
        }
        if current.as_ref() != addition.expected_checkpoint() {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "checkpoint changed before block commit",
                true,
            ));
        }

        write_journal(&transaction, &self.scope, height, &addition).await?;
        write_history(&transaction, &self.scope, height, &addition).await?;
        write_created(&transaction, &self.scope, height, &addition).await?;
        write_spent(&transaction, &self.scope, height, &addition).await?;
        move_checkpoint(&transaction, &self.scope, addition.block()).await?;

        transaction.commit().await.map_err(crate::store)?;
        Ok(BlockOutcome::Applied)
    }
}

pub(crate) async fn locked_checkpoint(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
) -> Result<Option<BlockRef>, IndexError> {
    let statement = prepare_in(transaction, LOCK_CHECKPOINT).await?;
    let row = transaction
        .query_opt(&statement, &[&scope.chain.0, &scope.network])
        .await
        .map_err(crate::store)?;
    row.as_ref().map(|row| row::block(row, "")).transpose()
}

async fn journalled_block(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
) -> Result<Option<Vec<u8>>, IndexError> {
    let statement = prepare_in(transaction, JOURNALLED_HASH).await?;
    let row = transaction
        .query_opt(&statement, &[&scope.chain.0, &scope.network, &height])
        .await
        .map_err(crate::store)?;
    row.map(|row| row.try_get::<_, Vec<u8>>("block_hash"))
        .transpose()
        .map_err(crate::store)
}

pub(crate) fn optional_block(
    row: &tokio_postgres::Row,
    prefix: &str,
) -> Result<Option<BlockRef>, IndexError> {
    let height: Option<i64> = row
        .try_get(&*format!("{prefix}height"))
        .map_err(crate::store)?;
    match height {
        None => Ok(None),
        Some(_) => row::block(row, prefix).map(Some),
    }
}

pub(crate) async fn move_checkpoint(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    block: &BlockRef,
) -> Result<(), IndexError> {
    let height = row::as_i64(block.height.0, "block height")?;
    let timestamp = block
        .timestamp
        .map(|value| row::as_i64(value, "block timestamp"))
        .transpose()?;
    let parent = block.parent_hash.as_ref().map(|hash| hash.0.clone());
    let statement = prepare_in(transaction, MOVE_CHECKPOINT).await?;
    transaction
        .execute(
            &statement,
            &[
                &scope.chain.0,
                &scope.network,
                &height,
                &block.hash.0,
                &parent,
                &timestamp,
            ],
        )
        .await
        .map_err(crate::store)?;
    Ok(())
}

async fn write_journal(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
    addition: &BlockAddition,
) -> Result<(), IndexError> {
    let previous = addition.expected_checkpoint();
    let previous_height = previous
        .map(|block| row::as_i64(block.height.0, "block height"))
        .transpose()?;
    let previous_time = previous
        .and_then(|block| block.timestamp)
        .map(|value| row::as_i64(value, "block timestamp"))
        .transpose()?;
    // Below the retention window nothing has aged out yet. A height no row can
    // hold keeps the folded prune inert rather than needing a second statement.
    let oldest = addition
        .block()
        .height
        .0
        .checked_sub(addition.retention())
        .map(|value| row::as_i64(value, "block height"))
        .transpose()?
        .unwrap_or(-1);
    let block_timestamp = addition
        .block()
        .timestamp
        .map(|value| row::as_i64(value, "block timestamp"))
        .transpose()?;
    let statement = prepare_in(transaction, WRITE_JOURNAL).await?;
    transaction
        .execute(
            &statement,
            &[
                &scope.chain.0,
                &scope.network,
                &height,
                &addition.block().hash.0,
                &addition
                    .block()
                    .parent_hash
                    .as_ref()
                    .map(|hash| hash.0.clone()),
                &block_timestamp,
                &previous_height,
                &previous.map(|block| block.hash.0.clone()),
                &previous.and_then(|block| block.parent_hash.as_ref().map(|hash| hash.0.clone())),
                &previous_time,
                &oldest,
            ],
        )
        .await
        .map_err(crate::store)?;
    Ok(())
}

/// Writes the block's canonical transactions and their movements.
///
/// History is address-primary, so a transaction paying two watched addresses is
/// listed under both. Every transaction in a block carries the same block
/// reference — `BlockAddition` builds them from it — so the block columns bind
/// once for the whole statement.
async fn write_history(
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
    let block_parent = block.parent_hash.as_ref().map(|hash| hash.0.clone());
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
                &block.hash.0,
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

async fn write_created(
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

/// Applies the block's spends.
///
/// Required and tracked spends run as two statements because they differ only
/// in what a miss means: a required spend that matched no live output is an
/// invalid block, while a tracked spend outside the address filter is expected
/// to be absent and is simply skipped.
async fn write_spent(
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
    let parameters: [&(dyn ToSql + Sync); 5] = [
        &scope.chain.0,
        &scope.network,
        &keys.transaction_id,
        &keys.output_index,
        &height,
    ];
    transaction
        .execute(&statement, &parameters)
        .await
        .map_err(crate::store)
}

/// A unique violation means the same fact is already stored, which is a
/// retryable conflict rather than a corrupt store.
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
