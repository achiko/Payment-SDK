use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) async fn query_transactions_by_address(
        &self,
        request: HistoryQuery,
    ) -> Result<TransactionPage, IndexError> {
        self.check_scope(&request.scope)?;
        self.validate_address(&request.address)?;
        Self::validate_query_limit(request.limit)?;
        if let Some(after) = &request.after {
            self.validate_transaction_id(after)?;
        }
        self.verify_metadata().await?;
        self.ensure_semantic_available().await?;
        let generation = self.active_generation().await?;
        let prefix =
            keys::address_transaction_prefix(&self.config.scope, generation, &request.address);
        let after = request.after.as_ref().map(|after| {
            keys::address_transaction(&self.config.scope, generation, &request.address, after)
        });
        let page = self
            .storage
            .scan(ScanRequest {
                namespace: keys::namespace(),
                prefix,
                after,
                limit: request.limit,
            })
            .await
            .map_err(Self::storage_error)?;
        let has_more = page.next.is_some();
        let mut transactions = Vec::with_capacity(page.entries.len());
        for (_, stored) in page.entries {
            let transaction_id = record::ScopedValue::into_transaction(
                Self::decode::<ScopedValue>(&stored.value.0)?,
            );
            let transaction = self
                .current_observation(generation, &transaction_id)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "address index references a missing observation",
                        false,
                    )
                })?;
            transactions.push(record::ObservationRecord::into_domain(
                transaction.value.transaction,
            )?);
        }
        let next = has_more
            .then(|| {
                transactions
                    .last()
                    .map(|transaction| transaction.transaction_id.clone())
            })
            .flatten();
        Ok(TransactionPage { transactions, next })
    }

    pub(super) async fn query_watches_for_address(
        &self,
        request: AddressQuery,
    ) -> Result<Vec<WatchReceipt>, IndexError> {
        self.check_scope(&request.scope)?;
        self.validate_address(&request.address)?;
        self.verify_metadata().await?;
        self.ensure_semantic_available().await?;
        let watches = self
            .scan_records::<WatchRecord>(keys::watch_prefix(&self.config.scope))
            .await?;
        Ok(watches
            .into_iter()
            .filter(|(_, watch)| {
                record::SelectorRecord::into_domain(watch.value.selector.clone())
                    == WatchSelector::Address(request.address.clone())
            })
            .map(|(_, watch)| self.watch_receipt(&watch.value))
            .collect())
    }

    pub(super) async fn query_events(&self, request: EventQuery) -> Result<EventPage, IndexError> {
        self.check_scope(&request.scope)?;
        Self::validate_query_limit(request.limit)?;
        self.verify_metadata().await?;
        self.ensure_semantic_available().await?;
        let page = self
            .storage
            .scan(ScanRequest {
                namespace: keys::namespace(),
                prefix: keys::event_prefix(&self.config.scope),
                after: request
                    .after
                    .map(|after| keys::event(&self.config.scope, after)),
                limit: request.limit,
            })
            .await
            .map_err(Self::storage_error)?;
        let has_more = page.next.is_some();
        let mut events = Vec::with_capacity(page.entries.len());
        for (_, stored) in page.entries {
            events.push(record::EventRecord::into_domain(Self::decode::<
                EventRecord,
            >(
                &stored.value.0
            )?)?);
        }
        let next = has_more
            .then(|| events.last().map(|event| event.cursor))
            .flatten();
        Ok(EventPage { events, next })
    }

    pub(super) async fn query_status(&self, scope: &IndexScope) -> Result<SyncStatus, IndexError> {
        self.check_scope(scope)?;
        self.verify_metadata().await?;
        let mut status = self
            .get_record::<SyncRecord>(&keys::status(scope))
            .await?
            .map_or_else(
                || SyncStatus::starting(scope.clone(), self.config.confirmation_policy),
                |status| record::SyncRecord::into_domain(status.value),
            );
        record::ensure_record_scope(scope, &status.scope, "status")?;
        let generation = self.active_generation().await?;
        status.checkpoint = self
            .generation_checkpoint(generation)
            .await?
            .map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value));
        Ok(status)
    }

    pub(super) async fn persist_status(&self, status: SyncStatus) -> Result<(), IndexError> {
        self.check_scope(&status.scope)?;
        if status.confirmation_policy != self.config.confirmation_policy {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "status confirmation policy differs from repository configuration",
                false,
            ));
        }
        let mut batch = self.mutation_batch().await?;
        let status_key = keys::status(&self.config.scope);
        let existing = self.get_record::<SyncRecord>(&status_key).await?;
        if let Some(existing) = &existing {
            match record::SyncRecord::into_domain(existing.value.clone()).phase {
                SyncPhase::RebuildRequired if status.phase != SyncPhase::RebuildRequired => {
                    return Err(IndexError::new(
                        IndexErrorKind::RebuildRequired,
                        "rebuild-required status can only be cleared by atomic rebuild activation",
                        false,
                    ));
                }
                SyncPhase::Halted if status.phase != SyncPhase::Halted => {
                    return Err(IndexError::new(
                        IndexErrorKind::Halted,
                        "halted status cannot be cleared by the synchronization worker",
                        false,
                    ));
                }
                SyncPhase::Starting
                | SyncPhase::Reconciling
                | SyncPhase::CatchingUp
                | SyncPhase::Ready
                | SyncPhase::Reverting
                | SyncPhase::Replaying
                | SyncPhase::RebuildRequired
                | SyncPhase::Halted => {}
            }
        }
        Self::condition_for(&mut batch, status_key.clone(), existing.as_ref());
        Self::put(
            &mut batch,
            status_key,
            &record::SyncRecord::from_domain(&status),
        )?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(())
    }
}
