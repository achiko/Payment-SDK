use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn active_reservation_record(
        &self,
        collection: &Collection,
        participant: &CollectionParticipant,
    ) -> Result<Option<(CollectionId, StoredValue)>, DepositError> {
        self.collection_index(
            active_reservation_ns(),
            &reservation_key(&participant.reservation.deposit_id, &collection.asset)?,
        )
        .await
    }

    pub(super) async fn require_owned_active_reservation(
        &self,
        collection: &Collection,
    ) -> Result<StoredValue, DepositError> {
        let primary = collection
            .participants
            .first()
            .ok_or_else(|| storage_error("collection has no participant"))?;
        let (owner, stored) = self
            .active_reservation_record(collection, primary)
            .await?
            .ok_or_else(|| storage_error("active collection reservation index is missing"))?;
        if owner != collection.id {
            return Err(conflict(
                "deposit and asset are reserved by another collection",
            ));
        }
        Ok(stored)
    }

    pub(super) async fn require_owned_active_indexes(
        &self,
        collection: &Collection,
    ) -> Result<Vec<Condition>, DepositError> {
        let mut conditions = Vec::new();
        for participant in &collection.participants {
            let (owner, stored) = self
                .active_reservation_record(collection, participant)
                .await?
                .ok_or_else(|| storage_error("active collection reservation index is missing"))?;
            if owner != collection.id {
                return Err(conflict(
                    "deposit and asset are reserved by another collection",
                ));
            }
            conditions.push(Condition::Version {
                namespace: active_reservation_ns(),
                key: reservation_key(
                    &participant.reservation.deposit_id,
                    &participant.reservation.asset,
                )?,
                expected: stored.version,
            });
            for resource in &participant.spend_resources {
                let key = spend_resource_key(&resource.id)?;
                let (owner, stored) = self
                    .collection_index(active_spend_resource_ns(), &key)
                    .await?
                    .ok_or_else(|| storage_error("active spend-resource index is missing"))?;
                if owner != collection.id {
                    return Err(conflict(
                        "exact spend resource is reserved by another collection",
                    ));
                }
                conditions.push(Condition::Version {
                    namespace: active_spend_resource_ns(),
                    key,
                    expected: stored.version,
                });
            }
        }
        Ok(conditions)
    }

    pub(crate) async fn prepare_deposit_close_reservation_fence(
        &self,
        deposit_id: &DepositId,
        asset: &AssetId,
    ) -> Result<CloseFence, DepositError> {
        let reservation_key = reservation_key(deposit_id, asset)?;
        let Some((collection_id, reservation_stored)) = self
            .collection_index(active_reservation_ns(), &reservation_key)
            .await?
        else {
            return Ok(CloseFence {
                conditions: vec![Condition::Missing {
                    namespace: active_reservation_ns(),
                    key: reservation_key,
                }],
                operations: Vec::new(),
            });
        };
        let (collection, collection_stored) = self
            .stored_collection_record(&collection_id)
            .await?
            .ok_or_else(|| storage_error("retained reservation index is dangling"))?;
        let participant = collection.participant(deposit_id).ok_or_else(|| {
            storage_error("retained reservation index points to a non-participant collection")
        })?;
        if &participant.reservation.asset != asset {
            return Err(storage_error(
                "retained reservation index asset differs from its collection participant",
            ));
        }
        match participant.reservation.state {
            CollectionReservationState::Active => Err(invalid_state(
                "deposit cannot close while a collection reservation is active",
            )),
            CollectionReservationState::Consumed { .. }
                if collection.mode == CollectionMode::UtxoBatch =>
            {
                let index = IndexRecord {
                    version: RECORD_VERSION,
                    collection_id: collection.id.0.clone(),
                };
                Ok(CloseFence {
                    conditions: vec![
                        Condition::Version {
                            namespace: active_reservation_ns(),
                            key: reservation_key.clone(),
                            expected: reservation_stored.version,
                        },
                        Condition::Version {
                            namespace: collection_ns(),
                            key: key_text(&collection.id.0),
                            expected: collection_stored.version,
                        },
                    ],
                    // Rewriting the retained owner serializes close against a
                    // UTXO reorg transition that already read the same index.
                    // The index value and exact-resource ownership do not
                    // change, and a retried reorg can still reactivate them.
                    operations: vec![Operation::Put {
                        namespace: active_reservation_ns(),
                        key: reservation_key,
                        value: encode(&index)?,
                    }],
                })
            }
            CollectionReservationState::Consumed { .. } => Err(storage_error(
                "account-model collection retained a consumed reservation index",
            )),
            CollectionReservationState::Released { .. } => Err(storage_error(
                "released collection still owns a retained reservation index",
            )),
        }
    }
}
