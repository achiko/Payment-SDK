use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn commit_collection_update(
        &self,
        current_stored: &StoredValue,
        next: &Collection,
        mut conditions: Vec<Condition>,
        mut operations: Vec<Operation>,
    ) -> Result<(), DepositError> {
        conditions.insert(
            0,
            Condition::Version {
                namespace: collection_ns(),
                key: key_text(&next.id.0),
                expected: current_stored.version,
            },
        );
        operations.push(Operation::Put {
            namespace: collection_ns(),
            key: key_text(&next.id.0),
            value: encode(&StoredRecord::from(next))?,
        });
        self.storage()
            .commit(WriteBatch {
                conditions,
                operations,
            })
            .await
            .map_err(map_storage)?;
        Ok(())
    }

    pub(crate) async fn prepare_utxo_batch_projection_transition(
        &self,
        collection_id: &CollectionId,
        leg_id: &LegId,
        expected: &TransitionGuard,
        transaction_id: &TransactionRef,
        transition: &UtxoBatchProjectionTransition,
    ) -> Result<BatchTransition, DepositError> {
        let (mut collection, stored) = self.required_collection_record(collection_id).await?;
        if collection.mode != CollectionMode::UtxoBatch {
            return Err(invalid(
                "atomic UTXO-batch projection references another collection mode",
            ));
        }
        let position = find_leg(&collection, leg_id)?;
        validate_guard(&collection, &collection.legs[position], expected)?;
        validate_transaction_for_collection(&collection, transaction_id)?;
        if collection.legs[position].watch_id.is_none() {
            return Err(invalid_state(
                "UTXO-batch transition requires durable IX watch registration",
            ));
        }
        let reference_key = transaction_key(transaction_id)?;
        let (reference, reference_stored) = self
            .stored_leg_reference(transaction_id)
            .await?
            .ok_or_else(|| storage_error("UTXO-batch leg is missing transaction index"))?;
        if reference
            != (LegRef {
                collection_id: collection_id.clone(),
                leg_id: leg_id.clone(),
            })
        {
            return Err(conflict(
                "UTXO-batch transaction ID belongs to another collection leg",
            ));
        }
        let envelope_key = envelope_key(collection_id, leg_id)?;
        let (envelope, envelope_stored) = self
            .stored_signed_envelope(collection_id, leg_id)
            .await?
            .ok_or_else(|| storage_error("UTXO-batch leg lost its durable signed bytes"))?;
        if &envelope.expected_transaction_id != transaction_id {
            return Err(conflict(
                "UTXO-batch signed bytes identify another transaction",
            ));
        }
        let mut conditions = self.require_owned_active_indexes(&collection).await?;
        conditions.extend([
            Condition::Version {
                namespace: collection_ns(),
                key: key_text(&collection.id.0),
                expected: stored.version,
            },
            Condition::Version {
                namespace: transaction_leg_ns(),
                key: reference_key,
                expected: reference_stored.version,
            },
            Condition::Version {
                namespace: signed_envelope_ns(),
                key: envelope_key,
                expected: envelope_stored.version,
            },
        ]);

        match transition {
            UtxoBatchProjectionTransition::Reincluded { included_at } => {
                validate_transition_time(&collection, *included_at)?;
                match &collection.legs[position].state {
                    CollectionLegState::Reorged {
                        transaction_id: current,
                    } if current == transaction_id => {}
                    CollectionLegState::Reorged { .. } => {
                        return Err(conflict(
                            "UTXO-batch re-inclusion transaction differs from reorged leg",
                        ));
                    }
                    _ => {
                        return Err(invalid_state(
                            "only a reorged UTXO-batch leg can be canonically re-included",
                        ));
                    }
                }
                collection.state = CollectionState::InProgress;
                collection.last_error = None;
                collection.updated_at = *included_at;
                let leg = &mut collection.legs[position];
                leg.state = CollectionLegState::Broadcast {
                    transaction_id: transaction_id.clone(),
                };
                leg.last_error = None;
                leg.updated_at = *included_at;
                set_reservation_state(&mut collection, CollectionReservationState::Active);
            }
            UtxoBatchProjectionTransition::Confirmed {
                allocations,
                confirmed_at,
            } => {
                validate_transition_time(&collection, *confirmed_at)?;
                validate_allocations(&collection, allocations)?;
                if collection.legs[position].allocations != *allocations {
                    return Err(conflict(
                        "UTXO-batch confirmation attribution differs from signed-stage allocation",
                    ));
                }
                match &collection.legs[position].state {
                    CollectionLegState::Broadcast {
                        transaction_id: current,
                    }
                    | CollectionLegState::Reorged {
                        transaction_id: current,
                    } if current == transaction_id => {}
                    CollectionLegState::Broadcast { .. } | CollectionLegState::Reorged { .. } => {
                        return Err(conflict(
                            "UTXO-batch confirmation transaction differs from durable leg",
                        ));
                    }
                    _ => {
                        return Err(invalid_state(
                            "only broadcast or same-transaction reorged UTXO batch can confirm",
                        ));
                    }
                }
                collection.state = CollectionState::Completed;
                collection.last_error = None;
                collection.updated_at = *confirmed_at;
                let leg = &mut collection.legs[position];
                leg.state = CollectionLegState::Confirmed {
                    transaction_id: transaction_id.clone(),
                };
                leg.last_error = None;
                leg.updated_at = *confirmed_at;
                set_reservation_state(
                    &mut collection,
                    CollectionReservationState::Consumed {
                        transaction_id: transaction_id.clone(),
                        consumed_at: *confirmed_at,
                    },
                );
            }
            UtxoBatchProjectionTransition::Reorged { error, reorged_at } => {
                error.validate()?;
                validate_transition_time(&collection, *reorged_at)?;
                match &collection.legs[position].state {
                    CollectionLegState::Confirmed {
                        transaction_id: current,
                    } if current == transaction_id => {}
                    CollectionLegState::Confirmed { .. } => {
                        return Err(conflict(
                            "UTXO-batch reorg transaction differs from confirmed leg",
                        ));
                    }
                    _ => {
                        return Err(invalid_state(
                            "only a confirmed UTXO-batch leg can be reorged",
                        ));
                    }
                }
                collection.state = CollectionState::Reorged;
                collection.last_error = Some(error.clone());
                collection.updated_at = *reorged_at;
                let leg = &mut collection.legs[position];
                leg.state = CollectionLegState::Reorged {
                    transaction_id: transaction_id.clone(),
                };
                leg.last_error = Some(error.clone());
                leg.updated_at = *reorged_at;
                set_reservation_state(&mut collection, CollectionReservationState::Active);
            }
        }
        collection.validate_persisted()?;
        let operation = Operation::Put {
            namespace: collection_ns(),
            key: key_text(&collection.id.0),
            value: encode(&StoredRecord::from(&collection))?,
        };
        Ok(BatchTransition {
            collection,
            conditions,
            operations: vec![operation],
        })
    }

    pub(crate) async fn validate_utxo_batch_projection_replay(
        &self,
        collection_id: &CollectionId,
        leg_id: &LegId,
        transaction_id: &TransactionRef,
        transition: &UtxoBatchProjectionTransition,
    ) -> Result<Collection, DepositError> {
        let collection = self.required_collection_record(collection_id).await?.0;
        if collection.mode != CollectionMode::UtxoBatch {
            return Err(conflict(
                "projection retry references another collection mode",
            ));
        }
        let position = find_leg(&collection, leg_id)?;
        let matches = match transition {
            UtxoBatchProjectionTransition::Reincluded { included_at } => {
                collection.state == CollectionState::InProgress
                    && collection.updated_at == *included_at
                    && collection.legs[position].state
                        == (CollectionLegState::Broadcast {
                            transaction_id: transaction_id.clone(),
                        })
            }
            UtxoBatchProjectionTransition::Confirmed {
                allocations,
                confirmed_at,
            } => {
                collection.state == CollectionState::Completed
                    && collection.updated_at == *confirmed_at
                    && collection.legs[position].state
                        == (CollectionLegState::Confirmed {
                            transaction_id: transaction_id.clone(),
                        })
                    && collection.legs[position].allocations == *allocations
            }
            UtxoBatchProjectionTransition::Reorged { error, reorged_at } => {
                collection.state == CollectionState::Reorged
                    && collection.updated_at == *reorged_at
                    && collection.legs[position].state
                        == (CollectionLegState::Reorged {
                            transaction_id: transaction_id.clone(),
                        })
                    && collection.legs[position].last_error.as_ref() == Some(error)
            }
        };
        if !matches {
            return Err(conflict(
                "UTXO-batch projection retry changed its collection transition",
            ));
        }
        self.require_owned_active_indexes(&collection).await?;
        let envelope = self
            .stored_signed_envelope(collection_id, leg_id)
            .await?
            .ok_or_else(|| storage_error("UTXO-batch projection retry lost signed bytes"))?
            .0;
        if &envelope.expected_transaction_id != transaction_id {
            return Err(conflict(
                "UTXO-batch projection retry references different signed bytes",
            ));
        }
        Ok(collection)
    }
}
