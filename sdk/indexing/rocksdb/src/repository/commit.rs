use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) async fn commit_generation(
        &self,
        command: CommitBlock<C::Effect, C::Undo>,
        generation: RebuildGeneration,
        publish_events: bool,
        active_generation: Option<&StoredRecord<CounterRecord>>,
        rebuild: Option<&StoredRecord<RebuildRecord>>,
    ) -> Result<BlockOutcome, IndexError> {
        self.check_scope(&command.scope)?;
        self.validate_policy(command.confirmation_policy, command.reorg_retention)?;
        if command.block.block.height < self.config.bootstrap_height {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block precedes the configured bootstrap height",
                false,
            ));
        }

        let canonical_key =
            keys::canonical(&self.config.scope, generation, command.block.block.height);
        if let Some(existing) = self.get_record::<BlockRecord>(&canonical_key).await? {
            let existing = record::BlockRecord::into_domain(existing.value);
            if existing == command.block.block {
                return Ok(BlockOutcome::AlreadyApplied);
            }
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "another canonical hash is already stored at the block height",
                true,
            ));
        }
        if publish_events {
            self.ensure_semantic_available().await?;
        }

        let mut batch = self.mutation_batch().await?;
        if let Some(active) = active_generation {
            Self::condition_for(
                &mut batch,
                keys::active_generation(&self.config.scope),
                Some(active),
            );
        } else if publish_events {
            Self::condition_for::<CounterRecord>(
                &mut batch,
                keys::active_generation(&self.config.scope),
                None,
            );
        }

        let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, generation);
        let checkpoint = self.get_record::<BlockRecord>(&checkpoint_key).await?;
        let persisted_checkpoint = checkpoint
            .as_ref()
            .map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value.clone()));
        if persisted_checkpoint != command.expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "expected checkpoint no longer matches persistent state",
                true,
            ));
        }
        match &persisted_checkpoint {
            Some(checkpoint) => {
                let expected_height = checkpoint.height.0.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "checkpoint height is exhausted",
                        false,
                    )
                })?;
                if command.block.block.height != BlockHeight(expected_height)
                    || command.block.block.parent_hash.as_ref() != Some(&checkpoint.hash)
                {
                    return Err(IndexError::new(
                        IndexErrorKind::CannotConnect,
                        "block does not immediately connect to the persistent checkpoint",
                        true,
                    ));
                }
            }
            None if command.block.block.height != self.config.bootstrap_height => {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "the first persisted block must equal the configured bootstrap height",
                    false,
                ));
            }
            None => {}
        }

        let watch_version_key = keys::watch_version(&self.config.scope);
        let watch_version = self.watch_version_record().await?;
        let persisted_watch_version = watch_version.as_ref().map_or(0, |value| value.value.value);
        if persisted_watch_version != command.expected_watch_version.0 {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "watch set changed while the block was interpreted",
                true,
            ));
        }
        Self::condition_for(&mut batch, watch_version_key, watch_version.as_ref());
        let active_watch_ids = self.active_watch_ids(command.block.block.height).await?;

        let pending = self
            .scan_records::<PendingConfirmation>(keys::pending_confirmation_prefix(
                &self.config.scope,
                generation,
            ))
            .await?;
        let mut transitions = BTreeMap::<TransactionRef, Transition>::new();
        let mut pending_records = BTreeMap::<TransactionRef, (Key, Version)>::new();
        let transition_time = command
            .block
            .block
            .timestamp
            .unwrap_or(command.block.block.height.0);
        for (pending_key, pending) in pending {
            let transaction_id =
                record::ScopedValue::into_transaction(pending.value.transaction_id.clone());
            self.validate_transaction_id(&transaction_id)?;
            let current = self
                .current_observation(generation, &transaction_id)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "confirmation index references a missing observation",
                        false,
                    )
                })?;
            let current_domain =
                record::ObservationRecord::into_domain(current.value.transaction.clone())?;
            let (inclusion_block, confirmations) = match &current_domain.status {
                TransactionStatus::Included {
                    block,
                    confirmations,
                } => (block.clone(), *confirmations),
                _ => {
                    return Err(IndexError::new(
                        IndexErrorKind::Store,
                        "confirmation index references a non-included observation",
                        false,
                    ));
                }
            };
            if inclusion_block.height.0 != pending.value.inclusion_height {
                return Err(IndexError::new(
                    IndexErrorKind::Store,
                    "confirmation index inclusion height is inconsistent",
                    false,
                ));
            }
            let depth = command
                .block
                .block
                .height
                .0
                .checked_sub(inclusion_block.height.0)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "block tip cannot prove the indexed inclusion",
                        false,
                    )
                })?;
            pending_records.insert(transaction_id.clone(), (pending_key, pending.version));
            if depth <= confirmations {
                continue;
            }
            let status = if depth >= self.config.confirmation_policy.minimum_confirmations {
                TransactionStatus::Confirmed {
                    block: inclusion_block,
                    proof: ConfirmationProof::Depth {
                        required: self.config.confirmation_policy.minimum_confirmations,
                        observed: depth,
                    },
                }
            } else {
                TransactionStatus::Included {
                    block: inclusion_block,
                    confirmations: depth,
                }
            };
            let next = self.next_observation(
                Some(&current.value),
                &transaction_id,
                status,
                None,
                transition_time,
            )?;
            transitions.insert(
                transaction_id,
                Transition {
                    prior: Some(current.value),
                    prior_version: Some(current.version),
                    next,
                    included_here: false,
                    prior_indexed_in_generation: true,
                },
            );
        }

        let mut draft_ids = BTreeSet::new();
        for draft in &command.block.drafts {
            self.validate_draft(draft, &active_watch_ids)?;
            if !draft_ids.insert(draft.transaction_id.clone())
                || transitions.contains_key(&draft.transaction_id)
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains a duplicate transaction observation",
                    false,
                ));
            }
            let prior = self
                .current_observation(generation, &draft.transaction_id)
                .await?;
            let prior_is_canonical = if let Some(prior) = &prior {
                matches!(
                    record::ObservationRecord::into_domain(prior.value.transaction.clone())?.status,
                    TransactionStatus::Included { .. }
                        | TransactionStatus::Confirmed { .. }
                        | TransactionStatus::Failed { block: Some(_), .. }
                )
            } else {
                false
            };
            if prior_is_canonical {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "a canonical transaction is already included at another height",
                    false,
                ));
            }
            let status = match &draft.status {
                ObservationDraftStatus::Included => TransactionStatus::Included {
                    block: command.block.block.clone(),
                    confirmations: 1,
                },
                ObservationDraftStatus::Failed { reason } => TransactionStatus::Failed {
                    block: Some(command.block.block.clone()),
                    reason: reason.clone(),
                },
            };
            let next = self.next_observation(
                prior.as_ref().map(|prior| &prior.value),
                &draft.transaction_id,
                status,
                Some(draft),
                draft.observed_at,
            )?;
            transitions.insert(
                draft.transaction_id.clone(),
                Transition {
                    prior: prior.as_ref().map(|prior| prior.value.clone()),
                    prior_version: prior.as_ref().map(|prior| prior.version),
                    next,
                    included_here: true,
                    prior_indexed_in_generation: true,
                },
            );
        }

        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = if publish_events && !transitions.is_empty() {
            self.counter(&event_counter_key).await?
        } else {
            None
        };
        let mut next_cursor = event_counter
            .as_ref()
            .map_or(0, |counter| counter.value.value);
        for (transaction_id, transition) in &transitions {
            let cursor = if publish_events {
                next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "observation event cursor is exhausted",
                        false,
                    )
                })?;
                Some(EventCursor(next_cursor))
            } else {
                None
            };
            self.append_transition(&mut batch, generation, transition, cursor)?;
            self.update_pending_confirmation(
                &mut batch,
                generation,
                command.block.block.height,
                transaction_id,
                transition,
                &pending_records,
            )?;
        }
        if publish_events && !transitions.is_empty() {
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

        let projection = self.records.project(&command.block.effect)?;
        self.append_projection_batch(
            &mut batch,
            generation,
            &projection,
            IndexErrorKind::InvalidBlock,
        )
        .await?;

        let encoded_undo = self.records.encode_undo(&command.block.undo)?;
        let bundle = BundleRecord {
            block: record::BlockRecord::from_domain(&command.block.block),
            prior_checkpoint: command
                .expected_checkpoint
                .as_ref()
                .map(record::BlockRecord::from_domain),
            encoded_undo,
            raw_block: command.block.raw.block.clone(),
            raw_receipts: command.block.raw.receipts.clone(),
            changes: transitions
                .values()
                .map(|transition| BundleChange {
                    transaction_id: transition.next.transaction.transaction_id.clone(),
                    prior: transition.prior.clone(),
                    included_here: transition.included_here,
                })
                .collect(),
        };
        let bundle_key = keys::bundle(&self.config.scope, generation, command.block.block.height);
        batch.conditions.push(Condition::Missing {
            namespace: keys::namespace(),
            key: canonical_key.clone(),
        });
        batch.conditions.push(Condition::Missing {
            namespace: keys::namespace(),
            key: bundle_key.clone(),
        });
        Self::put(
            &mut batch,
            canonical_key,
            &record::BlockRecord::from_domain(&command.block.block),
        )?;
        Self::put(&mut batch, bundle_key, &bundle)?;
        Self::condition_for(&mut batch, checkpoint_key.clone(), checkpoint.as_ref());
        Self::put(
            &mut batch,
            checkpoint_key,
            &record::BlockRecord::from_domain(&command.block.block),
        )?;

        if command.block.block.height.0 >= command.reorg_retention {
            let anchor_height = BlockHeight(
                command
                    .block
                    .block
                    .height
                    .0
                    .saturating_sub(command.reorg_retention),
            );
            Self::delete(
                &mut batch,
                keys::bundle(&self.config.scope, generation, anchor_height),
            );
            Self::delete(
                &mut batch,
                keys::backfill_projection_rollback(&self.config.scope, generation, anchor_height),
            );
            if let Some(pruned_height) = anchor_height.0.checked_sub(1) {
                Self::delete(
                    &mut batch,
                    keys::canonical(&self.config.scope, generation, BlockHeight(pruned_height)),
                );
            }
        }

        if let Some(rebuild) = rebuild {
            let rebuild_key = keys::rebuild_state(&self.config.scope);
            batch.conditions.push(Condition::Version {
                namespace: keys::namespace(),
                key: rebuild_key.clone(),
                expected: rebuild.version,
            });
            let mut next_rebuild = rebuild.value.clone();
            next_rebuild.checkpoint = Some(record::BlockRecord::from_domain(&command.block.block));
            Self::put(&mut batch, rebuild_key, &next_rebuild)?;
        }

        self.append_projection_revision(&mut batch).await?;

        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(BlockOutcome::Applied)
    }

    fn update_pending_confirmation(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        height: BlockHeight,
        transaction_id: &TransactionRef,
        transition: &Transition,
        pending_records: &BTreeMap<TransactionRef, (Key, Version)>,
    ) -> Result<(), IndexError> {
        let status = record::TransactionStatusRecord::into_domain(
            transition.next.transaction.status.clone(),
        );
        if transition.included_here && matches!(status, TransactionStatus::Included { .. }) {
            let pending_key =
                keys::pending_confirmation(&self.config.scope, generation, height, transaction_id);
            batch.conditions.push(Condition::Missing {
                namespace: keys::namespace(),
                key: pending_key.clone(),
            });
            return Self::put(
                batch,
                pending_key,
                &PendingConfirmation {
                    transaction_id: record::ScopedValue::from_transaction(transaction_id),
                    inclusion_height: height.0,
                },
            );
        }
        if transition.included_here || !matches!(status, TransactionStatus::Confirmed { .. }) {
            return Ok(());
        }
        let (pending_key, pending_version) =
            pending_records.get(transaction_id).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "confirmation transition lost its pending index",
                    false,
                )
            })?;
        batch.conditions.push(Condition::Version {
            namespace: keys::namespace(),
            key: pending_key.clone(),
            expected: *pending_version,
        });
        Self::delete(batch, pending_key.clone());
        Ok(())
    }
}
