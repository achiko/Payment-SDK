use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) fn validate_backfill_height(command: &CommitBackfill) -> Result<(), IndexError> {
        if command.block.height != command.expected_next_height {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "backfill block height differs from the expected job cursor",
                false,
            ));
        }
        Ok(())
    }

    pub(super) async fn replayed_backfill(
        &self,
        command: &CommitBackfill,
        marker_key: &Key,
        height_marker_key: &Key,
    ) -> Result<Option<BackfillOutcome>, IndexError> {
        let Some(marker) = self.get_record::<BackfillMarker>(marker_key).await? else {
            return Ok(None);
        };
        if record::BlockRecord::into_domain(marker.value.block) != command.block {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "backfill applied marker contains another canonical block",
                false,
            ));
        }
        let height_marker = self
            .get_record::<HeightMarker>(height_marker_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "backfill applied marker is missing its height index",
                    false,
                )
            })?;
        if height_marker.value.watch_id != command.watch_id.0 {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "backfill applied height index references another watch",
                false,
            ));
        }
        let next_height = self
            .get_record::<BackfillRecord>(&keys::watch_backfill(
                &self.config.scope,
                &command.watch_id.0,
            ))
            .await?
            .map(|job| BlockHeight(job.value.next_height));
        Ok(Some(BackfillOutcome::AlreadyApplied { next_height }))
    }

    pub(super) async fn previous_backfill_marker(
        &self,
        command: &CommitBackfill,
        job: &StoredRecord<BackfillRecord>,
    ) -> Result<Option<PreviousMarker>, IndexError> {
        if command.block.height.0 <= job.value.from_height {
            return Ok(None);
        }
        let previous_height = BlockHeight(command.block.height.0.saturating_sub(1));
        let previous_key =
            keys::watch_backfill_applied(&self.config.scope, &command.watch_id.0, previous_height);
        let previous = self
            .get_record::<BackfillMarker>(&previous_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Conflict,
                    "previous backfill height has not been durably applied",
                    true,
                )
            })?;
        let previous_height_key = keys::watch_backfill_applied_height(
            &self.config.scope,
            previous_height,
            &command.watch_id.0,
        );
        let previous_height_marker = self
            .get_record::<HeightMarker>(&previous_height_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "previous backfill marker is missing its height index",
                    false,
                )
            })?;
        if previous_height_marker.value.watch_id != command.watch_id.0 {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "previous backfill height index references another watch",
                false,
            ));
        }
        let previous_block = record::BlockRecord::into_domain(previous.value.block.clone());
        if command.block.parent_hash.as_ref() != Some(&previous_block.hash) {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "historical block does not connect to the prior backfill height",
                true,
            ));
        }
        Ok(Some((
            previous_key,
            previous,
            previous_height_key,
            previous_height_marker,
        )))
    }

    pub(super) fn reconcile_backfill_status(
        prior: &StoredRecord<CurrentObservation>,
        draft: &ObservationDraft,
        block: &BlockRef,
        watch_id: &WatchId,
        requested: TransactionStatus,
    ) -> Result<Option<TransactionStatus>, IndexError> {
        let prior_domain = record::ObservationRecord::into_domain(prior.value.transaction.clone())?;
        let same_canonical_block = match &prior_domain.status {
            TransactionStatus::Included { block: prior, .. }
            | TransactionStatus::Confirmed { block: prior, .. } => prior == block,
            TransactionStatus::Failed {
                block: Some(prior), ..
            } => prior == block,
            TransactionStatus::Pending
            | TransactionStatus::Failed { block: None, .. }
            | TransactionStatus::Replaced { .. }
            | TransactionStatus::Dropped
            | TransactionStatus::Reorged { .. } => false,
        };
        let prior_is_canonical = matches!(
            prior_domain.status,
            TransactionStatus::Included { .. }
                | TransactionStatus::Confirmed { .. }
                | TransactionStatus::Failed { block: Some(_), .. }
        );
        if prior_is_canonical && !same_canonical_block
            || prior_domain.movements != draft.movements
            || prior_domain.fee != draft.fee
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "backfill fact conflicts with the current transaction projection",
                false,
            ));
        }
        let status = if same_canonical_block {
            prior_domain.status.clone()
        } else {
            requested
        };
        if prior.value.watch_ids.contains(&watch_id.0) && prior_domain.status == status {
            return Ok(None);
        }
        Ok(Some(status))
    }

    pub(super) async fn extend_backfill_bundle(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        block: &BlockRef,
        transitions: &BTreeMap<TransactionRef, Transition>,
        introduced_projection_keys: Vec<Vec<u8>>,
    ) -> Result<(), IndexError> {
        let bundle_key = keys::bundle(&self.config.scope, generation, block.height);
        let Some(bundle) = self.get_record::<BundleRecord>(&bundle_key).await? else {
            return Ok(());
        };
        let mut updated = bundle.value.clone();
        let existing: BTreeSet<_> = updated
            .changes
            .iter()
            .map(|change| record::ScopedValue::into_transaction(change.transaction_id.clone()))
            .collect();
        for (transaction_id, transition) in transitions {
            if !existing.contains(transaction_id) {
                updated.changes.push(BundleChange {
                    transaction_id: record::ScopedValue::from_transaction(transaction_id),
                    prior: transition.prior.clone(),
                    included_here: true,
                });
            }
        }
        if updated != bundle.value {
            Self::condition_for(batch, bundle_key.clone(), Some(&bundle));
            Self::put(batch, bundle_key, &updated)?;
        }
        if introduced_projection_keys.is_empty() {
            return Ok(());
        }
        let rollback_key =
            keys::backfill_projection_rollback(&self.config.scope, generation, block.height);
        let rollback = self.get_record::<BackfillRollback>(&rollback_key).await?;
        let mut rollback_value = rollback.as_ref().map_or_else(
            || BackfillRollback {
                block: record::BlockRecord::from_domain(block),
                relative_keys: Vec::new(),
            },
            |rollback| rollback.value.clone(),
        );
        if record::BlockRecord::into_domain(rollback_value.block.clone()) != *block {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "historical projection rollback record belongs to another block",
                false,
            ));
        }
        let mut rollback_keys = rollback_value
            .relative_keys
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if rollback_keys.len() != rollback_value.relative_keys.len() {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "historical projection rollback record contains duplicate keys",
                false,
            ));
        }
        rollback_keys.extend(introduced_projection_keys);
        rollback_value.relative_keys = rollback_keys.into_iter().collect();
        Self::condition_for(batch, rollback_key.clone(), rollback.as_ref());
        Self::put(batch, rollback_key, &rollback_value)?;
        Ok(())
    }
}
