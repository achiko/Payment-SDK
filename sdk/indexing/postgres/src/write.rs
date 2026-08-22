//! Atomic block commit and reorg reversal.
//!
//! Both run inside one database transaction. The checkpoint row is locked
//! `FOR UPDATE` first, which serialises concurrent writers on the same scope and
//! gives the compare-and-swap the redb implementation gets from conditional
//! batch writes.

use indexing::{
    BlockAddition, BlockOutcome, BlockRef, IndexError, IndexErrorKind, IndexScope, OutputKey,
};
use tokio_postgres::Transaction;

use crate::{Repository, row};

impl Repository {
    pub(crate) async fn write_block(
        &self,
        addition: BlockAddition,
    ) -> Result<BlockOutcome, IndexError> {
        self.check_scope(addition.scope())?;
        let mut client = self.pool.get().await.map_err(crate::unavailable)?;
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
        write_created(&transaction, &self.scope, &addition).await?;
        write_spent(&transaction, &self.scope, height, &addition).await?;
        move_checkpoint(&transaction, &self.scope, addition.block()).await?;
        prune_journal(&transaction, &self.scope, &addition).await?;

        transaction.commit().await.map_err(crate::store)?;
        Ok(BlockOutcome::Applied)
    }
}

/// Locks the scope's checkpoint for the rest of the transaction.
pub(crate) async fn locked_checkpoint(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
) -> Result<Option<BlockRef>, IndexError> {
    let row = transaction
        .query_opt(
            "SELECT height, hash, parent_hash AS parent, block_timestamp AS timestamp \
             FROM checkpoint WHERE chain = $1 AND network = $2 FOR UPDATE",
            &[&scope.chain.0, &scope.network],
        )
        .await
        .map_err(crate::store)?;
    row.as_ref().map(|row| row::block(row, "")).transpose()
}

async fn journalled_block(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
) -> Result<Option<Vec<u8>>, IndexError> {
    let row = transaction
        .query_opt(
            "SELECT block_hash FROM journal WHERE chain = $1 AND network = $2 AND height = $3",
            &[&scope.chain.0, &scope.network, &height],
        )
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
    transaction
        .execute(
            "INSERT INTO checkpoint (chain, network, height, hash, parent_hash, block_timestamp) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (chain, network) DO UPDATE SET height = EXCLUDED.height, \
             hash = EXCLUDED.hash, parent_hash = EXCLUDED.parent_hash, \
             block_timestamp = EXCLUDED.block_timestamp",
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
    transaction
        .execute(
            "INSERT INTO journal (chain, network, height, block_hash, block_parent, \
             block_timestamp, previous_checkpoint_height, previous_checkpoint_hash, \
             previous_checkpoint_parent, previous_checkpoint_time) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
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
                &addition
                    .block()
                    .timestamp
                    .map(|value| row::as_i64(value, "block timestamp"))
                    .transpose()?,
                &previous_height,
                &previous.map(|block| block.hash.0.clone()),
                &previous.and_then(|block| block.parent_hash.as_ref().map(|hash| hash.0.clone())),
                &previous_time,
            ],
        )
        .await
        .map_err(crate::store)?;
    Ok(())
}

async fn write_history(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
    addition: &BlockAddition,
) -> Result<(), IndexError> {
    for canonical in addition.transactions() {
        let (status, reason) = match &canonical.status {
            indexing::CanonicalStatus::Included { .. } => ("included", None),
            indexing::CanonicalStatus::Failed { reason, .. } => ("failed", reason.clone()),
        };
        let block = canonical.block();
        let fee = canonical.fee.as_ref();
        // One row per address the transaction touched: history is
        // address-primary, so a transaction paying two watched addresses is
        // listed under both.
        for address in canonical.addresses() {
            transaction
                .execute(
                    "INSERT INTO history (chain, network, address, height, transaction_id, \
                     status, failure_reason, block_hash, block_parent, block_timestamp, \
                     fee_asset, fee_amount, fee_payer) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,CAST($12 AS text)::numeric,$13)",
                    &[
                        &scope.chain.0,
                        &scope.network,
                        &address.value,
                        &height,
                        &canonical.transaction_id.value,
                        &status,
                        &reason,
                        &block.hash.0,
                        &block.parent_hash.as_ref().map(|hash| hash.0.clone()),
                        &block
                            .timestamp
                            .map(|value| row::as_i64(value, "block timestamp"))
                            .transpose()?,
                        &fee.map(|fee| fee.asset.asset.clone()),
                        &fee.map(|fee| fee.amount.to_string()),
                        &fee.and_then(|fee| fee.payer.as_ref().map(|payer| payer.value.clone())),
                    ],
                )
                .await
                .map_err(conflict_aware)?;

            for (ordinal, movement) in canonical.movements.iter().enumerate() {
                let ordinal = i32::try_from(ordinal)
                    .map_err(|_| row::store("transaction has too many movements"))?;
                transaction
                    .execute(
                        "INSERT INTO movement (chain, network, address, height, transaction_id, \
                         ordinal, kind, movement_id, asset_chain, asset, amount, from_address, \
                         to_address) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,CAST($11 AS text)::numeric,$12,$13)",
                        &[
                            &scope.chain.0,
                            &scope.network,
                            &address.value,
                            &height,
                            &canonical.transaction_id.value,
                            &ordinal,
                            &kind(movement),
                            &movement.id().0,
                            &movement.asset().chain.0,
                            &movement.asset().asset,
                            &movement.amount().to_string(),
                            &movement.from().map(|value| value.value.clone()),
                            &movement.to().map(|value| value.value.clone()),
                        ],
                    )
                    .await
                    .map_err(conflict_aware)?;
            }
        }
    }
    Ok(())
}

