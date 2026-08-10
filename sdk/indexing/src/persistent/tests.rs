use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use bincode::{Decode, config as bincode_config};
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use futures_executor::block_on;
use storage::{
    BoxFuture, CommitResult, Condition, Key, Namespace, Operation, ScanPage, ScanRequest, Storage,
    StorageError, StorageErrorKind, StoredValue, Version, WriteBatch,
};

use super::{
    IndexRecordCodec, PersistentIndexConfig, PersistentIndexRepository, RawBytesIndexCodec, keys,
    record::{PolicyMigrationIdRecordV1, PolicyMigrationRecordV1},
};
use crate::{
    AbortRebuildCommand, ActivateRebuildCommand, AddressWatchRequest, BeginRebuildCommand,
    BlockHash, BlockHeight, BlockRef, CleanupGenerationCommand, CleanupGenerationOutcome,
    CommitBlockCommand, CommitBlockOutcome, CommitRebuildBlockCommand, CommitWatchBackfillCommand,
    CommitWatchBackfillOutcome, ConfirmationPolicy, ConfirmationProof, EventCursor, IndexError,
    IndexErrorKind, IndexRepository, IndexScope, InterpretedBlock, MigrateIndexPolicyCommand,
    MigrateIndexPolicyOutcome, MovementId, MovementKind, ObservationDraft, ObservationDraftStatus,
    ObservationEventRequest, PolicyMigrationVersion, PrepareRebuildActivationCommand,
    ProjectionBatch, ProjectionGetRequest, ProjectionMutation, ProjectionQuery,
    ProjectionScanRequest, RawBlockData, RegisterWatchCommand, RegisterWatchOutcome,
    RevertTipCommand, RevertTipOutcome, SyncPhase, SyncStatus, TransactionPageRequest,
    TransactionRequest, TransactionStatus, UnwatchCommand, UnwatchOutcome, ValidateRebuildCommand,
    ValueMovement, WatchId, WatchRequest, WatchSelector, WatchVersion,
};

#[derive(Clone, Default)]
struct MemoryStorage {
    state: Arc<Mutex<MemoryState>>,
}

#[derive(Default)]
struct MemoryState {
    records: BTreeMap<(Namespace, Key), StoredValue>,
    version: u64,
    fail_before_commit: bool,
    lose_next_acknowledgement: bool,
}

impl MemoryStorage {
    fn fail_before_next_commit(&self) {
        self.state
            .lock()
            .expect("memory storage lock is healthy")
            .fail_before_commit = true;
    }

    fn lose_next_acknowledgement(&self) {
        self.state
            .lock()
            .expect("memory storage lock is healthy")
            .lose_next_acknowledgement = true;
    }
}

fn unavailable(message: &str) -> StorageError {
    StorageError {
        kind: StorageErrorKind::Unavailable,
        message: message.to_owned(),
    }
}

fn conflict(message: &str) -> StorageError {
    StorageError {
        kind: StorageErrorKind::Conflict,
        message: message.to_owned(),
    }
}

impl Storage for MemoryStorage {
    fn get<'a>(
        &'a self,
        namespace: &'a Namespace,
        key: &'a Key,
    ) -> BoxFuture<'a, Result<Option<StoredValue>, StorageError>> {
        let result = self
            .state
            .lock()
            .expect("memory storage lock is healthy")
            .records
            .get(&(namespace.clone(), key.clone()))
            .cloned();
        Box::pin(async move { Ok(result) })
    }

    fn scan<'a>(&'a self, request: ScanRequest) -> BoxFuture<'a, Result<ScanPage, StorageError>> {
        let result = if request.limit == 0 {
            Err(StorageError {
                kind: StorageErrorKind::InvalidRequest,
                message: "scan limit must be non-zero".to_owned(),
            })
        } else {
            let state = self.state.lock().expect("memory storage lock is healthy");
            let mut entries: Vec<_> = state
                .records
                .iter()
                .filter(|((namespace, key), _)| {
                    namespace == &request.namespace
                        && key.0.starts_with(&request.prefix)
                        && request.after.as_ref().is_none_or(|after| key > after)
                })
                .map(|((_, key), value)| (key.clone(), value.clone()))
                .take(request.limit.saturating_add(1))
                .collect();
            let next = if entries.len() > request.limit {
                entries.pop();
                entries.last().map(|(key, _)| key.clone())
            } else {
                None
            };
            Ok(ScanPage { entries, next })
        };
        Box::pin(async move { result })
    }

    fn commit<'a>(
        &'a self,
        batch: WriteBatch,
    ) -> BoxFuture<'a, Result<CommitResult, StorageError>> {
        let result = {
            let mut state = self.state.lock().expect("memory storage lock is healthy");
            if state.fail_before_commit {
                state.fail_before_commit = false;
                Err(unavailable("injected failure before atomic commit"))
            } else {
                let conditions = batch.conditions.iter().try_for_each(|condition| {
                    match condition {
                        Condition::Missing { namespace, key } => {
                            if state
                                .records
                                .contains_key(&(namespace.clone(), key.clone()))
                            {
                                return Err(conflict("missing condition failed"));
                            }
                        }
                        Condition::Version {
                            namespace,
                            key,
                            expected,
                        } => match state.records.get(&(namespace.clone(), key.clone())) {
                            Some(stored) if stored.version == *expected => {}
                            _ => return Err(conflict("version condition failed")),
                        },
                    }
                    Ok(())
                });
                conditions.and_then(|()| {
                    state.version = state.version.checked_add(1).ok_or_else(|| StorageError {
                        kind: StorageErrorKind::Other,
                        message: "test storage version exhausted".to_owned(),
                    })?;
                    let version = Version(state.version);
                    for operation in batch.operations {
                        match operation {
                            Operation::Put {
                                namespace,
                                key,
                                value,
                            } => {
                                state
                                    .records
                                    .insert((namespace, key), StoredValue { value, version });
                            }
                            Operation::Delete { namespace, key } => {
                                state.records.remove(&(namespace, key));
                            }
                        }
                    }
                    if state.lose_next_acknowledgement {
                        state.lose_next_acknowledgement = false;
                        Err(unavailable("injected response loss after atomic commit"))
                    } else {
                        Ok(CommitResult { version })
                    }
                })
            }
        };
        Box::pin(async move { result })
    }
}

type Repository = PersistentIndexRepository<MemoryStorage, RawBytesIndexCodec>;

#[derive(Clone, Copy, Debug, Default)]
struct ProjectionTestCodec;

impl IndexRecordCodec for ProjectionTestCodec {
    type Target = Vec<u8>;
    type Undo = Vec<u8>;

    fn encode_target(&self, target: &Self::Target) -> Result<Vec<u8>, IndexError> {
        Ok(target.clone())
    }

    fn decode_target(&self, encoded: &[u8]) -> Result<Self::Target, IndexError> {
        Ok(encoded.to_vec())
    }

    fn encode_undo(&self, undo: &Self::Undo) -> Result<Vec<u8>, IndexError> {
        Ok(undo.clone())
    }

    fn decode_undo(&self, encoded: &[u8]) -> Result<Self::Undo, IndexError> {
        Ok(encoded.to_vec())
    }

    fn rollback_projection(&self, undo: &Self::Undo) -> Result<ProjectionBatch, IndexError> {
        let mutations = match undo.as_slice() {
            [1, 1] => vec![
                ProjectionMutation::Delete {
                    key: b"address/a/1".to_vec(),
                },
                ProjectionMutation::Delete {
                    key: b"address/a/2".to_vec(),
                },
                ProjectionMutation::Delete {
                    key: b"utxo/1".to_vec(),
                },
            ],
            [1, 2] => vec![ProjectionMutation::Put {
                key: b"utxo/1".to_vec(),
                value: b"created".to_vec(),
            }],
            [9, 1] => vec![ProjectionMutation::Delete {
                key: b"conditional/marker".to_vec(),
            }],
            _ => Vec::new(),
        };
        Ok(ProjectionBatch::new(mutations))
    }
}

type ProjectionRepository = PersistentIndexRepository<MemoryStorage, ProjectionTestCodec>;

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("ethereum".to_owned()),
        network: "test".to_owned(),
    }
}

fn policy() -> ConfirmationPolicy {
    policy_at_depth(12)
}

fn policy_at_depth(minimum_confirmations: u64) -> ConfirmationPolicy {
    ConfirmationPolicy {
        minimum_confirmations,
        require_chain_finality: false,
    }
}

fn config(retention: u64) -> PersistentIndexConfig {
    PersistentIndexConfig::new(scope(), BlockHeight(1), policy(), retention)
        .expect("test repository configuration is valid")
}

fn make_repository(storage: MemoryStorage, retention: u64) -> Repository {
    PersistentIndexRepository::new(storage, RawBytesIndexCodec, config(retention))
}

fn make_projection_repository(storage: MemoryStorage, retention: u64) -> ProjectionRepository {
    PersistentIndexRepository::new(storage, ProjectionTestCodec, config(retention))
}

fn make_repository_with_policy(
    storage: MemoryStorage,
    minimum_confirmations: u64,
    retention: u64,
) -> Repository {
    let config = PersistentIndexConfig::new(
        scope(),
        BlockHeight(1),
        policy_at_depth(minimum_confirmations),
        retention,
    )
    .expect("test repository configuration is valid");
    PersistentIndexRepository::new(storage, RawBytesIndexCodec, config)
}

fn hash(height: u64) -> BlockHash {
    BlockHash(height.to_be_bytes().to_vec())
}

fn block_ref(height: u64) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: hash(height),
        parent_hash: Some(hash(height.saturating_sub(1))),
        timestamp: Some(1_000 + height),
    }
}

fn replacement_block_ref(height: u64, parent: &BlockRef, hash_byte: u8) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(vec![hash_byte; 32]),
        parent_hash: Some(parent.hash.clone()),
        timestamp: Some(2_000 + height),
    }
}

fn transaction_id(value: &str) -> CanonicalTransactionId {
    CanonicalTransactionId {
        chain: scope().chain,
        value: value.to_owned(),
    }
}

fn address(value: &str) -> CanonicalAddress {
    CanonicalAddress {
        chain: scope().chain,
        value: value.to_owned(),
    }
}

