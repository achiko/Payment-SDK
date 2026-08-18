use super::*;

impl<S, C> ChainWriter for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn commit_block<'a>(
        &'a self,
        command: CommitBlock<Self::Effect, Self::Undo>,
    ) -> crate::BoxFuture<'a, Result<BlockOutcome, IndexError>> {
        Box::pin(async move {
            let active = self.active_generation_record().await?;
            let generation = RebuildGeneration(
                active
                    .as_ref()
                    .map_or(BASE_GENERATION.0, |active| active.value.value),
            );
            self.commit_generation(command, generation, true, active.as_ref(), None)
                .await
        })
    }

    fn revert_tip<'a>(
        &'a self,
        command: RevertTip,
    ) -> crate::BoxFuture<'a, Result<RevertOutcome, IndexError>> {
        Box::pin(async move { self.revert_active_tip(command).await })
    }
}

impl<S, C> TransactionReader for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn transaction<'a>(
        &'a self,
        request: TransactionQuery,
    ) -> crate::BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        Box::pin(async move { self.query_transaction(request).await })
    }

    fn transactions_by_address<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> crate::BoxFuture<'a, Result<TransactionPage, IndexError>> {
        Box::pin(async move { self.query_transactions_by_address(request).await })
    }
}

impl<S, C> WatchLookup for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn watches_for_address<'a>(
        &'a self,
        request: AddressQuery,
    ) -> crate::BoxFuture<'a, Result<Vec<WatchReceipt>, IndexError>> {
        Box::pin(async move { self.query_watches_for_address(request).await })
    }
}

impl<S, C> EventReader for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn events<'a>(
        &'a self,
        request: EventQuery,
    ) -> crate::BoxFuture<'a, Result<EventPage, IndexError>> {
        Box::pin(async move { self.query_events(request).await })
    }

    fn event_high_water<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> crate::BoxFuture<'a, Result<Option<EventCursor>, IndexError>> {
        Box::pin(async move {
            self.check_scope(scope)?;
            self.verify_metadata().await?;
            self.counter(&keys::event_counter(scope))
                .await
                .map(|counter| counter.map(|counter| EventCursor(counter.value.value)))
        })
    }
}

impl<S, C> StatusStore for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn status<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> crate::BoxFuture<'a, Result<SyncStatus, IndexError>> {
        Box::pin(async move { self.query_status(scope).await })
    }

    fn set_status<'a>(
        &'a self,
        status: SyncStatus,
    ) -> crate::BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move { self.persist_status(status).await })
    }
}

impl<S, C> RebuildReader for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn rebuild_state<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> crate::BoxFuture<'a, Result<Option<RebuildState>, IndexError>> {
        Box::pin(async move { self.query_rebuild_state(scope).await })
    }
}

impl<S, C> RebuildBuilder for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn begin_rebuild<'a>(
        &'a self,
        command: BeginRebuild,
    ) -> crate::BoxFuture<'a, Result<RebuildState, IndexError>> {
        Box::pin(async move { self.start_rebuild(command).await })
    }

    fn commit_rebuild_block<'a>(
        &'a self,
        command: RebuildBlock<Self::Effect, Self::Undo>,
    ) -> crate::BoxFuture<'a, Result<BlockOutcome, IndexError>> {
        Box::pin(async move { self.commit_shadow_block(command).await })
    }

    fn validate_rebuild<'a>(
        &'a self,
        command: RebuildValidation,
    ) -> crate::BoxFuture<'a, Result<RebuildState, IndexError>> {
        Box::pin(async move { self.mark_rebuild_validating(command).await })
    }
}

impl<S, C> RebuildPublisher for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn prepare_rebuild_activation<'a>(
        &'a self,
        command: PrepareActivation,
    ) -> crate::BoxFuture<'a, Result<RebuildState, IndexError>> {
        Box::pin(async move { self.prepare_rebuild(command).await })
    }

    fn activate_rebuild<'a>(
        &'a self,
        command: RebuildActivation,
    ) -> crate::BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move { self.publish_rebuild(command).await })
    }
}

