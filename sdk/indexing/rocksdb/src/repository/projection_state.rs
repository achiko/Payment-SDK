use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) async fn append_projection_batch(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
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
            self.append_projection_mutation(batch, generation, mutation, invalid_kind)
                .await?;
        }
        Ok(())
    }

    async fn append_projection_mutation(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        mutation: &ProjectionMutation,
        invalid_kind: IndexErrorKind,
    ) -> Result<(), IndexError> {
        let target_key = keys::projection(&self.config.scope, generation, mutation.key());
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
                let required_key = keys::projection(&self.config.scope, generation, required_key);
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
        let key = keys::projection_revision(&self.config.scope);
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

    /// Applies historical projection discoveries without depending on
    /// chronological execution relative to the live sync loop.
    ///
    /// A historical adapter must represent both creation and consumption as
    /// disjoint immutable facts (`Put`). An identical existing value is an
    /// idempotent no-op (for example, a second watch of the same address); a
    /// conflicting value or a destructive `Delete` fails closed. The returned
    /// keys are exactly those first introduced by this commit and therefore
    /// need supplemental deletion if the retained canonical block is reverted.
    pub(super) async fn append_backfill_projection(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        projection: &ProjectionBatch,
    ) -> Result<Vec<Vec<u8>>, IndexError> {
        let mut keys_seen = BTreeSet::new();
        let mut introduced = Vec::new();
        for mutation in &projection.mutations {
            if !keys_seen.insert(mutation.key()) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "historical projection batch contains a duplicate relative key",
                    false,
                ));
            }
            let ProjectionMutation::Put { key, value } = mutation else {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "historical projection backfill must use unconditional order-independent put facts",
                    false,
                ));
            };
            let physical_key = keys::projection(&self.config.scope, generation, key);
            let current = self.get_projection_record(&physical_key).await?;
            match current {
                Some(current) if current.value == *value => {}
                Some(_) => {
                    return Err(IndexError::new(
                        IndexErrorKind::Store,
                        "historical projection conflicts with an existing canonical value",
                        false,
                    ));
                }
                None => {
                    Self::condition_for::<Vec<u8>>(batch, physical_key.clone(), None);
                    batch.operations.push(Operation::Put {
                        namespace: keys::namespace(),
                        key: physical_key,
                        value: Value(value.clone()),
                    });
                    introduced.push(key.clone());
                }
            }
        }
        Ok(introduced)
    }

    pub(super) async fn active_generation_record(
        &self,
    ) -> Result<Option<StoredRecord<CounterRecord>>, IndexError> {
        self.get_record::<CounterRecord>(&keys::active_generation(&self.config.scope))
            .await
    }

    pub(super) async fn active_generation(&self) -> Result<RebuildGeneration, IndexError> {
        self.active_generation_record().await.map(|record| {
            RebuildGeneration(record.map_or(BASE_GENERATION.0, |record| record.value.value))
        })
    }

    pub(super) async fn projection_snapshot(&self) -> Result<ProjectionSnapshot, IndexError> {
        let active = self.active_generation_record().await?;
        let generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        let revision = self
            .counter(&keys::projection_revision(&self.config.scope))
            .await?
            .map_or(0, |revision| revision.value.value);
        let checkpoint = self
            .generation_checkpoint(generation)
            .await?
            .map(|checkpoint| record::BlockRecord::into_domain(checkpoint.value));
        Ok(ProjectionSnapshot {
            generation,
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
        generation: RebuildGeneration,
    ) -> Result<Option<StoredRecord<BlockRecord>>, IndexError> {
        self.get_record(&keys::canonical_checkpoint(&self.config.scope, generation))
            .await
    }

    pub(super) async fn watch_version_record(
        &self,
    ) -> Result<Option<StoredRecord<CounterRecord>>, IndexError> {
        self.get_record(&keys::watch_version(&self.config.scope))
            .await
    }

    pub(super) async fn counter(
        &self,
        key: &Key,
    ) -> Result<Option<StoredRecord<CounterRecord>>, IndexError> {
        self.get_record(key).await
    }

    pub(super) fn watch_receipt(&self, watch: &WatchRecord) -> WatchReceipt {
        WatchReceipt {
            id: WatchId(watch.id.clone()),
            scope: record::ScopeRecord::into_domain(watch.scope.clone()),
            selector: record::SelectorRecord::into_domain(watch.selector.clone()),
            start_height: BlockHeight(watch.start_height),
            registered_at: watch
                .registered_at
                .clone()
                .map(record::BlockRecord::into_domain),
            inactive_from: watch.inactive_from.map(BlockHeight),
            confirmation_policy: self.config.confirmation_policy,
        }
    }

    pub(super) async fn current_observation(
        &self,
        generation: RebuildGeneration,
        transaction_id: &TransactionRef,
    ) -> Result<Option<StoredRecord<CurrentObservation>>, IndexError> {
        self.get_record(&keys::current_observation(
            &self.config.scope,
            generation,
            transaction_id,
        ))
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
        if transaction_id.scope == self.config.scope && !transaction_id.value.is_empty() {
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
        if address.scope == self.config.scope && !address.value.is_empty() {
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
            .get_record::<SyncRecord>(&keys::status(&self.config.scope))
            .await?
        else {
            return Ok(());
        };
        match record::SyncRecord::into_domain(status.value).phase {
            SyncPhase::RebuildRequired => Err(IndexError::new(
                IndexErrorKind::RebuildRequired,
                "semantic indexing operations are blocked until staged rebuild activation",
                false,
            )),
            SyncPhase::Halted => Err(IndexError::new(
                IndexErrorKind::Halted,
                "semantic indexing operations are blocked while the indexer is halted",
                false,
            )),
            SyncPhase::Starting
            | SyncPhase::Reconciling
            | SyncPhase::CatchingUp
            | SyncPhase::Ready
            | SyncPhase::Reverting
            | SyncPhase::Replaying => Ok(()),
        }
    }
}