fn register_watch(repository: &Repository, idempotency_key: &str) -> WatchId {
    let registered_at =
        block_on(repository.checkpoint(&scope())).expect("checkpoint query succeeds");
    let outcome = block_on(repository.register_watch(RegisterWatchCommand {
        request: WatchRequest {
            scope: scope(),
            selector: WatchSelector::Address(address("0xabc")),
            start_height: BlockHeight(1),
            idempotency_key: idempotency_key.to_owned(),
        },
        target: vec![1, 0, 1],
        registered_at,
    }))
    .expect("watch registration succeeds");
    match outcome {
        RegisterWatchOutcome::Registered(receipt) | RegisterWatchOutcome::Existing(receipt) => {
            receipt.id
        }
    }
}

fn draft(transaction: &str, watch_id: &WatchId) -> ObservationDraft {
    ObservationDraft {
        scope: scope(),
        transaction_id: transaction_id(transaction),
        status: ObservationDraftStatus::Included,
        movements: vec![ValueMovement {
            id: MovementId(format!("movement-{transaction}")),
            asset: AssetId {
                chain: scope().chain,
                asset: "native".to_owned(),
            },
            amount: AtomicAmount([1; 32]),
            from: Some(address("0xsender")),
            to: Some(address("0xabc")),
            kind: MovementKind::Transfer,
        }],
        fee: None,
        watch_ids: vec![watch_id.clone()],
        first_seen_at: 1_001,
        observed_at: 1_001,
    }
}

fn command(
    height: u64,
    expected_checkpoint: Option<BlockRef>,
    watch_version: u64,
    drafts: Vec<ObservationDraft>,
    retention: u64,
) -> CommitBlockCommand<Vec<u8>> {
    CommitBlockCommand {
        scope: scope(),
        expected_checkpoint,
        expected_watch_version: WatchVersion(watch_version),
        confirmation_policy: policy(),
        reorg_retention: retention,
        block: InterpretedBlock {
            block: block_ref(height),
            drafts,
            projection: ProjectionBatch::default(),
            undo: vec![1, height as u8],
            raw: RawBlockData {
                block: vec![2, height as u8],
                receipts: vec![vec![3, height as u8]],
            },
        },
    }
}

fn command_with_projection(
    height: u64,
    expected_checkpoint: Option<BlockRef>,
    projection: ProjectionBatch,
    retention: u64,
) -> CommitBlockCommand<Vec<u8>> {
    let mut command = command(height, expected_checkpoint, 0, Vec::new(), retention);
    command.block.projection = projection;
    command
}

fn policy_migration(idempotency_key: &str, reason: &str) -> MigrateIndexPolicyCommand {
    MigrateIndexPolicyCommand {
        scope: scope(),
        bootstrap_height: BlockHeight(1),
        expected_confirmation_policy: policy_at_depth(12),
        expected_reorg_retention: 50,
        target_confirmation_policy: policy_at_depth(6),
        target_reorg_retention: 20,
        idempotency_key: idempotency_key.to_owned(),
        reason: reason.to_owned(),
    }
}

fn events(repository: &Repository) -> Vec<crate::ObservationEvent> {
    block_on(repository.events(ObservationEventRequest {
        scope: scope(),
        after: None,
        limit: 1_000,
    }))
    .expect("event query succeeds")
    .events
}

fn stored_record_count(storage: &MemoryStorage, prefix: Vec<u8>) -> usize {
    let mut after = None;
    let mut count = 0;
    loop {
        let page = block_on(storage.scan(ScanRequest {
            namespace: keys::namespace(),
            prefix: prefix.clone(),
            after,
            limit: 128,
        }))
        .expect("test storage scan succeeds");
        count += page.entries.len();
        match page.next {
            Some(next) => after = Some(next),
            None => return count,
        }
    }
}

fn stored_record<T: Decode<()>>(storage: &MemoryStorage, key: &Key) -> Option<T> {
    block_on(storage.get(&keys::namespace(), key))
        .expect("test storage read succeeds")
        .map(|stored| {
            let (record, consumed) =
                bincode::decode_from_slice::<T, _>(&stored.value.0, bincode_config::standard())
                    .expect("test record decodes");
            assert_eq!(consumed, stored.value.0.len());
            record
        })
}

#[test]
fn event_high_water_is_read_only_and_does_not_allocate_a_cursor() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    register_watch(&repository, "feed-head-read");
    let version_before = storage
        .state
        .lock()
        .expect("memory storage lock is healthy")
        .version;

    assert_eq!(
        block_on(repository.event_high_water(&scope())).expect("feed head query succeeds"),
        None
    );
    assert_eq!(
        storage
            .state
            .lock()
            .expect("memory storage lock is healthy")
            .version,
        version_before,
        "feed head query must not mutate storage or allocate cursor zero"
    );
}

#[test]
fn watches_are_persistent_idempotent_conflict_safe_and_soft_deleted() {
    let storage = MemoryStorage::default();
    let first = make_repository(storage.clone(), 50);
    let watch_id = register_watch(&first, "deposit-1");
    let reopened = make_repository(storage, 50);

    let existing = block_on(reopened.register_watch(RegisterWatchCommand {
        request: WatchRequest {
            scope: scope(),
            selector: WatchSelector::Address(address("0xabc")),
            start_height: BlockHeight(1),
            idempotency_key: "deposit-1".to_owned(),
        },
        target: vec![1, 0, 1],
        registered_at: None,
    }))
    .expect("identical retry succeeds");
    assert!(matches!(existing, RegisterWatchOutcome::Existing(_)));

    let conflict = block_on(reopened.register_watch(RegisterWatchCommand {
        request: WatchRequest {
            scope: scope(),
            selector: WatchSelector::Address(address("0xabc")),
            start_height: BlockHeight(1),
            idempotency_key: "deposit-1".to_owned(),
        },
        target: vec![9],
        registered_at: None,
    }))
    .expect_err("changed target conflicts");
    assert_eq!(conflict.kind, IndexErrorKind::Conflict);

    assert_eq!(
        block_on(reopened.unwatch(UnwatchCommand {
            scope: scope(),
            watch_id,
            inactive_from: BlockHeight(5),
            expected_checkpoint: None,
        }))
        .expect("soft unwatch succeeds"),
        UnwatchOutcome::Deactivated
    );
    let historical = block_on(reopened.watches_at(&scope(), BlockHeight(4)))
        .expect("historical watch query succeeds");
    let inactive = block_on(reopened.watches_at(&scope(), BlockHeight(5)))
        .expect("inactive watch query succeeds");
    assert_eq!(historical.version, WatchVersion(2));
    assert_eq!(historical.watches.len(), 1);
    assert!(inactive.watches.is_empty());
}

#[test]
fn path_global_metadata_rejects_a_different_scope_on_reopen() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    register_watch(&repository, "scope-owner");

    let other_scope = IndexScope {
        chain: ChainId("ethereum".to_owned()),
        network: "another-network".to_owned(),
    };
    let other_config =
        PersistentIndexConfig::new(other_scope.clone(), BlockHeight(1), policy(), 50)
            .expect("alternate test configuration is structurally valid");
    let other = PersistentIndexRepository::new(storage, RawBytesIndexCodec, other_config);
    let error = block_on(other.checkpoint(&other_scope))
        .expect_err("one database path cannot silently host another scope");
    assert_eq!(error.kind, IndexErrorKind::PolicyMismatch);
}

#[test]
fn historical_watch_registration_creates_durable_hash_bounded_backfill_work() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    for height in 1..=3 {
        block_on(repository.commit_block(command(
            height,
            (height > 1).then(|| block_ref(height - 1)),
            0,
            Vec::new(),
            50,
        )))
        .expect("canonical history commits");
    }
    let watch_id = register_watch(&repository, "historical-deposit");
    let reopened = make_repository(storage.clone(), 50);
    let backfills = block_on(reopened.pending_watch_backfills(&scope(), 10))
        .expect("pending backfill query succeeds");
    assert_eq!(backfills.len(), 1);
    assert_eq!(backfills[0].watch_id, watch_id);
    assert_eq!(backfills[0].from_height, BlockHeight(1));
    assert_eq!(backfills[0].next_height, BlockHeight(1));
    assert_eq!(backfills[0].through, block_ref(3));

    assert_eq!(
        block_on(reopened.commit_watch_backfill(CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: watch_id.clone(),
            expected_next_height: BlockHeight(1),
            expected_checkpoint: block_ref(3),
            block: block_ref(1),
            drafts: vec![draft("0xhistorical", &watch_id)],
        }))
        .expect("historical fact commits"),
        CommitWatchBackfillOutcome::Applied {
            next_height: Some(BlockHeight(2))
        }
    );
    let historical = block_on(reopened.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xhistorical"),
    }))
    .expect("historical transaction query succeeds")
    .expect("historical transaction is indexed");
    assert!(matches!(
        historical.status,
        TransactionStatus::Included {
            confirmations: 3,
            ..
        }
    ));
    assert_eq!(
        block_on(reopened.checkpoint(&scope())).expect("checkpoint query succeeds"),
        Some(block_ref(3)),
        "historical application does not move the live checkpoint"
    );

    storage.lose_next_acknowledgement();
    let lost = block_on(reopened.commit_watch_backfill(CommitWatchBackfillCommand {
        scope: scope(),
        watch_id: watch_id.clone(),
        expected_next_height: BlockHeight(2),
        expected_checkpoint: block_ref(3),
        block: block_ref(2),
        drafts: Vec::new(),
    }))
    .expect_err("empty-height acknowledgement is intentionally lost");
    assert!(lost.retryable);
    assert_eq!(
        block_on(reopened.commit_watch_backfill(CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: watch_id.clone(),
            expected_next_height: BlockHeight(2),
            expected_checkpoint: block_ref(3),
            block: block_ref(2),
            drafts: Vec::new(),
        }))
        .expect("empty-height retry is idempotent"),
        CommitWatchBackfillOutcome::AlreadyApplied {
            next_height: Some(BlockHeight(3))
        }
    );

    let restarted = make_repository(storage, 50);
    assert_eq!(
        block_on(restarted.commit_watch_backfill(CommitWatchBackfillCommand {
            scope: scope(),
            watch_id,
            expected_next_height: BlockHeight(3),
            expected_checkpoint: block_ref(3),
            block: block_ref(3),
            drafts: Vec::new(),
        }))
        .expect("final historical height commits after restart"),
        CommitWatchBackfillOutcome::Applied { next_height: None }
    );
    assert!(
        block_on(restarted.pending_watch_backfills(&scope(), 10))
            .expect("backfill query succeeds")
            .is_empty()
    );
    assert_eq!(events(&restarted).len(), 1);

    for height in (1..=3).rev() {
        block_on(restarted.revert_tip(RevertTipCommand {
            scope: scope(),
            expected_tip: block_ref(height),
        }))
        .expect("retained historical chain tip reverts");
    }
    let corrected = block_on(restarted.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xhistorical"),
    }))
    .expect("corrected historical transaction query succeeds")
    .expect("historical transaction remains in the immutable projection");
    assert!(matches!(
        corrected.status,
        TransactionStatus::Reorged { .. }
    ));
    let corrected_events = events(&restarted);
    assert_eq!(corrected_events.len(), 4);
    assert!(matches!(
        corrected_events[1].transaction.status,
        TransactionStatus::Included {
            confirmations: 2,
            ..
        }
    ));
    assert!(matches!(
        corrected_events[2].transaction.status,
        TransactionStatus::Included {
            confirmations: 1,
            ..
        }
    ));
    assert!(matches!(
        corrected_events[3].transaction.status,
        TransactionStatus::Reorged { .. }
    ));
}

