use super::*;

impl CanonicalStore for Repository {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> crate::BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            self.check_scope(scope)?;
            self.verify_metadata().await?;
            self.generation_checkpoint().await.map(|checkpoint| {
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
            self.get_record::<BlockRecord>(&keys::canonical(scope, height))
                .await
                .map(|block| block.map(|block| record::BlockRecord::into_domain(block.value)))
        })
    }

    fn load_commit<'a>(
        &'a self,
        command: &'a CommitBlock<IndexChanges, IndexUndo>,
    ) -> crate::BoxFuture<'a, Result<CommitContext, IndexError>> {
        Box::pin(async move { self.load_commit_context(command).await })
    }
}

impl WatchStore for Repository {
    fn watches_at<'a>(
        &'a self,
        scope: &'a IndexScope,
        height: BlockHeight,
    ) -> crate::BoxFuture<'a, Result<WatchSnapshot<WatchSelector>, IndexError>> {
        Box::pin(async move {
            self.check_scope(scope)?;
            self.verify_metadata().await?;
            let watch_version = self.watch_version_record().await?;
            let records = self
                .scan_records::<WatchRecord>(keys::watch_prefix(scope))
                .await?;
            let watches = records
                .into_iter()
                .filter(|(_, watch)| watch.value.start_height <= height.0)
                .map(|(_, watch)| {
                    let record_scope = record::ScopeRecord::into_domain(watch.value.scope.clone());
                    record::ensure_record_scope(scope, &record_scope, "watch")?;
                    Ok(WatchTarget {
                        id: WatchId(watch.value.id),
                        scope: record_scope,
                        selector: record::ScopedValue::into_address(watch.value.selector),
                        target: index_record::decode_target(&watch.value.encoded_target)?,
                        idempotency_key: watch.value.idempotency_key,
                        start_height: BlockHeight(watch.value.start_height),
                        registered_at: watch
                            .value
                            .registered_at
                            .map(record::BlockRecord::into_domain),
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

    fn load_watch<'a>(
        &'a self,
        command: &'a RegisterWatch<WatchSelector>,
    ) -> crate::BoxFuture<'a, Result<WatchContext<WatchSelector>, IndexError>> {
        Box::pin(async move { self.load_watch_context(command).await })
    }

    fn save_watch<'a>(
        &'a self,
        plan: WatchPlan<WatchSelector>,
    ) -> crate::BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move { self.persist_watch(plan).await })
    }
}

impl Repository {
    async fn load_watch_context(
        &self,
        command: &RegisterWatch<WatchSelector>,
    ) -> Result<WatchContext<WatchSelector>, IndexError> {
        self.check_scope(&command.request.scope)?;
        self.verify_metadata().await?;
        self.ensure_semantic_available().await?;
        let idempotency_key =
            keys::watch_idempotency(&self.scope, &command.request.idempotency_key);
        let existing =
            if let Some(existing_id) = self.get_record::<WatchIdentity>(&idempotency_key).await? {
                let existing_key = keys::watch(&self.scope, &existing_id.value.watch_id);
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
                Some(WatchTarget {
                    id: WatchId(existing.value.id),
                    scope: record::ScopeRecord::into_domain(existing.value.scope),
                    selector: record::ScopedValue::into_address(existing.value.selector),
                    target: index_record::decode_target(&existing.value.encoded_target)?,
                    idempotency_key: existing.value.idempotency_key,
                    start_height: BlockHeight(existing.value.start_height),
                    registered_at: existing
                        .value
                        .registered_at
                        .map(record::BlockRecord::into_domain),
                })
            } else {
                None
            };
        let checkpoint = self
            .generation_checkpoint()
            .await?
            .map(|value| record::BlockRecord::into_domain(value.value));
        let counter = self
            .counter(&keys::watch_counter(&self.scope))
            .await?
            .map_or(0, |value| value.value.value);
        let version = self
            .watch_version_record()
            .await?
            .map_or(0, |value| value.value.value);
        Ok(WatchContext {
            checkpoint,
            version: WatchVersion(version),
            next_id: counter,
            existing,
        })
    }

    async fn persist_watch(&self, plan: WatchPlan<WatchSelector>) -> Result<(), IndexError> {
        self.check_scope(&plan.watch.scope)?;
        self.verify_metadata().await?;
        let mut batch = self.mutation_batch().await?;
        let checkpoint_key = keys::canonical_checkpoint(&self.scope);
        let checkpoint = self.get_record::<BlockRecord>(&checkpoint_key).await?;
        let persisted = checkpoint
            .as_ref()
            .map(|value| record::BlockRecord::into_domain(value.value.clone()));
        if persisted != plan.expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "watch checkpoint changed",
                true,
            ));
        }
        Self::condition_for(&mut batch, checkpoint_key, checkpoint.as_ref());
        let watch_counter_key = keys::watch_counter(&self.scope);
        let watch_version_key = keys::watch_version(&self.scope);
        let watch_counter = self.counter(&watch_counter_key).await?;
        let watch_version = self.watch_version_record().await?;
        let current_version = watch_version.as_ref().map_or(0, |value| value.value.value);
        if current_version != plan.expected_version.0 {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "watch version changed",
                true,
            ));
        }
        let next_version = current_version.checked_add(1).ok_or_else(|| {
            IndexError::new(IndexErrorKind::Store, "watch version is exhausted", false)
        })?;
        let next_watch = watch_counter.as_ref().map_or(Ok(1), |value| {
            value.value.value.checked_add(1).ok_or_else(|| {
                IndexError::new(IndexErrorKind::Conflict, "watch counter changed", true)
            })
        })?;
        if plan.watch.id.0 != format!("watch-{next_watch:020}") {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "watch counter changed",
                true,
            ));
        }
        let watch = WatchRecord {
            id: plan.watch.id.0.clone(),
            scope: record::ScopeRecord::from_domain(&plan.watch.scope),
            selector: record::ScopedValue::from_address(&plan.watch.selector),
            encoded_target: index_record::encode_target(&plan.watch.target)?,
            idempotency_key: plan.watch.idempotency_key.clone(),
            start_height: plan.watch.start_height.0,
            registered_at: plan
                .watch
                .registered_at
                .as_ref()
                .map(record::BlockRecord::from_domain),
        };
        let watch_key = keys::watch(&self.scope, &plan.watch.id.0);
        let idempotency_key = keys::watch_idempotency(&self.scope, &plan.watch.idempotency_key);
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
                watch_id: plan.watch.id.0.clone(),
            },
        )?;
        Self::put(&mut batch, watch_key, &watch)?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(())
    }
}
