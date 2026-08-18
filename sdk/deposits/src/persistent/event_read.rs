use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn mirrored_observation(
        &self,
        event_id: &EventId,
    ) -> Result<Option<(MirroredObservation, StoredValue)>, DepositError> {
        self.storage
            .get(&observation_ns(), &key_text(&event_id.0))
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: ObservationRecord = decode(&stored)?;
                Ok((record.try_into()?, stored))
            })
            .transpose()
    }

    pub(super) async fn stored_checkpoint(
        &self,
        name: ConsumerCheckpointName,
    ) -> Result<(ConsumerCheckpoint, Option<StoredValue>), DepositError> {
        let stored = self
            .storage
            .get(&consumer_checkpoint_ns(), &checkpoint_key(name))
            .await
            .map_err(map_storage)?;
        let checkpoint = match &stored {
            Some(stored) => {
                let record: CursorRecord = decode(stored)?;
                ensure_version(record.version)?;
                ConsumerCheckpoint {
                    name,
                    cursor: record.cursor.map(EventCursor),
                }
            }
            None => ConsumerCheckpoint { name, cursor: None },
        };
        Ok((checkpoint, stored))
    }
}