#[test]
fn historical_backfill_finishes_after_live_tip_moves_beyond_retention() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    for height in 1..=3 {
        block_on(repository.commit_block(command(
            height,
            (height > 1).then(|| block_ref(height - 1)),
            0,
            Vec::new(),
            50,
        )))
        .expect("registration-era canonical history commits");
    }
    let watch_id = register_watch(&repository, "slow-historical-deposit");
    for height in 4..=60 {
        block_on(repository.commit_block(command(
            height,
            Some(block_ref(height - 1)),
            1,
            Vec::new(),
            50,
        )))
        .expect("live canonical progress commits while backfill is pending");
    }
    assert_eq!(
        block_on(repository.canonical_block(&scope(), BlockHeight(3)))
            .expect("canonical query succeeds"),
        None,
        "the frozen registration checkpoint is deliberately outside retention"
    );

    for height in 1..=3 {
        let outcome = block_on(
            repository.commit_watch_backfill(CommitWatchBackfillCommand {
                scope: scope(),
                watch_id: watch_id.clone(),
                expected_next_height: BlockHeight(height),
                expected_checkpoint: block_ref(60),
                block: block_ref(height),
                drafts: (height == 1)
                    .then(|| draft("0xslow-history", &watch_id))
                    .into_iter()
                    .collect(),
            }),
        )
        .expect("durable hash anchor permits progress beyond live retention");
        let expected = (height < 3).then(|| BlockHeight(height + 1));
        assert_eq!(
            outcome,
            CommitWatchBackfillOutcome::Applied {
                next_height: expected
            }
        );
    }

    assert!(
        block_on(repository.pending_watch_backfills(&scope(), 10))
            .expect("backfill query succeeds")
            .is_empty()
    );
    let observation = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xslow-history"),
    }))
    .expect("historical transaction query succeeds")
    .expect("historical transaction is indexed");
    assert!(matches!(
        observation.status,
        TransactionStatus::Confirmed {
            proof: ConfirmationProof::Depth {
                required: 12,
                observed: 12
            },
            ..
        }
    ));
    assert_eq!(events(&repository).len(), 1);
}

#[test]
fn through_checkpoint_reorg_rewrites_pending_jobs_and_reverts_orphan_facts() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage, 50);
    for height in 1..=3 {
        block_on(repository.commit_block(command(
            height,
            (height > 1).then(|| block_ref(height - 1)),
            0,
            Vec::new(),
            50,
        )))
        .expect("registration-era canonical history commits");
    }
    let completed_watch = register_watch(&repository, "completed-before-reorg");
    let pending_watch = register_watch(&repository, "pending-during-reorg");

    for height in 1..=3 {
        block_on(
            repository.commit_watch_backfill(CommitWatchBackfillCommand {
                scope: scope(),
                watch_id: completed_watch.clone(),
                expected_next_height: BlockHeight(height),
                expected_checkpoint: block_ref(3),
                block: block_ref(height),
                drafts: (height == 3)
                    .then(|| draft("0xorphan-history", &completed_watch))
                    .into_iter()
                    .collect(),
            }),
        )
        .expect("first watch reaches the original through checkpoint");
    }
    block_on(
        repository.commit_watch_backfill(CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: pending_watch.clone(),
            expected_next_height: BlockHeight(1),
            expected_checkpoint: block_ref(3),
            block: block_ref(1),
            drafts: Vec::new(),
        }),
    )
    .expect("second watch remains pending below the through checkpoint");

    assert_eq!(
        block_on(repository.revert_tip(RevertTipCommand {
            scope: scope(),
            expected_tip: block_ref(3),
        }))
        .expect("through checkpoint reverts atomically"),
        RevertTipOutcome::Reverted {
            checkpoint: Some(block_ref(2))
        }
    );
    let pending = block_on(repository.pending_watch_backfills(&scope(), 10))
        .expect("pending job query succeeds");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].watch_id, pending_watch);
    assert_eq!(pending[0].next_height, BlockHeight(2));
    assert_eq!(pending[0].through, block_ref(2));

    let orphaned = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xorphan-history"),
    }))
    .expect("orphan transaction query succeeds")
    .expect("orphan fact remains append-only");
    assert_eq!(orphaned.revision, crate::ObservationRevision(2));
    assert!(matches!(orphaned.status, TransactionStatus::Reorged { .. }));

    assert_eq!(
        block_on(
            repository.commit_watch_backfill(CommitWatchBackfillCommand {
                scope: scope(),
                watch_id: pending[0].watch_id.clone(),
                expected_next_height: BlockHeight(2),
                expected_checkpoint: block_ref(2),
                block: block_ref(2),
                drafts: Vec::new(),
            },)
        )
        .expect("rewritten pending job completes at the common ancestor"),
        CommitWatchBackfillOutcome::Applied { next_height: None }
    );

    let replacement = replacement_block_ref(3, &block_ref(2), 0xa3);
    let mut replacement_command = command(
        3,
        Some(block_ref(2)),
        2,
        vec![draft("0xorphan-history", &completed_watch)],
        50,
    );
    replacement_command.block.block = replacement.clone();
    assert_eq!(
        block_on(repository.commit_block(replacement_command))
            .expect("replacement through height commits through live sync"),
        CommitBlockOutcome::Applied
    );
    let reincluded = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xorphan-history"),
    }))
    .expect("re-included transaction query succeeds")
    .expect("re-included transaction exists");
    assert_eq!(reincluded.revision, crate::ObservationRevision(3));
    assert!(matches!(
        reincluded.status,
        TransactionStatus::Included { block, confirmations: 1 }
            if block == replacement
    ));

    let persisted_events = events(&repository);
    assert_eq!(persisted_events.len(), 3);
    assert_eq!(
        persisted_events
            .iter()
            .map(|event| event.cursor)
            .collect::<Vec<_>>(),
        vec![EventCursor(1), EventCursor(2), EventCursor(3)]
    );
    assert_eq!(
        block_on(repository.commit_block(command(
            3,
            Some(block_ref(2)),
            2,
            vec![draft("0xorphan-history", &completed_watch)],
            50,
        )))
        .expect_err("the original block cannot be retried over its replacement")
        .kind,
        IndexErrorKind::CannotConnect
    );
    assert_eq!(events(&repository).len(), 3);
}

#[test]
fn surviving_backfill_inclusion_loses_confirmation_depth_once_on_tip_reorg() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage, 50);
    for height in 1..=12 {
        block_on(repository.commit_block(command(
            height,
            (height > 1).then(|| block_ref(height - 1)),
            0,
            Vec::new(),
            50,
        )))
        .expect("canonical confirmation history commits");
    }
    let watch_id = register_watch(&repository, "confirmation-reorg");
    block_on(
        repository.commit_watch_backfill(CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: watch_id.clone(),
            expected_next_height: BlockHeight(1),
            expected_checkpoint: block_ref(12),
            block: block_ref(1),
            drafts: vec![draft("0xconfirmation-reorg", &watch_id)],
        }),
    )
    .expect("historical inclusion commits at the confirmation threshold");

    let confirmed = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xconfirmation-reorg"),
    }))
    .expect("confirmed transaction query succeeds")
    .expect("confirmed transaction exists");
    assert!(matches!(
        confirmed.status,
        TransactionStatus::Confirmed {
            proof: ConfirmationProof::Depth {
                required: 12,
                observed: 12
            },
            ..
        }
    ));
    assert_eq!(events(&repository).len(), 1);

    assert_eq!(
        block_on(repository.revert_tip(RevertTipCommand {
            scope: scope(),
            expected_tip: block_ref(12),
        }))
        .expect("shallow tip reorg succeeds"),
        RevertTipOutcome::Reverted {
            checkpoint: Some(block_ref(11))
        }
    );
    let corrected = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xconfirmation-reorg"),
    }))
    .expect("corrected transaction query succeeds")
    .expect("corrected transaction exists");
    assert_eq!(corrected.revision, crate::ObservationRevision(2));
    assert!(matches!(
        corrected.status,
        TransactionStatus::Included {
            block,
            confirmations: 11
        } if block == block_ref(1)
    ));
    let corrected_events = events(&repository);
    assert_eq!(corrected_events.len(), 2);
    assert_eq!(corrected_events[1].cursor, EventCursor(2));
    assert!(matches!(
        corrected_events[1].previous_status,
        Some(TransactionStatus::Confirmed { .. })
    ));

    assert_eq!(
        block_on(repository.revert_tip(RevertTipCommand {
            scope: scope(),
            expected_tip: block_ref(12),
        }))
        .expect("duplicate revert acknowledgement is idempotent"),
        RevertTipOutcome::AlreadyReverted {
            checkpoint: Some(block_ref(11))
        }
    );
    assert_eq!(events(&repository).len(), 2);

    let replacement = replacement_block_ref(12, &block_ref(11), 0xac);
    let mut replacement_command = command(12, Some(block_ref(11)), 1, Vec::new(), 50);
    replacement_command.block.block = replacement.clone();
    block_on(repository.commit_block(replacement_command))
        .expect("replacement tip restores the confirmation proof");
    let reconfirmed = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xconfirmation-reorg"),
    }))
    .expect("reconfirmed transaction query succeeds")
    .expect("reconfirmed transaction exists");
    assert_eq!(reconfirmed.revision, crate::ObservationRevision(3));
    assert!(matches!(
        reconfirmed.status,
        TransactionStatus::Confirmed {
            proof: ConfirmationProof::Depth {
                required: 12,
                observed: 12
            },
            ..
        }
    ));
    assert_eq!(events(&repository).len(), 3);
}