async fn write_created(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    addition: &BlockAddition,
) -> Result<(), IndexError> {
    for output in &addition.outputs().created {
        let index = i32::try_from(output.id.index)
            .map_err(|_| row::store("output index exceeds the storage range"))?;
        transaction
            .execute(
                "INSERT INTO output (chain, network, transaction_id, output_index, address, \
                 asset_chain, asset, amount, evidence, created_at, coinbase) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,CAST($8 AS text)::numeric,$9,$10,$11)",
                &[
                    &scope.chain.0,
                    &scope.network,
                    &output.id.transaction.value,
                    &index,
                    &output.address.value,
                    &output.asset.chain.0,
                    &output.asset.asset,
                    &output.amount.to_string(),
                    &output.evidence,
                    &row::as_i64(output.created_at.0, "output height")?,
                    &output.coinbase,
                ],
            )
            .await
            .map_err(conflict_aware)?;
    }
    Ok(())
}

async fn write_spent(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
    addition: &BlockAddition,
) -> Result<(), IndexError> {
    let required = addition.outputs().spent.iter().map(|key| (key, true));
    let tracked = addition
        .outputs()
        .tracked_spends
        .iter()
        .map(|key| (key, false));
    for (key, required) in required.chain(tracked) {
        // Copy into the journal first, then remove from the live set: the
        // amount and script exist nowhere else once the row is gone.
        let moved = copy_to_journal(transaction, scope, height, key).await?;
        if moved == 0 {
            if required {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block spends an unknown indexed output",
                    false,
                ));
            }
            continue;
        }
        let index = i32::try_from(key.output.index)
            .map_err(|_| row::store("output index exceeds the storage range"))?;
        transaction
            .execute(
                "DELETE FROM output WHERE chain = $1 AND network = $2 \
                 AND transaction_id = $3 AND output_index = $4",
                &[
                    &scope.chain.0,
                    &scope.network,
                    &key.output.transaction.value,
                    &index,
                ],
            )
            .await
            .map_err(crate::store)?;
    }
    Ok(())
}

async fn copy_to_journal(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    height: i64,
    key: &OutputKey,
) -> Result<u64, IndexError> {
    let index = i32::try_from(key.output.index)
        .map_err(|_| row::store("output index exceeds the storage range"))?;
    transaction
        .execute(
            "INSERT INTO journal_output (chain, network, height, transaction_id, output_index, \
             address, asset_chain, asset, amount, evidence, created_at, coinbase) \
             SELECT chain, network, $5, transaction_id, output_index, address, asset_chain, \
             asset, amount, evidence, created_at, coinbase FROM output \
             WHERE chain = $1 AND network = $2 AND transaction_id = $3 AND output_index = $4",
            &[
                &scope.chain.0,
                &scope.network,
                &key.output.transaction.value,
                &index,
                &height,
            ],
        )
        .await
        .map_err(crate::store)
}

async fn prune_journal(
    transaction: &Transaction<'_>,
    scope: &IndexScope,
    addition: &BlockAddition,
) -> Result<(), IndexError> {
    if addition.block().height.0 < addition.retention() {
        return Ok(());
    }
    let oldest = row::as_i64(
        addition.block().height.0 - addition.retention(),
        "block height",
    )?;
    transaction
        .execute(
            "DELETE FROM journal WHERE chain = $1 AND network = $2 AND height <= $3",
            &[&scope.chain.0, &scope.network, &oldest],
        )
        .await
        .map_err(crate::store)?;
    Ok(())
}

const fn kind(movement: &indexing::ValueMovement) -> &'static str {
    match movement {
        indexing::ValueMovement::Transfer { .. } => "transfer",
        indexing::ValueMovement::Input { .. } => "input",
        indexing::ValueMovement::Output { .. } => "output",
        indexing::ValueMovement::Mint { .. } => "mint",
        indexing::ValueMovement::Burn { .. } => "burn",
    }
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
