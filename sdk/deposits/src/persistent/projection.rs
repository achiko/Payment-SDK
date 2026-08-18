use super::event::{checkpoint_condition, expected_next_cursor};
use super::ledger::{projection_entry, projection_network_fee, resolved_effect};
use super::*;

impl<S> EventProjector for PaymentStore<S>
where
    S: Store,
{
    fn mirror_and_advance<'a>(
        &'a self,
        command: MirrorObservation,
    ) -> BoxFuture<'a, Result<MirrorOutcome, DepositError>> {
        Box::pin(self.mirror_event(command))
    }

    fn project_and_advance<'a>(
        &'a self,
        command: ProjectObservation,
    ) -> BoxFuture<'a, Result<ProjectionOutcome, DepositError>> {
        Box::pin(async move {
            let affected_deposits = command
                .affected_deposits
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            if affected_deposits.len() != command.affected_deposits.len()
                || affected_deposits
                    .iter()
                    .any(|deposit_id| deposit_id.0.is_empty())
            {
                return Err(invalid(
                    "projection affected deposits must be unique and non-empty",
                ));
            }
            if command
                .ledger_updates
                .iter()
                .any(|update| !affected_deposits.contains(&update.deposit_id))
                || command
                    .reconciliation_cases
                    .iter()
                    .any(|case| !affected_deposits.contains(&case.deposit_id))
            {
                return Err(invalid(
                    "every projected ledger update and reconciliation case must identify an affected deposit",
                ));
            }
            if command.utxo_batch_transition.is_some()
                && command.fee_treatment != ProjectionFeeTreatment::IncludedInMovementEffect
            {
                return Err(invalid(
                    "UTXO-batch projection must treat its factual fee as included in input movements",
                ));
            }
            let ledger_update_deposits = command
                .ledger_updates
                .iter()
                .map(|update| update.deposit_id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            if command
                .reconciliation_cases
                .iter()
                .any(|case| !ledger_update_deposits.contains(&case.deposit_id))
            {
                return Err(invalid(
                    "every projected reconciliation case must share its deposit's atomic ledger update",
                ));
            }
            let (checkpoint, checkpoint_stored) = self
                .stored_checkpoint(ConsumerCheckpointName::IxProjection)
                .await?;
            if checkpoint.cursor == Some(command.through) {
                return self
                    .replay_projection(&command, checkpoint, &affected_deposits)
                    .await;
            }
            if checkpoint.cursor != command.expected_cursor {
                return Err(conflict(
                    "projection expected cursor does not match durable cursor",
                ));
            }
            if expected_next_cursor(checkpoint.cursor)? != command.through {
                return Err(conflict(
                    "mirrored observations must be projected in contiguous cursor order",
                ));
            }
            let mirrored = self
                .observations(LogQuery {
                    after: checkpoint.cursor,
                    limit: 1,
                })
                .await?;
            let event = mirrored
                .observations
                .first()
                .ok_or_else(|| not_found("projection cursor has no mirrored IX event"))?;
            if event.event.cursor != command.through {
                return Err(conflict(
                    "next mirrored event does not match projection target cursor",
                ));
            }

            let mut conditions = vec![checkpoint_condition(
                ConsumerCheckpointName::IxProjection,
                checkpoint_stored.as_ref(),
            )];
            let mut operations = Vec::new();
            let mut ledger_results = Vec::with_capacity(command.ledger_updates.len());
            let mut seen_deposits = std::collections::BTreeSet::new();
            for update in &command.ledger_updates {
                if update.event_id != event.event.id {
                    return Err(invalid(
                        "ledger projection references a different mirrored IX event",
                    ));
                }
                if !seen_deposits.insert(update.deposit_id.clone()) {
                    return Err(invalid(
                        "one projection command contains multiple updates for one deposit",
                    ));
                }
                let projection_id = ProjectionId::for_observation(
                    &event.event.id,
                    event.event.transaction.revision,
                    &update.deposit_id,
                );
                let projection_key = key_text(&projection_id.0);
                if self
                    .storage
                    .get(&projection_ns(), &projection_key)
                    .await
                    .map_err(map_storage)?
                    .is_some()
                {
                    return Err(conflict(
                        "projection ID exists while the projection cursor is behind",
                    ));
                }
                let (head, head_stored) = self
                    .stored_head(&update.deposit_id)
                    .await?
                    .ok_or_else(|| not_found("deposit ledger is not open"))?;
                if update.expected_head.as_ref() != Some(&head.id) {
                    return Err(conflict("ledger expected head does not match current head"));
                }
                let deposit = self
                    .deposit(&update.deposit_id)
                    .await?
                    .ok_or_else(|| not_found("observation projection deposit does not exist"))?;
                let entry = projection_entry(
                    update,
                    &event.event,
                    &head,
                    resolved_effect(&event.event, &deposit, &update.effect)?,
                    projection_network_fee(
                        &event.event,
                        &deposit,
                        &update.effect,
                        command.fee_treatment,
                    )?,
                )?;
                conditions.extend([
                    Condition::Missing {
                        namespace: projection_ns(),
                        key: projection_key.clone(),
                    },
                    Condition::Version {
                        namespace: ledger_head_ns(),
                        key: key_text(&update.deposit_id.0),
                        expected: head_stored.version,
                    },
                    Condition::Missing {
                        namespace: ledger_entry_ns(),
                        key: ledger_entry_key(&update.deposit_id, &entry.id)?,
                    },
                ]);
                operations.extend([
                    Operation::Put {
                        namespace: ledger_entry_ns(),
                        key: ledger_entry_key(&update.deposit_id, &entry.id)?,
                        value: encode(&LedgerRecord::from(&entry))?,
                    },
                    Operation::Put {
                        namespace: ledger_head_ns(),
                        key: key_text(&update.deposit_id.0),
                        value: encode(&IdRecord {
                            version: RECORD_VERSION,
                            id: entry.id.0.clone(),
                        })?,
                    },
                    Operation::Put {
                        namespace: projection_ns(),
                        key: projection_key,
                        value: encode(&IdRecord {
                            version: RECORD_VERSION,
                            id: entry.id.0.clone(),
                        })?,
                    },
                ]);
                ledger_results.push(ApplyResult::Appended { entry });
            }

            for deposit_id in &affected_deposits {
                if self.deposit(deposit_id).await?.is_none() {
                    return Err(not_found(
                        "deposit observation attribution references a missing deposit",
                    ));
                }
                let key = deposit_observation_key(deposit_id, command.through)?;
                if self
                    .storage
                    .get(&deposit_observation_ns(), &key)
                    .await
                    .map_err(map_storage)?
                    .is_some()
                {
                    return Err(conflict(
                        "deposit observation index exists while projection cursor is behind",
                    ));
                }
                conditions.push(Condition::Missing {
                    namespace: deposit_observation_ns(),
                    key: key.clone(),
                });
                operations.push(Operation::Put {
                    namespace: deposit_observation_ns(),
                    key,
                    value: encode(&IdRecord {
                        version: RECORD_VERSION,
                        id: event.event.id.0.clone(),
                    })?,
                });
            }

            let mut reconciliation_generation_deposits = std::collections::BTreeSet::new();
            for case in &command.reconciliation_cases {
                Self::validate_projection_case(case, &event.event.id)?;
                if self.case(&case.id).await?.is_some() {
                    return Err(conflict(
                        "reconciliation case exists while projection cursor is behind",
                    ));
                }
                self.append_reconciliation_generation(
                    &mut reconciliation_generation_deposits,
                    &mut conditions,
                    &mut operations,
                    &case.deposit_id,
                )
                .await?;
                conditions.extend([
                    Condition::Missing {
                        namespace: reconciliation_ns(),
                        key: key_text(&case.id.0),
                    },
                    Condition::Missing {
                        namespace: reconciliation_deposit_ns(),
                        key: reconciliation_deposit_key(&case.deposit_id, &case.id)?,
                    },
                ]);
                operations.extend([
                    Operation::Put {
                        namespace: reconciliation_ns(),
                        key: key_text(&case.id.0),
                        value: encode(&ReconciliationRecord::try_from(case)?)?,
                    },
                    Operation::Put {
                        namespace: reconciliation_deposit_ns(),
                        key: reconciliation_deposit_key(&case.deposit_id, &case.id)?,
                        value: encode(&IdRecord {
                            version: RECORD_VERSION,
                            id: case.id.0.clone(),
                        })?,
                    },
                ]);
            }
            if let Some(mutation) = &command.utxo_batch_transition {
                let prepared = self
                    .prepare_utxo_batch_projection_transition(
                        &mutation.collection_id,
                        &mutation.leg_id,
                        &mutation.expected,
                        &mutation.transaction_id,
                        &mutation.transition,
                    )
                    .await?;
                let participant_deposits = prepared
                    .collection
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
                    return Err(invalid(
                        "UTXO-batch projection must atomically cover every participant in canonical order",
                    ));
                }
                if event.event.transaction.transaction_id != mutation.transaction_id {
                    return Err(conflict(
                        "UTXO-batch projection event identifies another transaction",
                    ));
                }
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
                if !status_matches {
                    return Err(invalid(
                        "UTXO-batch collection transition does not match mirrored IX status",
                    ));
                }
                let fee = event.event.transaction.fee.as_ref().ok_or_else(|| {
                    invalid("UTXO-batch projection requires the factual network fee")
                })?;
                if fee.asset != prepared.collection.asset {
                    return Err(invalid(
                        "UTXO-batch factual network fee asset differs from collection asset",
                    ));
                }
                let allocated_fee = prepared
                    .collection
                    .legs
                    .iter()
                    .find(|leg| leg.id == mutation.leg_id)
                    .ok_or_else(|| storage_error("UTXO-batch projection leg disappeared"))?
                    .allocations
                    .iter()
                    .try_fold(Decimal::zero(), |total, allocation| {
                        amount::checked_add(&total, &allocation.allocated_fee).map_err(|_| {
                            invalid("UTXO-batch allocated network fee total overflows")
                        })
                    })?;
                if allocated_fee != fee.amount {
                    return Err(conflict(
                        "UTXO-batch allocated fee differs from mirrored factual network fee",
                    ));
                }
                conditions.extend(prepared.conditions);
                operations.extend(prepared.operations);
            }
            operations.push(Operation::Put {
                namespace: consumer_checkpoint_ns(),
                key: checkpoint_key(ConsumerCheckpointName::IxProjection),
                value: encode(&CursorRecord {
                    version: RECORD_VERSION,
                    cursor: Some(command.through.0),
                })?,
            });
            self.storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await
                .map_err(map_storage)?;
            Ok(ProjectionOutcome {
                checkpoint: ConsumerCheckpoint {
                    name: ConsumerCheckpointName::IxProjection,
                    cursor: Some(command.through),
                },
                ledger_results,
                reconciliation_cases: command.reconciliation_cases,
            })
        })
    }

    fn project_utxo_batch_and_advance<'a>(
        &'a self,
        command: ProjectBatch,
    ) -> BoxFuture<'a, Result<BatchOutcome, DepositError>> {
        Box::pin(self.project_batch(command))
    }
}