#[test]
fn unwatch_retries_when_checkpoint_advances_after_the_caller_snapshot() {
    let repository = make_repository(MemoryStorage::default(), 50);
    let watch_id = register_watch(&repository, "unwatch-checkpoint-race");
    block_on(repository.commit_block(command(1, None, 1, Vec::new(), 50)))
        .expect("block commits after the caller observed no checkpoint");

    let error = block_on(repository.unwatch(UnwatchCommand {
        scope: scope(),
        watch_id: watch_id.clone(),
        inactive_from: BlockHeight(1),
        expected_checkpoint: None,
    }))
    .expect_err("stale checkpoint must not backdate watch deactivation");
    assert_eq!(error.kind, IndexErrorKind::Conflict);
    assert!(error.retryable);
    assert_eq!(
        block_on(repository.watches_at(&scope(), BlockHeight(1)))
            .expect("watch remains active at the committed block")
            .watches
            .len(),
        1
    );

    assert_eq!(
        block_on(repository.unwatch(UnwatchCommand {
            scope: scope(),
            watch_id,
            inactive_from: BlockHeight(2),
            expected_checkpoint: Some(block_ref(1)),
        }))
        .expect("retry against the current checkpoint succeeds"),
        UnwatchOutcome::Deactivated
    );
}

#[test]
fn empty_blocks_append_each_depth_and_confirm_at_twelve_after_reopen() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    let watch_id = register_watch(&repository, "deposit-confirmation");
    let first = command(1, None, 1, vec![draft("0xtx", &watch_id)], 50);
    assert_eq!(
        block_on(repository.commit_block(first)).expect("first block commits"),
        CommitBlockOutcome::Applied
    );
    for height in 2..=12 {
        block_on(repository.commit_block(command(
            height,
            Some(block_ref(height - 1)),
            1,
            Vec::new(),
            50,
        )))
        .expect("empty confirmation block commits");
    }

    let reopened = make_repository(storage, 50);
    let persisted_events = events(&reopened);
    assert_eq!(persisted_events.len(), 12);
    for (index, event) in persisted_events.iter().take(11).enumerate() {
        assert_eq!(event.cursor, EventCursor(index as u64 + 1));
        assert!(matches!(
            event.transaction.status,
            TransactionStatus::Included { confirmations, .. }
                if confirmations == index as u64 + 1
        ));
    }
    assert!(matches!(
        persisted_events
            .last()
            .map(|event| &event.transaction.status),
        Some(TransactionStatus::Confirmed {
            proof: ConfirmationProof::Depth {
                required: 12,
                observed: 12
            },
            ..
        })
    ));
    let transaction = block_on(reopened.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xtx"),
    }))
    .expect("transaction query succeeds")
    .expect("transaction remains present");
    assert_eq!(transaction.revision.0, 12);
    let page = block_on(reopened.transactions_by_address(TransactionPageRequest {
        scope: scope(),
        address: address("0xabc"),
        after: None,
        limit: 10,
    }))
    .expect("address query succeeds");
    assert_eq!(page.transactions.len(), 1);
    assert_eq!(
        block_on(reopened.watches_for_address(AddressWatchRequest {
            scope: scope(),
            address: address("0xabc"),
        }))
        .expect("address watch query succeeds")
        .len(),
        1
    );
}

#[test]
fn response_loss_retry_is_already_applied_without_duplicate_identity() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    let watch_id = register_watch(&repository, "deposit-response-loss");
    let block = command(1, None, 1, vec![draft("0xlost", &watch_id)], 50);
    storage.lose_next_acknowledgement();

    let error = block_on(repository.commit_block(block.clone()))
        .expect_err("commit acknowledgement is intentionally lost");
    assert!(error.retryable);
    let reopened = make_repository(storage, 50);
    assert_eq!(
        block_on(reopened.commit_block(block)).expect("retry detects durable block"),
        CommitBlockOutcome::AlreadyApplied
    );
    let persisted_events = events(&reopened);
    assert_eq!(persisted_events.len(), 1);
    assert_eq!(persisted_events[0].cursor, EventCursor(1));
    assert_eq!(persisted_events[0].transaction.revision.0, 1);
}

#[test]
fn revert_response_loss_retry_is_already_reverted_without_duplicate_correction() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    let watch_id = register_watch(&repository, "revert-response-loss");
    block_on(repository.commit_block(command(
        1,
        None,
        1,
        vec![draft("0xrevert-lost", &watch_id)],
        50,
    )))
    .expect("inclusion commits");
    storage.lose_next_acknowledgement();
    let error = block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(1),
    }))
    .expect_err("revert acknowledgement is intentionally lost");
    assert!(error.retryable);

    assert!(matches!(
        block_on(repository.revert_tip(RevertTipCommand {
            scope: scope(),
            expected_tip: block_ref(1),
        }))
        .expect("revert retry detects the durable checkpoint move"),
        RevertTipOutcome::AlreadyReverted { checkpoint: None }
    ));
    assert_eq!(events(&repository).len(), 2);
}

#[test]
fn watch_starting_at_the_current_checkpoint_backfills_that_already_committed_block() {
    let repository = make_repository(MemoryStorage::default(), 50);
    block_on(repository.commit_block(command(1, None, 0, Vec::new(), 50)))
        .expect("checkpoint block commits before watch registration");
    let registered_at =
        block_on(repository.checkpoint(&scope())).expect("checkpoint query succeeds");
    let outcome = block_on(repository.register_watch(RegisterWatchCommand {
        request: WatchRequest {
            scope: scope(),
            selector: WatchSelector::Address(address("0xcheckpoint")),
            start_height: BlockHeight(1),
            idempotency_key: "checkpoint-birthday".to_owned(),
        },
        target: vec![1, 2, 3],
        registered_at,
    }))
    .expect("checkpoint-height watch registration succeeds");
    let watch_id = match outcome {
        RegisterWatchOutcome::Registered(receipt) | RegisterWatchOutcome::Existing(receipt) => {
            receipt.id
        }
    };

    let jobs = block_on(repository.pending_watch_backfills(&scope(), 10))
        .expect("checkpoint-height watch creates backfill work");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].watch_id, watch_id);
    assert_eq!(jobs[0].from_height, BlockHeight(1));
    assert_eq!(jobs[0].through, block_ref(1));
}

#[test]
fn unwatch_conflicts_until_its_historical_backfill_finishes() {
    let repository = make_repository(MemoryStorage::default(), 50);
    block_on(repository.commit_block(command(1, None, 0, Vec::new(), 50)))
        .expect("canonical checkpoint commits before watch registration");
    let watch_id = match block_on(repository.register_watch(RegisterWatchCommand {
        request: WatchRequest {
            scope: scope(),
            selector: WatchSelector::Address(address("0xpending-backfill")),
            start_height: BlockHeight(1),
            idempotency_key: "pending-unwatch".to_owned(),
        },
        target: vec![7, 7],
        registered_at: Some(block_ref(1)),
    }))
    .expect("historical watch registers")
    {
        RegisterWatchOutcome::Registered(receipt) | RegisterWatchOutcome::Existing(receipt) => {
            receipt.id
        }
    };

    let error = block_on(repository.unwatch(UnwatchCommand {
        scope: scope(),
        watch_id: watch_id.clone(),
        inactive_from: BlockHeight(2),
        expected_checkpoint: Some(block_ref(1)),
    }))
    .expect_err("pending historical projection must keep its watch active");
    assert_eq!(error.kind, IndexErrorKind::Conflict);
    assert!(error.retryable);
    assert_eq!(
        block_on(repository.watches_at(&scope(), BlockHeight(2)))
            .expect("watch state remains readable")
            .watches
            .len(),
        1
    );

    block_on(
        repository.commit_watch_backfill(CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: watch_id.clone(),
            expected_next_height: BlockHeight(1),
            expected_checkpoint: block_ref(1),
            block: block_ref(1),
            drafts: Vec::new(),
        }),
    )
    .expect("single-height backfill finishes");
    assert_eq!(
        block_on(repository.unwatch(UnwatchCommand {
            scope: scope(),
            watch_id,
            inactive_from: BlockHeight(2),
            expected_checkpoint: Some(block_ref(1)),
        }))
        .expect("unwatch succeeds after historical work completes"),
        UnwatchOutcome::Deactivated
    );
}

#[test]
fn failure_before_commit_leaves_no_partial_checkpoint_or_feed_rows() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    storage.fail_before_next_commit();
    let error = block_on(repository.commit_block(command(1, None, 0, Vec::new(), 50)))
        .expect_err("injected storage failure is returned");
    assert!(error.retryable);
    assert_eq!(
        block_on(repository.checkpoint(&scope())).expect("checkpoint query succeeds"),
        None
    );
    assert!(events(&repository).is_empty());
    assert_eq!(
        block_on(repository.commit_block(command(1, None, 0, Vec::new(), 50)))
            .expect("clean retry commits"),
        CommitBlockOutcome::Applied
    );
}

