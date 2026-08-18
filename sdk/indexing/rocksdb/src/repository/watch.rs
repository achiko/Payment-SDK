use super::*;

impl<S, C> IndexTypes for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    type Target = C::Target;
    type Effect = C::Effect;
    type Undo = C::Undo;
}

impl<S, C> CanonicalReader for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> crate::BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            self.check_scope(scope)?;
            self.verify_metadata().await?;
            let generation = self.active_generation().await?;
            self.generation_checkpoint(generation)
                .await
                .map(|checkpoint| {
                    checkpoint.map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value))
                })
        })
    }

    fn canonical_block<'a>(
        &'a self,
        scope: &'a IndexScope,
        height: BlockHeight,
    ) -> crate::BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            self.check_scope(scope)?;
            self.verify_metadata().await?;
            let generation = self.active_generation().await?;
            self.get_record::<BlockRecord>(&keys::canonical(scope, generation, height))
                .await
                .map(|block| block.map(|block| record::BlockRecord::into_domain(block.value)))
        })
    }
}

impl<S, C> WatchReader for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn watches_at<'a>(
        &'a self,
        scope: &'a IndexScope,
        height: BlockHeight,
    ) -> crate::BoxFuture<'a, Result<WatchSnapshot<Self::Target>, IndexError>> {
        Box::pin(async move {
            self.check_scope(scope)?;
            self.verify_metadata().await?;
            let watch_version = self.watch_version_record().await?;
            let records = self
                .scan_records::<WatchRecord>(keys::watch_prefix(scope))
                .await?;
            let watches = records
                .into_iter()
                .filter(|(_, watch)| {
                    watch.value.start_height <= height.0
                        && watch
                            .value
                            .inactive_from
                            .is_none_or(|inactive| height.0 < inactive)
                })
                .map(|(_, watch)| {
                    let record_scope = record::ScopeRecord::into_domain(watch.value.scope.clone());
                    record::ensure_record_scope(scope, &record_scope, "watch")?;
                    Ok(WatchTarget {
                        id: WatchId(watch.value.id),
                        scope: record_scope,
                        selector: record::SelectorRecord::into_domain(watch.value.selector),
                        target: self.records.decode_target(&watch.value.encoded_target)?,
                        idempotency_key: watch.value.idempotency_key,
                        start_height: BlockHeight(watch.value.start_height),
                        registered_at: watch
                            .value
                            .registered_at
                            .map(record::BlockRecord::into_domain),
                        inactive_from: watch.value.inactive_from.map(BlockHeight),
                    })
                })
                .collect::<Result<Vec<_>, IndexError>>()?;
            Ok(WatchSnapshot {
                version: WatchVersion(
                    watch_version
                        .as_ref()
                        .map_or(0, |version| version.value.value),
                ),
                watches,
            })
        })
    }
}

impl<S, C> BackfillReader for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn pending_watch_backfills<'a>(
        &'a self,
        scope: &'a IndexScope,
        limit: usize,
    ) -> crate::BoxFuture<'a, Result<Vec<WatchBackfill>, IndexError>> {
        Box::pin(async move { self.query_watch_backfills(scope, limit).await })
    }
}

