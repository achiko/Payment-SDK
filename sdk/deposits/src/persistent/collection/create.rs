use super::*;

impl<S> CollectionCreator for PaymentStore<S>
where
    S: Store,
{
    fn create_or_replay_collection<'a>(
        &'a self,
        command: CollectionPlan,
    ) -> BoxFuture<'a, Result<CreateCollectionOutcome, DepositError>> {
        Box::pin(async move {
            let expected = Collection::from_create(&command)?;
            if let Some(collection) = self.validate_create_replay(&expected).await? {
                return Ok(CreateCollectionOutcome::Replayed { collection });
            }

            let collection_key = key_text(&expected.id.0);
            let job_key = key_text(&expected.job_id.0);
            let deposit_key = deposit_collection_key(expected.deposit_id(), &expected.id)?;
            let reservation_key = reservation_key(expected.deposit_id(), &expected.asset)?;
            let (eligibility_condition, eligibility_operation) = self
                .collection_eligibility_generation_change(expected.deposit_id(), &expected.asset)
                .await?;
            let index = IndexRecord {
                version: RECORD_VERSION,
                collection_id: expected.id.0.clone(),
            };
            let result = self
                .storage()
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: collection_ns(),
                            key: collection_key.clone(),
                        },
                        Condition::Missing {
                            namespace: collection_job_ns(),
                            key: job_key.clone(),
                        },
                        Condition::Missing {
                            namespace: deposit_collection_ns(),
                            key: deposit_key.clone(),
                        },
                        Condition::Missing {
                            namespace: active_reservation_ns(),
                            key: reservation_key.clone(),
                        },
                        eligibility_condition,
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: collection_ns(),
                            key: collection_key,
                            value: encode(&StoredRecord::from(&expected))?,
                        },
                        Operation::Put {
                            namespace: collection_job_ns(),
                            key: job_key,
                            value: encode(&index)?,
                        },
                        Operation::Put {
                            namespace: deposit_collection_ns(),
                            key: deposit_key,
                            value: encode(&index)?,
                        },
                        Operation::Put {
                            namespace: active_reservation_ns(),
                            key: reservation_key,
                            value: encode(&index)?,
                        },
                        eligibility_operation,
                    ],
                })
                .await
                .map_err(map_storage);
            match result {
                Ok(_) => Ok(CreateCollectionOutcome::Created {
                    collection: expected,
                }),
                Err(error) if error.kind == DepositErrorKind::Conflict => self
                    .validate_create_replay(&expected)
                    .await?
                    .map(|collection| CreateCollectionOutcome::Replayed { collection })
                    .ok_or(error),
                Err(error) => Err(error),
            }
        })
    }

    fn create_or_replay_utxo_batch<'a>(
        &'a self,
        command: CreateBatch,
    ) -> BoxFuture<'a, Result<CreateCollectionOutcome, DepositError>> {
        Box::pin(async move {
            let expected = Collection::from_batch(&command)?;
            self.validate_utxo_batch_job_and_participants(&command)
                .await?;
            if let Some(collection) = self.validate_create_replay(&expected).await? {
                return Ok(CreateCollectionOutcome::Replayed { collection });
            }

            let collection_key = key_text(&expected.id.0);
            let job_key = key_text(&expected.job_id.0);
            let index = IndexRecord {
                version: RECORD_VERSION,
                collection_id: expected.id.0.clone(),
            };
            let mut conditions = vec![
                Condition::Missing {
                    namespace: collection_ns(),
                    key: collection_key.clone(),
                },
                Condition::Missing {
                    namespace: collection_job_ns(),
                    key: job_key.clone(),
                },
            ];
            let mut operations = vec![
                Operation::Put {
                    namespace: collection_ns(),
                    key: collection_key,
                    value: encode(&StoredRecord::from(&expected))?,
                },
                Operation::Put {
                    namespace: collection_job_ns(),
                    key: job_key,
                    value: encode(&index)?,
                },
            ];
            for participant in &expected.participants {
                let command_participant = command
                    .participants
                    .iter()
                    .find(|candidate| candidate.deposit_id == participant.reservation.deposit_id)
                    .ok_or_else(|| {
                        storage_error("UTXO-batch command participant disappeared after validation")
                    })?;
                let deposit_key =
                    deposit_collection_key(&participant.reservation.deposit_id, &expected.id)?;
                let active_key = reservation_key(
                    &participant.reservation.deposit_id,
                    &participant.reservation.asset,
                )?;
                let (eligibility_condition, eligibility_operation) = self
                    .collection_eligibility_generation_change(
                        &participant.reservation.deposit_id,
                        &participant.reservation.asset,
                    )
                    .await?;
                let ledger_head_condition = self
                    .expected_ledger_head_condition(
                        &participant.reservation.deposit_id,
                        &command_participant.expected_ledger_head,
                    )
                    .await?;
                conditions.extend([
                    Condition::Missing {
                        namespace: deposit_collection_ns(),
                        key: deposit_key.clone(),
                    },
                    Condition::Missing {
                        namespace: active_reservation_ns(),
                        key: active_key.clone(),
                    },
                    ledger_head_condition,
                    eligibility_condition,
                ]);
                operations.extend([
                    Operation::Put {
                        namespace: deposit_collection_ns(),
                        key: deposit_key,
                        value: encode(&index)?,
                    },
                    Operation::Put {
                        namespace: active_reservation_ns(),
                        key: active_key,
                        value: encode(&index)?,
                    },
                    eligibility_operation,
                ]);
                Self::append_spend_resources(
                    &mut conditions,
                    &mut operations,
                    participant,
                    &index,
                )?;
            }
            let result = self
                .storage()
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await
                .map_err(map_storage);
            match result {
                Ok(_) => Ok(CreateCollectionOutcome::Created {
                    collection: expected,
                }),
                Err(error) if error.kind == DepositErrorKind::Conflict => self
                    .validate_create_replay(&expected)
                    .await?
                    .map(|collection| CreateCollectionOutcome::Replayed { collection })
                    .ok_or(error),
                Err(error) => Err(error),
            }
        })
    }
}

impl<S> PaymentStore<S>
where
    S: Store,
{
    fn append_spend_resources(
        conditions: &mut Vec<Condition>,
        operations: &mut Vec<Operation>,
        participant: &CollectionParticipant,
        index: &IndexRecord,
    ) -> Result<(), DepositError> {
        for resource in &participant.spend_resources {
            let resource_key = spend_resource_key(&resource.id)?;
            conditions.push(Condition::Missing {
                namespace: active_spend_resource_ns(),
                key: resource_key.clone(),
            });
            operations.push(Operation::Put {
                namespace: active_spend_resource_ns(),
                key: resource_key,
                value: encode(index)?,
            });
        }
        Ok(())
    }
}
