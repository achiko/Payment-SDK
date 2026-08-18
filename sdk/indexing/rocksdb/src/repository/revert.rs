use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) async fn revert_active_tip(
        &self,
        command: RevertTip,
    ) -> Result<RevertOutcome, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let active = self.active_generation_record().await?;
        let generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, generation);
        let checkpoint = self.get_record::<BlockRecord>(&checkpoint_key).await?;
        let current_checkpoint = checkpoint
            .as_ref()
            .map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value.clone()));
        if current_checkpoint.as_ref() != Some(&command.expected_tip) {
            if current_checkpoint
                .as_ref()
                .is_none_or(|checkpoint| checkpoint.height < command.expected_tip.height)
            {
                return Ok(RevertOutcome::AlreadyReverted {
                    checkpoint: current_checkpoint,
                });
            }
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "revert must target the exact newest canonical tip",
                true,
            ));
        }

        let canonical_key =
            keys::canonical(&self.config.scope, generation, command.expected_tip.height);
        let canonical = self
            .get_record::<BlockRecord>(&canonical_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "canonical tip record is missing",
                    false,
                )
            })?;
        if record::BlockRecord::into_domain(canonical.value.clone()) != command.expected_tip {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "canonical tip record does not match the checkpoint",
                false,
            ));
        }
        let bundle_key = keys::bundle(&self.config.scope, generation, command.expected_tip.height);
        let bundle = self
            .get_record::<BundleRecord>(&bundle_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::ReorgBeyondRetention,
                    "tip undo bundle is outside the retained rollback window",
                    false,
                )
            })?;
        if record::BlockRecord::into_domain(bundle.value.block.clone()) != command.expected_tip {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "undo bundle does not match the canonical tip",
                false,
            ));
        }
        // Decoding detects an application-schema or on-disk undo incompatibility
        // before canonical state changes. The schema adapter may also derive
        // inverse projection changes from the decoded chain-owned undo.
        let decoded_undo = self.records.decode_undo(&bundle.value.encoded_undo)?;
        let mut rollback_projection = self.records.rollback_projection(&decoded_undo)?;
        let backfill_rollback_key = keys::backfill_projection_rollback(
            &self.config.scope,
            generation,
            command.expected_tip.height,
        );
        let backfill_rollback = self
            .get_record::<BackfillRollback>(&backfill_rollback_key)
            .await?;
        if let Some(backfill_rollback) = &backfill_rollback {
            if record::BlockRecord::into_domain(backfill_rollback.value.block.clone())
                != command.expected_tip
            {
                return Err(IndexError::new(
                    IndexErrorKind::Store,
                    "historical projection rollback record does not match the canonical tip",
                    false,
                ));
            }
            let mut rollback_keys = rollback_projection
                .mutations
                .iter()
                .map(ProjectionMutation::key)
                .map(<[u8]>::to_vec)
                .collect::<BTreeSet<_>>();
            if rollback_keys.len() != rollback_projection.mutations.len() {
                return Err(IndexError::new(
                    IndexErrorKind::Store,
                    "chain projection rollback contains duplicate keys",
                    false,
                ));
            }
            for key in &backfill_rollback.value.relative_keys {
                if !rollback_keys.insert(key.clone()) {
                    let is_same_delete = rollback_projection.mutations.iter().any(|mutation| {
                        matches!(mutation, ProjectionMutation::Delete { key: existing } if existing == key)
                    });
                    if !is_same_delete {
                        return Err(IndexError::new(
                            IndexErrorKind::Store,
                            "chain and historical projection rollback overlap is not an identical delete",
                            false,
                        ));
                    }
                    continue;
                }
                rollback_projection
                    .mutations
                    .push(ProjectionMutation::Delete { key: key.clone() });
            }
        }

        let prior_checkpoint = bundle
            .value
            .prior_checkpoint
            .clone()
            .map(record::BlockRecord::into_domain);
        let observed_at = prior_checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.timestamp)
            .unwrap_or(command.expected_tip.height.0);
        let mut batch = self.mutation_batch().await?;
        Self::condition_for(
            &mut batch,
            keys::active_generation(&self.config.scope),
            active.as_ref(),
        );
        Self::condition_for(&mut batch, checkpoint_key.clone(), checkpoint.as_ref());
        Self::condition_for(&mut batch, canonical_key.clone(), Some(&canonical));
        Self::condition_for(&mut batch, bundle_key.clone(), Some(&bundle));
        Self::condition_for(
            &mut batch,
            backfill_rollback_key.clone(),
            backfill_rollback.as_ref(),
        );
        self.append_projection_batch(
            &mut batch,
            generation,
            &rollback_projection,
            IndexErrorKind::Store,
        )
        .await?;
        if backfill_rollback.is_some() {
            Self::delete(&mut batch, backfill_rollback_key);
        }

        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = if bundle.value.changes.is_empty() {
            None
        } else {
            self.counter(&event_counter_key).await?
        };
        let mut next_cursor = event_counter
            .as_ref()
            .map_or(0, |counter| counter.value.value);
        for change in &bundle.value.changes {
            let transaction_id =
                record::ScopedValue::into_transaction(change.transaction_id.clone());
            let current = self
                .current_observation(generation, &transaction_id)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "undo bundle references a missing current observation",
                        false,
                    )
                })?;
            let current_domain =
                record::ObservationRecord::into_domain(current.value.transaction.clone())?;
            let next = if change.included_here {
                self.next_observation(
                    Some(&current.value),
                    &transaction_id,
                    TransactionStatus::Reorged {
                        previous_block: command.expected_tip.clone(),
                    },
                    None,
                    observed_at,
                )?
            } else {
                let prior = change.prior.as_ref().ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "confirmation rollback is missing its prior observation",
                        false,
                    )
                })?;
                let mut prior_domain =
                    record::ObservationRecord::into_domain(prior.transaction.clone())?;
                prior_domain.revision = ObservationRevision(
                    current_domain.revision.0.checked_add(1).ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Store,
                            "observation revision is exhausted",
                            false,
                        )
                    })?,
                );
                prior_domain.observed_at = observed_at;
                CurrentObservation {
                    transaction: record::ObservationRecord::from_domain(&prior_domain),
                    watch_ids: prior.watch_ids.clone(),
                }
            };
            let transition = Transition {
                prior: Some(current.value.clone()),
                prior_version: Some(current.version),
                next,
                included_here: change.included_here,
                prior_indexed_in_generation: true,
            };
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
                &transition,
                Some(EventCursor(next_cursor)),
            )?;
            self.reconcile_reverted_pending(
                &mut batch,
                generation,
                &command.expected_tip,
                &transaction_id,
                &current_domain.status,
                &transition,
            )
            .await?;
        }
        if !bundle.value.changes.is_empty() {
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
        self.reconcile_watch_backfills_for_revert(
            &mut batch,
            &command.expected_tip,
            prior_checkpoint.as_ref(),
        )
        .await?;
        Self::delete(&mut batch, bundle_key);
        Self::delete(&mut batch, canonical_key);
        match &prior_checkpoint {
            Some(prior) => Self::put(
                &mut batch,
                checkpoint_key,
                &record::BlockRecord::from_domain(prior),
            )?,
            None => Self::delete(&mut batch, checkpoint_key),
        }
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(RevertOutcome::Reverted {
            checkpoint: prior_checkpoint,
        })
    }

    async fn reconcile_reverted_pending(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        reverted: &BlockRef,
        transaction_id: &TransactionRef,
        current_status: &TransactionStatus,
        transition: &Transition,
    ) -> Result<(), IndexError> {
        if transition.included_here {
            if !matches!(
                current_status,
                TransactionStatus::Included { .. } | TransactionStatus::Confirmed { .. }
            ) {
                return Ok(());
            }
            let pending_key = keys::pending_confirmation(
                &self.config.scope,
                generation,
                reverted.height,
                transaction_id,
            );
            let pending = self.get_record::<PendingConfirmation>(&pending_key).await?;
            if let Some(pending) = pending {
                Self::condition_for(batch, pending_key.clone(), Some(&pending));
                Self::delete(batch, pending_key);
            }
            return Ok(());
        }
        let TransactionStatus::Included { block, .. } =
            record::ObservationRecord::into_domain(transition.next.transaction.clone())?.status
        else {
            return Ok(());
        };
        let pending_key = keys::pending_confirmation(
            &self.config.scope,
            generation,
            block.height,
            transaction_id,
        );
        if self
            .get_record::<PendingConfirmation>(&pending_key)
            .await?
            .is_some()
        {
            return Ok(());
        }
        Self::condition_for::<PendingConfirmation>(batch, pending_key.clone(), None);
        Self::put(
            batch,
            pending_key,
            &PendingConfirmation {
                transaction_id: record::ScopedValue::from_transaction(transaction_id),
                inclusion_height: block.height.0,
            },
        )?;
        Ok(())
    }

    pub(super) async fn reconcile_watch_backfills_for_revert(
        &self,
        batch: &mut WriteBatch,
        reverted: &BlockRef,
        prior_checkpoint: Option<&BlockRef>,
    ) -> Result<(), IndexError> {
        let height_markers = self
            .scan_records::<HeightMarker>(keys::watch_backfill_applied_height_prefix(
                &self.config.scope,
                reverted.height,
            ))
            .await?;
        let mut affected_watches = BTreeSet::new();
        for (height_marker_key, height_marker) in height_markers {
            if !affected_watches.insert(height_marker.value.watch_id.clone()) {
                return Err(IndexError::new(
                    IndexErrorKind::Store,
                    "backfill height index contains a duplicate watch",
                    false,
                ));
            }
            let marker_key = keys::watch_backfill_applied(
                &self.config.scope,
                &height_marker.value.watch_id,
                reverted.height,
            );
            let marker = self
                .get_record::<BackfillMarker>(&marker_key)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "backfill height index references a missing applied marker",
                        false,
                    )
                })?;
            if record::BlockRecord::into_domain(marker.value.block.clone()) != *reverted {
                return Err(IndexError::new(
                    IndexErrorKind::Store,
                    "backfill applied marker does not match the reverted canonical block",
                    false,
                ));
            }
            Self::condition_for(batch, height_marker_key.clone(), Some(&height_marker));
            Self::condition_for(batch, marker_key.clone(), Some(&marker));
            Self::delete(batch, height_marker_key);
            Self::delete(batch, marker_key);
        }

        let jobs = self
            .scan_records::<BackfillRecord>(keys::watch_backfill_prefix(&self.config.scope))
            .await?;
        for (job_key, job) in jobs {
            let job_scope = record::ScopeRecord::into_domain(job.value.scope.clone());
            record::ensure_record_scope(&self.config.scope, &job_scope, "watch backfill")?;
            let through = record::BlockRecord::into_domain(job.value.through.clone());
            if through.height < reverted.height {
                continue;
            }
            if through != *reverted {
                return Err(IndexError::new(
                    IndexErrorKind::Store,
                    "watch backfill anchor is ahead of or differs from the reverted tip",
                    false,
                ));
            }

            Self::condition_for(batch, job_key.clone(), Some(&job));
            match prior_checkpoint {
                Some(prior)
                    if prior.height.0 >= job.value.from_height
                        && job.value.next_height <= prior.height.0 =>
                {
                    let mut updated = job.value;
                    updated.through = record::BlockRecord::from_domain(prior);
                    Self::put(batch, job_key, &updated)?;
                }
                Some(_) | None => Self::delete(batch, job_key),
            }
        }
        Ok(())
    }

    pub(super) async fn query_transaction(
        &self,
        request: TransactionQuery,
    ) -> Result<Option<ObservedTransaction>, IndexError> {
        self.check_scope(&request.scope)?;
        self.validate_transaction_id(&request.transaction_id)?;
        self.verify_metadata().await?;
        self.ensure_semantic_available().await?;
        let generation = self.active_generation().await?;
        self.current_observation(generation, &request.transaction_id)
            .await?
            .map(|current| record::ObservationRecord::into_domain(current.value.transaction))
            .transpose()
    }

    pub(super) async fn query_watch_backfills(
        &self,
        scope: &IndexScope,
        limit: usize,
    ) -> Result<Vec<WatchBackfill>, IndexError> {
        self.check_scope(scope)?;
        Self::validate_query_limit(limit)?;
        self.verify_metadata().await?;
        self.ensure_semantic_available().await?;
        let page = self
            .storage
            .scan(ScanRequest {
                namespace: keys::namespace(),
                prefix: keys::watch_backfill_prefix(scope),
                after: None,
                limit,
            })
            .await
            .map_err(Self::storage_error)?;
        page.entries
            .into_iter()
            .map(|(_, stored)| {
                let backfill = Self::decode::<BackfillRecord>(&stored.value.0)?;
                let backfill_scope = record::ScopeRecord::into_domain(backfill.scope);
                record::ensure_record_scope(scope, &backfill_scope, "watch backfill")?;
                Ok(WatchBackfill {
                    scope: backfill_scope,
                    watch_id: WatchId(backfill.watch_id),
                    from_height: BlockHeight(backfill.from_height),
                    next_height: BlockHeight(backfill.next_height),
                    through: record::BlockRecord::into_domain(backfill.through),
                })
            })
            .collect()
    }
}
