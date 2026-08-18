use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) async fn publish_rebuild(
        &self,
        command: RebuildActivation,
    ) -> Result<(), IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let active = self.active_generation_record().await?;
        let active_generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        let Some(rebuild) = self.get_record::<RebuildRecord>(&rebuild_key).await? else {
            if active_generation == command.generation
                && self
                    .generation_checkpoint(active_generation)
                    .await?
                    .is_some_and(|checkpoint| {
                        record::BlockRecord::into_domain(checkpoint.value)
                            == command.expected_checkpoint
                    })
            {
                return Ok(());
            }
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "no matching staged rebuild is active",
                false,
            ));
        };
        let rebuild_state = record::RebuildRecord::into_domain(rebuild.value.clone());
        if rebuild_state.generation != command.generation
            || rebuild_state.checkpoint.as_ref() != Some(&command.expected_checkpoint)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild generation or checkpoint does not match its durable manifest",
                false,
            ));
        }
        if rebuild_state.phase != RebuildPhase::ReadyToActivate {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild generation has not been prepared for activation",
                false,
            ));
        }
        let shadow_checkpoint = self
            .generation_checkpoint(command.generation)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "staged generation has no checkpoint",
                    false,
                )
            })?;
        if record::BlockRecord::into_domain(shadow_checkpoint.value.clone())
            != command.expected_checkpoint
        {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "staged checkpoint differs from the rebuild manifest",
                false,
            ));
        }

        let prepared_events = self
            .scan_records::<EventRecord>(keys::prepared_rebuild_event_prefix(
                &self.config.scope,
                command.generation,
            ))
            .await?;
        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = self.counter(&event_counter_key).await?;
        let published_cursor = event_counter
            .as_ref()
            .map_or(EventCursor(0), |counter| EventCursor(counter.value.value));
        if published_cursor != rebuild_state.published_event_high_water {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "published event cursor changed after rebuild corrections were prepared",
                true,
            ));
        }
        let mut next_cursor = rebuild_state.published_event_high_water.0;
        for (_, event) in &prepared_events {
            next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "observation event cursor is exhausted",
                    false,
                )
            })?;
            let transaction =
                record::ObservationRecord::into_domain(event.value.transaction.clone())?;
            let expected_id = Self::event_id(EventCursor(next_cursor), transaction.revision);
            if event.value.cursor != next_cursor || event.value.id != expected_id {
                return Err(IndexError::new(
                    IndexErrorKind::Store,
                    "prepared rebuild events are corrupt or non-contiguous",
                    false,
                ));
            }
        }

        let mut batch = self.mutation_batch().await?;
        Self::condition_for(
            &mut batch,
            keys::active_generation(&self.config.scope),
            active.as_ref(),
        );
        Self::condition_for(&mut batch, rebuild_key.clone(), Some(&rebuild));
        Self::condition_for(
            &mut batch,
            keys::canonical_checkpoint(&self.config.scope, command.generation),
            Some(&shadow_checkpoint),
        );
        Self::condition_for(
            &mut batch,
            event_counter_key.clone(),
            event_counter.as_ref(),
        );
        for (prepared_key, prepared) in &prepared_events {
            let cursor = EventCursor(prepared.value.cursor);
            let event_key = keys::event(&self.config.scope, cursor);
            let event_id_key = keys::event_id(&self.config.scope, &prepared.value.id);
            batch.conditions.push(Condition::Version {
                namespace: keys::namespace(),
                key: prepared_key.clone(),
                expected: prepared.version,
            });
            batch.conditions.push(Condition::Missing {
                namespace: keys::namespace(),
                key: event_key.clone(),
            });
            batch.conditions.push(Condition::Missing {
                namespace: keys::namespace(),
                key: event_id_key.clone(),
            });
            Self::put(&mut batch, event_key, &prepared.value)?;
            Self::put(
                &mut batch,
                event_id_key,
                &EventPointer {
                    cursor: prepared.value.cursor,
                },
            )?;
            Self::delete(&mut batch, prepared_key.clone());
        }
        if !prepared_events.is_empty() {
            Self::put(
                &mut batch,
                event_counter_key,
                &CounterRecord { value: next_cursor },
            )?;
        }
        Self::put(
            &mut batch,
            keys::active_generation(&self.config.scope),
            &CounterRecord {
                value: command.generation.0,
            },
        )?;
        Self::delete(&mut batch, rebuild_key);

        let status_key = keys::status(&self.config.scope);
        let persisted_status = self.get_record::<SyncRecord>(&status_key).await?;
        let mut status = persisted_status.as_ref().map_or_else(
            || SyncStatus::starting(self.config.scope.clone(), self.config.confirmation_policy),
            |status| record::SyncRecord::into_domain(status.value.clone()),
        );
        status.checkpoint = Some(command.expected_checkpoint);
        status.phase = SyncPhase::Ready;
        status.rebuild_reason = None;
        status.halted_reason = None;
        Self::condition_for(&mut batch, status_key.clone(), persisted_status.as_ref());
        Self::put(
            &mut batch,
            status_key,
            &record::SyncRecord::from_domain(&status),
        )?;
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(())
    }

    pub(super) async fn cancel_rebuild(&self, command: AbortRebuild) -> Result<(), IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let active = self.active_generation().await?;
        if active == command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "the active generation cannot be aborted",
                false,
            ));
        }
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        let Some(rebuild) = self.get_record::<RebuildRecord>(&rebuild_key).await? else {
            return Ok(());
        };
        let state = record::RebuildRecord::into_domain(rebuild.value.clone());
        if state.generation != command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "abort targets another rebuild generation",
                false,
            ));
        }
        let mut batch = self.mutation_batch().await?;
        batch.conditions.push(Condition::Version {
            namespace: keys::namespace(),
            key: rebuild_key.clone(),
            expected: rebuild.version,
        });
        for key in self.generation_cleanup_keys(command.generation).await? {
            Self::delete(&mut batch, key);
        }
        Self::delete(&mut batch, rebuild_key);
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(())
    }

    pub(super) async fn generation_cleanup_keys(
        &self,
        generation: RebuildGeneration,
    ) -> Result<Vec<Key>, IndexError> {
        let mut keys_to_delete = Vec::new();
        for prefix in keys::generation_prefixes(&self.config.scope, generation) {
            keys_to_delete.extend(self.generation_keys(prefix).await?);
        }
        Ok(keys_to_delete)
    }

    // Generation records have different DTOs, so cleanup scans raw values and
    // uses only their already-validated logical keys.
    async fn generation_keys(&self, prefix: Vec<u8>) -> Result<Vec<Key>, IndexError> {
        let mut keys = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: keys::namespace(),
                    prefix: prefix.clone(),
                    after,
                    limit: SCAN_CHUNK,
                })
                .await
                .map_err(Self::storage_error)?;
            keys.extend(page.entries.into_iter().map(|(key, _)| key));
            let Some(next) = page.next else {
                break;
            };
            after = Some(next);
        }
        Ok(keys)
    }

    pub(super) async fn remove_generation(
        &self,
        command: CleanupGeneration,
    ) -> Result<CleanupOutcome, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let active = self.active_generation_record().await?;
        let active_generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        if active_generation == command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "active generation cannot be cleaned up",
                false,
            ));
        }
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        let rebuild = self.get_record::<RebuildRecord>(&rebuild_key).await?;
        if rebuild
            .as_ref()
            .is_some_and(|rebuild| rebuild.value.generation == command.generation.0)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "current staged rebuild generation cannot be cleaned up; abort it explicitly",
                false,
            ));
        }
        let keys_to_delete = self.generation_cleanup_keys(command.generation).await?;
        if keys_to_delete.is_empty() {
            return Ok(CleanupOutcome::AlreadyAbsent);
        }
        let removed = u64::try_from(keys_to_delete.len()).map_err(|_| {
            IndexError::new(
                IndexErrorKind::Store,
                "generation cleanup record count does not fit in u64",
                false,
            )
        })?;
        let mut batch = self.mutation_batch().await?;
        Self::condition_for(
            &mut batch,
            keys::active_generation(&self.config.scope),
            active.as_ref(),
        );
        Self::condition_for(&mut batch, rebuild_key, rebuild.as_ref());
        for key in keys_to_delete {
            Self::delete(&mut batch, key);
        }
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(CleanupOutcome::Removed { records: removed })
    }
}
