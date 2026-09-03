//! Reorg reversal: removes an orphaned tip using only the rollback journal.

use indexing::{BlockRef, IndexError, IndexErrorKind, IndexScope};

use crate::{
    Repository, prepare_in, row,
    write::{lock_scope, locked_checkpoint, move_checkpoint, optional_block},
};

const JOURNAL_ENTRY: &str = "\
SELECT block_hash, previous_checkpoint_position AS previous_position,
       previous_checkpoint_height AS previous_height,
       previous_checkpoint_hash AS previous_hash,
       previous_checkpoint_parent_position AS previous_parent_position,
       previous_checkpoint_parent AS previous_parent,
       previous_checkpoint_time AS previous_timestamp
FROM journal WHERE chain = $1 AND network = $2 AND height = $3";

/// Movements are removed by the same predicate as their history rows rather
/// than by cascading from them: the foreign key that would cascade costs more
/// on every insert than the delete saves on the rare reorg.
const DELETE_MOVEMENT: &str =
    "DELETE FROM movement WHERE chain = $1 AND network = $2 AND height = $3";

const DELETE_HISTORY: &str =
    "DELETE FROM history WHERE chain = $1 AND network = $2 AND height = $3";

const DELETE_CREATED: &str =
    "DELETE FROM output WHERE chain = $1 AND network = $2 AND created_at = $3";

const RESTORE_SPENT: &str = "\
INSERT INTO output (chain, network, transaction_id, output_index, address, asset_chain, asset,
                    amount, evidence, created_at, coinbase)
SELECT chain, network, transaction_id, output_index, address, asset_chain, asset, amount,
       evidence, created_at, coinbase
FROM journal_output WHERE chain = $1 AND network = $2 AND height = $3";

const DROP_CHECKPOINT: &str = "DELETE FROM checkpoint WHERE chain = $1 AND network = $2";

const DROP_JOURNAL: &str = "DELETE FROM journal WHERE chain = $1 AND network = $2 AND height = $3";

impl Repository {
    pub(crate) async fn remove_tip(
        &self,
        scope: &IndexScope,
        expected_tip: &BlockRef,
    ) -> Result<Option<BlockRef>, IndexError> {
        self.check_scope(scope)?;
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(crate::store)?;

        lock_scope(&transaction, scope).await?;
        let current = locked_checkpoint(&transaction, scope).await?;
        if current.as_ref() != Some(expected_tip) {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "revert must target the current checkpoint",
                true,
            ));
        }
        let height = row::as_i64(expected_tip.height.0, "block height")?;
        let statement = prepare_in(&transaction, JOURNAL_ENTRY).await?;
        let entry = transaction
            .query_opt(&statement, &[&scope.chain.0, &scope.network, &height])
            .await
            .map_err(crate::store)?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::ReorgTooDeep,
                    "rollback journal is not retained",
                    false,
                )
            })?;
        let stored_hash: Vec<u8> = entry.try_get("block_hash").map_err(crate::store)?;
        if stored_hash != expected_tip.hash.0 {
            return Err(row::store(
                "rollback journal does not match the canonical tip",
            ));
        }

        // Everything the block wrote except its spends is identified by the
        // block's height, so it is deleted by predicate rather than recorded in
        // the journal. Movements go first: nothing cascades them any more.
        for sql in [DELETE_MOVEMENT, DELETE_HISTORY, DELETE_CREATED] {
            let statement = prepare_in(&transaction, sql).await?;
            transaction
                .execute(&statement, &[&scope.chain.0, &scope.network, &height])
                .await
                .map_err(crate::store)?;
        }
        // Spent outputs are not recoverable, so they come back from the journal.
        let statement = prepare_in(&transaction, RESTORE_SPENT).await?;
        transaction
            .execute(&statement, &[&scope.chain.0, &scope.network, &height])
            .await
            .map_err(crate::store)?;

        let previous = optional_block(&entry, "previous_")?;
        match &previous {
            Some(block) => move_checkpoint(&transaction, scope, block).await?,
            None => {
                let statement = prepare_in(&transaction, DROP_CHECKPOINT).await?;
                transaction
                    .execute(&statement, &[&scope.chain.0, &scope.network])
                    .await
                    .map_err(crate::store)?;
            }
        }
        // journal_output cascades with the journal row.
        let statement = prepare_in(&transaction, DROP_JOURNAL).await?;
        transaction
            .execute(&statement, &[&scope.chain.0, &scope.network, &height])
            .await
            .map_err(crate::store)?;

        transaction.commit().await.map_err(crate::store)?;
        Ok(previous)
    }
}
