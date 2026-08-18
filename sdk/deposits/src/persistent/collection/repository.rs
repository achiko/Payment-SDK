use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn validate_utxo_batch_job_and_participants(
        &self,
        command: &CreateBatch,
    ) -> Result<(), DepositError> {
        let job = self
            .job(&command.job_id)
            .await?
            .ok_or_else(|| not_found("UTXO-batch collection job was not found"))?;
        let participant_deposit_ids = command
            .participants
            .iter()
            .map(|participant| participant.deposit_id.clone())
            .collect::<Vec<_>>();
        let payload = match &job.payload {
            JobPayload::CreateBatch(payload) => payload,
            _ => {
                return Err(conflict(
                    "UTXO-batch collection requires a matching multi-deposit create job",
                ));
            }
        };
        if job.resource != JobResource::Collection(command.id.clone())
            || payload.collection_id != command.id
            || payload.deposit_ids != participant_deposit_ids
            || job.policy != command.policy
            || job.user_id
                != command
                    .participants
                    .first()
                    .ok_or_else(|| invalid("UTXO-batch collection has no participants"))?
                    .user_id
        {
            return Err(conflict(
                "UTXO-batch collection differs from its durable job association",
            ));
        }
        for participant in &command.participants {
            let deposit = self
                .deposit(&participant.deposit_id)
                .await?
                .ok_or_else(|| not_found("UTXO-batch participant deposit was not found"))?;
            if deposit.user_id != participant.user_id || deposit.asset != command.asset {
                return Err(conflict(
                    "UTXO-batch participant differs from its durable deposit",
                ));
            }
            let user = self
                .user(&participant.user_id)
                .await?
                .ok_or_else(|| storage_error("UTXO-batch participant user is missing"))?;
            if user.owner != job.user_owner {
                return Err(conflict(
                    "UTXO-batch participant belongs to another authenticated owner",
                ));
            }
        }
        Ok(())
    }

    pub(crate) async fn collection_eligibility_generation_change(
        &self,
        deposit_id: &DepositId,
        asset: &AssetId,
    ) -> Result<(Condition, Operation), DepositError> {
        let key = reservation_key(deposit_id, asset)?;
        let stored = self
            .storage()
            .get(&collection_eligibility_generation_ns(), &key)
            .await
            .map_err(map_storage)?;
        if let Some(stored) = &stored {
            let record: IndexRecord = decode(stored)?;
            ensure_version(record.version)?;
            if record.collection_id != deposit_id.0 {
                return Err(storage_error(
                    "collection eligibility generation belongs to another deposit",
                ));
            }
        }
        let condition = stored.map_or_else(
            || Condition::Missing {
                namespace: collection_eligibility_generation_ns(),
                key: key.clone(),
            },
            |stored| Condition::Version {
                namespace: collection_eligibility_generation_ns(),
                key: key.clone(),
                expected: stored.version,
            },
        );
        let operation = Operation::Put {
            namespace: collection_eligibility_generation_ns(),
            key,
            value: encode(&IndexRecord {
                version: RECORD_VERSION,
                collection_id: deposit_id.0.clone(),
            })?,
        };
        Ok((condition, operation))
    }

    pub(super) async fn stored_collection_record(
        &self,
        id: &CollectionId,
    ) -> Result<Option<(Collection, StoredValue)>, DepositError> {
        self.storage()
            .get(&collection_ns(), &key_text(&id.0))
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let version = stored
                    .value
                    .0
                    .get(..2)
                    .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
                    .map(u16::from_be_bytes)
                    .ok_or_else(|| storage_error("PS collection record is truncated"))?;
                let collection = match version {
                    COLLECTION_RECORD_VERSION => decode::<StoredRecord>(&stored)?.try_into()?,
                    _ => {
                        return Err(storage_error(format!(
                            "unsupported PS collection record version {version}"
                        )));
                    }
                };
                Ok((collection, stored))
            })
            .transpose()
    }

    pub(super) async fn required_collection_record(
        &self,
        id: &CollectionId,
    ) -> Result<(Collection, StoredValue), DepositError> {
        self.stored_collection_record(id)
            .await?
            .ok_or_else(|| not_found("collection was not found"))
    }

    pub(super) async fn collection_index(
        &self,
        namespace: Namespace,
        key: &Key,
    ) -> Result<Option<(CollectionId, StoredValue)>, DepositError> {
        self.storage()
            .get(&namespace, key)
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: IndexRecord = decode(&stored)?;
                ensure_version(record.version)?;
                Ok((CollectionId(record.collection_id), stored))
            })
            .transpose()
    }

    pub(super) async fn stored_leg_reference(
        &self,
        transaction_id: &TransactionRef,
    ) -> Result<Option<(LegRef, StoredValue)>, DepositError> {
        self.storage()
            .get(&transaction_leg_ns(), &transaction_key(transaction_id)?)
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: LegIndex = decode(&stored)?;
                ensure_version(record.version)?;
                Ok((
                    LegRef {
                        collection_id: CollectionId(record.collection_id),
                        leg_id: LegId(record.leg_id),
                    },
                    stored,
                ))
            })
            .transpose()
    }

    pub(super) async fn stored_signed_envelope(
        &self,
        collection_id: &CollectionId,
        leg_id: &LegId,
    ) -> Result<Option<(SignedEnvelope, StoredValue)>, DepositError> {
        self.storage()
            .get(&signed_envelope_ns(), &envelope_key(collection_id, leg_id)?)
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: EnvelopeRecord = decode(&stored)?;
                Ok((record.try_into()?, stored))
            })
            .transpose()
    }

    pub(super) async fn validate_create_replay(
        &self,
        expected: &Collection,
    ) -> Result<Option<Collection>, DepositError> {
        let by_id = self.stored_collection_record(&expected.id).await?;
        let job_key = key_text(&expected.job_id.0);
        let by_job = self.collection_index(collection_job_ns(), &job_key).await?;
        if let Some((collection, _)) = &by_id {
            if collection != expected {
                return Err(conflict(
                    "collection ID was reused with a different aggregate",
                ));
            }
        }
        if let Some((indexed_id, _)) = &by_job {
            if indexed_id != &expected.id {
                return Err(conflict(
                    "collection job is already associated with another collection",
                ));
            }
            let indexed = self
                .stored_collection_record(indexed_id)
                .await?
                .map(|(collection, _)| collection)
                .ok_or_else(|| storage_error("collection job index is dangling"))?;
            if &indexed != expected {
                return Err(conflict(
                    "collection job was replayed with a different aggregate",
                ));
            }
        }
        let Some((collection, _)) = by_id else {
            if by_job.is_some() {
                return Err(storage_error("collection job index is dangling"));
            }
            for participant in &expected.participants {
                if let Some((owner, _)) = self
                    .collection_index(
                        active_reservation_ns(),
                        &reservation_key(
                            &participant.reservation.deposit_id,
                            &participant.reservation.asset,
                        )?,
                    )
                    .await?
                {
                    return Err(conflict(format!(
                        "deposit and asset are already reserved by collection {}",
                        owner.0
                    )));
                }
                for resource in &participant.spend_resources {
                    if let Some((owner, _)) = self
                        .collection_index(
                            active_spend_resource_ns(),
                            &spend_resource_key(&resource.id)?,
                        )
                        .await?
                    {
                        return Err(conflict(format!(
                            "exact spend resource is already reserved by collection {}",
                            owner.0
                        )));
                    }
                }
            }
            return Ok(None);
        };

        let job_index = by_job.ok_or_else(|| storage_error("collection job index is missing"))?;
        if job_index.0 != expected.id {
            return Err(storage_error(
                "collection job index points to another aggregate",
            ));
        }
        for participant in &expected.participants {
            let deposit_index = self
                .collection_index(
                    deposit_collection_ns(),
                    &deposit_collection_key(&participant.reservation.deposit_id, &expected.id)?,
                )
                .await?
                .ok_or_else(|| storage_error("collection deposit index is missing"))?;
            if deposit_index.0 != expected.id {
                return Err(storage_error(
                    "collection deposit index points to another aggregate",
                ));
            }
            let reservation_index = self
                .collection_index(
                    active_reservation_ns(),
                    &reservation_key(
                        &participant.reservation.deposit_id,
                        &participant.reservation.asset,
                    )?,
                )
                .await?
                .ok_or_else(|| storage_error("active collection reservation index is missing"))?;
            if reservation_index.0 != expected.id {
                return Err(conflict(
                    "deposit and asset are reserved by another collection",
                ));
            }
            for resource in &participant.spend_resources {
                let resource_index = self
                    .collection_index(
                        active_spend_resource_ns(),
                        &spend_resource_key(&resource.id)?,
                    )
                    .await?
                    .ok_or_else(|| storage_error("active spend-resource index is missing"))?;
                if resource_index.0 != expected.id {
                    return Err(conflict(
                        "exact spend resource is reserved by another collection",
                    ));
                }
            }
        }
        Ok(Some(collection))
    }
}
