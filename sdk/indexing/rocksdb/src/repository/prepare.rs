use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) async fn prepare_rebuild(
        &self,
        command: PrepareActivation,
    ) -> Result<RebuildState, IndexError> {
        let (rebuild, mut state, shadow_checkpoint) = self
            .rebuild_for_checkpoint(
                &command.scope,
                command.generation,
                &command.expected_checkpoint,
            )
            .await?;
        match state.phase {
            RebuildPhase::Building => {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "rebuild generation must be validated before activation is prepared",
                    false,
                ));
            }
            RebuildPhase::ReadyToActivate => return Ok(state),
            RebuildPhase::Validating => {}
        }

        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = self.counter(&event_counter_key).await?;
        let published_cursor = event_counter
            .as_ref()
            .map_or(EventCursor(0), |counter| EventCursor(counter.value.value));
        if published_cursor != state.published_event_high_water {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "published event cursor changed while the rebuild was staged",
                true,
            ));
        }
        if !self
            .scan_records::<EventRecord>(keys::prepared_rebuild_event_prefix(
                &self.config.scope,
                command.generation,
            ))
            .await?
            .is_empty()
        {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "validating rebuild already contains prepared correction events",
                false,
            ));
        }

        let active = self.active_generation_record().await?;
        let active_generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        if active_generation == command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "the staged rebuild generation is already active",
                false,
            ));
        }
        let old = self.generation_observations(active_generation).await?;
        let new = self.generation_observations(command.generation).await?;
        let transaction_ids: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
        let mut next_cursor = state.published_event_high_water.0;
        let rebuild_key = keys::rebuild_state(&self.config.scope);
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
        Self::condition_for(&mut batch, event_counter_key, event_counter.as_ref());

        for transaction_id in transaction_ids {
            let same_projection = match (old.get(&transaction_id), new.get(&transaction_id)) {
                (Some(old), Some(new)) => Self::same_projection(&old.value, &new.value)?,
                _ => false,
            };
            match (old.get(&transaction_id), new.get(&transaction_id)) {
                (Some(old), Some(new)) if same_projection => {
                    self.carry_matching_revision(
                        &mut batch,
                        command.generation,
                        &transaction_id,
                        old,
                        new,
                    )
                    .await?;
                }
                (Some(old), Some(new)) => {
                    let old_domain =
                        record::ObservationRecord::into_domain(old.value.transaction.clone())?;
                    let mut new_domain =
                        record::ObservationRecord::into_domain(new.value.transaction.clone())?;
                    new_domain.revision = ObservationRevision(
                        old_domain
                            .revision
                            .0
                            .max(new_domain.revision.0)
                            .checked_add(1)
                            .ok_or_else(|| {
                                IndexError::new(
                                    IndexErrorKind::Store,
                                    "observation revision is exhausted",
                                    false,
                                )
                            })?,
                    );
                    let transition = Transition {
                        prior: Some(old.value.clone()),
                        prior_version: Some(new.version),
                        next: CurrentObservation {
                            transaction: record::ObservationRecord::from_domain(&new_domain),
                            watch_ids: new.value.watch_ids.clone(),
                        },
                        included_here: false,
                        prior_indexed_in_generation: true,
                    };
                    next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Store,
                            "observation event cursor is exhausted",
                            false,
                        )
                    })?;
                    self.append_transition(&mut batch, command.generation, &transition, None)?;
                    self.append_prepared_rebuild_event(
                        &mut batch,
                        command.generation,
                        &transition.next,
                        transition
                            .prior
                            .as_ref()
                            .map(|prior| prior.transaction.status.clone()),
                        EventCursor(next_cursor),
                    )?;
                }
                (Some(old), None) => {
                    let old_domain =
                        record::ObservationRecord::into_domain(old.value.transaction.clone())?;
                    let next = self.next_observation(
                        Some(&old.value),
                        &transaction_id,
                        TransactionStatus::Reorged {
                            previous_block: Self::status_block(
                                &old_domain.status,
                                &command.expected_checkpoint,
                            ),
                        },
                        None,
                        command
                            .expected_checkpoint
                            .timestamp
                            .unwrap_or(command.expected_checkpoint.height.0),
                    )?;
                    let transition = Transition {
                        prior: Some(old.value.clone()),
                        prior_version: None,
                        next,
                        included_here: false,
                        prior_indexed_in_generation: false,
                    };
                    next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Store,
                            "observation event cursor is exhausted",
                            false,
                        )
                    })?;
                    self.append_transition(&mut batch, command.generation, &transition, None)?;
                    self.append_prepared_rebuild_event(
                        &mut batch,
                        command.generation,
                        &transition.next,
                        transition
                            .prior
                            .as_ref()
                            .map(|prior| prior.transaction.status.clone()),
                        EventCursor(next_cursor),
                    )?;
                }
                (None, Some(new)) => {
                    next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Store,
                            "observation event cursor is exhausted",
                            false,
                        )
                    })?;
                    self.append_prepared_rebuild_event(
                        &mut batch,
                        command.generation,
                        &new.value,
                        None,
                        EventCursor(next_cursor),
                    )?;
                }
                (None, None) => {}
            }
        }

        state.phase = RebuildPhase::ReadyToActivate;
        Self::put(
            &mut batch,
            rebuild_key,
            &record::RebuildRecord::from_domain(&state),
        )?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(state)
    }

    async fn carry_matching_revision(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        transaction_id: &TransactionRef,
        old: &StoredRecord<CurrentObservation>,
        new: &StoredRecord<CurrentObservation>,
    ) -> Result<(), IndexError> {
        let old_domain = record::ObservationRecord::into_domain(old.value.transaction.clone())?;
        let mut new_domain = record::ObservationRecord::into_domain(new.value.transaction.clone())?;
        if old_domain.revision <= new_domain.revision {
            return Ok(());
        }
        new_domain.revision = old_domain.revision;
        let carried = CurrentObservation {
            transaction: record::ObservationRecord::from_domain(&new_domain),
            watch_ids: new.value.watch_ids.clone(),
        };
        let current_key = keys::current_observation(&self.config.scope, generation, transaction_id);
        Self::condition_for(batch, current_key.clone(), Some(new));
        Self::put(batch, current_key, &carried)?;
        let revision_key = keys::observation_revision(
            &self.config.scope,
            generation,
            transaction_id,
            new_domain.revision,
        );
        if self
            .get_record::<ObservationRecord>(&revision_key)
            .await?
            .is_none()
        {
            Self::condition_for::<ObservationRecord>(batch, revision_key.clone(), None);
            Self::put(batch, revision_key, &carried.transaction)?;
        }
        Ok(())
    }
}
