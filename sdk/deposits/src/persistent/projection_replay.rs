use super::ledger::{projection_entry, projection_network_fee, resolved_effect};
use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn replay_projection(
        &self,
        command: &ProjectObservation,
        checkpoint: ConsumerCheckpoint,
        affected_deposits: &std::collections::BTreeSet<DepositId>,
    ) -> Result<ProjectionOutcome, DepositError> {
        let mut ledger_results = Vec::with_capacity(command.ledger_updates.len());
        for update in &command.ledger_updates {
            let mirrored = self.observation(&update.event_id).await?.ok_or_else(|| {
                storage_error("projection cursor advanced without its mirrored event")
            })?;
            if mirrored.event.cursor != command.through {
                return Err(conflict(
                    "projection retry references a different IX cursor",
                ));
            }
            let deposit = self
                .deposit(&update.deposit_id)
                .await?
                .ok_or_else(|| storage_error("projected deposit is missing"))?;
            let expected_head_id = update
                .expected_head
                .as_ref()
                .ok_or_else(|| conflict("observation projection requires an expected head"))?;
            let expected_head = self
                .stored_ledger_entry(&update.deposit_id, expected_head_id)
                .await?
                .ok_or_else(|| conflict("ledger expected head does not exist"))?;
            let expected_entry = projection_entry(
                update,
                &mirrored.event,
                &expected_head,
                resolved_effect(&mirrored.event, &deposit, &update.effect)?,
                projection_network_fee(
                    &mirrored.event,
                    &deposit,
                    &update.effect,
                    command.fee_treatment,
                )?,
            )?;
            let projection_id = ProjectionId::for_observation(
                &mirrored.event.id,
                mirrored.event.transaction.revision,
                &update.deposit_id,
            );
            let stored = self
                .storage
                .get(&projection_ns(), &key_text(&projection_id.0))
                .await
                .map_err(map_storage)?
                .ok_or_else(|| {
                    storage_error("projection cursor advanced without a projection record")
                })?;
            let index: IdRecord = decode(&stored)?;
            ensure_version(index.version)?;
            let entry = self
                .stored_ledger_entry(&update.deposit_id, &EntryId(index.id))
                .await?
                .ok_or_else(|| storage_error("projection record is dangling"))?;
            if entry != expected_entry {
                return Err(conflict(
                    "projection retry changed the deterministic ledger effect",
                ));
            }
            ledger_results.push(ApplyResult::AlreadyPresent { entry });
        }
        let mut cases = Vec::with_capacity(command.reconciliation_cases.len());
        for case in &command.reconciliation_cases {
            cases.push(self.case(&case.id).await?.ok_or_else(|| {
                storage_error("projection cursor advanced without a reconciliation case")
            })?);
        }
        for deposit_id in affected_deposits {
            let stored = self
                .storage
                .get(
                    &deposit_observation_ns(),
                    &deposit_observation_key(deposit_id, command.through)?,
                )
                .await
                .map_err(map_storage)?
                .ok_or_else(|| {
                    storage_error("projection cursor advanced without a deposit observation index")
                })?;
            let index: IdRecord = decode(&stored)?;
            ensure_version(index.version)?;
            let event = self.observation(&EventId(index.id)).await?;
            if event.as_ref().map(|observation| observation.event.cursor) != Some(command.through) {
                return Err(conflict(
                    "projection retry changed a deposit observation attribution",
                ));
            }
        }
        if let Some(mutation) = &command.utxo_batch_transition {
            let collection = self
                .validate_utxo_batch_projection_replay(
                    &mutation.collection_id,
                    &mutation.leg_id,
                    &mutation.transaction_id,
                    &mutation.transition,
                )
                .await?;
            let participant_deposits = collection
                .participants
                .iter()
                .map(|participant| participant.reservation.deposit_id.clone())
                .collect::<Vec<_>>();
            let ledger_deposits = command
                .ledger_updates
                .iter()
                .map(|update| update.deposit_id.clone())
                .collect::<Vec<_>>();
            if command.affected_deposits != participant_deposits
                || ledger_deposits != participant_deposits
            {
                return Err(conflict(
                    "UTXO-batch projection retry changed participant coverage",
                ));
            }
            let event_id = command
                .ledger_updates
                .first()
                .map(|update| &update.event_id)
                .ok_or_else(|| {
                    conflict("UTXO-batch projection retry omitted participant ledgers")
                })?;
            let event = self
                .observation(event_id)
                .await?
                .ok_or_else(|| storage_error("UTXO-batch projection event is missing"))?;
            let status_matches = matches!(
                (&mutation.transition, &event.event.transaction.status),
                (
                    UtxoBatchProjectionTransition::Reincluded { .. },
                    TransactionStatus::Included { .. }
                ) | (
                    UtxoBatchProjectionTransition::Confirmed { .. },
                    TransactionStatus::Confirmed { .. }
                ) | (
                    UtxoBatchProjectionTransition::Reorged { .. },
                    TransactionStatus::Reorged { .. }
                )
            );
            if event.event.transaction.transaction_id != mutation.transaction_id || !status_matches
            {
                return Err(conflict(
                    "UTXO-batch projection retry changed its mirrored IX fact",
                ));
            }
            let fee = event.event.transaction.fee.as_ref().ok_or_else(|| {
                conflict("UTXO-batch projection retry lost its factual network fee")
            })?;
            let allocated_fee = collection
                .legs
                .iter()
                .find(|leg| leg.id == mutation.leg_id)
                .ok_or_else(|| storage_error("UTXO-batch retry leg disappeared"))?
                .allocations
                .iter()
                .try_fold(Decimal::zero(), |total, allocation| {
                    amount::checked_add(&total, &allocation.allocated_fee)
                        .map_err(|_| conflict("UTXO-batch retry allocated fee total overflows"))
                })?;
            if fee.asset != collection.asset || fee.amount != allocated_fee {
                return Err(conflict(
                    "UTXO-batch projection retry changed factual fee attribution",
                ));
            }
        }
        Ok(ProjectionOutcome {
            checkpoint,
            ledger_results,
            reconciliation_cases: cases,
        })
    }
}
