use super::*;

impl Repository {
    pub(super) async fn load_revert_context(
        &self,
        command: &RevertTip,
    ) -> Result<RevertContext<IndexUndo>, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let checkpoint = self
            .get_record::<BlockRecord>(&keys::canonical_checkpoint(&self.scope))
            .await?;
        let current_checkpoint = checkpoint
            .as_ref()
            .map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value.clone()));
        if current_checkpoint.as_ref() != Some(&command.expected_tip) {
            return Ok(RevertContext {
                checkpoint: current_checkpoint,
                block: None,
            });
        }
        let bundle_key = keys::bundle(&self.scope, command.expected_tip.height);
        let Some(bundle) = self.get_record::<BundleRecord>(&bundle_key).await? else {
            return Ok(RevertContext {
                checkpoint: current_checkpoint,
                block: None,
            });
        };
        let mut observations = Vec::with_capacity(bundle.value.changes.len());
        for change in &bundle.value.changes {
            let transaction_id =
                record::ScopedValue::into_transaction(change.transaction_id.clone());
            let current = self
                .current_observation(&transaction_id)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "undo data references a missing observation",
                        false,
                    )
                })?;
            observations.push(RevertObservation {
                current: Self::stored_observation(current.value)?,
                prior: change
                    .prior
                    .clone()
                    .map(Self::stored_observation)
                    .transpose()?,
                included_here: change.included_here,
            });
        }
        Ok(RevertContext {
            checkpoint: current_checkpoint,
            block: Some(RevertBlock {
                block: record::BlockRecord::into_domain(bundle.value.block.clone()),
                prior_checkpoint: bundle
                    .value
                    .prior_checkpoint
                    .clone()
                    .map(record::BlockRecord::into_domain),
                undo: index_record::decode_undo(&bundle.value.encoded_undo)?,
                observations,
            }),
        })
    }

    pub(super) async fn persist_revert(
        &self,
        plan: RevertPlan<IndexUndo>,
    ) -> Result<(), IndexError> {
        self.check_scope(&plan.scope)?;
        self.verify_metadata().await?;
        let checkpoint_key = keys::canonical_checkpoint(&self.scope);
        let checkpoint = self.get_record::<BlockRecord>(&checkpoint_key).await?;
        if checkpoint
            .as_ref()
            .map(|value| record::BlockRecord::into_domain(value.value.clone()))
            .as_ref()
            != Some(&plan.expected_tip)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "revert checkpoint changed",
                true,
            ));
        }
        let canonical_key = keys::canonical(&self.scope, plan.expected_tip.height);
        let canonical = self
            .get_record::<BlockRecord>(&canonical_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Conflict,
                    "revert canonical block changed",
                    true,
                )
            })?;
        let bundle_key = keys::bundle(&self.scope, plan.expected_tip.height);
        let bundle = self
            .get_record::<BundleRecord>(&bundle_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(IndexErrorKind::Conflict, "revert undo data changed", true)
            })?;
        let rollback_projection = index_record::rollback_projection(&plan.undo)?;
        let mut batch = self.mutation_batch().await?;
        Self::condition_for(&mut batch, checkpoint_key.clone(), checkpoint.as_ref());
        Self::condition_for(&mut batch, canonical_key.clone(), Some(&canonical));
        Self::condition_for(&mut batch, bundle_key.clone(), Some(&bundle));
        self.append_projection_batch(&mut batch, &rollback_projection, IndexErrorKind::Store)
            .await?;
        for (transaction_id, planned) in &plan.transitions {
            let current = self
                .current_observation(transaction_id)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "undo bundle references a missing current observation",
                        false,
                    )
                })?;
            let transition = Transition {
                prior: Some(current.value.clone()),
                prior_version: Some(current.version),
                next: CurrentObservation {
                    transaction: record::ObservationRecord::from_domain(&planned.next.transaction),
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
            };
            self.append_transition(&mut batch, &transition)?;
            self.append_pending(&mut batch, transaction_id, &transition.pending)
                .await?;
        }
        Self::delete(&mut batch, bundle_key);
        Self::delete(&mut batch, canonical_key);
        match &plan.checkpoint {
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
        self.current_observation(&request.transaction_id)
            .await?
            .map(|current| record::ObservationRecord::into_domain(current.value.transaction))
            .transpose()
    }
}