#[test]
fn projection_mutations_commit_scan_and_revert_with_the_canonical_block() {
    let storage = MemoryStorage::default();
    let repository = make_projection_repository(storage, 50);
    let first_projection = ProjectionBatch::new(vec![
        ProjectionMutation::Put {
            key: b"address/a/2".to_vec(),
            value: b"second".to_vec(),
        },
        ProjectionMutation::Put {
            key: b"utxo/1".to_vec(),
            value: b"created".to_vec(),
        },
        ProjectionMutation::Put {
            key: b"address/a/1".to_vec(),
            value: b"first".to_vec(),
        },
    ]);
    block_on(repository.commit_block(command_with_projection(1, None, first_projection, 50)))
        .expect("projection-bearing block commits");

    let value = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"utxo/1".to_vec(),
        expected_snapshot: None,
    }))
    .expect("projection point read succeeds");
    assert_eq!(value.snapshot.generation, crate::RebuildGeneration(0));
    assert_eq!(value.snapshot.revision, 1);
    assert_eq!(value.snapshot.checkpoint, Some(block_ref(1)));
    assert_eq!(value.value, Some(b"created".to_vec()));

    let first_page = block_on(repository.projection_scan(ProjectionScanRequest {
        scope: scope(),
        prefix: b"address/a/".to_vec(),
        after: None,
        limit: 1,
    }))
    .expect("first projection page succeeds");
    assert_eq!(first_page.snapshot, value.snapshot);
    assert_eq!(first_page.entries.len(), 1);
    assert_eq!(first_page.entries[0].key, b"address/a/1".to_vec());
    let fenced_value = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"address/a/1".to_vec(),
        expected_snapshot: Some(first_page.snapshot.clone()),
    }))
    .expect("dependent lookup accepts the scan snapshot");
    assert_eq!(fenced_value.value, Some(b"first".to_vec()));
    let cursor = first_page.next.expect("another ordered entry remains");
    let second_page = block_on(repository.projection_scan(ProjectionScanRequest {
        scope: scope(),
        prefix: b"address/a/".to_vec(),
        after: Some(cursor.clone()),
        limit: 1,
    }))
    .expect("second projection page succeeds");
    assert_eq!(second_page.entries.len(), 1);
    assert_eq!(second_page.entries[0].key, b"address/a/2".to_vec());
    assert!(second_page.next.is_none());

    block_on(repository.commit_block(command_with_projection(
        2,
        Some(block_ref(1)),
        ProjectionBatch::new(vec![ProjectionMutation::Delete {
            key: b"utxo/1".to_vec(),
        }]),
        50,
    )))
    .expect("spend projection commits");
    let stale_lookup = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"address/a/1".to_vec(),
        expected_snapshot: Some(fenced_value.snapshot),
    }))
    .expect_err("same-generation checkpoint movement invalidates a dependent lookup");
    assert_eq!(stale_lookup.kind, IndexErrorKind::Conflict);
    assert!(stale_lookup.retryable);
    let stale_cursor = block_on(repository.projection_scan(ProjectionScanRequest {
        scope: scope(),
        prefix: b"address/a/".to_vec(),
        after: Some(cursor),
        limit: 1,
    }))
    .expect_err("same-generation projection movement invalidates a scan cursor");
    assert_eq!(stale_cursor.kind, IndexErrorKind::Conflict);
    assert!(stale_cursor.retryable);
    let spent = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"utxo/1".to_vec(),
        expected_snapshot: None,
    }))
    .expect("spent projection read succeeds");
    assert_eq!(spent.snapshot.revision, 2);
    assert_eq!(spent.snapshot.checkpoint, Some(block_ref(2)));
    assert_eq!(spent.value, None);

    block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(2),
    }))
    .expect("spend block reverts");
    let restored = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"utxo/1".to_vec(),
        expected_snapshot: None,
    }))
    .expect("restored projection read succeeds");
    assert_eq!(restored.snapshot.revision, 3);
    assert_eq!(restored.snapshot.checkpoint, Some(block_ref(1)));
    assert_eq!(restored.value, Some(b"created".to_vec()));

    block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(1),
    }))
    .expect("creation block reverts");
    let removed = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"utxo/1".to_vec(),
        expected_snapshot: None,
    }))
    .expect("removed projection read succeeds");
    assert_eq!(removed.snapshot.revision, 4);
    assert_eq!(removed.snapshot.checkpoint, None);
    assert_eq!(removed.value, None);
    assert!(
        block_on(repository.projection_scan(ProjectionScanRequest {
            scope: scope(),
            prefix: b"address/".to_vec(),
            after: None,
            limit: 10,
        }))
        .expect("empty projection scan succeeds")
        .entries
        .is_empty()
    );
}

#[test]
fn conditional_projection_put_requires_a_fenced_creation_and_reverts_safely() {
    let absent_repository = make_projection_repository(MemoryStorage::default(), 50);
    block_on(absent_repository.commit_block(command_with_projection(
        1,
        None,
        ProjectionBatch::new(vec![ProjectionMutation::PutIfPresent {
            required_key: b"conditional/creation".to_vec(),
            key: b"conditional/marker".to_vec(),
            value: b"spent".to_vec(),
        }]),
        50,
    )))
    .expect("conditional block commits when its required fact is absent");
    assert_eq!(
        block_on(absent_repository.projection_get(ProjectionGetRequest {
            scope: scope(),
            key: b"conditional/marker".to_vec(),
            expected_snapshot: None,
        }))
        .expect("absent conditional marker reads")
        .value,
        None
    );

    let repository = make_projection_repository(MemoryStorage::default(), 50);
    block_on(repository.commit_block(command_with_projection(
        1,
        None,
        ProjectionBatch::new(vec![ProjectionMutation::Put {
            key: b"conditional/creation".to_vec(),
            value: b"output".to_vec(),
        }]),
        50,
    )))
    .expect("required creation commits first");
    let mut conditional = command_with_projection(
        2,
        Some(block_ref(1)),
        ProjectionBatch::new(vec![ProjectionMutation::PutIfPresent {
            required_key: b"conditional/creation".to_vec(),
            key: b"conditional/marker".to_vec(),
            value: b"spent".to_vec(),
        }]),
        50,
    );
    conditional.block.undo = vec![9, 1];
    block_on(repository.commit_block(conditional))
        .expect("conditional marker commits against an existing creation");
    assert_eq!(
        block_on(repository.projection_get(ProjectionGetRequest {
            scope: scope(),
            key: b"conditional/marker".to_vec(),
            expected_snapshot: None,
        }))
        .expect("materialized conditional marker reads")
        .value,
        Some(b"spent".to_vec())
    );

    block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(2),
    }))
    .expect("conditional marker block reverts");
    assert_eq!(
        block_on(repository.projection_get(ProjectionGetRequest {
            scope: scope(),
            key: b"conditional/marker".to_vec(),
            expected_snapshot: None,
        }))
        .expect("reverted conditional marker reads")
        .value,
        None
    );
    assert_eq!(
        block_on(repository.projection_get(ProjectionGetRequest {
            scope: scope(),
            key: b"conditional/creation".to_vec(),
            expected_snapshot: None,
        }))
        .expect("required creation survives marker revert")
        .value,
        Some(b"output".to_vec())
    );
}

#[test]
fn conditional_live_spend_then_backfill_reverts_one_shared_marker_safely() {
    let repository = make_projection_repository(MemoryStorage::default(), 50);
    block_on(repository.commit_block(command_with_projection(
        1,
        None,
        ProjectionBatch::default(),
        50,
    )))
    .expect("historical creation block commits before watch registration");
    let mut live_spend = command_with_projection(
        2,
        Some(block_ref(1)),
        ProjectionBatch::new(vec![ProjectionMutation::PutIfPresent {
            required_key: b"conditional/creation".to_vec(),
            key: b"conditional/marker".to_vec(),
            value: b"spent".to_vec(),
        }]),
        50,
    );
    live_spend.block.undo = vec![9, 1];
    block_on(repository.commit_block(live_spend))
        .expect("live conditional spend commits while its creation is absent");

    let watch_id = match block_on(repository.register_watch(RegisterWatchCommand {
        request: WatchRequest {
            scope: scope(),
            selector: WatchSelector::Address(address("0xconditional-backfill")),
            start_height: BlockHeight(1),
            idempotency_key: "conditional-backfill-overlap".to_owned(),
        },
        target: vec![9, 2],
        registered_at: Some(block_ref(2)),
    }))
    .expect("historical watch registers after the live spend")
    {
        RegisterWatchOutcome::Registered(receipt) | RegisterWatchOutcome::Existing(receipt) => {
            receipt.id
        }
    };
    block_on(repository.commit_watch_backfill_projection(
        CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: watch_id.clone(),
            expected_next_height: BlockHeight(1),
            expected_checkpoint: block_ref(2),
            block: block_ref(1),
            drafts: Vec::new(),
        },
        ProjectionBatch::new(vec![ProjectionMutation::Put {
            key: b"conditional/creation".to_vec(),
            value: b"output".to_vec(),
        }]),
    ))
    .expect("historical creation materializes");
    block_on(repository.commit_watch_backfill_projection(
        CommitWatchBackfillCommand {
            scope: scope(),
            watch_id,
            expected_next_height: BlockHeight(2),
            expected_checkpoint: block_ref(2),
            block: block_ref(2),
            drafts: Vec::new(),
        },
        ProjectionBatch::new(vec![ProjectionMutation::Put {
            key: b"conditional/marker".to_vec(),
            value: b"spent".to_vec(),
        }]),
    ))
    .expect("historical spend materializes the marker recorded by live undo");

    block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(2),
    }))
    .expect("shared chain/backfill delete must de-duplicate during reorg");
    assert_eq!(
        block_on(repository.projection_get(ProjectionGetRequest {
            scope: scope(),
            key: b"conditional/marker".to_vec(),
            expected_snapshot: None,
        }))
        .expect("marker reads after reorg")
        .value,
        None
    );
    assert_eq!(
        block_on(repository.projection_get(ProjectionGetRequest {
            scope: scope(),
            key: b"conditional/creation".to_vec(),
            expected_snapshot: None,
        }))
        .expect("creation reads after spend reorg")
        .value,
        Some(b"output".to_vec())
    );
}

