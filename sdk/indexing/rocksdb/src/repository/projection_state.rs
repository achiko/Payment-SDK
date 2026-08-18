use super::*;

impl Repository {
    pub(super) async fn append_projection_batch(
        &self,
        batch: &mut WriteBatch,
        projection: &ProjectionBatch,
        invalid_kind: IndexErrorKind,
    ) -> Result<(), IndexError> {
        let mut keys_seen = BTreeSet::new();
        for mutation in &projection.mutations {
            if !keys_seen.insert(mutation.key()) {
                return Err(IndexError::new(
                    invalid_kind,
                    "projection batch contains a duplicate relative key",
                    false,
                ));
            }
            self.append_projection_mutation(batch, mutation, invalid_kind)
                .await?;
        }
        Ok(())
    }

    async fn append_projection_mutation(
        &self,
        batch: &mut WriteBatch,
        mutation: &ProjectionMutation,
        invalid_kind: IndexErrorKind,
    ) -> Result<(), IndexError> {
        let target_key = keys::projection(&self.scope, mutation.key());
        let current_target = self.get_projection_record(&target_key).await?;
        Self::condition_for(batch, target_key.clone(), current_target.as_ref());
        match mutation {
            ProjectionMutation::Put { value, .. } => Self::put_projection(batch, target_key, value),
            ProjectionMutation::PutIfPresent {
                required_key,
                value,
                ..
            } => {
                if required_key.as_slice() == mutation.key() {
                    return Err(IndexError::new(
                        invalid_kind,
                        "conditional projection target must differ from its required key",
                        false,
                    ));
                }
                let required_key = keys::projection(&self.scope, required_key);
                let required = self.get_projection_record(&required_key).await?;
                Self::condition_for(batch, required_key, required.as_ref());
                if required.is_some() {
                    Self::put_projection(batch, target_key, value);
                }
            }
            ProjectionMutation::Delete { .. } => Self::delete(batch, target_key),
        }
        Ok(())
    }

    fn put_projection(batch: &mut WriteBatch, key: Key, value: &[u8]) {
        batch.operations.push(Operation::Put {
            namespace: keys::namespace(),
            key,
            value: Value(value.to_vec()),
        });
    }

    pub(super) async fn append_projection_revision(
        &self,
        batch: &mut WriteBatch,
    ) -> Result<u64, IndexError> {
        let key = keys::projection_revision(&self.scope);
        let current = self.counter(&key).await?;
        let next = current.as_ref().map_or(Ok(1), |revision| {
            revision.value.value.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "projection revision is exhausted",
                    false,
                )
            })
        })?;
        Self::condition_for(batch, key.clone(), current.as_ref());
        Self::put(batch, key, &CounterRecord { value: next })?;
        Ok(next)
    }

    pub(super) async fn projection_snapshot(&self) -> Result<ProjectionSnapshot, IndexError> {
        let revision = self
            .counter(&keys::projection_revision(&self.scope))
            .await?
            .map_or(0, |revision| revision.value.value);
        let checkpoint = self
            .generation_checkpoint()
            .await?
            .map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value));
        Ok(ProjectionSnapshot {
            revision,
            checkpoint,
        })
    }

    pub(super) fn ensure_projection_snapshot(
        expected: &ProjectionSnapshot,
        actual: &ProjectionSnapshot,
        message: &'static str,
    ) -> Result<(), IndexError> {
        if expected == actual {
            Ok(())
        } else {
            Err(IndexError::new(IndexErrorKind::Conflict, message, true))
        }
    }

    pub(super) async fn generation_checkpoint(
        &self,
    ) -> Result<Option<StoredRecord<BlockRecord>>, IndexError> {
        self.get_record(&keys::canonical_checkpoint(&self.scope))
            .await
    }

    pub(super) async fn watch_version_record(
        &self,
    ) -> Result<Option<StoredRecord<CounterRecord>>, IndexError> {
        self.get_record(&keys::watch_version(&self.scope)).await
    }

    pub(super) async fn counter(
        &self,
        key: &Key,
    ) -> Result<Option<StoredRecord<CounterRecord>>, IndexError> {
        self.get_record(key).await
    }

    pub(super) async fn current_observation(
        &self,
        transaction_id: &TransactionRef,
    ) -> Result<Option<StoredRecord<CurrentObservation>>, IndexError> {
        self.get_record(&keys::current_observation(&self.scope, transaction_id))
            .await
    }

    pub(super) fn validate_query_limit(limit: usize) -> Result<(), IndexError> {
        if (1..=MAX_QUERY_PAGE).contains(&limit) {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "query limit must be between 1 and 1000",
                false,
            ))
        }
    }

    pub(super) fn validate_transaction_id(
        &self,
        transaction_id: &TransactionRef,
    ) -> Result<(), IndexError> {
        if transaction_id.scope == self.scope && !transaction_id.value.is_empty() {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "transaction identifier does not belong to the repository chain",
                false,
            ))
        }
    }

    pub(super) fn validate_address(&self, address: &CanonicalAddress) -> Result<(), IndexError> {
        if address.scope == self.scope && !address.value.is_empty() {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "address does not belong to the repository chain",
                false,
            ))
        }
    }

    pub(super) async fn ensure_semantic_available(&self) -> Result<(), IndexError> {
        let Some(status) = self
            .get_record::<SyncRecord>(&keys::status(&self.scope))
            .await?
        else {
            return Ok(());
        };
        match record::SyncRecord::into_domain(status.value).phase {
            SyncPhase::Halted => Err(IndexError::new(
                IndexErrorKind::Halted,
                "semantic indexing operations are blocked while the indexer is halted",
                false,
            )),
            SyncPhase::Starting
            | SyncPhase::Reconciling
            | SyncPhase::CatchingUp
            | SyncPhase::Ready
            | SyncPhase::Reverting => Ok(()),
        }
    }
}
