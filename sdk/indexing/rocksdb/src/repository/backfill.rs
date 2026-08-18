use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) async fn apply_watch_backfill(
        &self,
        command: CommitBackfill,
        projection: ProjectionBatch,
    ) -> Result<BackfillOutcome, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        Self::validate_backfill_height(&command)?;
        let marker_key = keys::watch_backfill_applied(
            &self.config.scope,
            &command.watch_id.0,
            command.block.height,
        );
        let height_marker_key = keys::watch_backfill_applied_height(
            &self.config.scope,
            command.block.height,
            &command.watch_id.0,
        );
        if let Some(outcome) = self
            .replayed_backfill(&command, &marker_key, &height_marker_key)
            .await?
        {
            return Ok(outcome);
        }
        self.ensure_semantic_available().await?;

        let job_key = keys::watch_backfill(&self.config.scope, &command.watch_id.0);
        let job = self
            .get_record::<BackfillRecord>(&job_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "watch has no pending historical backfill",
                    false,
                )
            })?;
        let job_scope = record::ScopeRecord::into_domain(job.value.scope.clone());
        record::ensure_record_scope(&self.config.scope, &job_scope, "watch backfill")?;
        if job.value.watch_id != command.watch_id.0
            || job.value.next_height != command.expected_next_height.0
            || command.block.height.0 > job.value.through.height
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "backfill command no longer matches the durable job cursor",
                true,
            ));
        }

        let mut batch = self.mutation_batch().await?;
        let active = self.active_generation_record().await?;
        let generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, generation);
        let checkpoint = self
            .get_record::<BlockRecord>(&checkpoint_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Conflict,
                    "backfill cannot run without a live canonical checkpoint",
                    true,
                )
            })?;
        let live_checkpoint = record::BlockRecord::into_domain(checkpoint.value.clone());
        if live_checkpoint != command.expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "live checkpoint changed while the historical block was interpreted",
                true,
            ));
        }
        let through = record::BlockRecord::into_domain(job.value.through.clone());
        if command.block.height > through.height {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "backfill block is beyond its durable registration checkpoint",
                false,
            ));
        }
        if through.height > live_checkpoint.height {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "backfill registration checkpoint is ahead of the live canonical checkpoint",
                true,
            ));
        }
        // `through` is a durable hash anchor, not a dependency on the live
        // retention window. The live tip may move arbitrarily far ahead while
        // this job progresses. A shallow reorg rewrites the anchor atomically
        // in `revert_active_tip`; reaching the terminal height must still match
        // the exact registration-era block.
        if command.block.height == through.height && command.block != through {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "backfill through checkpoint is no longer canonical",
                true,
            ));
        }
        if let Some(canonical) = self
            .get_record::<BlockRecord>(&keys::canonical(
                &self.config.scope,
                generation,
                command.block.height,
            ))
            .await?
        {
            if record::BlockRecord::into_domain(canonical.value) != command.block {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "historical block hash differs from retained canonical state",
                    true,
                ));
            }
        }
        let previous_marker = self.previous_backfill_marker(&command, &job).await?;

        let watch_key = keys::watch(&self.config.scope, &command.watch_id.0);
        let watch = self
            .get_record::<WatchRecord>(&watch_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "backfill watch record is missing",
                    false,
                )
            })?;
        if watch.value.start_height > command.block.height.0
            || watch
                .value
                .inactive_from
                .is_some_and(|inactive| command.block.height.0 >= inactive)
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidWatch,
                "backfill watch is not active at the historical height",
                false,
            ));
        }
        let active_watch_ids = BTreeSet::from([command.watch_id.clone()]);
        let mut draft_ids = BTreeSet::new();
        let depth = live_checkpoint
            .height
            .0
            .checked_sub(command.block.height.0)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "live checkpoint cannot prove the historical inclusion",
                    false,
                )
            })?;
        let mut transitions = BTreeMap::new();
        for draft in &command.drafts {
            self.validate_draft(draft, &active_watch_ids)?;
            if draft.watch_ids.as_slice() != [command.watch_id.clone()]
                || !draft_ids.insert(draft.transaction_id.clone())
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "backfill drafts must be unique and belong only to the backfill watch",
                    false,
                ));
            }
            let prior = self
                .current_observation(generation, &draft.transaction_id)
                .await?;
            let mut status = match &draft.status {
                ObservationDraftStatus::Included
                    if depth >= self.config.confirmation_policy.minimum_confirmations =>
                {
                    TransactionStatus::Confirmed {
                        block: command.block.clone(),
                        proof: ConfirmationProof::Depth {
                            required: self.config.confirmation_policy.minimum_confirmations,
                            // Match live synchronization: confirmation proof is
                            // pinned to the threshold, not the discovery tip.
                            observed: self.config.confirmation_policy.minimum_confirmations,
                        },
                    }
                }
                ObservationDraftStatus::Included => TransactionStatus::Included {
                    block: command.block.clone(),
                    confirmations: depth,
                },
                ObservationDraftStatus::Failed { reason } => TransactionStatus::Failed {
                    block: Some(command.block.clone()),
                    reason: reason.clone(),
                },
            };
            if let Some(prior) = &prior {
                let Some(reconciled) = Self::reconcile_backfill_status(
                    prior,
                    draft,
                    &command.block,
                    &command.watch_id,
                    status,
                )?
                else {
                    continue;
                };
                status = reconciled;
            }
            let mut merged = draft.clone();
            if let Some(prior) = &prior {
                merged
                    .watch_ids
                    .extend(prior.value.watch_ids.iter().cloned().map(WatchId));
                merged.watch_ids.sort();
                merged.watch_ids.dedup();
            }
            let next = self.next_observation(
                prior.as_ref().map(|prior| &prior.value),
                &draft.transaction_id,
                status,
                Some(&merged),
                draft.observed_at,
            )?;
            transitions.insert(
                draft.transaction_id.clone(),
                Transition {
                    prior: prior.as_ref().map(|prior| prior.value.clone()),
                    prior_version: prior.as_ref().map(|prior| prior.version),
                    next,
                    included_here: false,
                    prior_indexed_in_generation: true,
                },
            );
        }

        Self::condition_for(
            &mut batch,
            keys::active_generation(&self.config.scope),
            active.as_ref(),
        );
        Self::condition_for(&mut batch, checkpoint_key, Some(&checkpoint));
        Self::condition_for(&mut batch, job_key.clone(), Some(&job));
        Self::condition_for(&mut batch, watch_key, Some(&watch));
        if let Some((previous_key, previous, previous_height_key, previous_height_marker)) =
            &previous_marker
        {
            Self::condition_for(&mut batch, previous_key.clone(), Some(previous));
            Self::condition_for(
                &mut batch,
                previous_height_key.clone(),
                Some(previous_height_marker),
            );
            Self::delete(&mut batch, previous_key.clone());
            Self::delete(&mut batch, previous_height_key.clone());
        }
        Self::condition_for::<BackfillMarker>(&mut batch, marker_key.clone(), None);
        Self::condition_for::<HeightMarker>(&mut batch, height_marker_key.clone(), None);

        let introduced_projection_keys = self
            .append_backfill_projection(&mut batch, generation, &projection)
            .await?;

        self.extend_backfill_confirmation_undo(
            &mut batch,
            generation,
            &command.block,
            &live_checkpoint,
            &transitions,
            &command.watch_id,
        )
        .await?;

        self.extend_backfill_bundle(
            &mut batch,
            generation,
            &command.block,
            &transitions,
            introduced_projection_keys,
        )
        .await?;

        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = if transitions.is_empty() {
            None
        } else {
            self.counter(&event_counter_key).await?
        };
        let mut next_cursor = event_counter
            .as_ref()
            .map_or(0, |counter| counter.value.value);
        for (transaction_id, transition) in &transitions {
            next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "observation event cursor is exhausted",
                    false,
                )
            })?;
            self.append_transition(
                &mut batch,
                generation,
                transition,
                Some(EventCursor(next_cursor)),
            )?;
            let next_status = record::TransactionStatusRecord::into_domain(
                transition.next.transaction.status.clone(),
            );
            let pending_key = keys::pending_confirmation(
                &self.config.scope,
                generation,
                command.block.height,
                transaction_id,
            );
            let pending = self.get_record::<PendingConfirmation>(&pending_key).await?;
            match next_status {
                TransactionStatus::Included { .. } if pending.is_none() => {
                    Self::condition_for::<PendingConfirmation>(
                        &mut batch,
                        pending_key.clone(),
                        None,
                    );
                    Self::put(
                        &mut batch,
                        pending_key,
                        &PendingConfirmation {
                            transaction_id: record::ScopedValue::from_transaction(transaction_id),
                            inclusion_height: command.block.height.0,
                        },
                    )?;
                }
                TransactionStatus::Confirmed { .. } if pending.is_some() => {
                    Self::condition_for(&mut batch, pending_key.clone(), pending.as_ref());
                    Self::delete(&mut batch, pending_key);
                }
                TransactionStatus::Pending
                | TransactionStatus::Included { .. }
                | TransactionStatus::Confirmed { .. }
                | TransactionStatus::Failed { .. }
                | TransactionStatus::Replaced { .. }
                | TransactionStatus::Dropped
                | TransactionStatus::Reorged { .. } => {}
            }
        }
        if !transitions.is_empty() {
            Self::condition_for(
                &mut batch,
                event_counter_key.clone(),
                event_counter.as_ref(),
            );
            Self::put(
                &mut batch,
                event_counter_key,
                &CounterRecord { value: next_cursor },
            )?;
        }
        Self::put(
            &mut batch,
            marker_key,
            &BackfillMarker {
                block: record::BlockRecord::from_domain(&command.block),
            },
        )?;
        Self::put(
            &mut batch,
            height_marker_key,
            &HeightMarker {
                watch_id: command.watch_id.0.clone(),
            },
        )?;
        let next_height = if command.block.height.0 < through.height.0 {
            Some(BlockHeight(
                command.block.height.0.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "watch backfill height is exhausted",
                        false,
                    )
                })?,
            ))
        } else {
            None
        };
        match next_height {
            Some(next_height) => {
                let mut updated = job.value;
                updated.next_height = next_height.0;
                Self::put(&mut batch, job_key, &updated)?;
            }
            None => Self::delete(&mut batch, job_key),
        }
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(BackfillOutcome::Applied { next_height })
    }
}
