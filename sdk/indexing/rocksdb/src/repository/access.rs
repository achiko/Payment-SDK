use super::*;

impl BlockStore for Repository {
    fn commit_block<'a>(
        &'a self,
        plan: CommitPlan<IndexChanges, IndexUndo>,
    ) -> crate::BoxFuture<'a, Result<BlockOutcome, IndexError>> {
        Box::pin(async move { self.commit_generation(plan).await })
    }

    fn load_revert<'a>(
        &'a self,
        command: &'a RevertTip,
    ) -> crate::BoxFuture<'a, Result<RevertContext<IndexUndo>, IndexError>> {
        Box::pin(async move { self.load_revert_context(command).await })
    }

    fn save_revert<'a>(
        &'a self,
        plan: RevertPlan<IndexUndo>,
    ) -> crate::BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move { self.persist_revert(plan).await })
    }
}

impl HistoryStore for Repository {
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

impl StatusStore for Repository {
    fn status<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> crate::BoxFuture<'a, Result<Option<SyncStatus>, IndexError>> {
        Box::pin(async move { self.query_status(scope).await })
    }

    fn set_status<'a>(
        &'a self,
        status: SyncStatus,
    ) -> crate::BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move { self.persist_status(status).await })
    }
}

impl Repository {
    pub(crate) async fn projection_get(
        &self,
        request: ProjectionGet,
    ) -> Result<ProjectionResult, IndexError> {
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
        let key = keys::projection(&request.scope, &request.key);
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
    }

    pub(crate) async fn projection_scan(
        &self,
        request: ProjectionScan,
    ) -> Result<ProjectionPage, IndexError> {
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

        let base_prefix = keys::projection_prefix(&request.scope, &[]);
        let physical_prefix = keys::projection_prefix(&request.scope, &request.prefix);
        let page = self
            .storage
            .scan(ScanRequest {
                namespace: keys::namespace(),
                prefix: physical_prefix,
                after: request
                    .after
                    .as_ref()
                    .map(|cursor| keys::projection(&request.scope, &cursor.key)),
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
        Self::ensure_projection_snapshot(&snapshot, &after, "projection changed during the scan")?;

        Ok(ProjectionPage {
            snapshot,
            entries,
            next,
        })
    }
}