impl<S> PaymentStore<S>
where
    S: Store,
{
    fn validate_projection_case(
        case: &ReconciliationCase,
        event_id: &EventId,
    ) -> Result<(), DepositError> {
        if &case.triggering_event_id != event_id || case.state != ReconciliationState::Open {
            return Err(invalid(
                "projection reconciliation case must be open and reference its IX event",
            ));
        }
        match &case.reason {
            ReconciliationReason::PostCreditReorg {
                accounted,
                corrected_confirmed,
            } if accounted <= corrected_confirmed => Err(invalid(
                "post-credit reorg case requires accounted to exceed corrected confirmed",
            )),
            ReconciliationReason::ReservedSpendConflict {
                collection_id,
                transaction_id,
            } if collection_id.0.is_empty()
                || transaction_id.scope.chain.0.is_empty()
                || transaction_id.scope.network.is_empty()
                || transaction_id.value.is_empty() =>
            {
                Err(invalid(
                    "reserved-spend conflict requires collection and transaction identity",
                ))
            }
            ReconciliationReason::PostCreditReorg { .. }
            | ReconciliationReason::ReservedSpendConflict { .. } => Ok(()),
        }
    }

    async fn append_reconciliation_generation(
        &self,
        seen: &mut std::collections::BTreeSet<DepositId>,
        conditions: &mut Vec<Condition>,
        operations: &mut Vec<Operation>,
        deposit_id: &DepositId,
    ) -> Result<(), DepositError> {
        if seen.insert(deposit_id.clone()) {
            let (condition, operation) = self.reconciliation_generation_change(deposit_id).await?;
            conditions.push(condition);
            operations.push(operation);
        }
        Ok(())
    }
}
