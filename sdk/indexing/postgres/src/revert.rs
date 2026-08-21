//! Reorg reversal: removes an orphaned tip using only the rollback journal.

use indexing::{BlockRef, IndexError, IndexErrorKind, IndexScope};

use crate::{
    Repository, row,
    write::{locked_checkpoint, move_checkpoint, optional_block},
};

impl Repository {
    pub(crate) async fn remove_tip(
        &self,
        scope: &IndexScope,
        expected_tip: &BlockRef,
    ) -> Result<Option<BlockRef>, IndexError> {
        self.check_scope(scope)?;
        let mut client = self.pool.get().await.map_err(crate::unavailable)?;
        let transaction = client.transaction().await.map_err(crate::store)?;

        let current = locked_checkpoint(&transaction, scope).await?;
        if current.as_ref() != Some(expected_tip) {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "revert must target the current checkpoint",
                true,
            ));
        }
        let height = row::as_i64(expected_tip.height.0, "block height")?;
        let entry = transaction
            .query_opt(
                "SELECT block_hash, previous_checkpoint_height AS previous_height, \
                 previous_checkpoint_hash AS previous_hash, \
                 previous_checkpoint_parent AS previous_parent, \
                 previous_checkpoint_time AS previous_timestamp \
                 FROM journal WHERE chain = $1 AND network = $2 AND height = $3",
                &[&scope.chain.0, &scope.network, &height],
            )
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

        // History and created outputs are recoverable by predicate, so they are
        // simply deleted; movements cascade from history.
        for statement in [
            "DELETE FROM history WHERE chain = $1 AND network = $2 AND height = $3",
            "DELETE FROM output  WHERE chain = $1 AND network = $2 AND created_at = $3",
        ] {
            transaction
                .execute(statement, &[&scope.chain.0, &scope.network, &height])
                .await
                .map_err(crate::store)?;
        }
        // Spent outputs are not recoverable, so they come back from the journal.
        transaction
            .execute(
                "INSERT INTO output (chain, network, transaction_id, output_index, address, \
                 asset_chain, asset, amount, evidence, created_at, coinbase) \
                 SELECT chain, network, transaction_id, output_index, address, asset_chain, \
                 asset, amount, evidence, created_at, coinbase FROM journal_output \
                 WHERE chain = $1 AND network = $2 AND height = $3",
                &[&scope.chain.0, &scope.network, &height],
            )
            .await
            .map_err(crate::store)?;

        let previous = optional_block(&entry, "previous_")?;
        match &previous {
            Some(block) => move_checkpoint(&transaction, scope, block).await?,
            None => {
                transaction
                    .execute(
                        "DELETE FROM checkpoint WHERE chain = $1 AND network = $2",
                        &[&scope.chain.0, &scope.network],
                    )
                    .await
                    .map_err(crate::store)?;
            }
        }
        // journal_output cascades with the journal row.
        transaction
            .execute(
                "DELETE FROM journal WHERE chain = $1 AND network = $2 AND height = $3",
                &[&scope.chain.0, &scope.network, &height],
            )
            .await
            .map_err(crate::store)?;

        transaction.commit().await.map_err(crate::store)?;
        Ok(previous)
    }
}
