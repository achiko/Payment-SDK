use super::*;

impl<S> LegOutcome for PaymentStore<S>
where
    S: Store,
{
    fn confirm_leg<'a>(
        &'a self,
        command: ConfirmLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.mode == CollectionMode::UtxoBatch {
                return Err(invalid_state(
                    "UTXO-batch confirmation must use atomic collection projection",
                ));
            }
            if collection.legs[position].state
                == (CollectionLegState::Confirmed {
                    transaction_id: command.transaction_id.clone(),
                })
            {
                if collection.legs[position].allocation == command.allocation {
                    return Ok(collection);
                }
                return Err(conflict(
                    "confirmed collection leg was replayed with different attribution",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if collection.state != CollectionState::InProgress {
                return Err(invalid_state(
                    "only an in-progress collection can confirm a leg",
                ));
            }
            match &collection.legs[position].state {
                CollectionLegState::Broadcast { transaction_id }
                    if transaction_id == &command.transaction_id => {}
                CollectionLegState::Broadcast { .. } => {
                    return Err(conflict(
                        "confirmation transaction ID does not match broadcast leg",
                    ));
                }
                _ => {
                    return Err(invalid_state(
                        "only a broadcast collection leg can be confirmed",
                    ));
                }
            }
            if collection.legs[position].watch_id.is_none() {
                return Err(invalid_state(
                    "collection leg cannot confirm before durable IX watch registration",
                ));
            }
            validate_transition_time(&collection, command.confirmed_at)?;
            validate_transaction_for_collection(&collection, &command.transaction_id)?;
            match collection.legs[position].kind {
                CollectionLegKind::GasFunding if command.allocation.is_some() => {
                    return Err(invalid(
                        "gas-funding confirmation must not contain sweep attribution",
                    ));
                }
                CollectionLegKind::Sweep => {
                    let allocation = command.allocation.as_ref().ok_or_else(|| {
                        invalid("sweep confirmation requires factual collection attribution")
                    })?;
                    validate_allocation(&collection, allocation)?;
                }
                _ => {}
            }
            let expected_reference = LegRef {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let reference = self
                .stored_leg_reference(&command.transaction_id)
                .await?
                .map(|(reference, _)| reference)
                .ok_or_else(|| storage_error("broadcast leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            let ownership_conditions = self.require_owned_active_indexes(&collection).await?;
            collection.updated_at = command.confirmed_at;
            collection.last_error = None;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Confirmed {
                transaction_id: command.transaction_id.clone(),
            };
            leg.allocations = command.allocation.iter().cloned().collect();
            leg.allocation = command.allocation;
            leg.last_error = None;
            leg.updated_at = command.confirmed_at;

            let mut operations = Vec::new();
            if collection.all_legs_confirmed() {
                collection.state = CollectionState::Completed;
                set_reservation_state(
                    &mut collection,
                    CollectionReservationState::Consumed {
                        transaction_id: command.transaction_id,
                        consumed_at: command.confirmed_at,
                    },
                );
                operations.push(Operation::Delete {
                    namespace: active_reservation_ns(),
                    key: reservation_key(collection.deposit_id(), &collection.asset)?,
                });
            } else {
                collection.state = CollectionState::InProgress;
            }
            self.commit_collection_update(&stored, &collection, ownership_conditions, operations)
                .await?;
            Ok(collection)
        })
    }

    fn fail_leg<'a>(&'a self, command: FailLeg) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            command.error.validate()?;
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            if collection.mode == CollectionMode::UtxoBatch {
                return Err(invalid_state(
                    "signed UTXO-batch collections cannot enter terminal failure",
                ));
            }
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.legs[position].state
                == (CollectionLegState::Failed {
                    transaction_id: command.transaction_id.clone(),
                })
            {
                if collection.legs[position].last_error.as_ref() == Some(&command.error) {
                    return Ok(collection);
                }
                return Err(conflict(
                    "failed collection leg was replayed with a different safe error",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if collection.state != CollectionState::InProgress {
                return Err(invalid_state(
                    "only an in-progress collection can enter terminal failure",
                ));
            }
            match &collection.legs[position].state {
                CollectionLegState::Broadcast { transaction_id }
                    if transaction_id == &command.transaction_id => {}
                CollectionLegState::Broadcast { .. } => {
                    return Err(conflict(
                        "failure transaction ID does not match broadcast leg",
                    ));
                }
                _ => {
                    return Err(invalid_state(
                        "only a broadcast collection leg can enter terminal failure",
                    ));
                }
            }
            validate_transition_time(&collection, command.failed_at)?;
            let expected_reference = LegRef {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let reference = self
                .stored_leg_reference(&command.transaction_id)
                .await?
                .map(|(reference, _)| reference)
                .ok_or_else(|| storage_error("broadcast leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            let ownership_conditions = self.require_owned_active_indexes(&collection).await?;
            collection.state = CollectionState::Failed;
            collection.last_error = Some(command.error.clone());
            collection.updated_at = command.failed_at;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Failed {
                transaction_id: command.transaction_id,
            };
            leg.last_error = Some(command.error);
            leg.updated_at = command.failed_at;
            self.commit_collection_update(&stored, &collection, ownership_conditions, Vec::new())
                .await?;
            Ok(collection)
        })
    }

    fn reorg_leg<'a>(
        &'a self,
        command: ReorgLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            command.error.validate()?;
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.mode == CollectionMode::UtxoBatch {
                return Err(invalid_state(
                    "UTXO-batch reorg must use atomic collection projection",
                ));
            }
            if collection.legs[position].state
                == (CollectionLegState::Reorged {
                    transaction_id: command.transaction_id.clone(),
                })
            {
                if collection.legs[position].last_error.as_ref() == Some(&command.error) {
                    return Ok(collection);
                }
                return Err(conflict(
                    "reorged collection leg was replayed with a different safe error",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if !matches!(
                collection.state,
                CollectionState::InProgress | CollectionState::Completed
            ) {
                return Err(invalid_state(
                    "only an in-progress or completed collection can be reorged",
                ));
            }
            match &collection.legs[position].state {
                CollectionLegState::Confirmed { transaction_id }
                    if transaction_id == &command.transaction_id => {}
                CollectionLegState::Confirmed { .. } => {
                    return Err(conflict(
                        "reorg transaction ID does not match confirmed leg",
                    ));
                }
                _ => {
                    return Err(invalid_state(
                        "only a confirmed collection leg can be reorged",
                    ));
                }
            }
            validate_transition_time(&collection, command.reorged_at)?;
            let expected_reference = LegRef {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let reference = self
                .stored_leg_reference(&command.transaction_id)
                .await?
                .map(|(reference, _)| reference)
                .ok_or_else(|| storage_error("confirmed leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }

            let reservation_key = reservation_key(collection.deposit_id(), &collection.asset)?;
            let mut conditions = Vec::new();
            let mut operations = Vec::new();
            match &collection.reservation().state {
                CollectionReservationState::Active => {
                    let reservation = self.require_owned_active_reservation(&collection).await?;
                    conditions.push(Condition::Version {
                        namespace: active_reservation_ns(),
                        key: reservation_key.clone(),
                        expected: reservation.version,
                    });
                }
                CollectionReservationState::Consumed { .. } => {
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
                            "reorged value cannot be reserved because collection {} already owns the deposit and asset",
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
                CollectionReservationState::Released { .. } => {
                    return Err(invalid_state(
                        "released collection reservation cannot be reorged without reconciliation",
                    ));
                }
            }
            collection.state = CollectionState::Reorged;
            collection.last_error = Some(command.error.clone());
            collection.updated_at = command.reorged_at;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Reorged {
                transaction_id: command.transaction_id,
            };
            leg.last_error = Some(command.error);
            leg.updated_at = command.reorged_at;
            self.commit_collection_update(&stored, &collection, conditions, operations)
                .await?;
            Ok(collection)
        })
    }
}
