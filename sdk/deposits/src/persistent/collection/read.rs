use super::*;

impl<S> CollectionReader for PaymentStore<S>
where
    S: Store,
{
    fn collection<'a>(
        &'a self,
        id: &'a CollectionId,
    ) -> BoxFuture<'a, Result<Option<Collection>, DepositError>> {
        Box::pin(async move {
            Ok(self
                .stored_collection_record(id)
                .await?
                .map(|(collection, _)| collection))
        })
    }

    fn active_collection_for<'a>(
        &'a self,
        deposit_id: &'a DepositId,
        asset: &'a AssetId,
    ) -> BoxFuture<'a, Result<Option<Collection>, DepositError>> {
        Box::pin(async move {
            let Some((collection_id, _)) = self
                .collection_index(
                    active_reservation_ns(),
                    &reservation_key(deposit_id, asset)?,
                )
                .await?
            else {
                return Ok(None);
            };
            let collection = self
                .stored_collection_record(&collection_id)
                .await?
                .map(|(collection, _)| collection)
                .ok_or_else(|| storage_error("active reservation index is dangling"))?;
            let participant = collection.participant(deposit_id).ok_or_else(|| {
                storage_error("active reservation index points to a non-participant collection")
            })?;
            if &participant.reservation.asset != asset {
                return Err(storage_error(
                    "active reservation index asset differs from its collection participant",
                ));
            }
            match participant.reservation.state {
                CollectionReservationState::Active => Ok(Some(collection)),
                CollectionReservationState::Consumed { .. } => Ok(None),
                CollectionReservationState::Released { .. } => Err(storage_error(
                    "released collection still owns an active reservation index",
                )),
            }
        })
    }

    fn retained_collection_for<'a>(
        &'a self,
        deposit_id: &'a DepositId,
        asset: &'a AssetId,
    ) -> BoxFuture<'a, Result<Option<Collection>, DepositError>> {
        Box::pin(async move {
            let Some((collection_id, _)) = self
                .collection_index(
                    active_reservation_ns(),
                    &reservation_key(deposit_id, asset)?,
                )
                .await?
            else {
                return Ok(None);
            };
            let collection = self
                .stored_collection_record(&collection_id)
                .await?
                .map(|(collection, _)| collection)
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
                CollectionReservationState::Active => Ok(Some(collection)),
                CollectionReservationState::Consumed { .. }
                    if collection.mode == CollectionMode::UtxoBatch =>
                {
                    Ok(Some(collection))
                }
                CollectionReservationState::Consumed { .. } => Err(storage_error(
                    "account-model collection retained a consumed reservation index",
                )),
                CollectionReservationState::Released { .. } => Err(storage_error(
                    "released collection still owns a retained reservation index",
                )),
            }
        })
    }
}

impl<S> CollectionHistory for PaymentStore<S>
where
    S: Store,
{
    fn collections_for_deposit<'a>(
        &'a self,
        deposit_id: &'a DepositId,
        request: CollectionQuery,
    ) -> BoxFuture<'a, Result<CollectionPage, DepositError>> {
        Box::pin(async move {
            request.validate()?;
            let page = self
                .storage()
                .scan(ScanRequest {
                    namespace: deposit_collection_ns(),
                    prefix: deposit_collection_prefix(deposit_id)?,
                    after: request
                        .after
                        .as_ref()
                        .map(|id| deposit_collection_key(deposit_id, id))
                        .transpose()?,
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let has_next = page.next.is_some();
            let mut collections = Vec::with_capacity(page.entries.len());
            for (_, stored) in page.entries {
                let index: IndexRecord = decode(&stored)?;
                ensure_version(index.version)?;
                collections.push(
                    self.stored_collection_record(&CollectionId(index.collection_id))
                        .await?
                        .map(|(collection, _)| collection)
                        .ok_or_else(|| storage_error("collection deposit index is dangling"))?,
                );
            }
            let next = has_next
                .then(|| collections.last().map(|collection| collection.id.clone()))
                .flatten();
            Ok(CollectionPage { collections, next })
        })
    }

    fn leg_for_transaction<'a>(
        &'a self,
        transaction_id: &'a TransactionRef,
    ) -> BoxFuture<'a, Result<Option<LegRef>, DepositError>> {
        Box::pin(async move {
            Ok(self
                .stored_leg_reference(transaction_id)
                .await?
                .map(|(reference, _)| reference))
        })
    }

    fn signed_envelope<'a>(
        &'a self,
        collection_id: &'a CollectionId,
        leg_id: &'a LegId,
    ) -> BoxFuture<'a, Result<Option<SignedEnvelope>, DepositError>> {
        Box::pin(async move {
            Ok(self
                .stored_signed_envelope(collection_id, leg_id)
                .await?
                .map(|(envelope, _)| envelope))
        })
    }
}
