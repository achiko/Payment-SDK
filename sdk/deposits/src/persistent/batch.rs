use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn project_batch(
        &self,
        command: ProjectBatch,
    ) -> Result<BatchOutcome, DepositError> {
        if command.projection.utxo_batch_transition.is_some() {
            return Err(invalid(
                "semantic UTXO-batch command must not contain a nested collection transition",
            ));
        }
        let mutation = BatchMutation {
            collection_id: command.collection_id,
            leg_id: command.leg_id,
            expected: command.expected,
            transaction_id: command.transaction_id,
            transition: command.transition,
        };
        let mut projection_command = command.projection;
        projection_command.utxo_batch_transition = Some(mutation.clone());
        let projection = self.project_and_advance(projection_command).await?;
        let collection = self
            .validate_utxo_batch_projection_replay(
                &mutation.collection_id,
                &mutation.leg_id,
                &mutation.transaction_id,
                &mutation.transition,
            )
            .await?;
        Ok(BatchOutcome {
            projection,
            collection,
        })
    }
}
