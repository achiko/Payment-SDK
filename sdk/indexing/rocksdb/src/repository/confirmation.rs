use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) async fn extend_backfill_confirmation_undo(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        inclusion: &BlockRef,
        live_checkpoint: &BlockRef,
        transitions: &BTreeMap<TransactionRef, Transition>,
        watch_id: &WatchId,
    ) -> Result<(), IndexError> {
        if transitions.is_empty()
            || self.config.confirmation_policy.minimum_confirmations <= 1
            || inclusion.height >= live_checkpoint.height
        {
            return Ok(());
        }

        let confirmation_height = inclusion
            .height
            .0
            .checked_add(
                self.config
                    .confirmation_policy
                    .minimum_confirmations
                    .saturating_sub(1),
            )
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "backfill confirmation height is exhausted",
                    false,
                )
            })?;
        let terminal_height = live_checkpoint.height.0.min(confirmation_height);
        let oldest_retained_bundle = live_checkpoint
            .height
            .0
            .saturating_sub(self.config.reorg_retention)
            .saturating_add(1);
        let first_height = inclusion
            .height
            .0
            .checked_add(1)
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "backfill inclusion height is exhausted",
                    false,
                )
            })?
            .max(oldest_retained_bundle);
        if first_height > terminal_height {
            return Ok(());
        }

        for height in first_height..=terminal_height {
            let height = BlockHeight(height);
            let bundle_key = keys::bundle(&self.config.scope, generation, height);
            let bundle = self
                .get_record::<BundleRecord>(&bundle_key)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::ReorgBeyondRetention,
                        "retained confirmation rollback bundle is missing",
                        false,
                    )
                })?;
            if bundle.value.block.height != height.0 {
                return Err(IndexError::new(
                    IndexErrorKind::Store,
                    "confirmation rollback bundle has an unexpected height",
                    false,
                ));
            }
            let mut updated = bundle.value.clone();
            for (transaction_id, transition) in transitions {
                Self::extend_confirmation_change(
                    &mut updated,
                    transaction_id,
                    transition,
                    inclusion,
                    height,
                    watch_id,
                )?;
            }
            if updated != bundle.value {
                Self::condition_for(batch, bundle_key.clone(), Some(&bundle));
                Self::put(batch, bundle_key, &updated)?;
            }
        }
        Ok(())
    }

    fn extend_confirmation_change(
        bundle: &mut BundleRecord,
        transaction_id: &TransactionRef,
        transition: &Transition,
        inclusion: &BlockRef,
        height: BlockHeight,
        watch_id: &WatchId,
    ) -> Result<(), IndexError> {
        if !matches!(
            record::TransactionStatusRecord::into_domain(
                transition.next.transaction.status.clone()
            ),
            TransactionStatus::Included { .. } | TransactionStatus::Confirmed { .. }
        ) {
            return Ok(());
        }
        let transaction_record = record::ScopedValue::from_transaction(transaction_id);
        let prior_was_canonical_here = transition.prior.as_ref().is_some_and(|prior| {
            match record::TransactionStatusRecord::into_domain(prior.transaction.status.clone()) {
                TransactionStatus::Included { block, .. }
                | TransactionStatus::Confirmed { block, .. } => block == *inclusion,
                TransactionStatus::Failed {
                    block: Some(block), ..
                } => block == *inclusion,
                TransactionStatus::Pending
                | TransactionStatus::Failed { block: None, .. }
                | TransactionStatus::Replaced { .. }
                | TransactionStatus::Dropped
                | TransactionStatus::Reorged { .. } => false,
            }
        });
        if prior_was_canonical_here {
            let change = bundle
                .changes
                .iter_mut()
                .find(|change| change.transaction_id == transaction_record)
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "canonical observation is missing retained confirmation undo",
                        false,
                    )
                })?;
            let prior = change.prior.as_mut().ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "confirmation rollback is missing its prior observation",
                    false,
                )
            })?;
            if !prior.watch_ids.contains(&watch_id.0) {
                prior.watch_ids.push(watch_id.0.clone());
                prior.watch_ids.sort();
                prior.watch_ids.dedup();
            }
            return Ok(());
        }
        if bundle
            .changes
            .iter()
            .any(|change| change.transaction_id == transaction_record)
        {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "new backfill observation conflicts with retained confirmation undo",
                false,
            ));
        }
        let prior_depth = height.0.checked_sub(inclusion.height.0).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::Store,
                "confirmation rollback height precedes the inclusion",
                false,
            )
        })?;
        let mut prior = transition.next.clone();
        let mut prior_domain = record::ObservationRecord::into_domain(prior.transaction.clone())?;
        prior_domain.status = TransactionStatus::Included {
            block: inclusion.clone(),
            confirmations: prior_depth,
        };
        prior.transaction = record::ObservationRecord::from_domain(&prior_domain);
        bundle.changes.push(BundleChange {
            transaction_id: transaction_record,
            prior: Some(prior),
            included_here: false,
        });
        Ok(())
    }
}
