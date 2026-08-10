use std::collections::{BTreeMap, BTreeSet};

use bincode::{Decode, Encode, config};
use chain_identity::{CanonicalAddress, CanonicalTransactionId};
use storage::{
    Condition, Key, Operation, ScanRequest, Storage, StorageError, StorageErrorKind, Value,
    Version, WriteBatch,
};

use super::{
    BASE_GENERATION, IndexRecordCodec, PersistentIndexRepository, keys,
    record::{
        self, BackfillProjectionRollbackRecordV1, BlockRefRecordV1, BundleChangeRecordV1,
        BundleRecordV1, ChainValueRecordV1, CounterRecordV1, CurrentObservationRecordV1,
        EventIdRecordV1, EventRecordV1, ObservationRecordV1, PendingConfirmationRecordV1,
        PolicyMigrationIdRecordV1, PolicyMigrationRecordV1, RebuildStateRecordV1,
        RepositoryMetaRecordV1, SyncStatusRecordV1, WatchBackfillAppliedHeightRecordV1,
        WatchBackfillAppliedRecordV1, WatchBackfillRecordV1, WatchIdempotencyRecordV1,
        WatchRecordV1,
    },
};
use crate::{
    AbortRebuildCommand, ActivateRebuildCommand, AddressWatchRequest, BeginRebuildCommand,
    BlockHeight, BlockRef, CleanupGenerationCommand, CleanupGenerationOutcome, CommitBlockCommand,
    CommitBlockOutcome, CommitRebuildBlockCommand, CommitWatchBackfillCommand,
    CommitWatchBackfillOutcome, ConfirmationProof, EventCursor, IndexError, IndexErrorKind,
    IndexRepository, IndexScope, MigrateIndexPolicyCommand, MigrateIndexPolicyOutcome,
    ObservationDraft, ObservationDraftStatus, ObservationEventPage, ObservationEventRequest,
    ObservationRevision, ObservedTransaction, PolicyMigrationVersion,
    PrepareRebuildActivationCommand, ProjectionBatch, ProjectionCursor, ProjectionEntry,
    ProjectionGetRequest, ProjectionGetResponse, ProjectionMutation, ProjectionPage,
    ProjectionQuery, ProjectionScanRequest, ProjectionSnapshot, RebuildGeneration, RebuildPhase,
    RebuildReason, RebuildState, RegisterWatchCommand, RegisterWatchOutcome, RevertTipCommand,
    RevertTipOutcome, SyncPhase, SyncStatus, TransactionPage, TransactionPageRequest,
    TransactionRequest, TransactionStatus, UnwatchCommand, UnwatchOutcome, ValidateRebuildCommand,
    WatchBackfill, WatchId, WatchReceipt, WatchSelector, WatchSnapshot, WatchTarget, WatchVersion,
};

const RECORD_FORMAT_VERSION: u16 = 1;
const SCAN_CHUNK: usize = 512;
const MAX_QUERY_PAGE: usize = 1_000;

struct Versioned<T> {
    value: T,
    version: Version,
}

struct Transition {
    prior: Option<CurrentObservationRecordV1>,
    prior_version: Option<Version>,
    next: CurrentObservationRecordV1,
    included_here: bool,
    // Rebuild corrections can use an active-generation observation as their
    // semantic prior even when no corresponding shadow index exists yet.
    prior_indexed_in_generation: bool,
}

