//! Reorg reversal: removes an orphaned tip using only the rollback journal.

use indexing::{BlockRef, IndexError, IndexErrorKind, IndexScope};

use crate::{Repository, row, write::move_checkpoint};

const JOURNAL_ENTRY: &str = "\
SELECT block_hash, previous_checkpoint_position AS previous_position,
       previous_checkpoint_height AS previous_height,
       previous_checkpoint_hash AS previous_hash,
       previous_checkpoint_parent_position AS previous_parent_position,
       previous_checkpoint_parent AS previous_parent,
       previous_checkpoint_time AS previous_timestamp
FROM payments_journal WHERE chain = $1 AND network = $2 AND height = $3";

/// Movements are removed by the same predicate as their history rows rather
/// than by cascading from them: the foreign key that would cascade costs more
/// on every insert than the delete saves on the rare reorg.
const DELETE_MOVEMENT: &str =
    "DELETE FROM payments_movement WHERE chain = $1 AND network = $2 AND height = $3";

const DELETE_HISTORY: &str =
    "DELETE FROM payments_history WHERE chain = $1 AND network = $2 AND height = $3";

const DELETE_CREATED: &str =
    "DELETE FROM payments_output WHERE chain = $1 AND network = $2 AND created_at = $3";

const RESTORE_SPENT: &str = "\
INSERT INTO payments_output (chain, network, transaction_id, output_index, address, asset_chain,
                             asset, amount, evidence, created_at, coinbase)
SELECT chain, network, transaction_id, output_index, address, asset_chain, asset, amount,
       evidence, created_at, coinbase
FROM payments_journal_output WHERE chain = $1 AND network = $2 AND height = $3";

const DROP_CHECKPOINT: &str = "DELETE FROM payments_checkpoint WHERE chain = $1 AND network = $2";

const DROP_JOURNAL: &str =
    "DELETE FROM payments_journal WHERE chain = $1 AND network = $2 AND height = $3";

impl Repository {
    pub(crate) async fn remove_tip(
        &self,
        scope: &IndexScope,
        expected_tip: &BlockRef,
    ) -> Result<Option<BlockRef>, IndexError> {
        self.check_scope(scope)?;
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(crate::store)?;

        self.lock_scope(&transaction).await?;
        let current = self.locked_checkpoint(&transaction).await?;
        if current.as_ref() != Some(expected_tip) {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "revert must target the current checkpoint",
                true,
            ));
        }
        let height = row::as_i64(expected_tip.height.0, "block height")?;
        let statement = transaction
            .prepare_cached(JOURNAL_ENTRY)
            .await
            .map_err(crate::store)?;
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
            let statement = transaction
                .prepare_cached(sql)
                .await
                .map_err(crate::store)?;
            transaction
                .execute(&statement, &[&scope.chain.0, &scope.network, &height])
                .await
                .map_err(crate::store)?;
        }
        // Spent outputs are not recoverable, so they come back from the journal.
        let statement = transaction
            .prepare_cached(RESTORE_SPENT)
            .await
            .map_err(crate::store)?;
        transaction
            .execute(&statement, &[&scope.chain.0, &scope.network, &height])
            .await
            .map_err(crate::store)?;

        let previous_height: Option<i64> =
            entry.try_get("previous_height").map_err(crate::store)?;
        let previous = match previous_height {
            None => None,
            Some(_) => Some(row::block(&entry, "previous_")?),
        };
        match &previous {
            Some(block) => move_checkpoint(&transaction, scope, block).await?,
            None => {
                let statement = transaction
                    .prepare_cached(DROP_CHECKPOINT)
                    .await
                    .map_err(crate::store)?;
                transaction
                    .execute(&statement, &[&scope.chain.0, &scope.network])
                    .await
                    .map_err(crate::store)?;
            }
        }
        // journal_output cascades with the journal row.
        let statement = transaction
            .prepare_cached(DROP_JOURNAL)
            .await
            .map_err(crate::store)?;
        transaction
            .execute(&statement, &[&scope.chain.0, &scope.network, &height])
            .await
            .map_err(crate::store)?;

        transaction.commit().await.map_err(crate::store)?;
        Ok(previous)
    }
}