#[test]
fn historical_projection_backfill_is_atomic_order_independent_and_reorg_safe() {
    let storage = MemoryStorage::default();
    let repository = make_projection_repository(storage, 50);
    for height in 1..=3 {
        block_on(repository.commit_block(command_with_projection(
            height,
            (height > 1).then(|| block_ref(height - 1)),
            ProjectionBatch::default(),
            50,
        )))
        .expect("canonical history commits before the watch is registered");
    }
    let watch_id = match block_on(repository.register_watch(RegisterWatchCommand {
        request: WatchRequest {
            scope: scope(),
            selector: WatchSelector::Address(address("0xbackfilled")),
            start_height: BlockHeight(1),
            idempotency_key: "projection-backfill".to_owned(),
        },
        target: vec![9, 9],
        registered_at: Some(block_ref(3)),
    }))
    .expect("historical projection watch registers")
    {
        RegisterWatchOutcome::Registered(receipt) | RegisterWatchOutcome::Existing(receipt) => {
            receipt.id
        }
    };

    block_on(repository.commit_watch_backfill_projection(
        CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: watch_id.clone(),
            expected_next_height: BlockHeight(1),
            expected_checkpoint: block_ref(3),
            block: block_ref(1),
            drafts: Vec::new(),
        },
        ProjectionBatch::new(vec![ProjectionMutation::Put {
            key: b"backfill/create".to_vec(),
            value: b"output".to_vec(),
        }]),
    ))
    .expect("historical creation commits with its cursor");

    let conditional = block_on(repository.commit_watch_backfill_projection(
        CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: watch_id.clone(),
            expected_next_height: BlockHeight(2),
            expected_checkpoint: block_ref(3),
            block: block_ref(2),
            drafts: Vec::new(),
        },
        ProjectionBatch::new(vec![ProjectionMutation::PutIfPresent {
            required_key: b"backfill/create".to_vec(),
            key: b"backfill/spent".to_vec(),
            value: b"marker".to_vec(),
        }]),
    ))
    .expect_err("historical backfill must not contain conditional mutations");
    assert_eq!(conditional.kind, IndexErrorKind::InvalidBlock);

    let destructive = block_on(repository.commit_watch_backfill_projection(
        CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: watch_id.clone(),
            expected_next_height: BlockHeight(2),
            expected_checkpoint: block_ref(3),
            block: block_ref(2),
            drafts: Vec::new(),
        },
        ProjectionBatch::new(vec![ProjectionMutation::Delete {
            key: b"backfill/create".to_vec(),
        }]),
    ))
    .expect_err("order-sensitive historical deletion must fail closed");
    assert_eq!(destructive.kind, IndexErrorKind::InvalidBlock);

    block_on(repository.commit_watch_backfill_projection(
        CommitWatchBackfillCommand {
            scope: scope(),
            watch_id: watch_id.clone(),
            expected_next_height: BlockHeight(2),
            expected_checkpoint: block_ref(3),
            block: block_ref(2),
            drafts: Vec::new(),
        },
        ProjectionBatch::new(vec![ProjectionMutation::Put {
            key: b"backfill/spent".to_vec(),
            value: b"marker".to_vec(),
        }]),
    ))
    .expect("historical spent marker commits after the rejected deletion");
    block_on(repository.commit_watch_backfill_projection(
        CommitWatchBackfillCommand {
            scope: scope(),
            watch_id,
            expected_next_height: BlockHeight(3),
            expected_checkpoint: block_ref(3),
            block: block_ref(3),
            drafts: Vec::new(),
        },
        ProjectionBatch::default(),
    ))
    .expect("historical backfill finishes");

    let completed_snapshot = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"backfill/create".to_vec(),
        expected_snapshot: None,
    }))
    .expect("completed historical projection reads")
    .snapshot;
    assert_eq!(completed_snapshot.revision, 6);
    assert_eq!(completed_snapshot.checkpoint, Some(block_ref(3)));

    for (key, expected) in [
        (b"backfill/create".as_slice(), b"output".as_slice()),
        (b"backfill/spent".as_slice(), b"marker".as_slice()),
    ] {
        assert_eq!(
            block_on(repository.projection_get(ProjectionGetRequest {
                scope: scope(),
                key: key.to_vec(),
                expected_snapshot: Some(completed_snapshot.clone()),
            }))
            .expect("backfilled projection reads")
            .value
            .as_deref(),
            Some(expected)
        );
    }

    block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(3),
    }))
    .expect("tip without historical projection reverts");
    block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(2),
    }))
    .expect("historical spend-marker block reverts");
    assert_eq!(
        block_on(repository.projection_get(ProjectionGetRequest {
            scope: scope(),
            key: b"backfill/spent".to_vec(),
            expected_snapshot: None,
        }))
        .expect("spent marker read after reorg")
        .value,
        None
    );
    let retained_creation = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"backfill/create".to_vec(),
        expected_snapshot: None,
    }))
    .expect("creation remains while its block is canonical");
    assert_eq!(retained_creation.snapshot.revision, 8);
    assert_eq!(retained_creation.snapshot.checkpoint, Some(block_ref(1)));
    assert_eq!(retained_creation.value, Some(b"output".to_vec()));
    block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(1),
    }))
    .expect("historical creation block reverts");
    let orphaned_creation = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"backfill/create".to_vec(),
        expected_snapshot: None,
    }))
    .expect("orphaned historical creation is removed");
    assert_eq!(orphaned_creation.snapshot.revision, 9);
    assert_eq!(orphaned_creation.snapshot.checkpoint, None);
    assert_eq!(orphaned_creation.value, None);
}

#[test]
fn projection_changes_are_atomic_and_duplicate_keys_fail_before_commit() {
    let storage = MemoryStorage::default();
    let repository = make_projection_repository(storage.clone(), 50);
    let projected = command_with_projection(
        1,
        None,
        ProjectionBatch::new(vec![ProjectionMutation::Put {
            key: b"utxo/atomic".to_vec(),
            value: b"value".to_vec(),
        }]),
        50,
    );
    storage.fail_before_next_commit();
    let error = block_on(repository.commit_block(projected))
        .expect_err("injected atomic commit failure is returned");
    assert!(error.retryable);
    assert_eq!(
        block_on(repository.checkpoint(&scope())).expect("checkpoint read succeeds"),
        None
    );
    assert_eq!(
        block_on(repository.projection_get(ProjectionGetRequest {
            scope: scope(),
            key: b"utxo/atomic".to_vec(),
            expected_snapshot: None,
        }))
        .expect("projection read succeeds")
        .value,
        None
    );

    let duplicate = command_with_projection(
        1,
        None,
        ProjectionBatch::new(vec![
            ProjectionMutation::Put {
                key: b"utxo/duplicate".to_vec(),
                value: b"first".to_vec(),
            },
            ProjectionMutation::Delete {
                key: b"utxo/duplicate".to_vec(),
            },
        ]),
        50,
    );
    let error = block_on(repository.commit_block(duplicate))
        .expect_err("duplicate projection keys are rejected");
    assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    assert_eq!(
        block_on(repository.checkpoint(&scope())).expect("checkpoint remains absent"),
        None
    );
}

#[test]
fn projection_reads_follow_atomic_generation_activation_and_cleanup() {
    let storage = MemoryStorage::default();
    let repository = make_projection_repository(storage.clone(), 50);
    block_on(repository.commit_block(command_with_projection(
        1,
        None,
        ProjectionBatch::new(vec![
            ProjectionMutation::Put {
                key: b"utxo/1".to_vec(),
                value: b"old-one".to_vec(),
            },
            ProjectionMutation::Put {
                key: b"utxo/2".to_vec(),
                value: b"old-two".to_vec(),
            },
        ]),
        50,
    )))
    .expect("active projection commits");
    let old_page = block_on(repository.projection_scan(ProjectionScanRequest {
        scope: scope(),
        prefix: b"utxo/".to_vec(),
        after: None,
        limit: 1,
    }))
    .expect("active projection page succeeds");
    assert_eq!(old_page.snapshot.revision, 1);
    let old_cursor = old_page.next.expect("active projection has another entry");

    let rebuild = block_on(repository.begin_rebuild(BeginRebuildCommand {
        scope: scope(),
        bootstrap_height: BlockHeight(1),
    }))
    .expect("rebuild begins");
    block_on(repository.commit_rebuild_block(CommitRebuildBlockCommand {
        generation: rebuild.generation,
        command: command_with_projection(
            1,
            None,
            ProjectionBatch::new(vec![
                ProjectionMutation::Put {
                    key: b"utxo/1".to_vec(),
                    value: b"new-one".to_vec(),
                },
                ProjectionMutation::Put {
                    key: b"utxo/2".to_vec(),
                    value: b"new-two".to_vec(),
                },
            ]),
            50,
        ),
    }))
    .expect("hidden projection commits");
    let active_before_activation = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"utxo/1".to_vec(),
        expected_snapshot: None,
    }))
    .expect("active projection remains readable");
    assert_eq!(
        active_before_activation.snapshot.generation,
        crate::RebuildGeneration(0)
    );
    assert_eq!(active_before_activation.snapshot.revision, 2);
    assert_eq!(
        active_before_activation.snapshot.checkpoint,
        Some(block_ref(1))
    );
    assert_eq!(active_before_activation.value, Some(b"old-one".to_vec()));

    block_on(repository.validate_rebuild(ValidateRebuildCommand {
        scope: scope(),
        generation: rebuild.generation,
        expected_checkpoint: block_ref(1),
    }))
    .expect("hidden projection validates");
    block_on(
        repository.prepare_rebuild_activation(PrepareRebuildActivationCommand {
            scope: scope(),
            generation: rebuild.generation,
            expected_checkpoint: block_ref(1),
        }),
    )
    .expect("hidden projection prepares");
    block_on(repository.activate_rebuild(ActivateRebuildCommand {
        scope: scope(),
        generation: rebuild.generation,
        expected_checkpoint: block_ref(1),
    }))
    .expect("hidden projection activates");

    let active = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"utxo/1".to_vec(),
        expected_snapshot: None,
    }))
    .expect("new active projection reads");
    assert_eq!(active.snapshot.generation, rebuild.generation);
    assert_eq!(active.snapshot.revision, 3);
    assert_eq!(active.snapshot.checkpoint, Some(block_ref(1)));
    assert_eq!(active.value, Some(b"new-one".to_vec()));
    let cursor_error = block_on(repository.projection_scan(ProjectionScanRequest {
        scope: scope(),
        prefix: b"utxo/".to_vec(),
        after: Some(old_cursor),
        limit: 1,
    }))
    .expect_err("old generation cursor cannot cross activation");
    assert_eq!(cursor_error.kind, IndexErrorKind::Conflict);
    assert!(cursor_error.retryable);

    assert!(matches!(
        block_on(repository.cleanup_generation(CleanupGenerationCommand {
            scope: scope(),
            generation: crate::RebuildGeneration(0),
        }))
        .expect("old generation cleanup succeeds"),
        CleanupGenerationOutcome::Removed { records } if records > 0
    ));
    assert_eq!(
        stored_record_count(
            &storage,
            keys::projection_prefix(&scope(), crate::RebuildGeneration(0), &[]),
        ),
        0
    );
    let after_cleanup = block_on(repository.projection_get(ProjectionGetRequest {
        scope: scope(),
        key: b"utxo/1".to_vec(),
        expected_snapshot: None,
    }))
    .expect("active projection remains readable after old-generation cleanup");
    assert_eq!(after_cleanup.snapshot.revision, 4);
    assert_eq!(
        stored_record_count(
            &storage,
            keys::projection_prefix(&scope(), rebuild.generation, &[]),
        ),
        2
    );

    let aborted = block_on(repository.begin_rebuild(BeginRebuildCommand {
        scope: scope(),
        bootstrap_height: BlockHeight(1),
    }))
    .expect("another rebuild begins");
    block_on(repository.commit_rebuild_block(CommitRebuildBlockCommand {
        generation: aborted.generation,
        command: command_with_projection(
            1,
            None,
            ProjectionBatch::new(vec![ProjectionMutation::Put {
                key: b"utxo/aborted".to_vec(),
                value: b"hidden".to_vec(),
            }]),
            50,
        ),
    }))
    .expect("aborted projection initially commits");
    assert_eq!(
        stored_record_count(
            &storage,
            keys::projection_prefix(&scope(), aborted.generation, &[]),
        ),
        1
    );
    block_on(repository.abort_rebuild(AbortRebuildCommand {
        scope: scope(),
        generation: aborted.generation,
    }))
    .expect("rebuild abort succeeds");
    assert_eq!(
        stored_record_count(
            &storage,
            keys::projection_prefix(&scope(), aborted.generation, &[]),
        ),
        0
    );
}

