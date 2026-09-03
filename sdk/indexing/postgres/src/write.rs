//! Atomic block commit and reorg reversal.
//!
//! Both run inside one database transaction. A transaction-scoped advisory lock
//! serialises every writer for the exact scope, including the first commit when
//! no checkpoint row exists. The checkpoint row is then locked `FOR UPDATE` as
//! a second guard and gives the compare-and-swap the redb implementation gets
//! from conditional batch writes.
//!
//! Every set of rows is written by a single statement over parameter arrays,
//! so a block costs a fixed number of round trips no matter how many
//! transactions, movements, or outputs it carries.

use crate::{Repository, prepare_in, projection, row};
use deadpool_postgres::Transaction;
use indexing::{BlockAddition, BlockOutcome, BlockRef, IndexError, IndexErrorKind, IndexScope};

/// A stable framed scope tuple is hashed by the database into the signed
/// 64-bit key accepted by PostgreSQL advisory locks. Framing preserves the
/// chain/network boundary before hashing, so scopes such as (`ab`, `c`) and
/// (`a`, `bc`) cannot become the same input.
const LOCK_SCOPE: &str = "\
SELECT pg_advisory_xact_lock(hashtextextended(
    octet_length($1::text)::text || ':' || $1::text ||
    octet_length($2::text)::text || ':' || $2::text,
    5787213827046134867
))";

/// Locks the scope's checkpoint for the rest of the transaction.
const LOCK_CHECKPOINT: &str = "SELECT position, height, hash, parent_position, \
                               parent_hash AS parent, block_timestamp AS timestamp \
                               FROM checkpoint WHERE chain = $1 AND network = $2 FOR UPDATE";

const JOURNALLED_HASH: &str =
    "SELECT block_hash FROM journal WHERE chain = $1 AND network = $2 AND height = $3";

/// Records the block and drops what has aged out of the retention window in one
/// statement. The two touch disjoint heights, so folding the prune into the
/// insert saves a round trip without changing what either does.
const WRITE_JOURNAL: &str = "\
WITH pruned AS (
    DELETE FROM journal WHERE chain = $1 AND network = $2 AND height <= $15
)
INSERT INTO journal (chain, network, height, block_position, block_hash,
                     block_parent_position, block_parent, block_timestamp,
                     previous_checkpoint_height, previous_checkpoint_position,
                     previous_checkpoint_hash, previous_checkpoint_parent_position,
                     previous_checkpoint_parent, previous_checkpoint_time)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)";

const MOVE_CHECKPOINT: &str = "\
INSERT INTO checkpoint (chain, network, position, height, hash, parent_position, parent_hash,
                        block_timestamp)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
ON CONFLICT (chain, network) DO UPDATE SET position = EXCLUDED.position,
    height = EXCLUDED.height, hash = EXCLUDED.hash,
    parent_position = EXCLUDED.parent_position, parent_hash = EXCLUDED.parent_hash,
    block_timestamp = EXCLUDED.block_timestamp";

impl Repository {
    pub(crate) async fn write_block(
        &self,
        addition: BlockAddition,
    ) -> Result<BlockOutcome, IndexError> {
        self.check_scope(addition.scope())?;
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(crate::store)?;

        lock_scope(&transaction, &self.scope).await?;
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
        projection::write_history(&transaction, &self.scope, height, &addition).await?;
        projection::write_created(&transaction, &self.scope, height, &addition).await?;
        projection::write_spent(&transaction, &self.scope, height, &addition).await?;
        move_checkpoint(&transaction, &self.scope, addition.block()).await?;

        transaction.commit().await.map_err(crate::store)?;
        Ok(BlockOutcome::Applied)
    }
}

pub(crate) async fn lock_scope(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
) -> Result<(), IndexError> {
    let statement = prepare_in(transaction, LOCK_SCOPE).await?;
    transaction
        .query_one(&statement, &[&scope.chain.0, &scope.network])
        .await
        .map_err(crate::store)?;
    Ok(())
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
    let position = row::as_i64(block.position.0, "block position")?;
    let height = row::as_i64(block.height.0, "block height")?;
    let timestamp = block
        .timestamp
        .map(|value| row::as_i64(value, "block timestamp"))
        .transpose()?;
    let parent_position = block
        .parent
        .as_ref()
        .map(|parent| row::as_i64(parent.position.0, "parent block position"))
        .transpose()?;
    let parent = block.parent.as_ref().map(|parent| parent.hash.0.clone());
    let statement = prepare_in(transaction, MOVE_CHECKPOINT).await?;
    transaction
        .execute(
            &statement,
            &[
                &scope.chain.0,
                &scope.network,
                &position,
                &height,
                &block.hash.0,
                &parent_position,
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
    let previous_position = previous
        .map(|block| row::as_i64(block.position.0, "block position"))
        .transpose()?;
    let previous_parent_position = previous
        .and_then(|block| block.parent.as_ref())
        .map(|parent| row::as_i64(parent.position.0, "parent block position"))
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
    let block_position = row::as_i64(addition.block().position.0, "block position")?;
    let block_parent_position = addition
        .block()
        .parent
        .as_ref()
        .map(|parent| row::as_i64(parent.position.0, "parent block position"))
        .transpose()?;
    let statement = prepare_in(transaction, WRITE_JOURNAL).await?;
    transaction
        .execute(
            &statement,
            &[
                &scope.chain.0,
                &scope.network,
                &height,
                &block_position,
                &addition.block().hash.0,
                &block_parent_position,
                &addition
                    .block()
                    .parent
                    .as_ref()
                    .map(|parent| parent.hash.0.clone()),
                &block_timestamp,
                &previous_height,
                &previous_position,
                &previous.map(|block| block.hash.0.clone()),
                &previous_parent_position,
                &previous
                    .and_then(|block| block.parent.as_ref().map(|parent| parent.hash.0.clone())),
                &previous_time,
                &oldest,
            ],
        )
        .await
        .map_err(crate::store)?;
    Ok(())
}