impl<S, C> PersistentIndexRepository<S, C>
where
    S: Storage,
    C: IndexRecordCodec,
{
    fn encode<T: Encode>(value: &T) -> Result<Value, IndexError> {
        bincode::encode_to_vec(value, config::standard())
            .map(Value)
            .map_err(|error| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    format!("failed to encode an IX RecordV1: {error}"),
                    false,
                )
            })
    }

    fn decode<T: Decode<()>>(value: &[u8]) -> Result<T, IndexError> {
        let (decoded, consumed) = bincode::decode_from_slice::<T, _>(value, config::standard())
            .map_err(|error| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    format!("failed to decode an IX RecordV1: {error}"),
                    false,
                )
            })?;
        if consumed != value.len() {
            return Err(IndexError::new(
                IndexErrorKind::Storage,
                "persisted IX record contains trailing bytes",
                false,
            ));
        }
        Ok(decoded)
    }

    fn storage_error(error: StorageError) -> IndexError {
        match error.kind {
            StorageErrorKind::Conflict => {
                IndexError::new(IndexErrorKind::Conflict, error.message, true)
            }
            StorageErrorKind::Unavailable => {
                IndexError::new(IndexErrorKind::Storage, error.message, true)
            }
            StorageErrorKind::CorruptData
            | StorageErrorKind::InvalidRequest
            | StorageErrorKind::Other => {
                IndexError::new(IndexErrorKind::Storage, error.message, false)
            }
        }
    }

    fn check_scope(&self, scope: &IndexScope) -> Result<(), IndexError> {
        if scope == &self.config.scope {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "request scope does not match the persistent repository scope",
                false,
            ))
        }
    }

    fn validate_policy(
        &self,
        confirmation_policy: crate::ConfirmationPolicy,
        reorg_retention: u64,
    ) -> Result<(), IndexError> {
        if confirmation_policy != self.config.confirmation_policy {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "commit confirmation policy differs from persisted configuration",
                false,
            ));
        }
        if reorg_retention != self.config.reorg_retention {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "commit reorg retention differs from persisted configuration",
                false,
            ));
        }
        Ok(())
    }

    fn validate_policy_migration_command(
        &self,
        command: &MigrateIndexPolicyCommand,
    ) -> Result<(), IndexError> {
        self.check_scope(&command.scope)?;
        if command.bootstrap_height != self.config.bootstrap_height {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "policy migration cannot change the persisted bootstrap height",
                false,
            ));
        }
        if command.target_confirmation_policy != self.config.confirmation_policy
            || command.target_reorg_retention != self.config.reorg_retention
        {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "policy migration target differs from the repository runtime configuration",
                false,
            ));
        }
        if command.expected_confirmation_policy.minimum_confirmations == 0
            || command.target_confirmation_policy.minimum_confirmations == 0
            || command.expected_reorg_retention == 0
            || command.target_reorg_retention == 0
        {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "policy migration depths and retention must be greater than zero",
                false,
            ));
        }
        if command.expected_confirmation_policy.require_chain_finality
            || command.target_confirmation_policy.require_chain_finality
        {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "Ethereum v1 policy migration cannot enable chain-finality requirements",
                false,
            ));
        }
        if command.expected_confirmation_policy == command.target_confirmation_policy
            && command.expected_reorg_retention == command.target_reorg_retention
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "policy migration must change confirmation depth or reorg retention",
                false,
            ));
        }
        if command.idempotency_key.trim().is_empty() || command.idempotency_key.len() > 256 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "policy migration idempotency key must contain 1 through 256 bytes",
                false,
            ));
        }
        if command.reason.trim().is_empty() || command.reason.len() > 4_096 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "policy migration reason must contain 1 through 4096 bytes",
                false,
            ));
        }
        Ok(())
    }

    fn policy_migration_record(
        command: &MigrateIndexPolicyCommand,
        version: PolicyMigrationVersion,
    ) -> PolicyMigrationRecordV1 {
        PolicyMigrationRecordV1 {
            version: version.0,
            idempotency_key: command.idempotency_key.clone(),
            scope: record::scope_to_record(&command.scope),
            bootstrap_height: command.bootstrap_height.0,
            from_confirmation_policy: record::policy_to_record(
                command.expected_confirmation_policy,
            ),
            from_reorg_retention: command.expected_reorg_retention,
            to_confirmation_policy: record::policy_to_record(command.target_confirmation_policy),
            to_reorg_retention: command.target_reorg_retention,
            reason: command.reason.clone(),
        }
    }

    async fn get_record<T: Decode<()>>(
        &self,
        key: &Key,
    ) -> Result<Option<Versioned<T>>, IndexError> {
        let stored = self
            .storage
            .get(&keys::namespace(), key)
            .await
            .map_err(Self::storage_error)?;
        stored
            .map(|stored| {
                Self::decode(&stored.value.0).map(|value| Versioned {
                    value,
                    version: stored.version,
                })
            })
            .transpose()
    }

    async fn get_projection_record(
        &self,
        key: &Key,
    ) -> Result<Option<Versioned<Vec<u8>>>, IndexError> {
        self.storage
            .get(&keys::namespace(), key)
            .await
            .map_err(Self::storage_error)
            .map(|stored| {
                stored.map(|stored| Versioned {
                    value: stored.value.0,
                    version: stored.version,
                })
            })
    }

    async fn scan_records<T: Decode<()>>(
        &self,
        prefix: Vec<u8>,
    ) -> Result<Vec<(Key, Versioned<T>)>, IndexError> {
        let mut after = None;
        let mut records = Vec::new();
        loop {
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: keys::namespace(),
                    prefix: prefix.clone(),
                    after,
                    limit: SCAN_CHUNK,
                })
                .await
                .map_err(Self::storage_error)?;
            for (key, stored) in page.entries {
                records.push((
                    key,
                    Versioned {
                        value: Self::decode(&stored.value.0)?,
                        version: stored.version,
                    },
                ));
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        Ok(records)
    }

    async fn verify_metadata(&self) -> Result<(), IndexError> {
        let key = keys::meta(&self.config.scope);
        if let Some(meta) = self.get_record::<RepositoryMetaRecordV1>(&key).await? {
            self.validate_meta(&meta.value)?;
        }
        Ok(())
    }

    fn expected_meta(&self) -> RepositoryMetaRecordV1 {
        RepositoryMetaRecordV1 {
            format_version: RECORD_FORMAT_VERSION,
            scope: record::scope_to_record(&self.config.scope),
            bootstrap_height: self.config.bootstrap_height.0,
            confirmation_depth: self.config.confirmation_policy.minimum_confirmations,
            require_chain_finality: self.config.confirmation_policy.require_chain_finality,
            reorg_retention: self.config.reorg_retention,
        }
    }

    fn validate_meta(&self, meta: &RepositoryMetaRecordV1) -> Result<(), IndexError> {
        if meta == &self.expected_meta() {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "persisted IX scope, bootstrap height, confirmation policy, or retention differs from runtime configuration",
                false,
            ))
        }
    }

    async fn mutation_batch(&self) -> Result<WriteBatch, IndexError> {
        let namespace = keys::namespace();
        let meta_key = keys::meta(&self.config.scope);
        let guard_key = keys::mutation_guard(&self.config.scope);
        let meta = self.get_record::<RepositoryMetaRecordV1>(&meta_key).await?;
        let guard = self.get_record::<CounterRecordV1>(&guard_key).await?;
        let mut batch = WriteBatch::default();

        match meta {
            Some(meta) => {
                self.validate_meta(&meta.value)?;
                batch.conditions.push(Condition::Version {
                    namespace: namespace.clone(),
                    key: meta_key,
                    expected: meta.version,
                });
            }
            None => {
                batch.conditions.push(Condition::Missing {
                    namespace: namespace.clone(),
                    key: meta_key.clone(),
                });
                batch.operations.push(Operation::Put {
                    namespace: namespace.clone(),
                    key: meta_key,
                    value: Self::encode(&self.expected_meta())?,
                });
            }
        }

        let next_guard = match guard {
            Some(guard) => {
                batch.conditions.push(Condition::Version {
                    namespace: namespace.clone(),
                    key: guard_key.clone(),
                    expected: guard.version,
                });
                guard.value.value.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "IX mutation guard is exhausted",
                        false,
                    )
                })?
            }
            None => {
                batch.conditions.push(Condition::Missing {
                    namespace: namespace.clone(),
                    key: guard_key.clone(),
                });
                1
            }
        };
        batch.operations.push(Operation::Put {
            namespace,
            key: guard_key,
            value: Self::encode(&CounterRecordV1 { value: next_guard })?,
        });
        Ok(batch)
    }

    fn condition_for<T>(batch: &mut WriteBatch, key: Key, record: Option<&Versioned<T>>) {
        let namespace = keys::namespace();
        match record {
            Some(record) => batch.conditions.push(Condition::Version {
                namespace,
                key,
                expected: record.version,
            }),
            None => batch.conditions.push(Condition::Missing { namespace, key }),
        }
    }

    fn put<T: Encode>(batch: &mut WriteBatch, key: Key, value: &T) -> Result<(), IndexError> {
        batch.operations.push(Operation::Put {
            namespace: keys::namespace(),
            key,
            value: Self::encode(value)?,
        });
        Ok(())
    }

    fn delete(batch: &mut WriteBatch, key: Key) {
        batch.operations.push(Operation::Delete {
            namespace: keys::namespace(),
            key,
        });
    }

    async fn append_projection_batch(
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
            let target_key = keys::projection(&self.config.scope, generation, mutation.key());
            let current_target = self.get_projection_record(&target_key).await?;
            Self::condition_for(batch, target_key.clone(), current_target.as_ref());
            match mutation {
                ProjectionMutation::Put { value, .. } => {
                    batch.operations.push(Operation::Put {
                        namespace: keys::namespace(),
                        key: target_key,
                        value: Value(value.clone()),
                    });
                }
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
                    let required_key =
                        keys::projection(&self.config.scope, generation, required_key);
                    let required = self.get_projection_record(&required_key).await?;
                    Self::condition_for(batch, required_key, required.as_ref());
                    if required.is_some() {
                        batch.operations.push(Operation::Put {
                            namespace: keys::namespace(),
                            key: target_key,
                            value: Value(value.clone()),
                        });
                    }
                }
                ProjectionMutation::Delete { .. } => Self::delete(batch, target_key),
            }
        }
        Ok(())
    }

    async fn append_projection_revision(&self, batch: &mut WriteBatch) -> Result<u64, IndexError> {
        let key = keys::projection_revision(&self.config.scope);
        let current = self.counter(&key).await?;
        let next = current.as_ref().map_or(Ok(1), |revision| {
            revision.value.value.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "projection revision is exhausted",
                    false,
                )
            })
        })?;
        Self::condition_for(batch, key.clone(), current.as_ref());
        Self::put(batch, key, &CounterRecordV1 { value: next })?;
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
    async fn append_backfill_projection(
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
                        IndexErrorKind::Storage,
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

    async fn active_generation_record(
        &self,
    ) -> Result<Option<Versioned<CounterRecordV1>>, IndexError> {
        self.get_record::<CounterRecordV1>(&keys::active_generation(&self.config.scope))
            .await
    }

    async fn active_generation(&self) -> Result<RebuildGeneration, IndexError> {
        self.active_generation_record().await.map(|record| {
            RebuildGeneration(record.map_or(BASE_GENERATION.0, |record| record.value.value))
        })
    }

    async fn projection_snapshot(&self) -> Result<ProjectionSnapshot, IndexError> {
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
            .map(|checkpoint| record::block_from_record(checkpoint.value));
        Ok(ProjectionSnapshot {
            generation,
            revision,
            checkpoint,
        })
    }

    fn ensure_projection_snapshot(
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

    async fn generation_checkpoint(
        &self,
        generation: RebuildGeneration,
    ) -> Result<Option<Versioned<BlockRefRecordV1>>, IndexError> {
        self.get_record(&keys::canonical_checkpoint(&self.config.scope, generation))
            .await
    }

    async fn watch_version_record(&self) -> Result<Option<Versioned<CounterRecordV1>>, IndexError> {
        self.get_record(&keys::watch_version(&self.config.scope))
            .await
    }

    async fn counter(&self, key: &Key) -> Result<Option<Versioned<CounterRecordV1>>, IndexError> {
        self.get_record(key).await
    }

    fn watch_receipt(&self, watch: &WatchRecordV1) -> WatchReceipt {
        WatchReceipt {
            id: WatchId(watch.id.clone()),
            scope: record::scope_from_record(watch.scope.clone()),
            selector: record::selector_from_record(watch.selector.clone()),
            start_height: BlockHeight(watch.start_height),
            registered_at: watch.registered_at.clone().map(record::block_from_record),
            inactive_from: watch.inactive_from.map(BlockHeight),
            confirmation_policy: self.config.confirmation_policy,
        }
    }

    async fn current_observation(
        &self,
        generation: RebuildGeneration,
        transaction_id: &CanonicalTransactionId,
    ) -> Result<Option<Versioned<CurrentObservationRecordV1>>, IndexError> {
        self.get_record(&keys::current_observation(
            &self.config.scope,
            generation,
            &transaction_id.chain.0,
            &transaction_id.value,
        ))
        .await
    }

    fn validate_query_limit(limit: usize) -> Result<(), IndexError> {
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

    fn validate_transaction_id(
        &self,
        transaction_id: &CanonicalTransactionId,
    ) -> Result<(), IndexError> {
        if transaction_id.chain == self.config.scope.chain && !transaction_id.value.is_empty() {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "transaction identifier does not belong to the repository chain",
                false,
            ))
        }
    }

    fn validate_address(&self, address: &CanonicalAddress) -> Result<(), IndexError> {
        if address.chain == self.config.scope.chain && !address.value.is_empty() {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "address does not belong to the repository chain",
                false,
            ))
        }
    }

    async fn ensure_semantic_available(&self) -> Result<(), IndexError> {
        let Some(status) = self
            .get_record::<SyncStatusRecordV1>(&keys::status(&self.config.scope))
            .await?
        else {
            return Ok(());
        };
        match record::sync_status_from_record(status.value).phase {
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

impl<S, C> IndexRepository for PersistentIndexRepository<S, C>
where
    S: Storage,
    C: IndexRecordCodec,
{
    type Target = C::Target;
    type Undo = C::Undo;

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
                    checkpoint.map(|checkpoint| record::block_from_record(checkpoint.value))
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
            self.get_record::<BlockRefRecordV1>(&keys::canonical(scope, generation, height))
                .await
                .map(|block| block.map(|block| record::block_from_record(block.value)))
        })
    }

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
                .scan_records::<WatchRecordV1>(keys::watch_prefix(scope))
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
                    let record_scope = record::scope_from_record(watch.value.scope.clone());
                    record::ensure_record_scope(scope, &record_scope, "watch")?;
                    Ok(WatchTarget {
                        id: WatchId(watch.value.id),
                        scope: record_scope,
                        selector: record::selector_from_record(watch.value.selector),
                        target: self.codec.decode_target(&watch.value.encoded_target)?,
                        idempotency_key: watch.value.idempotency_key,
                        start_height: BlockHeight(watch.value.start_height),
                        registered_at: watch.value.registered_at.map(record::block_from_record),
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

    fn pending_watch_backfills<'a>(
        &'a self,
        scope: &'a IndexScope,
        limit: usize,
    ) -> crate::BoxFuture<'a, Result<Vec<WatchBackfill>, IndexError>> {
        Box::pin(async move { self.query_watch_backfills(scope, limit).await })
    }

    fn commit_watch_backfill<'a>(
        &'a self,
        command: CommitWatchBackfillCommand,
    ) -> crate::BoxFuture<'a, Result<CommitWatchBackfillOutcome, IndexError>> {
        Box::pin(async move {
            self.apply_watch_backfill(command, ProjectionBatch::default())
                .await
        })
    }

    fn commit_watch_backfill_projection<'a>(
        &'a self,
        command: CommitWatchBackfillCommand,
        projection: ProjectionBatch,
    ) -> crate::BoxFuture<'a, Result<CommitWatchBackfillOutcome, IndexError>> {
        Box::pin(async move { self.apply_watch_backfill(command, projection).await })
    }

    fn register_watch<'a>(
        &'a self,
        command: RegisterWatchCommand<Self::Target>,
    ) -> crate::BoxFuture<'a, Result<RegisterWatchOutcome, IndexError>> {
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
            let encoded_target = self.codec.encode_target(&command.target)?;
            let idempotency_key =
                keys::watch_idempotency(&self.config.scope, &command.request.idempotency_key);
            if let Some(existing_id) = self
                .get_record::<WatchIdempotencyRecordV1>(&idempotency_key)
                .await?
            {
                let existing_key = keys::watch(&self.config.scope, &existing_id.value.watch_id);
                let existing = self
                    .get_record::<WatchRecordV1>(&existing_key)
                    .await?
                    .ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Storage,
                            "watch idempotency record references a missing watch",
                            false,
                        )
                    })?;
                let same_payload = existing.value.scope
                    == record::scope_to_record(&command.request.scope)
                    && existing.value.selector
                        == record::selector_to_record(&command.request.selector)
                    && existing.value.start_height == command.request.start_height.0
                    && existing.value.encoded_target == encoded_target;
                if !same_payload {
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "watch idempotency key was reused with a different payload",
                        false,
                    ));
                }
                return Ok(RegisterWatchOutcome::Existing(
                    self.watch_receipt(&existing.value),
                ));
            }

            let mut batch = self.mutation_batch().await?;
            let active_generation = self.active_generation_record().await?;
            let generation = RebuildGeneration(
                active_generation
                    .as_ref()
                    .map_or(BASE_GENERATION.0, |active| active.value.value),
            );
            let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, generation);
            let checkpoint = self.get_record::<BlockRefRecordV1>(&checkpoint_key).await?;
            let persisted_checkpoint = checkpoint
                .as_ref()
                .map(|checkpoint| record::block_from_record(checkpoint.value.clone()));
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
                        IndexErrorKind::Storage,
                        "watch ID counter is exhausted",
                        false,
                    )
                })
            })?;
            let next_version = watch_version.as_ref().map_or(Ok(1), |version| {
                version.value.value.checked_add(1).ok_or_else(|| {
                    IndexError::new(IndexErrorKind::Storage, "watch version is exhausted", false)
                })
            })?;
            let watch_id = WatchId(format!("watch-{next_watch:020}"));
            let watch = WatchRecordV1 {
                id: watch_id.0.clone(),
                scope: record::scope_to_record(&command.request.scope),
                selector: record::selector_to_record(&command.request.selector),
                encoded_target,
                idempotency_key: command.request.idempotency_key.clone(),
                start_height: command.request.start_height.0,
                registered_at: command.registered_at.as_ref().map(record::block_to_record),
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
            Self::condition_for::<WatchIdempotencyRecordV1>(
                &mut batch,
                idempotency_key.clone(),
                None,
            );
            Self::condition_for::<WatchRecordV1>(&mut batch, watch_key.clone(), None);
            Self::put(
                &mut batch,
                watch_counter_key,
                &CounterRecordV1 { value: next_watch },
            )?;
            Self::put(
                &mut batch,
                watch_version_key,
                &CounterRecordV1 {
                    value: next_version,
                },
            )?;
            Self::put(
                &mut batch,
                idempotency_key,
                &WatchIdempotencyRecordV1 {
                    watch_id: watch_id.0.clone(),
                },
            )?;
            Self::put(&mut batch, watch_key, &watch)?;
            if let Some(through) = checkpoint
                .as_ref()
                .filter(|checkpoint| command.request.start_height.0 <= checkpoint.value.height)
            {
                let backfill_key = keys::watch_backfill(&self.config.scope, &watch_id.0);
                Self::condition_for::<WatchBackfillRecordV1>(
                    &mut batch,
                    backfill_key.clone(),
                    None,
                );
                Self::put(
                    &mut batch,
                    backfill_key,
                    &WatchBackfillRecordV1 {
                        scope: record::scope_to_record(&self.config.scope),
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
            Ok(RegisterWatchOutcome::Registered(self.watch_receipt(&watch)))
        })
    }

    fn unwatch<'a>(
        &'a self,
        command: UnwatchCommand,
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
            let checkpoint = self.get_record::<BlockRefRecordV1>(&checkpoint_key).await?;
            let current_checkpoint = checkpoint
                .as_ref()
                .map(|checkpoint| record::block_from_record(checkpoint.value.clone()));
            if current_checkpoint != command.expected_checkpoint {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "canonical checkpoint changed before watch deactivation",
                    true,
                ));
            }
            let watch_key = keys::watch(&self.config.scope, &command.watch_id.0);
            let watch = self
                .get_record::<WatchRecordV1>(&watch_key)
                .await?
                .ok_or_else(|| {
                    IndexError::new(IndexErrorKind::InvalidWatch, "unknown watch ID", false)
                })?;
            if watch.value.inactive_from.is_some() {
                return Ok(UnwatchOutcome::AlreadyInactive);
            }
            let backfill_key = keys::watch_backfill(&self.config.scope, &command.watch_id.0);
            if self
                .get_record::<WatchBackfillRecordV1>(&backfill_key)
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
                    IndexError::new(IndexErrorKind::Storage, "watch version is exhausted", false)
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
            Self::condition_for::<WatchBackfillRecordV1>(&mut batch, backfill_key, None);
            Self::put(&mut batch, watch_key, &updated)?;
            Self::put(
                &mut batch,
                watch_version_key,
                &CounterRecordV1 {
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

    fn commit_block<'a>(
        &'a self,
        command: CommitBlockCommand<Self::Undo>,
    ) -> crate::BoxFuture<'a, Result<CommitBlockOutcome, IndexError>> {
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
        command: RevertTipCommand,
    ) -> crate::BoxFuture<'a, Result<RevertTipOutcome, IndexError>> {
        Box::pin(async move { self.revert_active_tip(command).await })
    }

    fn transaction<'a>(
        &'a self,
        request: TransactionRequest,
    ) -> crate::BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        Box::pin(async move { self.query_transaction(request).await })
    }

    fn transactions_by_address<'a>(
        &'a self,
        request: TransactionPageRequest,
    ) -> crate::BoxFuture<'a, Result<TransactionPage, IndexError>> {
        Box::pin(async move { self.query_transactions_by_address(request).await })
    }

    fn watches_for_address<'a>(
        &'a self,
        request: AddressWatchRequest,
    ) -> crate::BoxFuture<'a, Result<Vec<WatchReceipt>, IndexError>> {
        Box::pin(async move { self.query_watches_for_address(request).await })
    }

    fn events<'a>(
        &'a self,
        request: ObservationEventRequest,
    ) -> crate::BoxFuture<'a, Result<ObservationEventPage, IndexError>> {
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

    fn migrate_policy<'a>(
        &'a self,
        command: MigrateIndexPolicyCommand,
    ) -> crate::BoxFuture<'a, Result<MigrateIndexPolicyOutcome, IndexError>> {
        Box::pin(async move { self.apply_policy_migration(command).await })
    }

    fn rebuild_state<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> crate::BoxFuture<'a, Result<Option<RebuildState>, IndexError>> {
        Box::pin(async move { self.query_rebuild_state(scope).await })
    }

    fn begin_rebuild<'a>(
        &'a self,
        command: BeginRebuildCommand,
    ) -> crate::BoxFuture<'a, Result<RebuildState, IndexError>> {
        Box::pin(async move { self.start_rebuild(command).await })
    }

    fn commit_rebuild_block<'a>(
        &'a self,
        command: CommitRebuildBlockCommand<Self::Undo>,
    ) -> crate::BoxFuture<'a, Result<CommitBlockOutcome, IndexError>> {
        Box::pin(async move { self.commit_shadow_block(command).await })
    }

    fn validate_rebuild<'a>(
        &'a self,
        command: ValidateRebuildCommand,
    ) -> crate::BoxFuture<'a, Result<RebuildState, IndexError>> {
        Box::pin(async move { self.mark_rebuild_validating(command).await })
    }

    fn prepare_rebuild_activation<'a>(
        &'a self,
        command: PrepareRebuildActivationCommand,
    ) -> crate::BoxFuture<'a, Result<RebuildState, IndexError>> {
        Box::pin(async move { self.prepare_rebuild(command).await })
    }

    fn activate_rebuild<'a>(
        &'a self,
        command: ActivateRebuildCommand,
    ) -> crate::BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move { self.publish_rebuild(command).await })
    }

    fn abort_rebuild<'a>(
        &'a self,
        command: AbortRebuildCommand,
    ) -> crate::BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move { self.cancel_rebuild(command).await })
    }

    fn cleanup_generation<'a>(
        &'a self,
        command: CleanupGenerationCommand,
    ) -> crate::BoxFuture<'a, Result<CleanupGenerationOutcome, IndexError>> {
        Box::pin(async move { self.remove_generation(command).await })
    }
}