#[test]
fn one_atomic_block_allocates_unique_deterministic_feed_cursors() {
    let repository = make_repository(MemoryStorage::default(), 50);
    let watch_id = register_watch(&repository, "cursor-allocation");
    block_on(repository.commit_block(command(
        1,
        None,
        1,
        vec![draft("0xtx-b", &watch_id), draft("0xtx-a", &watch_id)],
        50,
    )))
    .expect("multi-observation block commits");
    let persisted = events(&repository);
    assert_eq!(persisted.len(), 2);
    assert_eq!(persisted[0].cursor, EventCursor(1));
    assert_eq!(
        persisted[0].transaction.transaction_id,
        transaction_id("0xtx-a")
    );
    assert_eq!(persisted[1].cursor, EventCursor(2));
    assert_eq!(
        persisted[1].transaction.transaction_id,
        transaction_id("0xtx-b")
    );
    assert_eq!(persisted[0].transaction.revision.0, 1);
    assert_eq!(persisted[1].transaction.revision.0, 1);
}

#[test]
fn revert_is_newest_first_and_appends_corrections() {
    let repository = make_repository(MemoryStorage::default(), 50);
    let watch_id = register_watch(&repository, "deposit-revert");
    block_on(repository.commit_block(command(1, None, 1, vec![draft("0xreorg", &watch_id)], 50)))
        .expect("inclusion commits");
    block_on(repository.commit_block(command(2, Some(block_ref(1)), 1, Vec::new(), 50)))
        .expect("confirmation commits");

    assert!(matches!(
        block_on(repository.revert_tip(RevertTipCommand {
            scope: scope(),
            expected_tip: block_ref(2),
        }))
        .expect("tip reverts"),
        RevertTipOutcome::Reverted {
            checkpoint: Some(BlockRef {
                height: BlockHeight(1),
                ..
            })
        }
    ));
    let after_depth_revert = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xreorg"),
    }))
    .expect("transaction query succeeds")
    .expect("transaction exists");
    assert!(matches!(
        after_depth_revert.status,
        TransactionStatus::Included {
            confirmations: 1,
            ..
        }
    ));
    block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(1),
    }))
    .expect("inclusion tip reverts");
    let reorged = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xreorg"),
    }))
    .expect("transaction query succeeds")
    .expect("reorged transaction remains queryable");
    assert!(matches!(reorged.status, TransactionStatus::Reorged { .. }));
    assert_eq!(events(&repository).len(), 4);
}

#[test]
fn retention_keeps_fifty_bundles_plus_hash_only_anchor() {
    let repository = make_repository(MemoryStorage::default(), 50);
    for height in 1..=55 {
        block_on(repository.commit_block(command(
            height,
            (height > 1).then(|| block_ref(height - 1)),
            0,
            Vec::new(),
            50,
        )))
        .expect("linear block commits");
    }
    assert_eq!(
        block_on(repository.canonical_block(&scope(), BlockHeight(4)))
            .expect("canonical query succeeds"),
        None
    );
    assert_eq!(
        block_on(repository.canonical_block(&scope(), BlockHeight(5)))
            .expect("anchor query succeeds")
            .map(|block| block.hash),
        Some(hash(5))
    );
    for height in (6..=55).rev() {
        block_on(repository.revert_tip(RevertTipCommand {
            scope: scope(),
            expected_tip: block_ref(height),
        }))
        .expect("retained tip reverts");
    }
    let beyond = block_on(repository.revert_tip(RevertTipCommand {
        scope: scope(),
        expected_tip: block_ref(5),
    }))
    .expect_err("anchor has no reversible bundle");
    assert_eq!(beyond.kind, IndexErrorKind::ReorgBeyondRetention);
}

#[test]
fn staged_generation_is_hidden_then_published_atomically_with_corrections() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    let watch_id = register_watch(&repository, "deposit-rebuild");
    block_on(repository.commit_block(command(1, None, 1, vec![draft("0xold", &watch_id)], 50)))
        .expect("active generation commits");
    block_on(repository.set_status(SyncStatus {
        scope: scope(),
        checkpoint: Some(block_ref(1)),
        observed_tip: Some(block_ref(1)),
        confirmation_policy: policy(),
        phase: SyncPhase::RebuildRequired,
        rebuild_reason: None,
        halted_reason: None,
    }))
    .expect("rebuild-required status persists");

    let rebuild = block_on(repository.begin_rebuild(BeginRebuildCommand {
        scope: scope(),
        bootstrap_height: BlockHeight(1),
    }))
    .expect("rebuild begins");
    block_on(repository.commit_rebuild_block(CommitRebuildBlockCommand {
        generation: rebuild.generation,
        command: command(1, None, 1, vec![draft("0xnew", &watch_id)], 50),
    }))
    .expect("shadow block commits");
    let blocked_query = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xnew"),
    }))
    .expect_err("semantic queries fail closed while rebuild is required");
    assert_eq!(blocked_query.kind, IndexErrorKind::RebuildRequired);
    let staged_cleanup = block_on(repository.cleanup_generation(CleanupGenerationCommand {
        scope: scope(),
        generation: rebuild.generation,
    }))
    .expect_err("current staged generation cleanup is rejected");
    assert_eq!(staged_cleanup.kind, IndexErrorKind::Conflict);

    let early_activation = block_on(repository.activate_rebuild(ActivateRebuildCommand {
        scope: scope(),
        generation: rebuild.generation,
        expected_checkpoint: block_ref(1),
    }))
    .expect_err("building generation cannot activate");
    assert_eq!(early_activation.kind, IndexErrorKind::Conflict);
    let early_prepare = block_on(repository.prepare_rebuild_activation(
        PrepareRebuildActivationCommand {
            scope: scope(),
            generation: rebuild.generation,
            expected_checkpoint: block_ref(1),
        },
    ))
    .expect_err("building generation cannot prepare correction events");
    assert_eq!(early_prepare.kind, IndexErrorKind::Conflict);
    let validating = block_on(repository.validate_rebuild(ValidateRebuildCommand {
        scope: scope(),
        generation: rebuild.generation,
        expected_checkpoint: block_ref(1),
    }))
    .expect("shadow checkpoint validates");
    assert_eq!(validating.phase, crate::RebuildPhase::Validating);
    let ready = block_on(
        repository.prepare_rebuild_activation(PrepareRebuildActivationCommand {
            scope: scope(),
            generation: rebuild.generation,
            expected_checkpoint: block_ref(1),
        }),
    )
    .expect("corrections are prepared in the hidden generation");
    assert_eq!(ready.phase, crate::RebuildPhase::ReadyToActivate);

    block_on(repository.activate_rebuild(ActivateRebuildCommand {
        scope: scope(),
        generation: rebuild.generation,
        expected_checkpoint: block_ref(1),
    }))
    .expect("shadow generation activates");
    let new = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xnew"),
    }))
    .expect("new transaction query succeeds");
    let old = block_on(repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xold"),
    }))
    .expect("old transaction query succeeds")
    .expect("removed old transaction has a correction");
    assert!(new.is_some());
    assert!(matches!(old.status, TransactionStatus::Reorged { .. }));
    let old_address_page = block_on(repository.transactions_by_address(TransactionPageRequest {
        scope: scope(),
        address: address("0xabc"),
        after: None,
        limit: 10,
    }))
    .expect("old-only reorg correction remains indexed by its movement address");
    let old_by_address = old_address_page
        .transactions
        .iter()
        .find(|transaction| transaction.transaction_id == transaction_id("0xold"))
        .expect("old-only correction is present in the address projection");
    assert!(matches!(
        old_by_address.status,
        TransactionStatus::Reorged { .. }
    ));
    assert_eq!(
        block_on(repository.status(&scope()))
            .expect("status query succeeds")
            .phase,
        SyncPhase::Ready
    );
    assert!(
        block_on(repository.rebuild_state(&scope()))
            .expect("manifest query succeeds")
            .is_none()
    );
    assert_eq!(events(&repository).len(), 3);

    let feed_before_cleanup = events(&repository);
    assert!(matches!(
        block_on(repository.cleanup_generation(CleanupGenerationCommand {
            scope: scope(),
            generation: crate::RebuildGeneration(0),
        }))
        .expect("inactive generation cleanup succeeds"),
        CleanupGenerationOutcome::Removed { records } if records > 0
    ));
    assert_eq!(events(&repository), feed_before_cleanup);
    assert!(
        block_on(storage.get(
            &keys::namespace(),
            &keys::observation_revision(
                &scope(),
                crate::RebuildGeneration(0),
                "ethereum",
                "0xold",
                crate::ObservationRevision(1),
            ),
        ))
        .expect("revision journal lookup succeeds")
        .is_some(),
        "generation cleanup preserves immutable revision journals"
    );
    let active_cleanup = block_on(repository.cleanup_generation(CleanupGenerationCommand {
        scope: scope(),
        generation: rebuild.generation,
    }))
    .expect_err("active generation cleanup is rejected");
    assert_eq!(active_cleanup.kind, IndexErrorKind::Conflict);
}

