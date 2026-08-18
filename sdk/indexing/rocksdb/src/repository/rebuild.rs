use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) async fn query_rebuild_state(
        &self,
        scope: &IndexScope,
    ) -> Result<Option<RebuildState>, IndexError> {
        self.check_scope(scope)?;
        self.verify_metadata().await?;
        self.get_record::<RebuildRecord>(&keys::rebuild_state(scope))
            .await
            .map(|state| state.map(|state| record::RebuildRecord::into_domain(state.value)))
    }

    pub(super) async fn start_rebuild(
        &self,
        command: BeginRebuild,
    ) -> Result<RebuildState, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        if command.bootstrap_height != self.config.bootstrap_height {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "rebuild bootstrap height differs from persistent configuration",
                false,
            ));
        }
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        if let Some(existing) = self.get_record::<RebuildRecord>(&rebuild_key).await? {
            let existing = record::RebuildRecord::into_domain(existing.value);
            if existing.bootstrap_height == command.bootstrap_height {
                return Ok(existing);
            }
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "another staged rebuild is already active",
                false,
            ));
        }
        let mut batch = self.mutation_batch().await?;
        let counter_key = keys::rebuild_counter(&self.config.scope);
        let counter = self.counter(&counter_key).await?;
        let generation = RebuildGeneration(counter.as_ref().map_or(Ok(1), |counter| {
            counter.value.value.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "rebuild generation counter is exhausted",
                    false,
                )
            })
        })?);
        let active = self.active_generation().await?;
        if generation == active {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "new rebuild generation collides with the active generation",
                false,
            ));
        }
        let event_counter = self
            .counter(&keys::event_counter(&self.config.scope))
            .await?;
        let state = RebuildState {
            scope: self.config.scope.clone(),
            generation,
            phase: RebuildPhase::Building,
            bootstrap_height: self.config.bootstrap_height,
            checkpoint: None,
            published_event_high_water: EventCursor(
                event_counter
                    .as_ref()
                    .map_or(0, |counter| counter.value.value),
            ),
        };
        Self::condition_for(&mut batch, counter_key.clone(), counter.as_ref());
        Self::condition_for::<RebuildRecord>(&mut batch, rebuild_key.clone(), None);
        Self::put(
            &mut batch,
            counter_key,
            &CounterRecord {
                value: generation.0,
            },
        )?;
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

    pub(super) async fn commit_shadow_block(
        &self,
        command: RebuildBlock<C::Effect, C::Undo>,
    ) -> Result<BlockOutcome, IndexError> {
        let rebuild = self
            .get_record::<RebuildRecord>(&keys::rebuild_state(&self.config.scope))
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "no staged rebuild is active",
                    false,
                )
            })?;
        let state = record::RebuildRecord::into_domain(rebuild.value.clone());
        if state.generation != command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild command targets another generation",
                false,
            ));
        }
        if state.phase != RebuildPhase::Building {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild generation is not accepting blocks",
                false,
            ));
        }
        if state.checkpoint != command.command.expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild expected checkpoint differs from its durable manifest",
                true,
            ));
        }
        self.commit_generation(
            command.command,
            command.generation,
            false,
            None,
            Some(&rebuild),
        )
        .await
    }

    pub(super) async fn generation_observations(
        &self,
        generation: RebuildGeneration,
    ) -> Result<BTreeMap<TransactionRef, StoredRecord<CurrentObservation>>, IndexError> {
        self.scan_records::<CurrentObservation>(keys::current_observation_prefix(
            &self.config.scope,
            generation,
        ))
        .await?
        .into_iter()
        .map(|(_, current)| {
            let transaction =
                record::ObservationRecord::into_domain(current.value.transaction.clone())?;
            self.validate_transaction_id(&transaction.transaction_id)?;
            Ok((transaction.transaction_id, current))
        })
        .collect()
    }

    pub(super) fn same_projection(
        left: &CurrentObservation,
        right: &CurrentObservation,
    ) -> Result<bool, IndexError> {
        let left_transaction = record::ObservationRecord::into_domain(left.transaction.clone())?;
        let right_transaction = record::ObservationRecord::into_domain(right.transaction.clone())?;
        Ok(left_transaction.scope == right_transaction.scope
            && left_transaction.transaction_id == right_transaction.transaction_id
            && left_transaction.status == right_transaction.status
            && left_transaction.movements == right_transaction.movements
            && left_transaction.fee == right_transaction.fee
            && left_transaction.first_seen_at == right_transaction.first_seen_at
            && left.watch_ids == right.watch_ids)
    }

    pub(super) fn status_block(status: &TransactionStatus, fallback: &BlockRef) -> BlockRef {
        match status {
            TransactionStatus::Included { block, .. }
            | TransactionStatus::Confirmed { block, .. } => block.clone(),
            TransactionStatus::Failed {
                block: Some(block), ..
            } => block.clone(),
            TransactionStatus::Reorged { previous_block } => previous_block.clone(),
            TransactionStatus::Pending
            | TransactionStatus::Failed { block: None, .. }
            | TransactionStatus::Replaced { .. }
            | TransactionStatus::Dropped => fallback.clone(),
        }
    }

    pub(super) fn make_event(
        current: &CurrentObservation,
        previous_status: Option<record::TransactionStatusRecord>,
        cursor: EventCursor,
    ) -> Result<EventRecord, IndexError> {
        let transaction = record::ObservationRecord::into_domain(current.transaction.clone())?;
        Ok(EventRecord {
            id: Self::event_id(cursor, transaction.revision),
            cursor: cursor.0,
            watch_ids: current.watch_ids.clone(),
            previous_status,
            transaction: current.transaction.clone(),
        })
    }

    pub(super) fn append_prepared_rebuild_event(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        current: &CurrentObservation,
        previous_status: Option<record::TransactionStatusRecord>,
        cursor: EventCursor,
    ) -> Result<(), IndexError> {
        let key = keys::prepared_rebuild_event(&self.config.scope, generation, cursor);
        batch.conditions.push(Condition::Missing {
            namespace: keys::namespace(),
            key: key.clone(),
        });
        let event = Self::make_event(current, previous_status, cursor)?;
        Self::put(batch, key, &event)
    }

    pub(super) async fn rebuild_for_checkpoint(
        &self,
        scope: &IndexScope,
        generation: RebuildGeneration,
        expected_checkpoint: &BlockRef,
    ) -> Result<
        (
            StoredRecord<RebuildRecord>,
            RebuildState,
            StoredRecord<BlockRecord>,
        ),
        IndexError,
    > {
        self.check_scope(scope)?;
        self.verify_metadata().await?;
        let rebuild = self
            .get_record::<RebuildRecord>(&keys::rebuild_state(&self.config.scope))
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "no staged rebuild is active",
                    false,
                )
            })?;
        let state = record::RebuildRecord::into_domain(rebuild.value.clone());
        if state.generation != generation || state.checkpoint.as_ref() != Some(expected_checkpoint)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild generation or checkpoint does not match its durable manifest",
                false,
            ));
        }
        let shadow_checkpoint = self
            .generation_checkpoint(generation)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "staged generation has no checkpoint",
                    false,
                )
            })?;
        if record::BlockRecord::into_domain(shadow_checkpoint.value.clone()) != *expected_checkpoint
        {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "staged checkpoint differs from the rebuild manifest",
                false,
            ));
        }
        Ok((rebuild, state, shadow_checkpoint))
    }

    pub(super) async fn mark_rebuild_validating(
        &self,
        command: RebuildValidation,
    ) -> Result<RebuildState, IndexError> {
        let (rebuild, mut state, shadow_checkpoint) = self
            .rebuild_for_checkpoint(
                &command.scope,
                command.generation,
                &command.expected_checkpoint,
            )
            .await?;
        match state.phase {
            RebuildPhase::Building => {}
            RebuildPhase::Validating | RebuildPhase::ReadyToActivate => return Ok(state),
        }

        state.phase = RebuildPhase::Validating;
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, command.generation);
        let mut batch = self.mutation_batch().await?;
        Self::condition_for(&mut batch, rebuild_key.clone(), Some(&rebuild));
        Self::condition_for(&mut batch, checkpoint_key, Some(&shadow_checkpoint));
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
}
