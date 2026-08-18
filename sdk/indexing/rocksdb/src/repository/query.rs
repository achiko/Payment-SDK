use super::*;

impl Repository {
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
        let prefix = keys::address_transaction_prefix(&self.scope, &request.address);
        let after = request
            .after
            .as_ref()
            .map(|after| keys::address_transaction(&self.scope, &request.address, after));
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
                .current_observation(&transaction_id)
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

    pub(super) async fn query_status(
        &self,
        scope: &IndexScope,
    ) -> Result<Option<SyncStatus>, IndexError> {
        self.check_scope(scope)?;
        self.verify_metadata().await?;
        let Some(stored) = self.get_record::<SyncRecord>(&keys::status(scope)).await? else {
            return Ok(None);
        };
        let mut status = record::SyncRecord::into_domain(stored.value);
        record::ensure_record_scope(scope, &status.scope, "status")?;
        status.checkpoint = self
            .generation_checkpoint()
            .await?
            .map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value));
        Ok(Some(status))
    }

    pub(super) async fn persist_status(&self, status: SyncStatus) -> Result<(), IndexError> {
        self.check_scope(&status.scope)?;
        let mut batch = self.mutation_batch().await?;
        let status_key = keys::status(&self.scope);
        let existing = self.get_record::<SyncRecord>(&status_key).await?;
        if let Some(existing) = &existing {
            match record::SyncRecord::into_domain(existing.value.clone()).phase {
                SyncPhase::Halted if status.phase != SyncPhase::Halted => {
                    return Err(IndexError::new(
                        IndexErrorKind::Halted,
                        "halted status cannot be cleared by the synchronization synchronizer",
                        false,
                    ));
                }
                SyncPhase::Starting
                | SyncPhase::Reconciling
                | SyncPhase::CatchingUp
                | SyncPhase::Ready
                | SyncPhase::Reverting
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