#[test]
fn rebuild_phases_and_hidden_events_resume_after_lost_acknowledgements() {
    let storage = MemoryStorage::default();
    let repository = make_repository(storage.clone(), 50);
    let watch_id = register_watch(&repository, "rebuild-resume");
    block_on(repository.commit_block(command(
        1,
        None,
        1,
        vec![draft("0xold-resume", &watch_id)],
        50,
    )))
    .expect("active generation commits");
    block_on(repository.set_status(SyncStatus {
        scope: scope(),
        checkpoint: Some(block_ref(1)),
        observed_tip: Some(block_ref(1)),
        confirmation_policy: policy(),
        phase: SyncPhase::RebuildRequired,
        rebuild_reason: None,
        halted_reason: None,
    }))
    .expect("rebuild-required status persists");

    let rebuild = block_on(repository.begin_rebuild(BeginRebuildCommand {
        scope: scope(),
        bootstrap_height: BlockHeight(1),
    }))
    .expect("rebuild begins");
    assert_eq!(rebuild.published_event_high_water, EventCursor(1));
    block_on(repository.commit_rebuild_block(CommitRebuildBlockCommand {
        generation: rebuild.generation,
        command: command(1, None, 1, vec![draft("0xnew-resume", &watch_id)], 50),
    }))
    .expect("shadow block commits");

    let validate = ValidateRebuildCommand {
        scope: scope(),
        generation: rebuild.generation,
        expected_checkpoint: block_ref(1),
    };
    storage.lose_next_acknowledgement();
    let lost_validation = block_on(repository.validate_rebuild(validate.clone()))
        .expect_err("validation acknowledgement is intentionally lost");
    assert!(lost_validation.retryable);

    let reopened = make_repository(storage.clone(), 50);
    let validating = block_on(reopened.rebuild_state(&scope()))
        .expect("manifest query succeeds")
        .expect("rebuild remains durable");
    assert_eq!(validating.phase, crate::RebuildPhase::Validating);
    assert_eq!(
        block_on(reopened.validate_rebuild(validate)).expect("validation retry is idempotent"),
        validating
    );
    let late_block = block_on(reopened.commit_rebuild_block(CommitRebuildBlockCommand {
        generation: rebuild.generation,
        command: command(2, Some(block_ref(1)), 1, Vec::new(), 50),
    }))
    .expect_err("validated generation no longer accepts blocks");
    assert_eq!(late_block.kind, IndexErrorKind::Conflict);

    let prepare = PrepareRebuildActivationCommand {
        scope: scope(),
        generation: rebuild.generation,
        expected_checkpoint: block_ref(1),
    };
    storage.lose_next_acknowledgement();
    let lost_prepare = block_on(reopened.prepare_rebuild_activation(prepare.clone()))
        .expect_err("preparation acknowledgement is intentionally lost");
    assert!(lost_prepare.retryable);

    let ready_repository = make_repository(storage.clone(), 50);
    let ready = block_on(ready_repository.rebuild_state(&scope()))
        .expect("manifest query succeeds")
        .expect("prepared rebuild remains durable");
    assert_eq!(ready.phase, crate::RebuildPhase::ReadyToActivate);
    assert_eq!(ready.published_event_high_water, EventCursor(1));
    assert_eq!(
        block_on(ready_repository.prepare_rebuild_activation(prepare))
            .expect("preparation retry is idempotent"),
        ready
    );
    assert_eq!(
        stored_record_count(&storage, keys::event_prefix(&scope())),
        1
    );
    assert_eq!(
        block_on(ready_repository.event_high_water(&scope()))
            .expect("published feed head remains queryable during rebuild"),
        Some(EventCursor(1)),
        "hidden prepared corrections must not advance the published feed head"
    );
    assert_eq!(
        stored_record_count(
            &storage,
            keys::prepared_rebuild_event_prefix(&scope(), rebuild.generation),
        ),
        2
    );

    let activate = ActivateRebuildCommand {
        scope: scope(),
        generation: rebuild.generation,
        expected_checkpoint: block_ref(1),
    };
    storage.lose_next_acknowledgement();
    let lost_activation = block_on(ready_repository.activate_rebuild(activate.clone()))
        .expect_err("activation acknowledgement is intentionally lost");
    assert!(lost_activation.retryable);

    let activated = make_repository(storage.clone(), 50);
    block_on(activated.activate_rebuild(activate)).expect("activation retry is idempotent");
    assert!(
        block_on(activated.rebuild_state(&scope()))
            .expect("manifest query succeeds")
            .is_none()
    );
    assert_eq!(events(&activated).len(), 3);
    assert_eq!(
        block_on(activated.event_high_water(&scope())).expect("activated feed head query succeeds"),
        Some(EventCursor(3))
    );
    assert_eq!(
        stored_record_count(
            &storage,
            keys::prepared_rebuild_event_prefix(&scope(), rebuild.generation),
        ),
        0
    );
}

#[test]
fn policy_migration_is_atomic_idempotent_audited_and_requires_rebuild() {
    let storage = MemoryStorage::default();
    let old_repository = make_repository(storage.clone(), 50);
    let watch_id = register_watch(&old_repository, "policy-migration");
    block_on(old_repository.commit_block(command(
        1,
        None,
        1,
        vec![draft("0xpolicy", &watch_id)],
        50,
    )))
    .expect("old-policy block commits");
    block_on(old_repository.set_status(SyncStatus {
        scope: scope(),
        checkpoint: Some(block_ref(1)),
        observed_tip: Some(block_ref(1)),
        confirmation_policy: policy_at_depth(12),
        phase: SyncPhase::Ready,
        rebuild_reason: None,
        halted_reason: None,
    }))
    .expect("ready status persists");

    let target_repository = make_repository_with_policy(storage.clone(), 6, 20);
    let mismatch = block_on(target_repository.status(&scope()))
        .expect_err("target policy cannot open before explicit migration");
    assert_eq!(mismatch.kind, IndexErrorKind::PolicyMismatch);

    let migration = policy_migration("policy-12-to-6", "reduce depth after operator review");
    storage.fail_before_next_commit();
    let failed = block_on(target_repository.migrate_policy(migration.clone()))
        .expect_err("injected failure prevents the atomic migration");
    assert!(failed.retryable);
    assert_eq!(
        block_on(old_repository.status(&scope()))
            .expect("old policy remains readable")
            .phase,
        SyncPhase::Ready
    );
    assert!(
        stored_record::<PolicyMigrationIdRecordV1>(
            &storage,
            &keys::policy_migration_id(&scope(), &migration.idempotency_key),
        )
        .is_none()
    );
    assert!(
        stored_record::<PolicyMigrationRecordV1>(&storage, &keys::policy_migration(&scope(), 1),)
            .is_none()
    );

    storage.lose_next_acknowledgement();
    let lost = block_on(target_repository.migrate_policy(migration.clone()))
        .expect_err("migration acknowledgement is intentionally lost");
    assert!(lost.retryable);
    assert_eq!(
        block_on(target_repository.migrate_policy(migration.clone()))
            .expect("migration retry resolves the immutable idempotency row"),
        MigrateIndexPolicyOutcome::AlreadyApplied {
            version: PolicyMigrationVersion(1),
        }
    );

    let audit =
        stored_record::<PolicyMigrationRecordV1>(&storage, &keys::policy_migration(&scope(), 1))
            .expect("immutable migration audit exists");
    assert_eq!(audit.version, 1);
    assert_eq!(audit.idempotency_key, migration.idempotency_key);
    assert_eq!(audit.from_confirmation_policy.minimum_confirmations, 12);
    assert_eq!(audit.from_reorg_retention, 50);
    assert_eq!(audit.to_confirmation_policy.minimum_confirmations, 6);
    assert_eq!(audit.to_reorg_retention, 20);
    assert_eq!(audit.reason, migration.reason);
    assert_eq!(
        stored_record::<PolicyMigrationIdRecordV1>(
            &storage,
            &keys::policy_migration_id(&scope(), "policy-12-to-6"),
        )
        .expect("idempotency row exists")
        .version,
        1
    );

    let mut conflicting_retry = migration.clone();
    conflicting_retry.reason = "different operator reason".to_owned();
    let conflict = block_on(target_repository.migrate_policy(conflicting_retry))
        .expect_err("changed payload cannot reuse the migration id");
    assert_eq!(conflict.kind, IndexErrorKind::Conflict);
    let old_mismatch = block_on(old_repository.status(&scope()))
        .expect_err("old runtime policy is rejected after migration");
    assert_eq!(old_mismatch.kind, IndexErrorKind::PolicyMismatch);

    let status = block_on(target_repository.status(&scope()))
        .expect("target runtime policy matches migrated metadata");
    assert_eq!(status.phase, SyncPhase::RebuildRequired);
    assert_eq!(status.confirmation_policy, policy_at_depth(6));
    assert_eq!(status.checkpoint, Some(block_ref(1)));
    let blocked = block_on(target_repository.transaction(TransactionRequest {
        scope: scope(),
        transaction_id: transaction_id("0xpolicy"),
    }))
    .expect_err("old projection stays unavailable until target-policy rebuild");
    assert_eq!(blocked.kind, IndexErrorKind::RebuildRequired);

    let staged = block_on(target_repository.begin_rebuild(BeginRebuildCommand {
        scope: scope(),
        bootstrap_height: BlockHeight(1),
    }))
    .expect("target-policy staged rebuild can begin");
    block_on(target_repository.abort_rebuild(crate::AbortRebuildCommand {
        scope: scope(),
        generation: staged.generation,
    }))
    .expect("unpublished target-policy rebuild aborts");
    assert_eq!(
        stored_record::<PolicyMigrationRecordV1>(&storage, &keys::policy_migration(&scope(), 1),)
            .expect("audit survives generation maintenance"),
        audit
    );
}

#[test]
fn policy_migration_rejects_an_active_rebuild_and_wrong_expected_policy() {
    let storage = MemoryStorage::default();
    let old_repository = make_repository(storage.clone(), 50);
    register_watch(&old_repository, "policy-rebuild-conflict");
    let rebuild = block_on(old_repository.begin_rebuild(BeginRebuildCommand {
        scope: scope(),
        bootstrap_height: BlockHeight(1),
    }))
    .expect("staged rebuild begins");
    let target_repository = make_repository_with_policy(storage.clone(), 6, 20);
    let command = policy_migration("blocked-by-rebuild", "policy review");
    let conflict = block_on(target_repository.migrate_policy(command.clone()))
        .expect_err("policy migration cannot race a staged rebuild");
    assert_eq!(conflict.kind, IndexErrorKind::Conflict);
    assert!(
        stored_record::<PolicyMigrationRecordV1>(&storage, &keys::policy_migration(&scope(), 1),)
            .is_none()
    );
    block_on(old_repository.abort_rebuild(crate::AbortRebuildCommand {
        scope: scope(),
        generation: rebuild.generation,
    }))
    .expect("staged rebuild aborts");

    let mut wrong_expected = command;
    wrong_expected.idempotency_key = "wrong-expected".to_owned();
    wrong_expected.expected_confirmation_policy = policy_at_depth(11);
    let mismatch = block_on(target_repository.migrate_policy(wrong_expected))
        .expect_err("migration must compare the exact persisted source policy");
    assert_eq!(mismatch.kind, IndexErrorKind::PolicyMismatch);
    assert!(
        stored_record::<PolicyMigrationRecordV1>(&storage, &keys::policy_migration(&scope(), 1),)
            .is_none()
    );
}
