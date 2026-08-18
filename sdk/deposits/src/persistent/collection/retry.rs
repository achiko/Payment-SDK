use super::*;

impl<S> CollectionRetry for PaymentStore<S>
where
    S: Store,
{
    fn retry_leg<'a>(
        &'a self,
        command: RetryLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.mode == CollectionMode::UtxoBatch {
                let expected_transaction_id = match &command.expected.leg_state {
                    CollectionLegState::Failed { transaction_id }
                    | CollectionLegState::Reorged { transaction_id } => Some(transaction_id),
                    _ => None,
                };
                let current_transaction_id = match &collection.legs[position].state {
                    CollectionLegState::Signed { transaction_id }
                        if collection.legs[position].updated_at == command.updated_at =>
                    {
                        Some(transaction_id)
                    }
                    CollectionLegState::Broadcast { transaction_id }
                        if collection.legs[position].updated_at >= command.updated_at =>
                    {
                        Some(transaction_id)
                    }
                    _ => None,
                };
                if expected_transaction_id.is_some()
                    && current_transaction_id == expected_transaction_id
                    && collection.legs[position].last_error.is_none()
                {
                    let envelope = self
                        .stored_signed_envelope(
                            &command.collection_id,
                            &collection.legs[position].id,
                        )
                        .await?
                        .ok_or_else(|| {
                            storage_error("UTXO-batch retry replay lost durable signed bytes")
                        })?
                        .0;
                    if Some(&envelope.expected_transaction_id) != current_transaction_id {
                        return Err(conflict(
                            "UTXO-batch retry replay envelope identifies another transaction",
                        ));
                    }
                    return Ok(collection);
                }
                validate_guard(&collection, &collection.legs[position], &command.expected)?;
                if matches!(
                    collection.reservation().state,
                    CollectionReservationState::Released { .. }
                ) {
                    return Err(invalid_state(
                        "released UTXO resources require a new batch and fresh chain validation",
                    ));
                }
                let transaction_id = match &collection.legs[position].state {
                    CollectionLegState::Failed { transaction_id }
                    | CollectionLegState::Reorged { transaction_id } => transaction_id.clone(),
                    _ => {
                        return Err(invalid_state(
                            "only failed or reorged UTXO batch can retry retained bytes",
                        ));
                    }
                };
                validate_transition_time(&collection, command.updated_at)?;
                let envelope_key = envelope_key(&collection.id, &collection.legs[position].id)?;
                let (envelope, envelope_stored) = self
                    .stored_signed_envelope(&collection.id, &collection.legs[position].id)
                    .await?
                    .ok_or_else(|| storage_error("UTXO-batch retry lost durable signed bytes"))?;
                if envelope.expected_transaction_id != transaction_id {
                    return Err(conflict(
                        "UTXO-batch retry envelope identifies another transaction",
                    ));
                }
                let transaction_key = transaction_key(&transaction_id)?;
                let (reference, reference_stored) = self
                    .stored_leg_reference(&transaction_id)
                    .await?
                    .ok_or_else(|| storage_error("UTXO-batch retry lost transaction index"))?;
                if reference
                    != (LegRef {
                        collection_id: collection.id.clone(),
                        leg_id: collection.legs[position].id.clone(),
                    })
                {
                    return Err(conflict(
                        "UTXO-batch retry transaction belongs to another collection leg",
                    ));
                }
                let mut conditions = self.require_owned_active_indexes(&collection).await?;
                conditions.extend([
                    Condition::Version {
                        namespace: signed_envelope_ns(),
                        key: envelope_key,
                        expected: envelope_stored.version,
                    },
                    Condition::Version {
                        namespace: transaction_leg_ns(),
                        key: transaction_key,
                        expected: reference_stored.version,
                    },
                ]);
                collection.state = CollectionState::InProgress;
                collection.last_error = None;
                collection.updated_at = command.updated_at;
                let leg = &mut collection.legs[position];
                // This is an explicit same-byte rebroadcast request, not a new
                // signing attempt. `Signed` routes the executor through its
                // one-attempt broadcast recovery path while the retained
                // envelope, allocation, watch, and resource ownership remain
                // unchanged.
                leg.state = CollectionLegState::Signed { transaction_id };
                leg.last_error = None;
                leg.updated_at = command.updated_at;
                self.commit_collection_update(&stored, &collection, conditions, Vec::new())
                    .await?;
                return Ok(collection);
            }
            if collection.legs[position].state == CollectionLegState::Required
                && collection.legs[position].updated_at == command.updated_at
                && collection.legs[position].last_error.is_none()
            {
                return Ok(collection);
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if !matches!(
                collection.state,
                CollectionState::Failed | CollectionState::Reorged
            ) || !matches!(
                collection.legs[position].state,
                CollectionLegState::Failed { .. } | CollectionLegState::Reorged { .. }
            ) {
                return Err(invalid_state(
                    "only a terminal failed or reorged collection leg can retry",
                ));
            }
            ensure_previous_legs_confirmed(&collection, position)?;
            validate_transition_time(&collection, command.updated_at)?;
            let reservation_key = reservation_key(collection.deposit_id(), &collection.asset)?;
            let mut conditions = Vec::new();
            let mut operations = Vec::new();
            match &collection.reservation().state {
                CollectionReservationState::Active => {
                    conditions.extend(self.require_owned_active_indexes(&collection).await?);
                }
                CollectionReservationState::Released { .. } => {
                    if let Some((owner, _)) = self
                        .active_reservation_record(
                            &collection,
                            collection
                                .participants
                                .first()
                                .ok_or_else(|| storage_error("collection has no participant"))?,
                        )
                        .await?
                    {
                        return Err(conflict(format!(
                            "retry cannot reserve value because collection {} already owns the deposit and asset",
                            owner.0
                        )));
                    }
                    conditions.push(Condition::Missing {
                        namespace: active_reservation_ns(),
                        key: reservation_key.clone(),
                    });
                    let (eligibility_condition, eligibility_operation) = self
                        .collection_eligibility_generation_change(
                            collection.deposit_id(),
                            &collection.asset,
                        )
                        .await?;
                    conditions.push(eligibility_condition);
                    operations.push(eligibility_operation);
                    operations.push(Operation::Put {
                        namespace: active_reservation_ns(),
                        key: reservation_key,
                        value: encode(&IndexRecord {
                            version: RECORD_VERSION,
                            collection_id: collection.id.0.clone(),
                        })?,
                    });
                    set_reservation_state(&mut collection, CollectionReservationState::Active);
                }
                CollectionReservationState::Consumed { .. } => {
                    return Err(invalid_state(
                        "consumed collection reservation cannot retry",
                    ));
                }
            }
            collection.state = if position == 0 {
                CollectionState::Required
            } else {
                CollectionState::InProgress
            };
            collection.last_error = None;
            collection.updated_at = command.updated_at;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Required;
            if leg.kind == CollectionLegKind::Sweep && collection.mode != CollectionMode::UtxoBatch
            {
                leg.planned_amount = None;
            }
            leg.watch_id = None;
            leg.allocation = None;
            leg.allocations.clear();
            leg.last_error = None;
            leg.updated_at = command.updated_at;
            self.commit_collection_update(&stored, &collection, conditions, operations)
                .await?;
            Ok(collection)
        })
    }

    fn release_reservation<'a>(
        &'a self,
        command: ReleaseReservation,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            if collection.mode == CollectionMode::UtxoBatch {
                return Err(invalid_state(
                    "UTXO-batch exact-resource ownership cannot be released",
                ));
            }
            let desired = CollectionReservationState::Released {
                reason: command.reason,
                released_at: command.released_at,
            };
            if collection.reservation().state == desired {
                return Ok(collection);
            }
            if collection.state != command.expected_collection_state {
                return Err(conflict(
                    "stale expected collection state for reservation release",
                ));
            }
            if collection.reservation().state != command.expected_reservation_state {
                return Err(conflict("stale expected collection reservation state"));
            }
            match (collection.state, command.reason) {
                (CollectionState::Failed, ReservationReleaseReason::TerminalFailure)
                | (CollectionState::Reorged, ReservationReleaseReason::Reorg) => {}
                _ => {
                    return Err(invalid_state(
                        "reservation release reason must match terminal failure or reorg state",
                    ));
                }
            }
            if collection.reservation().state != CollectionReservationState::Active {
                return Err(invalid_state(
                    "only an active collection reservation can be released",
                ));
            }
            validate_transition_time(&collection, command.released_at)?;
            let conditions = self.require_owned_active_indexes(&collection).await?;
            let operations = Self::reservation_release_operations(&collection)?;
            set_reservation_state(&mut collection, desired);
            collection.updated_at = command.released_at;
            self.commit_collection_update(&stored, &collection, conditions, operations)
                .await?;
            Ok(collection)
        })
    }
}

impl<S> PaymentStore<S>
where
    S: Store,
{
    fn reservation_release_operations(
        collection: &Collection,
    ) -> Result<Vec<Operation>, DepositError> {
        let mut operations = Vec::new();
        for participant in &collection.participants {
            operations.push(Operation::Delete {
                namespace: active_reservation_ns(),
                key: reservation_key(
                    &participant.reservation.deposit_id,
                    &participant.reservation.asset,
                )?,
            });
            for resource in &participant.spend_resources {
                operations.push(Operation::Delete {
                    namespace: active_spend_resource_ns(),
                    key: spend_resource_key(&resource.id)?,
                });
            }
        }
        Ok(operations)
    }
}