impl<S, C> RebuildAdmin for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn abort_rebuild<'a>(
        &'a self,
        command: AbortRebuild,
    ) -> crate::BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move { self.cancel_rebuild(command).await })
    }

    fn cleanup_generation<'a>(
        &'a self,
        command: CleanupGeneration,
    ) -> crate::BoxFuture<'a, Result<CleanupOutcome, IndexError>> {
        Box::pin(async move { self.remove_generation(command).await })
    }
}

impl<S, C> ProjectionQuery for Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    fn projection_get<'a>(
        &'a self,
        request: ProjectionGet,
    ) -> crate::BoxFuture<'a, Result<ProjectionResult, IndexError>> {
        Box::pin(async move {
            self.check_scope(&request.scope)?;
            self.verify_metadata().await?;
            self.ensure_semantic_available().await?;

            let snapshot = self.projection_snapshot().await?;
            if let Some(expected) = &request.expected_snapshot {
                Self::ensure_projection_snapshot(
                    expected,
                    &snapshot,
                    "projection changed before the dependent lookup",
                )?;
            }
            let key = keys::projection(&request.scope, snapshot.generation, &request.key);
            let value = self
                .get_projection_record(&key)
                .await?
                .map(|record| record.value);
            let after = self.projection_snapshot().await?;
            Self::ensure_projection_snapshot(
                &snapshot,
                &after,
                "projection changed during the lookup",
            )?;

            Ok(ProjectionResult { snapshot, value })
        })
    }

    fn projection_scan<'a>(
        &'a self,
        request: ProjectionScan,
    ) -> crate::BoxFuture<'a, Result<ProjectionPage, IndexError>> {
        Box::pin(async move {
            self.check_scope(&request.scope)?;
            self.verify_metadata().await?;
            self.ensure_semantic_available().await?;
            Self::validate_query_limit(request.limit)?;

            let snapshot = self.projection_snapshot().await?;
            if let Some(after) = &request.after {
                Self::ensure_projection_snapshot(
                    &after.snapshot,
                    &snapshot,
                    "projection cursor belongs to a snapshot that is no longer current",
                )?;
                if !after.key.starts_with(&request.prefix) {
                    return Err(IndexError::new(
                        IndexErrorKind::InvalidRequest,
                        "projection cursor does not match the requested prefix",
                        false,
                    ));
                }
            }

            let base_prefix = keys::projection_prefix(&request.scope, snapshot.generation, &[]);
            let physical_prefix =
                keys::projection_prefix(&request.scope, snapshot.generation, &request.prefix);
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: keys::namespace(),
                    prefix: physical_prefix,
                    after: request.after.as_ref().map(|cursor| {
                        keys::projection(&request.scope, snapshot.generation, &cursor.key)
                    }),
                    limit: request.limit,
                })
                .await
                .map_err(Self::storage_error)?;

            let relative_key = |key: Key| {
                key.0
                    .strip_prefix(base_prefix.as_slice())
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Store,
                            "projection scan returned a key outside its generation prefix",
                            false,
                        )
                    })
            };
            let entries = page
                .entries
                .into_iter()
                .map(|(key, stored)| {
                    Ok(ProjectionEntry {
                        key: relative_key(key)?,
                        value: stored.value.0,
                    })
                })
                .collect::<Result<Vec<_>, IndexError>>()?;
            let next = page
                .next
                .map(relative_key)
                .transpose()?
                .map(|key| ProjectionCursor {
                    snapshot: snapshot.clone(),
                    key,
                });
            let after = self.projection_snapshot().await?;
            Self::ensure_projection_snapshot(
                &snapshot,
                &after,
                "projection changed during the scan",
            )?;

            Ok(ProjectionPage {
                snapshot,
                entries,
                next,
            })
        })
    }
}