impl<S, C> ProjectionQuery for PersistentIndexRepository<S, C>
where
    S: Storage,
    C: IndexRecordCodec,
{
    fn projection_get<'a>(
        &'a self,
        request: ProjectionGetRequest,
    ) -> crate::BoxFuture<'a, Result<ProjectionGetResponse, IndexError>> {
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

            Ok(ProjectionGetResponse { snapshot, value })
        })
    }

    fn projection_scan<'a>(
        &'a self,
        request: ProjectionScanRequest,
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
                            IndexErrorKind::Storage,
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

impl<S, C> PersistentIndexRepository<S, C>
where
    S: Storage,
    C: IndexRecordCodec,
{
    fn validate_draft(
        &self,
        draft: &ObservationDraft,
        active_watch_ids: &BTreeSet<WatchId>,
    ) -> Result<(), IndexError> {
        if draft.scope != self.config.scope {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "observation draft belongs to another scope",
                false,
            ));
        }
        self.validate_transaction_id(&draft.transaction_id)?;
        if matches!(draft.status, ObservationDraftStatus::Failed { .. })
            && !draft.movements.is_empty()
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "failed observation draft contains movements",
                false,
            ));
        }
        let mut movement_ids = BTreeSet::new();
        for movement in &draft.movements {
            if movement.id.0.is_empty() || !movement_ids.insert(movement.id.clone()) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "observation draft contains an empty or duplicate movement ID",
                    false,
                ));
            }
            if movement.asset.chain != self.config.scope.chain
                || movement
                    .from
                    .as_ref()
                    .is_some_and(|address| address.chain != self.config.scope.chain)
                || movement
                    .to
                    .as_ref()
                    .is_some_and(|address| address.chain != self.config.scope.chain)
            {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "observation movement belongs to another chain",
                    false,
                ));
            }
        }
        if draft.fee.as_ref().is_some_and(|fee| {
            fee.asset.chain != self.config.scope.chain
                || fee
                    .payer
                    .as_ref()
                    .is_some_and(|payer| payer.chain != self.config.scope.chain)
        }) {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "observation fee belongs to another chain",
                false,
            ));
        }
        let mut watch_ids = BTreeSet::new();
        for watch_id in &draft.watch_ids {
            if !watch_ids.insert(watch_id.clone()) || !active_watch_ids.contains(watch_id) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "observation draft references a duplicate, unknown, or inactive watch",
                    false,
                ));
            }
        }
        Ok(())
    }

    async fn active_watch_ids(&self, height: BlockHeight) -> Result<BTreeSet<WatchId>, IndexError> {
        let records = self
            .scan_records::<WatchRecordV1>(keys::watch_prefix(&self.config.scope))
            .await?;
        records
            .into_iter()
            .filter(|(_, watch)| {
                watch.value.start_height <= height.0
                    && watch
                        .value
                        .inactive_from
                        .is_none_or(|inactive| height.0 < inactive)
            })
            .map(|(_, watch)| {
                let scope = record::scope_from_record(watch.value.scope.clone());
                record::ensure_record_scope(&self.config.scope, &scope, "watch")?;
                Ok(WatchId(watch.value.id))
            })
            .collect()
    }

    fn next_observation(
        &self,
        prior: Option<&CurrentObservationRecordV1>,
        transaction_id: &CanonicalTransactionId,
        status: TransactionStatus,
        draft: Option<&ObservationDraft>,
        observed_at: u64,
    ) -> Result<CurrentObservationRecordV1, IndexError> {
        let prior_domain =
            prior.map(|prior| record::observation_from_record(prior.transaction.clone()));
        let revision = prior_domain.as_ref().map_or(Ok(1), |prior| {
            prior.revision.0.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "observation revision is exhausted",
                    false,
                )
            })
        })?;
        let mut watch_ids = draft.map_or_else(
            || {
                prior
                    .map(|prior| prior.watch_ids.clone())
                    .unwrap_or_default()
            },
            |draft| draft.watch_ids.iter().map(|id| id.0.clone()).collect(),
        );
        watch_ids.sort();
        watch_ids.dedup();
        let transaction = ObservedTransaction {
            scope: self.config.scope.clone(),
            transaction_id: transaction_id.clone(),
            revision: ObservationRevision(revision),
            status,
            movements: draft.map_or_else(
                || {
                    prior_domain
                        .as_ref()
                        .map(|prior| prior.movements.clone())
                        .unwrap_or_default()
                },
                |draft| draft.movements.clone(),
            ),
            fee: draft.map_or_else(
                || prior_domain.as_ref().and_then(|prior| prior.fee.clone()),
                |draft| draft.fee.clone(),
            ),
            first_seen_at: draft.map_or_else(
                || {
                    prior_domain
                        .as_ref()
                        .map_or(observed_at, |prior| prior.first_seen_at)
                },
                |draft| {
                    prior_domain
                        .as_ref()
                        .map_or(draft.first_seen_at, |prior| prior.first_seen_at)
                },
            ),
            observed_at,
        };
        Ok(CurrentObservationRecordV1 {
            transaction: record::observation_to_record(&transaction),
            watch_ids,
        })
    }

    fn observation_addresses(
        observation: &CurrentObservationRecordV1,
    ) -> BTreeSet<CanonicalAddress> {
        record::observation_from_record(observation.transaction.clone())
            .movements
            .into_iter()
            .flat_map(|movement| movement.from.into_iter().chain(movement.to))
            .collect()
    }

    fn event_id(cursor: EventCursor, revision: ObservationRevision) -> String {
        format!("ix-v1-{:020}-{:020}", cursor.0, revision.0)
    }

    fn append_transition(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        transition: &Transition,
        cursor: Option<EventCursor>,
    ) -> Result<(), IndexError> {
        let transaction = record::observation_from_record(transition.next.transaction.clone());
        let current_key = keys::current_observation(
            &self.config.scope,
            generation,
            &transaction.transaction_id.chain.0,
            &transaction.transaction_id.value,
        );
        let namespace = keys::namespace();
        match transition.prior_version {
            Some(expected) => batch.conditions.push(Condition::Version {
                namespace: namespace.clone(),
                key: current_key.clone(),
                expected,
            }),
            None => batch.conditions.push(Condition::Missing {
                namespace: namespace.clone(),
                key: current_key.clone(),
            }),
        }
        Self::put(batch, current_key, &transition.next)?;

        let revision_key = keys::observation_revision(
            &self.config.scope,
            generation,
            &transaction.transaction_id.chain.0,
            &transaction.transaction_id.value,
            transaction.revision,
        );
        batch.conditions.push(Condition::Missing {
            namespace: namespace.clone(),
            key: revision_key.clone(),
        });
        Self::put(batch, revision_key, &transition.next.transaction)?;

        let prior_addresses = if transition.prior_indexed_in_generation {
            transition
                .prior
                .as_ref()
                .map(Self::observation_addresses)
                .unwrap_or_default()
        } else {
            BTreeSet::new()
        };
        let next_addresses = Self::observation_addresses(&transition.next);
        for address in prior_addresses.difference(&next_addresses) {
            Self::delete(
                batch,
                keys::address_transaction(
                    &self.config.scope,
                    generation,
                    &address.chain.0,
                    &address.value,
                    &transaction.transaction_id.chain.0,
                    &transaction.transaction_id.value,
                ),
            );
        }
        let transaction_record = record::chain_value_from_transaction(&transaction.transaction_id);
        for address in next_addresses.difference(&prior_addresses) {
            Self::put(
                batch,
                keys::address_transaction(
                    &self.config.scope,
                    generation,
                    &address.chain.0,
                    &address.value,
                    &transaction.transaction_id.chain.0,
                    &transaction.transaction_id.value,
                ),
                &transaction_record,
            )?;
        }

        if let Some(cursor) = cursor {
            let id = Self::event_id(cursor, transaction.revision);
            let event = EventRecordV1 {
                id: id.clone(),
                cursor: cursor.0,
                watch_ids: transition.next.watch_ids.clone(),
                previous_status: transition
                    .prior
                    .as_ref()
                    .map(|prior| prior.transaction.status.clone()),
                transaction: transition.next.transaction.clone(),
            };
            let event_key = keys::event(&self.config.scope, cursor);
            let event_id_key = keys::event_id(&self.config.scope, &id);
            batch.conditions.push(Condition::Missing {
                namespace: namespace.clone(),
                key: event_key.clone(),
            });
            batch.conditions.push(Condition::Missing {
                namespace,
                key: event_id_key.clone(),
            });
            Self::put(batch, event_key, &event)?;
            Self::put(batch, event_id_key, &EventIdRecordV1 { cursor: cursor.0 })?;
        }
        Ok(())
    }

    async fn commit_generation(
        &self,
        command: CommitBlockCommand<C::Undo>,
        generation: RebuildGeneration,
        publish_events: bool,
        active_generation: Option<&Versioned<CounterRecordV1>>,
        rebuild: Option<&Versioned<RebuildStateRecordV1>>,
    ) -> Result<CommitBlockOutcome, IndexError> {
        self.check_scope(&command.scope)?;
        self.validate_policy(command.confirmation_policy, command.reorg_retention)?;
        if command.block.block.height < self.config.bootstrap_height {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block precedes the configured bootstrap height",
                false,
            ));
        }

        let canonical_key =
            keys::canonical(&self.config.scope, generation, command.block.block.height);
        if let Some(existing) = self.get_record::<BlockRefRecordV1>(&canonical_key).await? {
            let existing = record::block_from_record(existing.value);
            if existing == command.block.block {
                return Ok(CommitBlockOutcome::AlreadyApplied);
            }
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "another canonical hash is already stored at the block height",
                true,
            ));
        }
        if publish_events {
            self.ensure_semantic_available().await?;
        }

        let mut batch = self.mutation_batch().await?;
        if let Some(active) = active_generation {
            Self::condition_for(
                &mut batch,
                keys::active_generation(&self.config.scope),
                Some(active),
            );
        } else if publish_events {
            Self::condition_for::<CounterRecordV1>(
                &mut batch,
                keys::active_generation(&self.config.scope),
                None,
            );
        }

        let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, generation);
        let checkpoint = self.get_record::<BlockRefRecordV1>(&checkpoint_key).await?;
        let persisted_checkpoint = checkpoint
            .as_ref()
            .map(|checkpoint| record::block_from_record(checkpoint.value.clone()));
        if persisted_checkpoint != command.expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "expected checkpoint no longer matches persistent state",
                true,
            ));
        }
        match &persisted_checkpoint {
            Some(checkpoint) => {
                let expected_height = checkpoint.height.0.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "checkpoint height is exhausted",
                        false,
                    )
                })?;
                if command.block.block.height != BlockHeight(expected_height)
                    || command.block.block.parent_hash.as_ref() != Some(&checkpoint.hash)
                {
                    return Err(IndexError::new(
                        IndexErrorKind::CannotConnect,
                        "block does not immediately connect to the persistent checkpoint",
                        true,
                    ));
                }
            }
            None if command.block.block.height != self.config.bootstrap_height => {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "the first persisted block must equal the configured bootstrap height",
                    false,
                ));
            }
            None => {}
        }

        let watch_version_key = keys::watch_version(&self.config.scope);
        let watch_version = self.watch_version_record().await?;
        let persisted_watch_version = watch_version.as_ref().map_or(0, |value| value.value.value);
        if persisted_watch_version != command.expected_watch_version.0 {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "watch set changed while the block was interpreted",
                true,
            ));
        }
        Self::condition_for(&mut batch, watch_version_key, watch_version.as_ref());
        let active_watch_ids = self.active_watch_ids(command.block.block.height).await?;

        let pending = self
            .scan_records::<PendingConfirmationRecordV1>(keys::pending_confirmation_prefix(
                &self.config.scope,
                generation,
            ))
            .await?;
        let mut transitions = BTreeMap::<CanonicalTransactionId, Transition>::new();
        let mut pending_records = BTreeMap::<CanonicalTransactionId, (Key, Version)>::new();
        let transition_time = command
            .block
            .block
            .timestamp
            .unwrap_or(command.block.block.height.0);
        for (pending_key, pending) in pending {
            let transaction_id =
                record::transaction_from_chain_value(pending.value.transaction_id.clone());
            self.validate_transaction_id(&transaction_id)?;
            let current = self
                .current_observation(generation, &transaction_id)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "confirmation index references a missing observation",
                        false,
                    )
                })?;
            let current_domain = record::observation_from_record(current.value.transaction.clone());
            let (inclusion_block, confirmations) = match &current_domain.status {
                TransactionStatus::Included {
                    block,
                    confirmations,
                } => (block.clone(), *confirmations),
                _ => {
                    return Err(IndexError::new(
                        IndexErrorKind::Storage,
                        "confirmation index references a non-included observation",
                        false,
                    ));
                }
            };
            if inclusion_block.height.0 != pending.value.inclusion_height {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "confirmation index inclusion height is inconsistent",
                    false,
                ));
            }
            let depth = command
                .block
                .block
                .height
                .0
                .checked_sub(inclusion_block.height.0)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "block tip cannot prove the indexed inclusion",
                        false,
                    )
                })?;
            pending_records.insert(transaction_id.clone(), (pending_key, pending.version));
            if depth <= confirmations {
                continue;
            }
            let status = if depth >= self.config.confirmation_policy.minimum_confirmations {
                TransactionStatus::Confirmed {
                    block: inclusion_block,
                    proof: ConfirmationProof::Depth {
                        required: self.config.confirmation_policy.minimum_confirmations,
                        observed: depth,
                    },
                }
            } else {
                TransactionStatus::Included {
                    block: inclusion_block,
                    confirmations: depth,
                }
            };
            let next = self.next_observation(
                Some(&current.value),
                &transaction_id,
                status,
                None,
                transition_time,
            )?;
            transitions.insert(
                transaction_id,
                Transition {
                    prior: Some(current.value),
                    prior_version: Some(current.version),
                    next,
                    included_here: false,
                    prior_indexed_in_generation: true,
                },
            );
        }

        let mut draft_ids = BTreeSet::new();
        for draft in &command.block.drafts {
            self.validate_draft(draft, &active_watch_ids)?;
            if !draft_ids.insert(draft.transaction_id.clone())
                || transitions.contains_key(&draft.transaction_id)
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains a duplicate transaction observation",
                    false,
                ));
            }
            let prior = self
                .current_observation(generation, &draft.transaction_id)
                .await?;
            if prior.as_ref().is_some_and(|prior| {
                matches!(
                    record::observation_from_record(prior.value.transaction.clone()).status,
                    TransactionStatus::Included { .. }
                        | TransactionStatus::Confirmed { .. }
                        | TransactionStatus::Failed { block: Some(_), .. }
                )
            }) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "a canonical transaction is already included at another height",
                    false,
                ));
            }
            let status = match &draft.status {
                ObservationDraftStatus::Included => TransactionStatus::Included {
                    block: command.block.block.clone(),
                    confirmations: 1,
                },
                ObservationDraftStatus::Failed { reason } => TransactionStatus::Failed {
                    block: Some(command.block.block.clone()),
                    reason: reason.clone(),
                },
            };
            let next = self.next_observation(
                prior.as_ref().map(|prior| &prior.value),
                &draft.transaction_id,
                status,
                Some(draft),
                draft.observed_at,
            )?;
            transitions.insert(
                draft.transaction_id.clone(),
                Transition {
                    prior: prior.as_ref().map(|prior| prior.value.clone()),
                    prior_version: prior.as_ref().map(|prior| prior.version),
                    next,
                    included_here: true,
                    prior_indexed_in_generation: true,
                },
            );
        }

        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = if publish_events && !transitions.is_empty() {
            self.counter(&event_counter_key).await?
        } else {
            None
        };
        let mut next_cursor = event_counter
            .as_ref()
            .map_or(0, |counter| counter.value.value);
        for (transaction_id, transition) in &transitions {
            let cursor = if publish_events {
                next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "observation event cursor is exhausted",
                        false,
                    )
                })?;
                Some(EventCursor(next_cursor))
            } else {
                None
            };
            self.append_transition(&mut batch, generation, transition, cursor)?;

            if transition.included_here {
                if matches!(
                    record::status_from_record(transition.next.transaction.status.clone()),
                    TransactionStatus::Included { .. }
                ) {
                    let pending_key = keys::pending_confirmation(
                        &self.config.scope,
                        generation,
                        command.block.block.height,
                        &transaction_id.chain.0,
                        &transaction_id.value,
                    );
                    batch.conditions.push(Condition::Missing {
                        namespace: keys::namespace(),
                        key: pending_key.clone(),
                    });
                    Self::put(
                        &mut batch,
                        pending_key,
                        &PendingConfirmationRecordV1 {
                            transaction_id: record::chain_value_from_transaction(transaction_id),
                            inclusion_height: command.block.block.height.0,
                        },
                    )?;
                }
            } else if matches!(
                record::status_from_record(transition.next.transaction.status.clone()),
                TransactionStatus::Confirmed { .. }
            ) {
                let (pending_key, pending_version) =
                    pending_records.get(transaction_id).ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Storage,
                            "confirmation transition lost its pending index",
                            false,
                        )
                    })?;
                batch.conditions.push(Condition::Version {
                    namespace: keys::namespace(),
                    key: pending_key.clone(),
                    expected: *pending_version,
                });
                Self::delete(&mut batch, pending_key.clone());
            }
        }
        if publish_events && !transitions.is_empty() {
            Self::condition_for(
                &mut batch,
                event_counter_key.clone(),
                event_counter.as_ref(),
            );
            Self::put(
                &mut batch,
                event_counter_key,
                &CounterRecordV1 { value: next_cursor },
            )?;
        }

        self.append_projection_batch(
            &mut batch,
            generation,
            &command.block.projection,
            IndexErrorKind::InvalidBlock,
        )
        .await?;

        let encoded_undo = self.codec.encode_undo(&command.block.undo)?;
        let bundle = BundleRecordV1 {
            block: record::block_to_record(&command.block.block),
            prior_checkpoint: command
                .expected_checkpoint
                .as_ref()
                .map(record::block_to_record),
            encoded_undo,
            raw_block: command.block.raw.block.clone(),
            raw_receipts: command.block.raw.receipts.clone(),
            changes: transitions
                .values()
                .map(|transition| BundleChangeRecordV1 {
                    transaction_id: transition.next.transaction.transaction_id.clone(),
                    prior: transition.prior.clone(),
                    included_here: transition.included_here,
                })
                .collect(),
        };
        let bundle_key = keys::bundle(&self.config.scope, generation, command.block.block.height);
        batch.conditions.push(Condition::Missing {
            namespace: keys::namespace(),
            key: canonical_key.clone(),
        });
        batch.conditions.push(Condition::Missing {
            namespace: keys::namespace(),
            key: bundle_key.clone(),
        });
        Self::put(
            &mut batch,
            canonical_key,
            &record::block_to_record(&command.block.block),
        )?;
        Self::put(&mut batch, bundle_key, &bundle)?;
        Self::condition_for(&mut batch, checkpoint_key.clone(), checkpoint.as_ref());
        Self::put(
            &mut batch,
            checkpoint_key,
            &record::block_to_record(&command.block.block),
        )?;

        if command.block.block.height.0 >= command.reorg_retention {
            let anchor_height = BlockHeight(
                command
                    .block
                    .block
                    .height
                    .0
                    .saturating_sub(command.reorg_retention),
            );
            Self::delete(
                &mut batch,
                keys::bundle(&self.config.scope, generation, anchor_height),
            );
            Self::delete(
                &mut batch,
                keys::backfill_projection_rollback(&self.config.scope, generation, anchor_height),
            );
            if let Some(pruned_height) = anchor_height.0.checked_sub(1) {
                Self::delete(
                    &mut batch,
                    keys::canonical(&self.config.scope, generation, BlockHeight(pruned_height)),
                );
            }
        }

        if let Some(rebuild) = rebuild {
            let rebuild_key = keys::rebuild_state(&self.config.scope);
            batch.conditions.push(Condition::Version {
                namespace: keys::namespace(),
                key: rebuild_key.clone(),
                expected: rebuild.version,
            });
            let mut next_rebuild = rebuild.value.clone();
            next_rebuild.checkpoint = Some(record::block_to_record(&command.block.block));
            Self::put(&mut batch, rebuild_key, &next_rebuild)?;
        }

        self.append_projection_revision(&mut batch).await?;

        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(CommitBlockOutcome::Applied)
    }

    async fn revert_active_tip(
        &self,
        command: RevertTipCommand,
    ) -> Result<RevertTipOutcome, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let active = self.active_generation_record().await?;
        let generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, generation);
        let checkpoint = self.get_record::<BlockRefRecordV1>(&checkpoint_key).await?;
        let current_checkpoint = checkpoint
            .as_ref()
            .map(|checkpoint| record::block_from_record(checkpoint.value.clone()));
        if current_checkpoint.as_ref() != Some(&command.expected_tip) {
            if current_checkpoint
                .as_ref()
                .is_none_or(|checkpoint| checkpoint.height < command.expected_tip.height)
            {
                return Ok(RevertTipOutcome::AlreadyReverted {
                    checkpoint: current_checkpoint,
                });
            }
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "revert must target the exact newest canonical tip",
                true,
            ));
        }

        let canonical_key =
            keys::canonical(&self.config.scope, generation, command.expected_tip.height);
        let canonical = self
            .get_record::<BlockRefRecordV1>(&canonical_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "canonical tip record is missing",
                    false,
                )
            })?;
        if record::block_from_record(canonical.value.clone()) != command.expected_tip {
            return Err(IndexError::new(
                IndexErrorKind::Storage,
                "canonical tip record does not match the checkpoint",
                false,
            ));
        }
        let bundle_key = keys::bundle(&self.config.scope, generation, command.expected_tip.height);
        let bundle = self
            .get_record::<BundleRecordV1>(&bundle_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::ReorgBeyondRetention,
                    "tip undo bundle is outside the retained rollback window",
                    false,
                )
            })?;
        if record::block_from_record(bundle.value.block.clone()) != command.expected_tip {
            return Err(IndexError::new(
                IndexErrorKind::Storage,
                "undo bundle does not match the canonical tip",
                false,
            ));
        }
        // Decoding detects a chain-codec or on-disk undo incompatibility before
        // any canonical state is changed. The codec may also derive inverse
        // opaque projection changes from the decoded chain-owned undo.
        let decoded_undo = self.codec.decode_undo(&bundle.value.encoded_undo)?;
        let mut rollback_projection = self.codec.rollback_projection(&decoded_undo)?;
        let backfill_rollback_key = keys::backfill_projection_rollback(
            &self.config.scope,
            generation,
            command.expected_tip.height,
        );
        let backfill_rollback = self
            .get_record::<BackfillProjectionRollbackRecordV1>(&backfill_rollback_key)
            .await?;
        if let Some(backfill_rollback) = &backfill_rollback {
            if record::block_from_record(backfill_rollback.value.block.clone())
                != command.expected_tip
            {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "historical projection rollback record does not match the canonical tip",
                    false,
                ));
            }
            let mut rollback_keys = rollback_projection
                .mutations
                .iter()
                .map(ProjectionMutation::key)
                .map(<[u8]>::to_vec)
                .collect::<BTreeSet<_>>();
            if rollback_keys.len() != rollback_projection.mutations.len() {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "chain projection rollback contains duplicate keys",
                    false,
                ));
            }
            for key in &backfill_rollback.value.relative_keys {
                if !rollback_keys.insert(key.clone()) {
                    let is_same_delete = rollback_projection.mutations.iter().any(|mutation| {
                        matches!(mutation, ProjectionMutation::Delete { key: existing } if existing == key)
                    });
                    if !is_same_delete {
                        return Err(IndexError::new(
                            IndexErrorKind::Storage,
                            "chain and historical projection rollback overlap is not an identical delete",
                            false,
                        ));
                    }
                    continue;
                }
                rollback_projection
                    .mutations
                    .push(ProjectionMutation::Delete { key: key.clone() });
            }
        }

        let prior_checkpoint = bundle
            .value
            .prior_checkpoint
            .clone()
            .map(record::block_from_record);
        let observed_at = prior_checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.timestamp)
            .unwrap_or(command.expected_tip.height.0);
        let mut batch = self.mutation_batch().await?;
        Self::condition_for(
            &mut batch,
            keys::active_generation(&self.config.scope),
            active.as_ref(),
        );
        Self::condition_for(&mut batch, checkpoint_key.clone(), checkpoint.as_ref());
        Self::condition_for(&mut batch, canonical_key.clone(), Some(&canonical));
        Self::condition_for(&mut batch, bundle_key.clone(), Some(&bundle));
        Self::condition_for(
            &mut batch,
            backfill_rollback_key.clone(),
            backfill_rollback.as_ref(),
        );
        self.append_projection_batch(
            &mut batch,
            generation,
            &rollback_projection,
            IndexErrorKind::Storage,
        )
        .await?;
        if backfill_rollback.is_some() {
            Self::delete(&mut batch, backfill_rollback_key);
        }

        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = if bundle.value.changes.is_empty() {
            None
        } else {
            self.counter(&event_counter_key).await?
        };
        let mut next_cursor = event_counter
            .as_ref()
            .map_or(0, |counter| counter.value.value);
        for change in &bundle.value.changes {
            let transaction_id =
                record::transaction_from_chain_value(change.transaction_id.clone());
            let current = self
                .current_observation(generation, &transaction_id)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "undo bundle references a missing current observation",
                        false,
                    )
                })?;
            let current_domain = record::observation_from_record(current.value.transaction.clone());
            let next = if change.included_here {
                self.next_observation(
                    Some(&current.value),
                    &transaction_id,
                    TransactionStatus::Reorged {
                        previous_block: command.expected_tip.clone(),
                    },
                    None,
                    observed_at,
                )?
            } else {
                let prior = change.prior.as_ref().ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "confirmation rollback is missing its prior observation",
                        false,
                    )
                })?;
                let mut prior_domain = record::observation_from_record(prior.transaction.clone());
                prior_domain.revision = ObservationRevision(
                    current_domain.revision.0.checked_add(1).ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Storage,
                            "observation revision is exhausted",
                            false,
                        )
                    })?,
                );
                prior_domain.observed_at = observed_at;
                CurrentObservationRecordV1 {
                    transaction: record::observation_to_record(&prior_domain),
                    watch_ids: prior.watch_ids.clone(),
                }
            };
            let transition = Transition {
                prior: Some(current.value.clone()),
                prior_version: Some(current.version),
                next,
                included_here: change.included_here,
                prior_indexed_in_generation: true,
            };
            next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "observation event cursor is exhausted",
                    false,
                )
            })?;
            self.append_transition(
                &mut batch,
                generation,
                &transition,
                Some(EventCursor(next_cursor)),
            )?;

            if change.included_here {
                if matches!(
                    current_domain.status,
                    TransactionStatus::Included { .. } | TransactionStatus::Confirmed { .. }
                ) {
                    let pending_key = keys::pending_confirmation(
                        &self.config.scope,
                        generation,
                        command.expected_tip.height,
                        &transaction_id.chain.0,
                        &transaction_id.value,
                    );
                    if let Some(pending) = self
                        .get_record::<PendingConfirmationRecordV1>(&pending_key)
                        .await?
                    {
                        Self::condition_for(&mut batch, pending_key.clone(), Some(&pending));
                        Self::delete(&mut batch, pending_key);
                    }
                }
            } else if let TransactionStatus::Included { block, .. } =
                record::observation_from_record(transition.next.transaction.clone()).status
            {
                let pending_key = keys::pending_confirmation(
                    &self.config.scope,
                    generation,
                    block.height,
                    &transaction_id.chain.0,
                    &transaction_id.value,
                );
                if self
                    .get_record::<PendingConfirmationRecordV1>(&pending_key)
                    .await?
                    .is_none()
                {
                    Self::condition_for::<PendingConfirmationRecordV1>(
                        &mut batch,
                        pending_key.clone(),
                        None,
                    );
                    Self::put(
                        &mut batch,
                        pending_key,
                        &PendingConfirmationRecordV1 {
                            transaction_id: record::chain_value_from_transaction(&transaction_id),
                            inclusion_height: block.height.0,
                        },
                    )?;
                }
            }
        }
        if !bundle.value.changes.is_empty() {
            Self::condition_for(
                &mut batch,
                event_counter_key.clone(),
                event_counter.as_ref(),
            );
            Self::put(
                &mut batch,
                event_counter_key,
                &CounterRecordV1 { value: next_cursor },
            )?;
        }
        self.reconcile_watch_backfills_for_revert(
            &mut batch,
            &command.expected_tip,
            prior_checkpoint.as_ref(),
        )
        .await?;
        Self::delete(&mut batch, bundle_key);
        Self::delete(&mut batch, canonical_key);
        match &prior_checkpoint {
            Some(prior) => Self::put(&mut batch, checkpoint_key, &record::block_to_record(prior))?,
            None => Self::delete(&mut batch, checkpoint_key),
        }
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(RevertTipOutcome::Reverted {
            checkpoint: prior_checkpoint,
        })
    }

    async fn reconcile_watch_backfills_for_revert(
        &self,
        batch: &mut WriteBatch,
        reverted: &BlockRef,
        prior_checkpoint: Option<&BlockRef>,
    ) -> Result<(), IndexError> {
        let height_markers = self
            .scan_records::<WatchBackfillAppliedHeightRecordV1>(
                keys::watch_backfill_applied_height_prefix(&self.config.scope, reverted.height),
            )
            .await?;
        let mut affected_watches = BTreeSet::new();
        for (height_marker_key, height_marker) in height_markers {
            if !affected_watches.insert(height_marker.value.watch_id.clone()) {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "backfill height index contains a duplicate watch",
                    false,
                ));
            }
            let marker_key = keys::watch_backfill_applied(
                &self.config.scope,
                &height_marker.value.watch_id,
                reverted.height,
            );
            let marker = self
                .get_record::<WatchBackfillAppliedRecordV1>(&marker_key)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "backfill height index references a missing applied marker",
                        false,
                    )
                })?;
            if record::block_from_record(marker.value.block.clone()) != *reverted {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "backfill applied marker does not match the reverted canonical block",
                    false,
                ));
            }
            Self::condition_for(batch, height_marker_key.clone(), Some(&height_marker));
            Self::condition_for(batch, marker_key.clone(), Some(&marker));
            Self::delete(batch, height_marker_key);
            Self::delete(batch, marker_key);
        }

        let jobs = self
            .scan_records::<WatchBackfillRecordV1>(keys::watch_backfill_prefix(&self.config.scope))
            .await?;
        for (job_key, job) in jobs {
            let job_scope = record::scope_from_record(job.value.scope.clone());
            record::ensure_record_scope(&self.config.scope, &job_scope, "watch backfill")?;
            let through = record::block_from_record(job.value.through.clone());
            if through.height < reverted.height {
                continue;
            }
            if through != *reverted {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "watch backfill anchor is ahead of or differs from the reverted tip",
                    false,
                ));
            }

            Self::condition_for(batch, job_key.clone(), Some(&job));
            match prior_checkpoint {
                Some(prior)
                    if prior.height.0 >= job.value.from_height
                        && job.value.next_height <= prior.height.0 =>
                {
                    let mut updated = job.value;
                    updated.through = record::block_to_record(prior);
                    Self::put(batch, job_key, &updated)?;
                }
                Some(_) | None => Self::delete(batch, job_key),
            }
        }
        Ok(())
    }

    async fn query_transaction(
        &self,
        request: TransactionRequest,
    ) -> Result<Option<ObservedTransaction>, IndexError> {
        self.check_scope(&request.scope)?;
        self.validate_transaction_id(&request.transaction_id)?;
        self.verify_metadata().await?;
        self.ensure_semantic_available().await?;
        let generation = self.active_generation().await?;
        self.current_observation(generation, &request.transaction_id)
            .await
            .map(|current| {
                current.map(|current| record::observation_from_record(current.value.transaction))
            })
    }

    async fn query_watch_backfills(
        &self,
        scope: &IndexScope,
        limit: usize,
    ) -> Result<Vec<WatchBackfill>, IndexError> {
        self.check_scope(scope)?;
        Self::validate_query_limit(limit)?;
        self.verify_metadata().await?;
        self.ensure_semantic_available().await?;
        let page = self
            .storage
            .scan(ScanRequest {
                namespace: keys::namespace(),
                prefix: keys::watch_backfill_prefix(scope),
                after: None,
                limit,
            })
            .await
            .map_err(Self::storage_error)?;
        page.entries
            .into_iter()
            .map(|(_, stored)| {
                let backfill = Self::decode::<WatchBackfillRecordV1>(&stored.value.0)?;
                let backfill_scope = record::scope_from_record(backfill.scope);
                record::ensure_record_scope(scope, &backfill_scope, "watch backfill")?;
                Ok(WatchBackfill {
                    scope: backfill_scope,
                    watch_id: WatchId(backfill.watch_id),
                    from_height: BlockHeight(backfill.from_height),
                    next_height: BlockHeight(backfill.next_height),
                    through: record::block_from_record(backfill.through),
                })
            })
            .collect()
    }

    async fn apply_watch_backfill(
        &self,
        command: CommitWatchBackfillCommand,
        projection: ProjectionBatch,
    ) -> Result<CommitWatchBackfillOutcome, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        if command.block.height != command.expected_next_height {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "backfill block height differs from the expected job cursor",
                false,
            ));
        }
        let marker_key = keys::watch_backfill_applied(
            &self.config.scope,
            &command.watch_id.0,
            command.block.height,
        );
        let height_marker_key = keys::watch_backfill_applied_height(
            &self.config.scope,
            command.block.height,
            &command.watch_id.0,
        );
        if let Some(marker) = self
            .get_record::<WatchBackfillAppliedRecordV1>(&marker_key)
            .await?
        {
            if record::block_from_record(marker.value.block) != command.block {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "backfill applied marker contains another canonical block",
                    false,
                ));
            }
            let height_marker = self
                .get_record::<WatchBackfillAppliedHeightRecordV1>(&height_marker_key)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "backfill applied marker is missing its height index",
                        false,
                    )
                })?;
            if height_marker.value.watch_id != command.watch_id.0 {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "backfill applied height index references another watch",
                    false,
                ));
            }
            let next_height = self
                .get_record::<WatchBackfillRecordV1>(&keys::watch_backfill(
                    &self.config.scope,
                    &command.watch_id.0,
                ))
                .await?
                .map(|job| BlockHeight(job.value.next_height));
            return Ok(CommitWatchBackfillOutcome::AlreadyApplied { next_height });
        }
        self.ensure_semantic_available().await?;

        let job_key = keys::watch_backfill(&self.config.scope, &command.watch_id.0);
        let job = self
            .get_record::<WatchBackfillRecordV1>(&job_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "watch has no pending historical backfill",
                    false,
                )
            })?;
        let job_scope = record::scope_from_record(job.value.scope.clone());
        record::ensure_record_scope(&self.config.scope, &job_scope, "watch backfill")?;
        if job.value.watch_id != command.watch_id.0
            || job.value.next_height != command.expected_next_height.0
            || command.block.height.0 > job.value.through.height
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "backfill command no longer matches the durable job cursor",
                true,
            ));
        }

        let mut batch = self.mutation_batch().await?;
        let active = self.active_generation_record().await?;
        let generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, generation);
        let checkpoint = self
            .get_record::<BlockRefRecordV1>(&checkpoint_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Conflict,
                    "backfill cannot run without a live canonical checkpoint",
                    true,
                )
            })?;
        let live_checkpoint = record::block_from_record(checkpoint.value.clone());
        if live_checkpoint != command.expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "live checkpoint changed while the historical block was interpreted",
                true,
            ));
        }
        let through = record::block_from_record(job.value.through.clone());
        if command.block.height > through.height {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "backfill block is beyond its durable registration checkpoint",
                false,
            ));
        }
        if through.height > live_checkpoint.height {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "backfill registration checkpoint is ahead of the live canonical checkpoint",
                true,
            ));
        }
        // `through` is a durable hash anchor, not a dependency on the live
        // retention window. The live tip may move arbitrarily far ahead while
        // this job progresses. A shallow reorg rewrites the anchor atomically
        // in `revert_active_tip`; reaching the terminal height must still match
        // the exact registration-era block.
        if command.block.height == through.height && command.block != through {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "backfill through checkpoint is no longer canonical",
                true,
            ));
        }
        if let Some(canonical) = self
            .get_record::<BlockRefRecordV1>(&keys::canonical(
                &self.config.scope,
                generation,
                command.block.height,
            ))
            .await?
        {
            if record::block_from_record(canonical.value) != command.block {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "historical block hash differs from retained canonical state",
                    true,
                ));
            }
        }
        let previous_marker = if command.block.height.0 > job.value.from_height {
            let previous_height = BlockHeight(command.block.height.0.saturating_sub(1));
            let previous_key = keys::watch_backfill_applied(
                &self.config.scope,
                &command.watch_id.0,
                previous_height,
            );
            let previous = self
                .get_record::<WatchBackfillAppliedRecordV1>(&previous_key)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Conflict,
                        "previous backfill height has not been durably applied",
                        true,
                    )
                })?;
            let previous_height_key = keys::watch_backfill_applied_height(
                &self.config.scope,
                previous_height,
                &command.watch_id.0,
            );
            let previous_height_marker = self
                .get_record::<WatchBackfillAppliedHeightRecordV1>(&previous_height_key)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "previous backfill marker is missing its height index",
                        false,
                    )
                })?;
            if previous_height_marker.value.watch_id != command.watch_id.0 {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "previous backfill height index references another watch",
                    false,
                ));
            }
            let previous_block = record::block_from_record(previous.value.block.clone());
            if command.block.parent_hash.as_ref() != Some(&previous_block.hash) {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "historical block does not connect to the prior backfill height",
                    true,
                ));
            }
            Some((
                previous_key,
                previous,
                previous_height_key,
                previous_height_marker,
            ))
        } else {
            None
        };

        let watch_key = keys::watch(&self.config.scope, &command.watch_id.0);
        let watch = self
            .get_record::<WatchRecordV1>(&watch_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "backfill watch record is missing",
                    false,
                )
            })?;
        if watch.value.start_height > command.block.height.0
            || watch
                .value
                .inactive_from
                .is_some_and(|inactive| command.block.height.0 >= inactive)
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidWatch,
                "backfill watch is not active at the historical height",
                false,
            ));
        }
        let active_watch_ids = BTreeSet::from([command.watch_id.clone()]);
        let mut draft_ids = BTreeSet::new();
        let depth = live_checkpoint
            .height
            .0
            .checked_sub(command.block.height.0)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "live checkpoint cannot prove the historical inclusion",
                    false,
                )
            })?;
        let mut transitions = BTreeMap::new();
        for draft in &command.drafts {
            self.validate_draft(draft, &active_watch_ids)?;
            if draft.watch_ids.as_slice() != [command.watch_id.clone()]
                || !draft_ids.insert(draft.transaction_id.clone())
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "backfill drafts must be unique and belong only to the backfill watch",
                    false,
                ));
            }
            let prior = self
                .current_observation(generation, &draft.transaction_id)
                .await?;
            let mut status = match &draft.status {
                ObservationDraftStatus::Included
                    if depth >= self.config.confirmation_policy.minimum_confirmations =>
                {
                    TransactionStatus::Confirmed {
                        block: command.block.clone(),
                        proof: ConfirmationProof::Depth {
                            required: self.config.confirmation_policy.minimum_confirmations,
                            // Match live synchronization: confirmation proof is
                            // pinned to the threshold, not the discovery tip.
                            observed: self.config.confirmation_policy.minimum_confirmations,
                        },
                    }
                }
                ObservationDraftStatus::Included => TransactionStatus::Included {
                    block: command.block.clone(),
                    confirmations: depth,
                },
                ObservationDraftStatus::Failed { reason } => TransactionStatus::Failed {
                    block: Some(command.block.clone()),
                    reason: reason.clone(),
                },
            };
            if let Some(prior) = &prior {
                let prior_domain = record::observation_from_record(prior.value.transaction.clone());
                let same_canonical_block = match &prior_domain.status {
                    TransactionStatus::Included { block, .. }
                    | TransactionStatus::Confirmed { block, .. } => block == &command.block,
                    TransactionStatus::Failed {
                        block: Some(block), ..
                    } => block == &command.block,
                    TransactionStatus::Pending
                    | TransactionStatus::Failed { block: None, .. }
                    | TransactionStatus::Replaced { .. }
                    | TransactionStatus::Dropped
                    | TransactionStatus::Reorged { .. } => false,
                };
                if matches!(
                    prior_domain.status,
                    TransactionStatus::Included { .. }
                        | TransactionStatus::Confirmed { .. }
                        | TransactionStatus::Failed { block: Some(_), .. }
                ) && !same_canonical_block
                    || prior_domain.movements != draft.movements
                    || prior_domain.fee != draft.fee
                {
                    return Err(IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "backfill fact conflicts with the current transaction projection",
                        false,
                    ));
                }
                if same_canonical_block {
                    // Another watch already caused this canonical fact to be
                    // indexed. Preserve its live-derived confirmation state;
                    // this backfill transition only merges watch ownership.
                    status = prior_domain.status.clone();
                }
                if prior.value.watch_ids.contains(&command.watch_id.0)
                    && prior_domain.status == status
                {
                    continue;
                }
            }
            let mut merged = draft.clone();
            if let Some(prior) = &prior {
                merged
                    .watch_ids
                    .extend(prior.value.watch_ids.iter().cloned().map(WatchId));
                merged.watch_ids.sort();
                merged.watch_ids.dedup();
            }
            let next = self.next_observation(
                prior.as_ref().map(|prior| &prior.value),
                &draft.transaction_id,
                status,
                Some(&merged),
                draft.observed_at,
            )?;
            transitions.insert(
                draft.transaction_id.clone(),
                Transition {
                    prior: prior.as_ref().map(|prior| prior.value.clone()),
                    prior_version: prior.as_ref().map(|prior| prior.version),
                    next,
                    included_here: false,
                    prior_indexed_in_generation: true,
                },
            );
        }

        Self::condition_for(
            &mut batch,
            keys::active_generation(&self.config.scope),
            active.as_ref(),
        );
        Self::condition_for(&mut batch, checkpoint_key, Some(&checkpoint));
        Self::condition_for(&mut batch, job_key.clone(), Some(&job));
        Self::condition_for(&mut batch, watch_key, Some(&watch));
        if let Some((previous_key, previous, previous_height_key, previous_height_marker)) =
            &previous_marker
        {
            Self::condition_for(&mut batch, previous_key.clone(), Some(previous));
            Self::condition_for(
                &mut batch,
                previous_height_key.clone(),
                Some(previous_height_marker),
            );
            Self::delete(&mut batch, previous_key.clone());
            Self::delete(&mut batch, previous_height_key.clone());
        }
        Self::condition_for::<WatchBackfillAppliedRecordV1>(&mut batch, marker_key.clone(), None);
        Self::condition_for::<WatchBackfillAppliedHeightRecordV1>(
            &mut batch,
            height_marker_key.clone(),
            None,
        );

        let introduced_projection_keys = self
            .append_backfill_projection(&mut batch, generation, &projection)
            .await?;

        self.extend_backfill_confirmation_undo(
            &mut batch,
            generation,
            &command.block,
            &live_checkpoint,
            &transitions,
            &command.watch_id,
        )
        .await?;

        // Extend a still-retained undo bundle so a later shallow reorg also
        // corrects facts discovered after the original block commit. Older
        // heights deliberately have no bundle and require staged rebuild if
        // their canonical ancestry changes.
        let bundle_key = keys::bundle(&self.config.scope, generation, command.block.height);
        if let Some(bundle) = self.get_record::<BundleRecordV1>(&bundle_key).await? {
            let mut updated = bundle.value.clone();
            let existing: BTreeSet<_> = updated
                .changes
                .iter()
                .map(|change| record::transaction_from_chain_value(change.transaction_id.clone()))
                .collect();
            for (transaction_id, transition) in &transitions {
                if !existing.contains(transaction_id) {
                    updated.changes.push(BundleChangeRecordV1 {
                        transaction_id: record::chain_value_from_transaction(transaction_id),
                        prior: transition.prior.clone(),
                        included_here: true,
                    });
                }
            }
            if updated != bundle.value {
                Self::condition_for(&mut batch, bundle_key.clone(), Some(&bundle));
                Self::put(&mut batch, bundle_key, &updated)?;
            }

            if !introduced_projection_keys.is_empty() {
                let rollback_key = keys::backfill_projection_rollback(
                    &self.config.scope,
                    generation,
                    command.block.height,
                );
                let rollback = self
                    .get_record::<BackfillProjectionRollbackRecordV1>(&rollback_key)
                    .await?;
                let mut rollback_value = rollback.as_ref().map_or_else(
                    || BackfillProjectionRollbackRecordV1 {
                        block: record::block_to_record(&command.block),
                        relative_keys: Vec::new(),
                    },
                    |rollback| rollback.value.clone(),
                );
                if record::block_from_record(rollback_value.block.clone()) != command.block {
                    return Err(IndexError::new(
                        IndexErrorKind::Storage,
                        "historical projection rollback record belongs to another block",
                        false,
                    ));
                }
                let mut rollback_keys = rollback_value
                    .relative_keys
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if rollback_keys.len() != rollback_value.relative_keys.len() {
                    return Err(IndexError::new(
                        IndexErrorKind::Storage,
                        "historical projection rollback record contains duplicate keys",
                        false,
                    ));
                }
                rollback_keys.extend(introduced_projection_keys);
                rollback_value.relative_keys = rollback_keys.into_iter().collect();
                Self::condition_for(&mut batch, rollback_key.clone(), rollback.as_ref());
                Self::put(&mut batch, rollback_key, &rollback_value)?;
            }
        }

        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = if transitions.is_empty() {
            None
        } else {
            self.counter(&event_counter_key).await?
        };
        let mut next_cursor = event_counter
            .as_ref()
            .map_or(0, |counter| counter.value.value);
        for (transaction_id, transition) in &transitions {
            next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "observation event cursor is exhausted",
                    false,
                )
            })?;
            self.append_transition(
                &mut batch,
                generation,
                transition,
                Some(EventCursor(next_cursor)),
            )?;
            let next_status =
                record::status_from_record(transition.next.transaction.status.clone());
            let pending_key = keys::pending_confirmation(
                &self.config.scope,
                generation,
                command.block.height,
                &transaction_id.chain.0,
                &transaction_id.value,
            );
            let pending = self
                .get_record::<PendingConfirmationRecordV1>(&pending_key)
                .await?;
            match next_status {
                TransactionStatus::Included { .. } if pending.is_none() => {
                    Self::condition_for::<PendingConfirmationRecordV1>(
                        &mut batch,
                        pending_key.clone(),
                        None,
                    );
                    Self::put(
                        &mut batch,
                        pending_key,
                        &PendingConfirmationRecordV1 {
                            transaction_id: record::chain_value_from_transaction(transaction_id),
                            inclusion_height: command.block.height.0,
                        },
                    )?;
                }
                TransactionStatus::Confirmed { .. } if pending.is_some() => {
                    Self::condition_for(&mut batch, pending_key.clone(), pending.as_ref());
                    Self::delete(&mut batch, pending_key);
                }
                TransactionStatus::Pending
                | TransactionStatus::Included { .. }
                | TransactionStatus::Confirmed { .. }
                | TransactionStatus::Failed { .. }
                | TransactionStatus::Replaced { .. }
                | TransactionStatus::Dropped
                | TransactionStatus::Reorged { .. } => {}
            }
        }
        if !transitions.is_empty() {
            Self::condition_for(
                &mut batch,
                event_counter_key.clone(),
                event_counter.as_ref(),
            );
            Self::put(
                &mut batch,
                event_counter_key,
                &CounterRecordV1 { value: next_cursor },
            )?;
        }
        Self::put(
            &mut batch,
            marker_key,
            &WatchBackfillAppliedRecordV1 {
                block: record::block_to_record(&command.block),
            },
        )?;
        Self::put(
            &mut batch,
            height_marker_key,
            &WatchBackfillAppliedHeightRecordV1 {
                watch_id: command.watch_id.0.clone(),
            },
        )?;
        let next_height = if command.block.height.0 < through.height.0 {
            Some(BlockHeight(
                command.block.height.0.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "watch backfill height is exhausted",
                        false,
                    )
                })?,
            ))
        } else {
            None
        };
        match next_height {
            Some(next_height) => {
                let mut updated = job.value;
                updated.next_height = next_height.0;
                Self::put(&mut batch, job_key, &updated)?;
            }
            None => Self::delete(&mut batch, job_key),
        }
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(CommitWatchBackfillOutcome::Applied { next_height })
    }

    async fn extend_backfill_confirmation_undo(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        inclusion: &BlockRef,
        live_checkpoint: &BlockRef,
        transitions: &BTreeMap<CanonicalTransactionId, Transition>,
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
                    IndexErrorKind::Storage,
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
                    IndexErrorKind::Storage,
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
                .get_record::<BundleRecordV1>(&bundle_key)
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
                    IndexErrorKind::Storage,
                    "confirmation rollback bundle has an unexpected height",
                    false,
                ));
            }
            let mut updated = bundle.value.clone();
            for (transaction_id, transition) in transitions {
                if !matches!(
                    record::status_from_record(transition.next.transaction.status.clone()),
                    TransactionStatus::Included { .. } | TransactionStatus::Confirmed { .. }
                ) {
                    continue;
                }
                let transaction_record = record::chain_value_from_transaction(transaction_id);
                let prior_was_canonical_here =
                    transition.prior.as_ref().is_some_and(
                        |prior| match record::status_from_record(prior.transaction.status.clone()) {
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
                        },
                    );
                if prior_was_canonical_here {
                    let change = updated
                        .changes
                        .iter_mut()
                        .find(|change| change.transaction_id == transaction_record)
                        .ok_or_else(|| {
                            IndexError::new(
                                IndexErrorKind::Storage,
                                "canonical observation is missing retained confirmation undo",
                                false,
                            )
                        })?;
                    let prior = change.prior.as_mut().ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Storage,
                            "confirmation rollback is missing its prior observation",
                            false,
                        )
                    })?;
                    if !prior.watch_ids.contains(&watch_id.0) {
                        prior.watch_ids.push(watch_id.0.clone());
                        prior.watch_ids.sort();
                        prior.watch_ids.dedup();
                    }
                    continue;
                }

                if updated
                    .changes
                    .iter()
                    .any(|change| change.transaction_id == transaction_record)
                {
                    return Err(IndexError::new(
                        IndexErrorKind::Storage,
                        "new backfill observation conflicts with retained confirmation undo",
                        false,
                    ));
                }
                let prior_depth = height.0.checked_sub(inclusion.height.0).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "confirmation rollback height precedes the inclusion",
                        false,
                    )
                })?;
                let mut prior = transition.next.clone();
                let mut prior_domain = record::observation_from_record(prior.transaction.clone());
                prior_domain.status = TransactionStatus::Included {
                    block: inclusion.clone(),
                    confirmations: prior_depth,
                };
                prior.transaction = record::observation_to_record(&prior_domain);
                updated.changes.push(BundleChangeRecordV1 {
                    transaction_id: transaction_record,
                    prior: Some(prior),
                    included_here: false,
                });
            }
            if updated != bundle.value {
                Self::condition_for(batch, bundle_key.clone(), Some(&bundle));
                Self::put(batch, bundle_key, &updated)?;
            }
        }
        Ok(())
    }

    async fn query_transactions_by_address(
        &self,
        request: TransactionPageRequest,
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
        let prefix = keys::address_transaction_prefix(
            &self.config.scope,
            generation,
            &request.address.chain.0,
            &request.address.value,
        );
        let after = request.after.as_ref().map(|after| {
            keys::address_transaction(
                &self.config.scope,
                generation,
                &request.address.chain.0,
                &request.address.value,
                &after.chain.0,
                &after.value,
            )
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
            let transaction_id =
                record::transaction_from_chain_value(Self::decode::<ChainValueRecordV1>(
                    &stored.value.0,
                )?);
            let transaction = self
                .current_observation(generation, &transaction_id)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "address index references a missing observation",
                        false,
                    )
                })?;
            transactions.push(record::observation_from_record(
                transaction.value.transaction,
            ));
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

    async fn query_watches_for_address(
        &self,
        request: AddressWatchRequest,
    ) -> Result<Vec<WatchReceipt>, IndexError> {
        self.check_scope(&request.scope)?;
        self.validate_address(&request.address)?;
        self.verify_metadata().await?;
        self.ensure_semantic_available().await?;
        let watches = self
            .scan_records::<WatchRecordV1>(keys::watch_prefix(&self.config.scope))
            .await?;
        Ok(watches
            .into_iter()
            .filter(|(_, watch)| {
                record::selector_from_record(watch.value.selector.clone())
                    == WatchSelector::Address(request.address.clone())
            })
            .map(|(_, watch)| self.watch_receipt(&watch.value))
            .collect())
    }

    async fn query_events(
        &self,
        request: ObservationEventRequest,
    ) -> Result<ObservationEventPage, IndexError> {
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
            events.push(record::event_from_record(Self::decode::<EventRecordV1>(
                &stored.value.0,
            )?));
        }
        let next = has_more
            .then(|| events.last().map(|event| event.cursor))
            .flatten();
        Ok(ObservationEventPage { events, next })
    }

    async fn query_status(&self, scope: &IndexScope) -> Result<SyncStatus, IndexError> {
        self.check_scope(scope)?;
        self.verify_metadata().await?;
        let mut status = self
            .get_record::<SyncStatusRecordV1>(&keys::status(scope))
            .await?
            .map_or_else(
                || SyncStatus::starting(scope.clone(), self.config.confirmation_policy),
                |status| record::sync_status_from_record(status.value),
            );
        record::ensure_record_scope(scope, &status.scope, "status")?;
        let generation = self.active_generation().await?;
        status.checkpoint = self
            .generation_checkpoint(generation)
            .await?
            .map(|checkpoint| record::block_from_record(checkpoint.value));
        Ok(status)
    }

    async fn persist_status(&self, status: SyncStatus) -> Result<(), IndexError> {
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
        let existing = self.get_record::<SyncStatusRecordV1>(&status_key).await?;
        if let Some(existing) = &existing {
            match record::sync_status_from_record(existing.value.clone()).phase {
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
            &record::sync_status_to_record(&status),
        )?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(())
    }

    async fn apply_policy_migration(
        &self,
        command: MigrateIndexPolicyCommand,
    ) -> Result<MigrateIndexPolicyOutcome, IndexError> {
        self.validate_policy_migration_command(&command)?;

        let meta_key = keys::meta(&command.scope);
        let meta = self
            .get_record::<RepositoryMetaRecordV1>(&meta_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "an uninitialized repository has no policy to migrate",
                    false,
                )
            })?;
        if meta.value.format_version != RECORD_FORMAT_VERSION {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "repository record format requires a separate schema migration",
                false,
            ));
        }
        let persisted_scope = record::scope_from_record(meta.value.scope.clone());
        if persisted_scope != command.scope
            || meta.value.bootstrap_height != command.bootstrap_height.0
        {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "policy migration cannot relabel repository scope or bootstrap height",
                false,
            ));
        }

        let id_key = keys::policy_migration_id(&command.scope, &command.idempotency_key);
        if let Some(id) = self
            .get_record::<PolicyMigrationIdRecordV1>(&id_key)
            .await?
        {
            let version = PolicyMigrationVersion(id.value.version);
            let audit = self
                .get_record::<PolicyMigrationRecordV1>(&keys::policy_migration(
                    &command.scope,
                    version.0,
                ))
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Storage,
                        "policy migration id points to a missing audit record",
                        false,
                    )
                })?;
            if audit.value != Self::policy_migration_record(&command, version) {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "policy migration idempotency key was reused with a different payload",
                    false,
                ));
            }
            return Ok(MigrateIndexPolicyOutcome::AlreadyApplied { version });
        }

        let persisted_policy = crate::ConfirmationPolicy {
            minimum_confirmations: meta.value.confirmation_depth,
            require_chain_finality: meta.value.require_chain_finality,
        };
        if persisted_policy != command.expected_confirmation_policy
            || meta.value.reorg_retention != command.expected_reorg_retention
        {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "persisted policy does not match the migration's expected source policy",
                false,
            ));
        }

        let rebuild_key = keys::rebuild_state(&command.scope);
        if self
            .get_record::<RebuildStateRecordV1>(&rebuild_key)
            .await?
            .is_some()
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "policy migration is forbidden while a staged rebuild is active",
                false,
            ));
        }

        let active_key = keys::active_generation(&command.scope);
        let active = self.get_record::<CounterRecordV1>(&active_key).await?;
        let active_generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        let checkpoint_key = keys::canonical_checkpoint(&command.scope, active_generation);
        let checkpoint = self.get_record::<BlockRefRecordV1>(&checkpoint_key).await?;
        let checkpoint_domain = checkpoint
            .as_ref()
            .map(|checkpoint| record::block_from_record(checkpoint.value.clone()));

        let status_key = keys::status(&command.scope);
        let persisted_status = self.get_record::<SyncStatusRecordV1>(&status_key).await?;
        let mut status = persisted_status.as_ref().map_or_else(
            || SyncStatus::starting(command.scope.clone(), command.expected_confirmation_policy),
            |status| record::sync_status_from_record(status.value.clone()),
        );
        record::ensure_record_scope(&command.scope, &status.scope, "policy migration status")?;
        if status.confirmation_policy != command.expected_confirmation_policy {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "persisted status disagrees with repository policy metadata",
                false,
            ));
        }
        if checkpoint_domain.is_some() && status.phase != SyncPhase::Ready {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "checkpointed policy migration requires a Ready index with no recovery in progress",
                false,
            ));
        }
        if checkpoint_domain.is_none()
            && !matches!(status.phase, SyncPhase::Starting | SyncPhase::Ready)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "policy migration cannot replace an existing recovery or halted status",
                false,
            ));
        }
        status.confirmation_policy = command.target_confirmation_policy;
        status.checkpoint = checkpoint_domain.clone();
        status.halted_reason = None;
        if let Some(checkpoint) = checkpoint_domain {
            status.phase = SyncPhase::RebuildRequired;
            status.rebuild_reason = Some(RebuildReason {
                checkpoint: checkpoint.clone(),
                oldest_retained: BlockHeight(
                    checkpoint
                        .height
                        .0
                        .saturating_sub(command.expected_reorg_retention)
                        .max(command.bootstrap_height.0),
                ),
                message: "index policy migration requires staged rebuild activation".to_owned(),
            });
        } else {
            status.phase = SyncPhase::Starting;
            status.rebuild_reason = None;
        }

        let counter_key = keys::policy_migration_counter(&command.scope);
        let counter = self.get_record::<CounterRecordV1>(&counter_key).await?;
        let version = PolicyMigrationVersion(counter.as_ref().map_or(Ok(1), |counter| {
            counter.value.value.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "policy migration version is exhausted",
                    false,
                )
            })
        })?);
        let audit_key = keys::policy_migration(&command.scope, version.0);
        let guard_key = keys::mutation_guard(&command.scope);
        let guard = self.get_record::<CounterRecordV1>(&guard_key).await?;
        let next_guard = guard.as_ref().map_or(Ok(1), |guard| {
            guard.value.value.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "IX mutation guard is exhausted",
                    false,
                )
            })
        })?;

        let mut batch = WriteBatch::default();
        Self::condition_for(&mut batch, meta_key.clone(), Some(&meta));
        Self::condition_for(&mut batch, guard_key.clone(), guard.as_ref());
        Self::condition_for(&mut batch, counter_key.clone(), counter.as_ref());
        Self::condition_for::<PolicyMigrationIdRecordV1>(&mut batch, id_key.clone(), None);
        Self::condition_for::<PolicyMigrationRecordV1>(&mut batch, audit_key.clone(), None);
        Self::condition_for::<RebuildStateRecordV1>(&mut batch, rebuild_key, None);
        Self::condition_for(&mut batch, active_key, active.as_ref());
        Self::condition_for(&mut batch, checkpoint_key, checkpoint.as_ref());
        Self::condition_for(&mut batch, status_key.clone(), persisted_status.as_ref());
        Self::put(
            &mut batch,
            guard_key,
            &CounterRecordV1 { value: next_guard },
        )?;
        Self::put(&mut batch, meta_key, &self.expected_meta())?;
        Self::put(
            &mut batch,
            counter_key,
            &CounterRecordV1 { value: version.0 },
        )?;
        Self::put(
            &mut batch,
            audit_key,
            &Self::policy_migration_record(&command, version),
        )?;
        Self::put(
            &mut batch,
            id_key,
            &PolicyMigrationIdRecordV1 { version: version.0 },
        )?;
        Self::put(
            &mut batch,
            status_key,
            &record::sync_status_to_record(&status),
        )?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(MigrateIndexPolicyOutcome::Applied { version })
    }

    async fn query_rebuild_state(
        &self,
        scope: &IndexScope,
    ) -> Result<Option<RebuildState>, IndexError> {
        self.check_scope(scope)?;
        self.verify_metadata().await?;
        self.get_record::<RebuildStateRecordV1>(&keys::rebuild_state(scope))
            .await
            .map(|state| state.map(|state| record::rebuild_state_from_record(state.value)))
    }

    async fn start_rebuild(
        &self,
        command: BeginRebuildCommand,
    ) -> Result<RebuildState, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        if command.bootstrap_height != self.config.bootstrap_height {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "rebuild bootstrap height differs from persistent configuration",
                false,
            ));
        }
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        if let Some(existing) = self
            .get_record::<RebuildStateRecordV1>(&rebuild_key)
            .await?
        {
            let existing = record::rebuild_state_from_record(existing.value);
            if existing.bootstrap_height == command.bootstrap_height {
                return Ok(existing);
            }
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "another staged rebuild is already active",
                false,
            ));
        }
        let mut batch = self.mutation_batch().await?;
        let counter_key = keys::rebuild_counter(&self.config.scope);
        let counter = self.counter(&counter_key).await?;
        let generation = RebuildGeneration(counter.as_ref().map_or(Ok(1), |counter| {
            counter.value.value.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "rebuild generation counter is exhausted",
                    false,
                )
            })
        })?);
        let active = self.active_generation().await?;
        if generation == active {
            return Err(IndexError::new(
                IndexErrorKind::Storage,
                "new rebuild generation collides with the active generation",
                false,
            ));
        }
        let event_counter = self
            .counter(&keys::event_counter(&self.config.scope))
            .await?;
        let state = RebuildState {
            scope: self.config.scope.clone(),
            generation,
            phase: RebuildPhase::Building,
            bootstrap_height: self.config.bootstrap_height,
            checkpoint: None,
            published_event_high_water: EventCursor(
                event_counter
                    .as_ref()
                    .map_or(0, |counter| counter.value.value),
            ),
        };
        Self::condition_for(&mut batch, counter_key.clone(), counter.as_ref());
        Self::condition_for::<RebuildStateRecordV1>(&mut batch, rebuild_key.clone(), None);
        Self::put(
            &mut batch,
            counter_key,
            &CounterRecordV1 {
                value: generation.0,
            },
        )?;
        Self::put(
            &mut batch,
            rebuild_key,
            &record::rebuild_state_to_record(&state),
        )?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(state)
    }

    async fn commit_shadow_block(
        &self,
        command: CommitRebuildBlockCommand<C::Undo>,
    ) -> Result<CommitBlockOutcome, IndexError> {
        let rebuild = self
            .get_record::<RebuildStateRecordV1>(&keys::rebuild_state(&self.config.scope))
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "no staged rebuild is active",
                    false,
                )
            })?;
        let state = record::rebuild_state_from_record(rebuild.value.clone());
        if state.generation != command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild command targets another generation",
                false,
            ));
        }
        if state.phase != RebuildPhase::Building {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild generation is not accepting blocks",
                false,
            ));
        }
        if state.checkpoint != command.command.expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild expected checkpoint differs from its durable manifest",
                true,
            ));
        }
        self.commit_generation(
            command.command,
            command.generation,
            false,
            None,
            Some(&rebuild),
        )
        .await
    }

    async fn generation_observations(
        &self,
        generation: RebuildGeneration,
    ) -> Result<BTreeMap<CanonicalTransactionId, Versioned<CurrentObservationRecordV1>>, IndexError>
    {
        self.scan_records::<CurrentObservationRecordV1>(keys::current_observation_prefix(
            &self.config.scope,
            generation,
        ))
        .await?
        .into_iter()
        .map(|(_, current)| {
            let transaction = record::observation_from_record(current.value.transaction.clone());
            self.validate_transaction_id(&transaction.transaction_id)?;
            Ok((transaction.transaction_id, current))
        })
        .collect()
    }

    fn same_projection(
        left: &CurrentObservationRecordV1,
        right: &CurrentObservationRecordV1,
    ) -> bool {
        let left_transaction = record::observation_from_record(left.transaction.clone());
        let right_transaction = record::observation_from_record(right.transaction.clone());
        left_transaction.scope == right_transaction.scope
            && left_transaction.transaction_id == right_transaction.transaction_id
            && left_transaction.status == right_transaction.status
            && left_transaction.movements == right_transaction.movements
            && left_transaction.fee == right_transaction.fee
            && left_transaction.first_seen_at == right_transaction.first_seen_at
            && left.watch_ids == right.watch_ids
    }

    fn status_block(status: &TransactionStatus, fallback: &BlockRef) -> BlockRef {
        match status {
            TransactionStatus::Included { block, .. }
            | TransactionStatus::Confirmed { block, .. } => block.clone(),
            TransactionStatus::Failed {
                block: Some(block), ..
            } => block.clone(),
            TransactionStatus::Reorged { previous_block } => previous_block.clone(),
            TransactionStatus::Pending
            | TransactionStatus::Failed { block: None, .. }
            | TransactionStatus::Replaced { .. }
            | TransactionStatus::Dropped => fallback.clone(),
        }
    }

    fn make_event(
        current: &CurrentObservationRecordV1,
        previous_status: Option<record::TransactionStatusRecordV1>,
        cursor: EventCursor,
    ) -> EventRecordV1 {
        let transaction = record::observation_from_record(current.transaction.clone());
        EventRecordV1 {
            id: Self::event_id(cursor, transaction.revision),
            cursor: cursor.0,
            watch_ids: current.watch_ids.clone(),
            previous_status,
            transaction: current.transaction.clone(),
        }
    }

    fn append_prepared_rebuild_event(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        current: &CurrentObservationRecordV1,
        previous_status: Option<record::TransactionStatusRecordV1>,
        cursor: EventCursor,
    ) -> Result<(), IndexError> {
        let key = keys::prepared_rebuild_event(&self.config.scope, generation, cursor);
        batch.conditions.push(Condition::Missing {
            namespace: keys::namespace(),
            key: key.clone(),
        });
        Self::put(
            batch,
            key,
            &Self::make_event(current, previous_status, cursor),
        )
    }

    async fn rebuild_for_checkpoint(
        &self,
        scope: &IndexScope,
        generation: RebuildGeneration,
        expected_checkpoint: &BlockRef,
    ) -> Result<
        (
            Versioned<RebuildStateRecordV1>,
            RebuildState,
            Versioned<BlockRefRecordV1>,
        ),
        IndexError,
    > {
        self.check_scope(scope)?;
        self.verify_metadata().await?;
        let rebuild = self
            .get_record::<RebuildStateRecordV1>(&keys::rebuild_state(&self.config.scope))
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "no staged rebuild is active",
                    false,
                )
            })?;
        let state = record::rebuild_state_from_record(rebuild.value.clone());
        if state.generation != generation || state.checkpoint.as_ref() != Some(expected_checkpoint)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild generation or checkpoint does not match its durable manifest",
                false,
            ));
        }
        let shadow_checkpoint = self
            .generation_checkpoint(generation)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "staged generation has no checkpoint",
                    false,
                )
            })?;
        if record::block_from_record(shadow_checkpoint.value.clone()) != *expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Storage,
                "staged checkpoint differs from the rebuild manifest",
                false,
            ));
        }
        Ok((rebuild, state, shadow_checkpoint))
    }

    async fn mark_rebuild_validating(
        &self,
        command: ValidateRebuildCommand,
    ) -> Result<RebuildState, IndexError> {
        let (rebuild, mut state, shadow_checkpoint) = self
            .rebuild_for_checkpoint(
                &command.scope,
                command.generation,
                &command.expected_checkpoint,
            )
            .await?;
        match state.phase {
            RebuildPhase::Building => {}
            RebuildPhase::Validating | RebuildPhase::ReadyToActivate => return Ok(state),
        }

        state.phase = RebuildPhase::Validating;
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        let checkpoint_key = keys::canonical_checkpoint(&self.config.scope, command.generation);
        let mut batch = self.mutation_batch().await?;
        Self::condition_for(&mut batch, rebuild_key.clone(), Some(&rebuild));
        Self::condition_for(&mut batch, checkpoint_key, Some(&shadow_checkpoint));
        Self::put(
            &mut batch,
            rebuild_key,
            &record::rebuild_state_to_record(&state),
        )?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(state)
    }

    async fn prepare_rebuild(
        &self,
        command: PrepareRebuildActivationCommand,
    ) -> Result<RebuildState, IndexError> {
        let (rebuild, mut state, shadow_checkpoint) = self
            .rebuild_for_checkpoint(
                &command.scope,
                command.generation,
                &command.expected_checkpoint,
            )
            .await?;
        match state.phase {
            RebuildPhase::Building => {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "rebuild generation must be validated before activation is prepared",
                    false,
                ));
            }
            RebuildPhase::ReadyToActivate => return Ok(state),
            RebuildPhase::Validating => {}
        }

        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = self.counter(&event_counter_key).await?;
        let published_cursor = event_counter
            .as_ref()
            .map_or(EventCursor(0), |counter| EventCursor(counter.value.value));
        if published_cursor != state.published_event_high_water {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "published event cursor changed while the rebuild was staged",
                true,
            ));
        }
        if !self
            .scan_records::<EventRecordV1>(keys::prepared_rebuild_event_prefix(
                &self.config.scope,
                command.generation,
            ))
            .await?
            .is_empty()
        {
            return Err(IndexError::new(
                IndexErrorKind::Storage,
                "validating rebuild already contains prepared correction events",
                false,
            ));
        }

        let active = self.active_generation_record().await?;
        let active_generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        if active_generation == command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "the staged rebuild generation is already active",
                false,
            ));
        }
        let old = self.generation_observations(active_generation).await?;
        let new = self.generation_observations(command.generation).await?;
        let transaction_ids: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
        let mut next_cursor = state.published_event_high_water.0;
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        let mut batch = self.mutation_batch().await?;
        Self::condition_for(
            &mut batch,
            keys::active_generation(&self.config.scope),
            active.as_ref(),
        );
        Self::condition_for(&mut batch, rebuild_key.clone(), Some(&rebuild));
        Self::condition_for(
            &mut batch,
            keys::canonical_checkpoint(&self.config.scope, command.generation),
            Some(&shadow_checkpoint),
        );
        Self::condition_for(&mut batch, event_counter_key, event_counter.as_ref());

        for transaction_id in transaction_ids {
            match (old.get(&transaction_id), new.get(&transaction_id)) {
                (Some(old), Some(new)) if Self::same_projection(&old.value, &new.value) => {
                    let old_domain = record::observation_from_record(old.value.transaction.clone());
                    let mut new_domain =
                        record::observation_from_record(new.value.transaction.clone());
                    if old_domain.revision > new_domain.revision {
                        new_domain.revision = old_domain.revision;
                        let carried = CurrentObservationRecordV1 {
                            transaction: record::observation_to_record(&new_domain),
                            watch_ids: new.value.watch_ids.clone(),
                        };
                        let current_key = keys::current_observation(
                            &self.config.scope,
                            command.generation,
                            &transaction_id.chain.0,
                            &transaction_id.value,
                        );
                        Self::condition_for(&mut batch, current_key.clone(), Some(new));
                        Self::put(&mut batch, current_key, &carried)?;
                        let revision_key = keys::observation_revision(
                            &self.config.scope,
                            command.generation,
                            &transaction_id.chain.0,
                            &transaction_id.value,
                            new_domain.revision,
                        );
                        if self
                            .get_record::<ObservationRecordV1>(&revision_key)
                            .await?
                            .is_none()
                        {
                            Self::condition_for::<ObservationRecordV1>(
                                &mut batch,
                                revision_key.clone(),
                                None,
                            );
                            Self::put(&mut batch, revision_key, &carried.transaction)?;
                        }
                    }
                }
                (Some(old), Some(new)) => {
                    let old_domain = record::observation_from_record(old.value.transaction.clone());
                    let mut new_domain =
                        record::observation_from_record(new.value.transaction.clone());
                    new_domain.revision = ObservationRevision(
                        old_domain
                            .revision
                            .0
                            .max(new_domain.revision.0)
                            .checked_add(1)
                            .ok_or_else(|| {
                                IndexError::new(
                                    IndexErrorKind::Storage,
                                    "observation revision is exhausted",
                                    false,
                                )
                            })?,
                    );
                    let transition = Transition {
                        prior: Some(old.value.clone()),
                        prior_version: Some(new.version),
                        next: CurrentObservationRecordV1 {
                            transaction: record::observation_to_record(&new_domain),
                            watch_ids: new.value.watch_ids.clone(),
                        },
                        included_here: false,
                        prior_indexed_in_generation: true,
                    };
                    next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Storage,
                            "observation event cursor is exhausted",
                            false,
                        )
                    })?;
                    self.append_transition(&mut batch, command.generation, &transition, None)?;
                    self.append_prepared_rebuild_event(
                        &mut batch,
                        command.generation,
                        &transition.next,
                        transition
                            .prior
                            .as_ref()
                            .map(|prior| prior.transaction.status.clone()),
                        EventCursor(next_cursor),
                    )?;
                }
                (Some(old), None) => {
                    let old_domain = record::observation_from_record(old.value.transaction.clone());
                    let next = self.next_observation(
                        Some(&old.value),
                        &transaction_id,
                        TransactionStatus::Reorged {
                            previous_block: Self::status_block(
                                &old_domain.status,
                                &command.expected_checkpoint,
                            ),
                        },
                        None,
                        command
                            .expected_checkpoint
                            .timestamp
                            .unwrap_or(command.expected_checkpoint.height.0),
                    )?;
                    let transition = Transition {
                        prior: Some(old.value.clone()),
                        prior_version: None,
                        next,
                        included_here: false,
                        prior_indexed_in_generation: false,
                    };
                    next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Storage,
                            "observation event cursor is exhausted",
                            false,
                        )
                    })?;
                    self.append_transition(&mut batch, command.generation, &transition, None)?;
                    self.append_prepared_rebuild_event(
                        &mut batch,
                        command.generation,
                        &transition.next,
                        transition
                            .prior
                            .as_ref()
                            .map(|prior| prior.transaction.status.clone()),
                        EventCursor(next_cursor),
                    )?;
                }
                (None, Some(new)) => {
                    next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::Storage,
                            "observation event cursor is exhausted",
                            false,
                        )
                    })?;
                    self.append_prepared_rebuild_event(
                        &mut batch,
                        command.generation,
                        &new.value,
                        None,
                        EventCursor(next_cursor),
                    )?;
                }
                (None, None) => {}
            }
        }

        state.phase = RebuildPhase::ReadyToActivate;
        Self::put(
            &mut batch,
            rebuild_key,
            &record::rebuild_state_to_record(&state),
        )?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(state)
    }

    async fn publish_rebuild(&self, command: ActivateRebuildCommand) -> Result<(), IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let active = self.active_generation_record().await?;
        let active_generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        let Some(rebuild) = self
            .get_record::<RebuildStateRecordV1>(&rebuild_key)
            .await?
        else {
            if active_generation == command.generation
                && self
                    .generation_checkpoint(active_generation)
                    .await?
                    .is_some_and(|checkpoint| {
                        record::block_from_record(checkpoint.value) == command.expected_checkpoint
                    })
            {
                return Ok(());
            }
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "no matching staged rebuild is active",
                false,
            ));
        };
        let rebuild_state = record::rebuild_state_from_record(rebuild.value.clone());
        if rebuild_state.generation != command.generation
            || rebuild_state.checkpoint.as_ref() != Some(&command.expected_checkpoint)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild generation or checkpoint does not match its durable manifest",
                false,
            ));
        }
        if rebuild_state.phase != RebuildPhase::ReadyToActivate {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "rebuild generation has not been prepared for activation",
                false,
            ));
        }
        let shadow_checkpoint = self
            .generation_checkpoint(command.generation)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "staged generation has no checkpoint",
                    false,
                )
            })?;
        if record::block_from_record(shadow_checkpoint.value.clone()) != command.expected_checkpoint
        {
            return Err(IndexError::new(
                IndexErrorKind::Storage,
                "staged checkpoint differs from the rebuild manifest",
                false,
            ));
        }

        let prepared_events = self
            .scan_records::<EventRecordV1>(keys::prepared_rebuild_event_prefix(
                &self.config.scope,
                command.generation,
            ))
            .await?;
        let event_counter_key = keys::event_counter(&self.config.scope);
        let event_counter = self.counter(&event_counter_key).await?;
        let published_cursor = event_counter
            .as_ref()
            .map_or(EventCursor(0), |counter| EventCursor(counter.value.value));
        if published_cursor != rebuild_state.published_event_high_water {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "published event cursor changed after rebuild corrections were prepared",
                true,
            ));
        }
        let mut next_cursor = rebuild_state.published_event_high_water.0;
        for (_, event) in &prepared_events {
            next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Storage,
                    "observation event cursor is exhausted",
                    false,
                )
            })?;
            let transaction = record::observation_from_record(event.value.transaction.clone());
            let expected_id = Self::event_id(EventCursor(next_cursor), transaction.revision);
            if event.value.cursor != next_cursor || event.value.id != expected_id {
                return Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "prepared rebuild events are corrupt or non-contiguous",
                    false,
                ));
            }
        }

        let mut batch = self.mutation_batch().await?;
        Self::condition_for(
            &mut batch,
            keys::active_generation(&self.config.scope),
            active.as_ref(),
        );
        Self::condition_for(&mut batch, rebuild_key.clone(), Some(&rebuild));
        Self::condition_for(
            &mut batch,
            keys::canonical_checkpoint(&self.config.scope, command.generation),
            Some(&shadow_checkpoint),
        );
        Self::condition_for(
            &mut batch,
            event_counter_key.clone(),
            event_counter.as_ref(),
        );
        for (prepared_key, prepared) in &prepared_events {
            let cursor = EventCursor(prepared.value.cursor);
            let event_key = keys::event(&self.config.scope, cursor);
            let event_id_key = keys::event_id(&self.config.scope, &prepared.value.id);
            batch.conditions.push(Condition::Version {
                namespace: keys::namespace(),
                key: prepared_key.clone(),
                expected: prepared.version,
            });
            batch.conditions.push(Condition::Missing {
                namespace: keys::namespace(),
                key: event_key.clone(),
            });
            batch.conditions.push(Condition::Missing {
                namespace: keys::namespace(),
                key: event_id_key.clone(),
            });
            Self::put(&mut batch, event_key, &prepared.value)?;
            Self::put(
                &mut batch,
                event_id_key,
                &EventIdRecordV1 {
                    cursor: prepared.value.cursor,
                },
            )?;
            Self::delete(&mut batch, prepared_key.clone());
        }
        if !prepared_events.is_empty() {
            Self::put(
                &mut batch,
                event_counter_key,
                &CounterRecordV1 { value: next_cursor },
            )?;
        }
        Self::put(
            &mut batch,
            keys::active_generation(&self.config.scope),
            &CounterRecordV1 {
                value: command.generation.0,
            },
        )?;
        Self::delete(&mut batch, rebuild_key);

        let status_key = keys::status(&self.config.scope);
        let persisted_status = self.get_record::<SyncStatusRecordV1>(&status_key).await?;
        let mut status = persisted_status.as_ref().map_or_else(
            || SyncStatus::starting(self.config.scope.clone(), self.config.confirmation_policy),
            |status| record::sync_status_from_record(status.value.clone()),
        );
        status.checkpoint = Some(command.expected_checkpoint);
        status.phase = SyncPhase::Ready;
        status.rebuild_reason = None;
        status.halted_reason = None;
        Self::condition_for(&mut batch, status_key.clone(), persisted_status.as_ref());
        Self::put(
            &mut batch,
            status_key,
            &record::sync_status_to_record(&status),
        )?;
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(())
    }

    async fn cancel_rebuild(&self, command: AbortRebuildCommand) -> Result<(), IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let active = self.active_generation().await?;
        if active == command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "the active generation cannot be aborted",
                false,
            ));
        }
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        let Some(rebuild) = self
            .get_record::<RebuildStateRecordV1>(&rebuild_key)
            .await?
        else {
            return Ok(());
        };
        let state = record::rebuild_state_from_record(rebuild.value.clone());
        if state.generation != command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "abort targets another rebuild generation",
                false,
            ));
        }
        let mut batch = self.mutation_batch().await?;
        batch.conditions.push(Condition::Version {
            namespace: keys::namespace(),
            key: rebuild_key.clone(),
            expected: rebuild.version,
        });
        for key in self.generation_cleanup_keys(command.generation).await? {
            Self::delete(&mut batch, key);
        }
        Self::delete(&mut batch, rebuild_key);
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(())
    }

    async fn generation_cleanup_keys(
        &self,
        generation: RebuildGeneration,
    ) -> Result<Vec<Key>, IndexError> {
        let mut keys_to_delete = Vec::new();
        for prefix in keys::generation_prefixes(&self.config.scope, generation) {
            // Generation records have different DTOs, so cleanup scans raw
            // values and uses only their already-validated logical keys.
            let mut after = None;
            loop {
                let page = self
                    .storage
                    .scan(ScanRequest {
                        namespace: keys::namespace(),
                        prefix: prefix.clone(),
                        after,
                        limit: SCAN_CHUNK,
                    })
                    .await
                    .map_err(Self::storage_error)?;
                keys_to_delete.extend(page.entries.into_iter().map(|(key, _)| key));
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        Ok(keys_to_delete)
    }

    async fn remove_generation(
        &self,
        command: CleanupGenerationCommand,
    ) -> Result<CleanupGenerationOutcome, IndexError> {
        self.check_scope(&command.scope)?;
        self.verify_metadata().await?;
        let active = self.active_generation_record().await?;
        let active_generation = RebuildGeneration(
            active
                .as_ref()
                .map_or(BASE_GENERATION.0, |active| active.value.value),
        );
        if active_generation == command.generation {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "active generation cannot be cleaned up",
                false,
            ));
        }
        let rebuild_key = keys::rebuild_state(&self.config.scope);
        let rebuild = self
            .get_record::<RebuildStateRecordV1>(&rebuild_key)
            .await?;
        if rebuild
            .as_ref()
            .is_some_and(|rebuild| rebuild.value.generation == command.generation.0)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "current staged rebuild generation cannot be cleaned up; abort it explicitly",
                false,
            ));
        }
        let keys_to_delete = self.generation_cleanup_keys(command.generation).await?;
        if keys_to_delete.is_empty() {
            return Ok(CleanupGenerationOutcome::AlreadyAbsent);
        }
        let removed = u64::try_from(keys_to_delete.len()).map_err(|_| {
            IndexError::new(
                IndexErrorKind::Storage,
                "generation cleanup record count does not fit in u64",
                false,
            )
        })?;
        let mut batch = self.mutation_batch().await?;
        Self::condition_for(
            &mut batch,
            keys::active_generation(&self.config.scope),
            active.as_ref(),
        );
        Self::condition_for(&mut batch, rebuild_key, rebuild.as_ref());
        for key in keys_to_delete {
            Self::delete(&mut batch, key);
        }
        self.append_projection_revision(&mut batch).await?;
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(CleanupGenerationOutcome::Removed { records: removed })
    }
}
