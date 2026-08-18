use super::*;

impl<S> SubmissionWriter for PaymentStore<S>
where
    S: Store,
{
    fn record_signed<'a>(
        &'a self,
        command: RecordSignature,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let desired_allocations = command.allocations;
            let desired_fee_limit = command.fee_limit;
            let expected_reference = LegRef {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let desired_envelope = SignedEnvelope {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
                expected_transaction_id: command.expected_transaction_id.clone(),
                bytes: command.envelope,
                signed_at: command.signed_at,
                expires_at: command.expires_at,
            };
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.legs[position].state
                == (CollectionLegState::Signed {
                    transaction_id: command.expected_transaction_id.clone(),
                })
            {
                let existing = self
                    .stored_signed_envelope(&command.collection_id, &command.leg_id)
                    .await?
                    .map(|(envelope, _)| envelope)
                    .ok_or_else(|| storage_error("signed leg is missing its durable envelope"))?;
                let reference = self
                    .stored_leg_reference(&command.expected_transaction_id)
                    .await?
                    .map(|(reference, _)| reference)
                    .ok_or_else(|| storage_error("signed leg is missing transaction index"))?;
                if existing == desired_envelope
                    && reference == expected_reference
                    && collection.legs[position].allocations == desired_allocations
                    && collection.legs[position].planned_amount == desired_fee_limit
                {
                    return Ok(collection);
                }
                return Err(conflict(
                    "signed collection leg was replayed with different durable attribution",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if !matches!(
                collection.state,
                CollectionState::Required | CollectionState::InProgress
            ) {
                return Err(invalid_state(
                    "terminal collection cannot persist a new signed leg",
                ));
            }
            if collection.legs[position].state != CollectionLegState::Required {
                return Err(invalid_state(
                    "only a required collection leg can persist a signed envelope",
                ));
            }
            ensure_previous_legs_confirmed(&collection, position)?;
            validate_transition_time(&collection, command.signed_at)?;
            if command.expires_at <= command.signed_at {
                return Err(invalid(
                    "signed collection envelope expiry must follow signing time",
                ));
            }
            validate_transaction_for_collection(&collection, &command.expected_transaction_id)?;
            if collection.mode == CollectionMode::UtxoBatch {
                validate_allocations(&collection, &desired_allocations)?;
                if desired_fee_limit.is_some() {
                    return Err(invalid(
                        "UTXO collection must have an exact fee, not a fee limit",
                    ));
                }
            } else if !desired_allocations.is_empty() {
                return Err(invalid(
                    "account-model signed collection leg must not pre-attach attribution",
                ));
            }
            if collection.legs[position].kind == CollectionLegKind::Sweep {
                if collection.mode != CollectionMode::UtxoBatch
                    && desired_fee_limit.as_ref().is_none_or(Decimal::is_zero)
                {
                    return Err(invalid("account-model sweep requires a positive fee limit"));
                }
            } else if desired_fee_limit.is_some() {
                return Err(invalid("gas-funding leg must not record a sweep fee limit"));
            }
            if let Some((reference, _)) = self
                .stored_leg_reference(&command.expected_transaction_id)
                .await?
            {
                if reference == expected_reference {
                    return Err(storage_error(
                        "transaction index exists before its collection leg is signed",
                    ));
                }
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            if collection.reservation().state != CollectionReservationState::Active {
                return Err(invalid_state(
                    "collection must hold an active reservation before signing",
                ));
            }
            let mut ownership_conditions = if collection.mode == CollectionMode::UtxoBatch {
                self.require_owned_active_indexes(&collection).await?
            } else {
                let reservation = self.require_owned_active_reservation(&collection).await?;
                vec![Condition::Version {
                    namespace: active_reservation_ns(),
                    key: reservation_key(collection.deposit_id(), &collection.asset)?,
                    expected: reservation.version,
                }]
            };
            let next_attempt = collection
                .attempt_count
                .checked_add(1)
                .ok_or_else(|| invalid("collection attempt counter is exhausted"))?;
            let next_leg_attempt = collection.legs[position]
                .attempt_count
                .checked_add(1)
                .ok_or_else(|| invalid("collection leg attempt counter is exhausted"))?;
            collection.state = CollectionState::InProgress;
            collection.attempt_count = next_attempt;
            collection.last_error = None;
            collection.updated_at = command.signed_at;
            let leg = &mut collection.legs[position];
            if desired_fee_limit.is_some() {
                leg.planned_amount = desired_fee_limit;
            }
            leg.state = CollectionLegState::Signed {
                transaction_id: command.expected_transaction_id.clone(),
            };
            leg.watch_id = None;
            leg.attempt_count = next_leg_attempt;
            leg.allocation =
                (desired_allocations.len() == 1).then(|| desired_allocations[0].clone());
            leg.allocations = desired_allocations.clone();
            leg.last_error = None;
            leg.updated_at = command.signed_at;
            let envelope_key = envelope_key(&command.collection_id, &command.leg_id)?;
            let transaction_key = transaction_key(&command.expected_transaction_id)?;
            ownership_conditions.extend([
                Condition::Missing {
                    namespace: signed_envelope_ns(),
                    key: envelope_key.clone(),
                },
                Condition::Missing {
                    namespace: transaction_leg_ns(),
                    key: transaction_key.clone(),
                },
            ]);
            let result = self
                .commit_collection_update(
                    &stored,
                    &collection,
                    ownership_conditions,
                    vec![
                        Operation::Put {
                            namespace: signed_envelope_ns(),
                            key: envelope_key,
                            value: encode(&EnvelopeRecord::from(&desired_envelope))?,
                        },
                        // Persist transaction attribution before any broadcast
                        // attempt. If the node accepts the transaction but its
                        // response is lost, IX facts can still be classified as
                        // this collection leg while PS recovers the receipt.
                        Operation::Put {
                            namespace: transaction_leg_ns(),
                            key: transaction_key,
                            value: encode(&LegIndex {
                                version: RECORD_VERSION,
                                collection_id: command.collection_id.0.clone(),
                                leg_id: command.leg_id.0.clone(),
                            })?,
                        },
                    ],
                )
                .await;
            match result {
                Ok(()) => Ok(collection),
                Err(error) if error.kind == DepositErrorKind::Conflict => {
                    self.replay_signed_conflict(
                        &command.collection_id,
                        &command.leg_id,
                        &desired_envelope,
                        &expected_reference,
                        &desired_allocations,
                        error,
                    )
                    .await
                }
                Err(error) => Err(error),
            }
        })
    }

    fn accept_broadcast<'a>(
        &'a self,
        command: AcceptBroadcast,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            let expected_reference = LegRef {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            if collection.legs[position].state.transaction_id() == Some(&command.transaction_id)
                && !matches!(
                    collection.legs[position].state,
                    CollectionLegState::Required | CollectionLegState::Signed { .. }
                )
            {
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
                let envelope_present = self
                    .stored_signed_envelope(&command.collection_id, &command.leg_id)
                    .await?
                    .is_some();
                if !envelope_present {
                    return Err(storage_error(
                        "accepted broadcast lost its durable signed envelope",
                    ));
                }
                return Ok(collection);
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if collection.state != CollectionState::InProgress {
                return Err(invalid_state(
                    "only an in-progress collection can accept broadcast",
                ));
            }
            match &collection.legs[position].state {
                CollectionLegState::Signed { transaction_id }
                    if transaction_id == &command.transaction_id => {}
                CollectionLegState::Signed { .. } => {
                    return Err(conflict(
                        "broadcast transaction ID does not match signed envelope hash",
                    ));
                }
                _ => {
                    return Err(invalid_state(
                        "only a signed collection leg can accept broadcast",
                    ));
                }
            }
            validate_transition_time(&collection, command.accepted_at)?;
            validate_transaction_for_collection(&collection, &command.transaction_id)?;
            let (envelope, envelope_stored) = self
                .stored_signed_envelope(&command.collection_id, &command.leg_id)
                .await?
                .ok_or_else(|| storage_error("signed leg is missing its durable envelope"))?;
            if envelope.expected_transaction_id != command.transaction_id {
                return Err(conflict(
                    "broadcast transaction ID does not match durable signed envelope hash",
                ));
            }
            // `expires_at` is a retention/alerting hint. Once PS has durably
            // recorded exact signed bytes it must be able to recover the
            // broadcast-response-loss window without silently re-signing a
            // different transaction.
            let (reference, reference_stored) = self
                .stored_leg_reference(&command.transaction_id)
                .await?
                .ok_or_else(|| storage_error("signed leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            let mut ownership_conditions = if collection.mode == CollectionMode::UtxoBatch {
                self.require_owned_active_indexes(&collection).await?
            } else {
                let reservation = self.require_owned_active_reservation(&collection).await?;
                vec![Condition::Version {
                    namespace: active_reservation_ns(),
                    key: reservation_key(collection.deposit_id(), &collection.asset)?,
                    expected: reservation.version,
                }]
            };
            collection.state = CollectionState::InProgress;
            collection.updated_at = command.accepted_at;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Broadcast {
                transaction_id: command.transaction_id.clone(),
            };
            leg.updated_at = command.accepted_at;
            let envelope_key = envelope_key(&command.collection_id, &command.leg_id)?;
            ownership_conditions.extend([
                Condition::Version {
                    namespace: signed_envelope_ns(),
                    key: envelope_key.clone(),
                    expected: envelope_stored.version,
                },
                Condition::Version {
                    namespace: transaction_leg_ns(),
                    key: transaction_key(&command.transaction_id)?,
                    expected: reference_stored.version,
                },
            ]);
            self.commit_collection_update(&stored, &collection, ownership_conditions, Vec::new())
                .await?;
            Ok(collection)
        })
    }

    fn attach_watch<'a>(
        &'a self,
        command: AttachWatch,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.legs[position].watch_id.as_ref() == Some(&command.watch_id) {
                if collection.legs[position].state.transaction_id()
                    == command.expected.leg_state.transaction_id()
                {
                    return Ok(collection);
                }
                return Err(conflict(
                    "IX watch is attached to a different transaction revision",
                ));
            }
            if collection.legs[position].watch_id.is_some() {
                return Err(conflict(
                    "collection leg is already attached to another IX watch",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if collection.state != CollectionState::InProgress {
                return Err(invalid_state(
                    "only an in-progress collection can register an IX watch",
                ));
            }
            let transaction_id = match &collection.legs[position].state {
                CollectionLegState::Broadcast { transaction_id } => transaction_id.clone(),
                _ => {
                    return Err(invalid_state(
                        "IX watch can only attach to a broadcast collection leg",
                    ));
                }
            };
            validate_transition_time(&collection, command.updated_at)?;
            let expected_reference = LegRef {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let reference = self
                .stored_leg_reference(&transaction_id)
                .await?
                .map(|(reference, _)| reference)
                .ok_or_else(|| storage_error("broadcast leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            let ownership_conditions = self.require_owned_active_indexes(&collection).await?;
            collection.updated_at = command.updated_at;
            collection.legs[position].watch_id = Some(command.watch_id);
            collection.legs[position].updated_at = command.updated_at;
            self.commit_collection_update(&stored, &collection, ownership_conditions, Vec::new())
                .await?;
            Ok(collection)
        })
    }
}

impl<S> PaymentStore<S>
where
    S: Store,
{
    async fn replay_signed_conflict(
        &self,
        collection_id: &CollectionId,
        leg_id: &LegId,
        desired_envelope: &SignedEnvelope,
        expected_reference: &LegRef,
        desired_allocations: &[CollectionAllocation],
        conflict_error: DepositError,
    ) -> Result<Collection, DepositError> {
        let current = self.required_collection_record(collection_id).await?.0;
        let current_position = find_leg(&current, leg_id)?;
        let envelope = self
            .stored_signed_envelope(collection_id, leg_id)
            .await?
            .map(|(envelope, _)| envelope);
        let reference = self
            .stored_leg_reference(&desired_envelope.expected_transaction_id)
            .await?
            .map(|(reference, _)| reference);
        let replayed = current.legs[current_position].state
            == (CollectionLegState::Signed {
                transaction_id: desired_envelope.expected_transaction_id.clone(),
            })
            && envelope.as_ref() == Some(desired_envelope)
            && reference.as_ref() == Some(expected_reference)
            && current.legs[current_position].allocations == desired_allocations;
        if replayed {
            Ok(current)
        } else {
            Err(conflict_error)
        }
    }
}
