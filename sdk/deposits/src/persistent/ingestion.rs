use super::event::{checkpoint_condition, expected_next_cursor};
use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn mirror_event(
        &self,
        command: MirrorObservation,
    ) -> Result<MirrorOutcome, DepositError> {
        let (checkpoint, checkpoint_stored) = self
            .stored_checkpoint(ConsumerCheckpointName::IxIngestion)
            .await?;
        let cursor = command.observation.event.cursor;
        if checkpoint.cursor == Some(cursor) {
            let existing = self
                .observation(&command.observation.event.id)
                .await?
                .ok_or_else(|| {
                    storage_error("ingestion cursor advanced without its mirrored event")
                })?;
            if existing == command.observation {
                return Ok(MirrorOutcome::AlreadyPresent { cursor });
            }
            return Err(conflict(
                "ingestion retry contains a different mirrored event payload",
            ));
        }
        if checkpoint.cursor != command.expected_cursor {
            return Err(conflict(
                "ingestion expected cursor does not match durable cursor",
            ));
        }
        if expected_next_cursor(checkpoint.cursor)? != cursor {
            return Err(conflict(
                "IX events must be mirrored in contiguous cursor order",
            ));
        }

        let existing_event = self
            .mirrored_observation(&command.observation.event.id)
            .await?;
        if let Some((existing, _)) = &existing_event {
            if existing != &command.observation {
                return Err(conflict(
                    "IX event ID was reused with a different mirrored payload",
                ));
            }
        }
        let cursor_index = self
            .storage
            .get(&observation_cursor_ns(), &cursor_key(cursor))
            .await
            .map_err(map_storage)?;
        if let Some(stored) = &cursor_index {
            let index: IdRecord = decode(stored)?;
            ensure_version(index.version)?;
            if index.id != command.observation.event.id.0 {
                return Err(conflict("IX cursor is assigned to a different event"));
            }
        }

        let mut conditions = vec![checkpoint_condition(
            ConsumerCheckpointName::IxIngestion,
            checkpoint_stored.as_ref(),
        )];
        let mut operations = Vec::new();
        if existing_event.is_none() {
            conditions.push(Condition::Missing {
                namespace: observation_ns(),
                key: key_text(&command.observation.event.id.0),
            });
            operations.push(Operation::Put {
                namespace: observation_ns(),
                key: key_text(&command.observation.event.id.0),
                value: encode(&ObservationRecord::from(&command.observation))?,
            });
        }
        if cursor_index.is_none() {
            conditions.push(Condition::Missing {
                namespace: observation_cursor_ns(),
                key: cursor_key(cursor),
            });
            operations.push(Operation::Put {
                namespace: observation_cursor_ns(),
                key: cursor_key(cursor),
                value: encode(&IdRecord {
                    version: RECORD_VERSION,
                    id: command.observation.event.id.0.clone(),
                })?,
            });
        }
        operations.push(Operation::Put {
            namespace: consumer_checkpoint_ns(),
            key: checkpoint_key(ConsumerCheckpointName::IxIngestion),
            value: encode(&CursorRecord {
                version: RECORD_VERSION,
                cursor: Some(cursor.0),
            })?,
        });
        self.storage
            .commit(WriteBatch {
                conditions,
                operations,
            })
            .await
            .map_err(map_storage)?;
        Ok(MirrorOutcome::Appended { cursor })
    }
}