impl<S, C> BackfillWriter for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn commit_watch_backfill<'a>(
        &'a self,
        command: CommitBackfill,
    ) -> crate::BoxFuture<'a, Result<BackfillOutcome, IndexError>> {
        Box::pin(async move {
            self.apply_watch_backfill(command, ProjectionBatch::default())
                .await
        })
    }

    fn commit_watch_backfill_effect<'a>(
        &'a self,
        command: CommitBackfill,
        effect: Self::Effect,
    ) -> crate::BoxFuture<'a, Result<BackfillOutcome, IndexError>> {
        Box::pin(async move {
            let projection = self.records.project(&effect)?;
            self.apply_watch_backfill(command, projection).await
        })
    }
}

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn register_watch_impl<'a>(
        &'a self,
        command: RegisterWatch<C::Target>,
    ) -> crate::BoxFuture<'a, Result<WatchOutcome, IndexError>> {
        Box::pin(async move {
            self.check_scope(&command.request.scope)?;
            self.verify_metadata().await?;
            self.ensure_semantic_available().await?;
            if command.request.idempotency_key.trim().is_empty() {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "watch idempotency key must not be empty",
                    false,
                ));
            }
            if command.request.start_height < self.config.bootstrap_height {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "watch start height precedes the configured bootstrap height",
                    false,
                ));
            }
            match &command.request.selector {
                WatchSelector::Address(address) => self.validate_address(address)?,
                WatchSelector::Transaction(transaction) => {
                    self.validate_transaction_id(transaction)?
                }
            }
            let encoded_target = self.records.encode_target(&command.target)?;
            let idempotency_key =
                keys::watch_idempotency(&self.config.scope, &command.request.idempotency_key);
            if let Some(existing_id) = self.get_record::<WatchIdentity>(&idempotency_key).await? {
                let existing_key = keys::watch(&self.config.scope, &existing_id.value.watch_id);
                let existing = self
                    .get_record::<WatchRecord>(&existing_key)
                    .await?
                    .ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Store,
                            "watch idempotency record references a missing watch",
                            false,
                        )
                    })?;
                let same_payload = existing.value.scope
                    == record::ScopeRecord::from_domain(&command.request.scope)
                    && existing.value.selector
                        == record::SelectorRecord::from_domain(&command.request.selector)
                    && existing.value.start_height == command.request.start_height.0
                    && existing.value.encoded_target == encoded_target;
                if !same_payload {
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "watch idempotency key was reused with a different payload",
                        false,
                    ));
                }
                return Ok(WatchOutcome::Existing(self.watch_receipt(&existing.value)));
            }

            let mut batch = self.mutation_batch().await?;
            let active_generation = self.active_generation_record().await?;
            let generation = RebuildGeneration(
                active_generation
                    .as_ref()
                    .map_or(BASE_GENERATION.0, |active| active.value.value),
            );
            let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, generation);
            let checkpoint = self.get_record::<BlockRecord>(&checkpoint_key).await?;
            let persisted_checkpoint = checkpoint
                .as_ref()
                .map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value.clone()));
            if command.registered_at != persisted_checkpoint {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "watch registration checkpoint changed before durable acknowledgement",
                    true,
                ));
            }
            Self::condition_for(
                &mut batch,
                keys::active_generation(&self.config.scope),
                active_generation.as_ref(),
            );
            Self::condition_for(&mut batch, checkpoint_key, checkpoint.as_ref());
            let watch_counter_key = keys::watch_counter(&self.config.scope);
            let watch_version_key = keys::watch_version(&self.config.scope);
            let watch_counter = self.counter(&watch_counter_key).await?;
            let watch_version = self.watch_version_record().await?;
            let next_watch = watch_counter.as_ref().map_or(Ok(1), |counter| {
                counter.value.value.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "watch ID counter is exhausted",
                        false,
                    )
                })
            })?;
            let next_version = watch_version.as_ref().map_or(Ok(1), |version| {
                version.value.value.checked_add(1).ok_or_else(|| {
                    IndexError::new(IndexErrorKind::Store, "watch version is exhausted", false)
                })
            })?;
            let watch_id = WatchId(format!("watch-{next_watch:020}"));
            let watch = WatchRecord {
                id: watch_id.0.clone(),
                scope: record::ScopeRecord::from_domain(&command.request.scope),
                selector: record::SelectorRecord::from_domain(&command.request.selector),
                encoded_target,
                idempotency_key: command.request.idempotency_key.clone(),
                start_height: command.request.start_height.0,
                registered_at: command
                    .registered_at
                    .as_ref()
                    .map(record::BlockRecord::from_domain),
                inactive_from: None,
            };
            let watch_key = keys::watch(&self.config.scope, &watch_id.0);
            Self::condition_for(
                &mut batch,
                watch_counter_key.clone(),
                watch_counter.as_ref(),
            );
            Self::condition_for(
                &mut batch,
                watch_version_key.clone(),
                watch_version.as_ref(),
            );
            Self::condition_for::<WatchIdentity>(&mut batch, idempotency_key.clone(), None);
            Self::condition_for::<WatchRecord>(&mut batch, watch_key.clone(), None);
            Self::put(
                &mut batch,
                watch_counter_key,
                &CounterRecord { value: next_watch },
            )?;
            Self::put(
                &mut batch,
                watch_version_key,
                &CounterRecord {
                    value: next_version,
                },
            )?;
            Self::put(
                &mut batch,
                idempotency_key,
                &WatchIdentity {
                    watch_id: watch_id.0.clone(),
                },
            )?;
            Self::put(&mut batch, watch_key, &watch)?;
            if let Some(through) = checkpoint
                .as_ref()
                .filter(|checkpoint| command.request.start_height.0 <= checkpoint.value.height)
            {
                let backfill_key = keys::watch_backfill(&self.config.scope, &watch_id.0);
                Self::condition_for::<BackfillRecord>(&mut batch, backfill_key.clone(), None);
                Self::put(
                    &mut batch,
                    backfill_key,
                    &BackfillRecord {
                        scope: record::ScopeRecord::from_domain(&self.config.scope),
                        watch_id: watch_id.0.clone(),
                        from_height: command.request.start_height.0,
                        next_height: command.request.start_height.0,
                        through: through.value.clone(),
                    },
                )?;
            }
            self.storage
                .commit(batch)
                .await
                .map_err(Self::storage_error)?;
            Ok(WatchOutcome::Registered(self.watch_receipt(&watch)))
        })
    }
}

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn deactivate_impl<'a>(
        &'a self,
        command: DeactivateWatch,
    ) -> crate::BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Box::pin(async move {
            self.check_scope(&command.scope)?;
            self.verify_metadata().await?;
            self.ensure_semantic_available().await?;
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
            if current_checkpoint != command.expected_checkpoint {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "canonical checkpoint changed before watch deactivation",
                    true,
                ));
            }
            let watch_key = keys::watch(&self.config.scope, &command.watch_id.0);
            let watch = self
                .get_record::<WatchRecord>(&watch_key)
                .await?
                .ok_or_else(|| {
                    IndexError::new(IndexErrorKind::InvalidWatch, "unknown watch ID", false)
                })?;
            if watch.value.inactive_from.is_some() {
                return Ok(UnwatchOutcome::AlreadyInactive);
            }
            let backfill_key = keys::watch_backfill(&self.config.scope, &command.watch_id.0);
            if self
                .get_record::<BackfillRecord>(&backfill_key)
                .await?
                .is_some()
            {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "watch cannot become inactive while historical backfill is pending",
                    true,
                ));
            }
            if command.inactive_from.0 < watch.value.start_height {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "watch cannot become inactive before its start height",
                    false,
                ));
            }
            let mut batch = self.mutation_batch().await?;
            let watch_version_key = keys::watch_version(&self.config.scope);
            let watch_version = self.watch_version_record().await?;
            let next_version = watch_version.as_ref().map_or(Ok(1), |version| {
                version.value.value.checked_add(1).ok_or_else(|| {
                    IndexError::new(IndexErrorKind::Store, "watch version is exhausted", false)
                })
            })?;
            let mut updated = watch.value.clone();
            updated.inactive_from = Some(command.inactive_from.0);
            Self::condition_for(&mut batch, watch_key.clone(), Some(&watch));
            Self::condition_for(
                &mut batch,
                keys::active_generation(&self.config.scope),
                active.as_ref(),
            );
            Self::condition_for(&mut batch, checkpoint_key, checkpoint.as_ref());
            Self::condition_for(
                &mut batch,
                watch_version_key.clone(),
                watch_version.as_ref(),
            );
            Self::condition_for::<BackfillRecord>(&mut batch, backfill_key, None);
            Self::put(&mut batch, watch_key, &updated)?;
            Self::put(
                &mut batch,
                watch_version_key,
                &CounterRecord {
                    value: next_version,
                },
            )?;
            self.storage
                .commit(batch)
                .await
                .map_err(Self::storage_error)?;
            Ok(UnwatchOutcome::Deactivated)
        })
    }
}

impl<S, C> WatchStore for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn register_watch<'a>(
        &'a self,
        command: RegisterWatch<Self::Target>,
    ) -> crate::BoxFuture<'a, Result<WatchOutcome, IndexError>> {
        self.register_watch_impl(command)
    }

    fn deactivate<'a>(
        &'a self,
        command: DeactivateWatch,
    ) -> crate::BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        self.deactivate_impl(command)
    }
}
