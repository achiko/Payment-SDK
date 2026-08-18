use super::*;

impl Repository {
    pub(super) async fn load_commit_context(
        &self,
        command: &CommitBlock<IndexChanges, IndexUndo>,
    ) -> Result<CommitContext, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let checkpoint = self
            .generation_checkpoint()
            .await?
            .map(|value| record::BlockRecord::into_domain(value.value));
        let watch_version = WatchVersion(
            self.watch_version_record()
                .await?
                .as_ref()
                .map_or(0, |value| value.value.value),
        );
        let active_watches = self.active_watch_ids(command.block.block.height).await?;
        let pending = self
            .scan_records::<PendingConfirmation>(keys::pending_confirmation_prefix(&self.scope))
            .await?;
        let mut observations = BTreeMap::new();
        let mut pending_confirmations = BTreeSet::new();
        for (_, pending) in pending {
            let transaction_id =
                record::ScopedValue::into_transaction(pending.value.transaction_id);
            let current = self
                .current_observation(&transaction_id)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "pending confirmation references missing observation",
                        false,
                    )
                })?;
            pending_confirmations.insert(transaction_id.clone());
            observations.insert(transaction_id, Self::stored_observation(current.value)?);
        }
        for draft in &command.block.drafts {
            if !observations.contains_key(&draft.transaction_id)
                && let Some(current) = self.current_observation(&draft.transaction_id).await?
            {
                observations.insert(
                    draft.transaction_id.clone(),
                    Self::stored_observation(current.value)?,
                );
            }
        }
        Ok(CommitContext {
            checkpoint,
            watch_version,
            active_watches,
            observations,
            pending_confirmations,
        })
    }

    pub(super) fn stored_observation(
        value: CurrentObservation,
    ) -> Result<StoredObservation, IndexError> {
        Ok(StoredObservation {
            transaction: record::ObservationRecord::into_domain(value.transaction)?,
            watch_ids: value.watch_ids.into_iter().map(WatchId).collect(),
        })
    }

    pub(super) async fn commit_generation(
        &self,
        plan: CommitPlan<IndexChanges, IndexUndo>,
    ) -> Result<BlockOutcome, IndexError> {
        self.check_scope(&plan.scope)?;

        let canonical_key = keys::canonical(&self.scope, plan.block.height);
        if let Some(existing) = self.get_record::<BlockRecord>(&canonical_key).await? {
            let existing = record::BlockRecord::into_domain(existing.value);
            if existing == plan.block {
                return Ok(BlockOutcome::AlreadyApplied);
            }
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "another canonical hash is already stored at the block height",
                true,
            ));
        }
        self.ensure_semantic_available().await?;

        let mut batch = self.mutation_batch().await?;
        let checkpoint_key = keys::canonical_checkpoint(&self.scope);
        let checkpoint = self.get_record::<BlockRecord>(&checkpoint_key).await?;
        let persisted_checkpoint = checkpoint
            .as_ref()
            .map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value.clone()));
        if persisted_checkpoint != plan.expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "expected checkpoint no longer matches persistent state",
                true,
            ));
        }
        if let Some(checkpoint) = &persisted_checkpoint {
            let expected_height = checkpoint.height.0.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "checkpoint height is exhausted",
                    false,
                )
            })?;
            if plan.block.height != BlockHeight(expected_height)
                || plan.block.parent_hash.as_ref() != Some(&checkpoint.hash)
            {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "block does not immediately connect to the persistent checkpoint",
                    true,
                ));
            }
        }

        let watch_version_key = keys::watch_version(&self.scope);
        let watch_version = self.watch_version_record().await?;
        let persisted_watch_version = watch_version.as_ref().map_or(0, |value| value.value.value);
        if persisted_watch_version != plan.expected_watch_version.0 {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "watch set changed while the block was interpreted",
                true,
            ));
        }
        Self::condition_for(&mut batch, watch_version_key, watch_version.as_ref());
        let mut current_records = BTreeMap::new();
        for transaction_id in plan.transitions.keys() {
            if !current_records.contains_key(transaction_id)
                && let Some(current) = self.current_observation(transaction_id).await?
            {
                current_records.insert(transaction_id.clone(), current);
            }
        }
        let transitions = plan
            .transitions
            .iter()
            .map(|(transaction_id, planned)| {
                let current = current_records.get(transaction_id);
                (
                    transaction_id.clone(),
                    Transition {
                        prior: current.map(|value| value.value.clone()),
                        prior_version: current.map(|value| value.version),
                        next: CurrentObservation {
                            transaction: record::ObservationRecord::from_domain(
                                &planned.next.transaction,
                            ),
                            watch_ids: planned
                                .next
                                .watch_ids
                                .iter()
                                .map(|id| id.0.clone())
                                .collect(),
                        },
                        included_here: planned.included_here,
                        prior_addresses: planned.prior_addresses.clone(),
                        next_addresses: planned.next_addresses.clone(),
                        pending: planned.pending.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        for (transaction_id, transition) in &transitions {
            self.append_transition(&mut batch, transition)?;
            self.append_pending(&mut batch, transaction_id, &transition.pending)
                .await?;
        }
        let projection = index_record::project(&plan.effect)?;
        self.append_projection_batch(&mut batch, &projection, IndexErrorKind::InvalidBlock)
            .await?;

        let encoded_undo = index_record::encode_undo(&plan.undo)?;
        let bundle = BundleRecord {
            block: record::BlockRecord::from_domain(&plan.block),
            prior_checkpoint: plan
                .expected_checkpoint
                .as_ref()
                .map(record::BlockRecord::from_domain),
            encoded_undo,
            changes: transitions
                .values()
                .map(|transition| BundleChange {
                    transaction_id: transition.next.transaction.transaction_id.clone(),
                    prior: transition.prior.clone(),
                    included_here: transition.included_here,
                })
                .collect(),
        };
        let bundle_key = keys::bundle(&self.scope, plan.block.height);
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
            &record::BlockRecord::from_domain(&plan.block),
        )?;
        Self::put(&mut batch, bundle_key, &bundle)?;
        Self::condition_for(&mut batch, checkpoint_key.clone(), checkpoint.as_ref());
        Self::put(
            &mut batch,
            checkpoint_key,
            &record::BlockRecord::from_domain(&plan.block),
        )?;

        if let Some(anchor_height) = plan.prune_before {
            Self::delete(&mut batch, keys::bundle(&self.scope, anchor_height));
            if let Some(pruned_height) = anchor_height.0.checked_sub(1) {
                Self::delete(
                    &mut batch,
                    keys::canonical(&self.scope, BlockHeight(pruned_height)),
                );
            }
        }

        self.append_projection_revision(&mut batch).await?;

        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(BlockOutcome::Applied)
    }
}
