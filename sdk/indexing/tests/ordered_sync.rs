use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use chain_identity::{CanonicalAddress, CanonicalTransactionId, ChainId};
use futures_executor::block_on;
use indexing::{
    AbortRebuildCommand, ActivateRebuildCommand, AddressWatchRequest, BeginRebuildCommand,
    BlockCommitObservation, BlockCommitObservationOutcome, BlockHash, BlockHeight,
    BlockInterpreter, BlockRef, BlockSource, BoxFuture, CleanupGenerationCommand,
    CleanupGenerationOutcome, CommitBlockCommand, CommitBlockOutcome, CommitRebuildBlockCommand,
    CommitWatchBackfillCommand, CommitWatchBackfillOutcome, ConfirmationPolicy, ConfirmationProof,
    EventCursor, IndexError, IndexErrorKind, IndexRepository, IndexedBlock, IndexingWorker,
    InterpretedBlock, MigrateIndexPolicyCommand, MigrateIndexPolicyOutcome, ObservationDraft,
    ObservationDraftStatus, ObservationEvent, ObservationEventId, ObservationEventPage,
    ObservationEventRequest, ObservationRevision, ObservedTransaction, OrderedSyncConfig,
    OrderedSyncWorker, PrepareRebuildActivationCommand, RawBlockData, RebuildGeneration,
    RebuildPhase, RebuildState, RegisterWatchCommand, RegisterWatchOutcome, ReorgDepth,
    ReorgObservation, RevertTipCommand, RevertTipOutcome, SourceError, SyncObserver, SyncPhase,
    SyncRequest, SyncStatus, TransactionPage, TransactionPageRequest, TransactionRequest,
    TransactionStatus, UnwatchCommand, UnwatchOutcome, ValidateRebuildCommand, WatchBackfill,
    WatchId, WatchReceipt, WatchSelector, WatchSnapshot, WatchTarget, WatchVersion,
};

fn scope() -> indexing::IndexScope {
    indexing::IndexScope {
        chain: ChainId("ethereum".to_owned()),
        network: "test".to_owned(),
    }
}

fn policy() -> ConfirmationPolicy {
    ConfirmationPolicy {
        minimum_confirmations: 12,
        require_chain_finality: false,
    }
}

fn hash(tag: &str, height: u64) -> BlockHash {
    BlockHash(format!("{tag}-{height}").into_bytes())
}

fn transaction_id(value: &str) -> CanonicalTransactionId {
    CanonicalTransactionId {
        chain: ChainId("ethereum".to_owned()),
        value: value.to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TestBlock {
    reference: BlockRef,
    drafts: Vec<ObservationDraft>,
}

impl IndexedBlock for TestBlock {
    fn block_ref(&self) -> BlockRef {
        self.reference.clone()
    }
}

#[derive(Clone)]
struct TestSource {
    blocks: Arc<Mutex<BTreeMap<u64, TestBlock>>>,
    next_failure: Arc<Mutex<Option<TestSourceFailure>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestSourceFailure {
    Tip,
    BlockAt(u64),
    CanonicalHash(u64),
}

impl TestSource {
    fn linear(tip: u64, transaction_at: Option<u64>) -> Self {
        let scope = scope();
        let mut blocks = BTreeMap::new();
        for height in 1..=tip {
            let drafts = transaction_at
                .filter(|included| *included == height)
                .map(|_| {
                    vec![ObservationDraft {
                        scope: scope.clone(),
                        transaction_id: transaction_id(&format!("a-tx-{height}")),
                        status: ObservationDraftStatus::Included,
                        movements: Vec::new(),
                        fee: None,
                        watch_ids: vec![WatchId("watch-1".to_owned())],
                        first_seen_at: height,
                        observed_at: height,
                    }]
                })
                .unwrap_or_default();
            blocks.insert(
                height,
                TestBlock {
                    reference: BlockRef {
                        height: BlockHeight(height),
                        hash: hash("a", height),
                        parent_hash: Some(hash("a", height - 1)),
                        timestamp: Some(height),
                    },
                    drafts,
                },
            );
        }
        Self {
            blocks: Arc::new(Mutex::new(blocks)),
            next_failure: Arc::new(Mutex::new(None)),
        }
    }

    fn fail_next(&self, failure: TestSourceFailure) {
        let previous = self
            .next_failure
            .lock()
            .expect("test source failure lock is healthy")
            .replace(failure);
        assert!(
            previous.is_none(),
            "test source already has a queued failure"
        );
    }

    fn should_fail(&self, operation: TestSourceFailure) -> bool {
        let mut failure = self
            .next_failure
            .lock()
            .expect("test source failure lock is healthy");
        if failure.as_ref() == Some(&operation) {
            failure.take();
            true
        } else {
            false
        }
    }

    fn truncate_after(&self, tip: u64) -> BTreeMap<u64, TestBlock> {
        self.blocks
            .lock()
            .expect("test source lock is healthy")
            .split_off(
                &tip.checked_add(1)
                    .expect("test source truncation height does not overflow"),
            )
    }

    fn restore_blocks(&self, blocks: BTreeMap<u64, TestBlock>) {
        self.blocks
            .lock()
            .expect("test source lock is healthy")
            .extend(blocks);
    }

    fn replace_suffix(&self, common_height: u64, tip: u64, tag: &str) {
        let mut blocks = self.blocks.lock().expect("test source lock is healthy");
        blocks.retain(|height, _| *height <= common_height);
        let parent_hash = blocks.get(&common_height).map_or_else(
            || hash("a", common_height),
            |block| block.reference.hash.clone(),
        );
        let mut parent = parent_hash;
        for height in (common_height + 1)..=tip {
            let block_hash = hash(tag, height);
            blocks.insert(
                height,
                TestBlock {
                    reference: BlockRef {
                        height: BlockHeight(height),
                        hash: block_hash.clone(),
                        parent_hash: Some(parent),
                        timestamp: Some(height + 1_000),
                    },
                    drafts: Vec::new(),
                },
            );
            parent = block_hash;
        }
    }

    fn corrupt_only_block_height(&self, height: u64) {
        let mut blocks = self.blocks.lock().expect("test source lock is healthy");
        let block = blocks
            .get_mut(&height)
            .expect("test block selected for corruption must exist");
        block.reference.height = BlockHeight(height + 1);
    }
}

impl BlockSource for TestSource {
    type Block = TestBlock;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        let result = if self.should_fail(TestSourceFailure::Tip) {
            Err(SourceError {
                message: "simulated retryable tip outage".to_owned(),
                retryable: true,
            })
        } else {
            self.blocks
                .lock()
                .expect("test source lock is healthy")
                .last_key_value()
                .map(|(_, block)| block.reference.clone())
                .ok_or_else(|| SourceError {
                    message: "test source has no blocks".to_owned(),
                    retryable: false,
                })
        };
        Box::pin(async move { result })
    }

    fn block_at<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Self::Block, SourceError>> {
        let result = if self.should_fail(TestSourceFailure::BlockAt(height.0)) {
            Err(SourceError {
                message: format!("simulated retryable block {} outage", height.0),
                retryable: true,
            })
        } else {
            self.blocks
                .lock()
                .expect("test source lock is healthy")
                .get(&height.0)
                .cloned()
                .ok_or_else(|| SourceError {
                    message: format!("missing test block {}", height.0),
                    retryable: true,
                })
        };
        Box::pin(async move { result })
    }

    fn canonical_hash<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
        let result = if self.should_fail(TestSourceFailure::CanonicalHash(height.0)) {
            Err(SourceError {
                message: format!("simulated retryable canonical hash {} outage", height.0),
                retryable: true,
            })
        } else {
            let value = self
                .blocks
                .lock()
                .expect("test source lock is healthy")
                .get(&height.0)
                .map(|block| block.reference.hash.clone());
            Ok(value)
        };
        Box::pin(async move { result })
    }
}

struct TestInterpreter;

impl BlockInterpreter for TestInterpreter {
    type Block = TestBlock;
    type Target = ();
    type Undo = ();

    fn inspect(
        &self,
        block: &Self::Block,
        _watches: &[WatchTarget<Self::Target>],
    ) -> Result<InterpretedBlock<Self::Undo>, IndexError> {
        Ok(InterpretedBlock {
            block: block.reference.clone(),
            drafts: block.drafts.clone(),
            projection: indexing::ProjectionBatch::default(),
            undo: (),
            raw: RawBlockData {
                block: block.reference.hash.0.clone(),
                receipts: Vec::new(),
            },
        })
    }
}

#[derive(Clone, Default)]
struct RecordingSyncObserver {
    commits: Arc<Mutex<Vec<BlockCommitObservation>>>,
    reorgs: Arc<Mutex<Vec<ReorgObservation>>>,
}

impl RecordingSyncObserver {
    fn commits(&self) -> Vec<BlockCommitObservation> {
        self.commits
            .lock()
            .expect("test observer commit lock is healthy")
            .clone()
    }

    fn reorgs(&self) -> Vec<ReorgObservation> {
        self.reorgs
            .lock()
            .expect("test observer reorg lock is healthy")
            .clone()
    }
}

impl SyncObserver for RecordingSyncObserver {
    fn block_commit(&self, observation: BlockCommitObservation) {
        self.commits
            .lock()
            .expect("test observer commit lock is healthy")
            .push(observation);
    }

    fn reorg_detected(&self, observation: ReorgObservation) {
        self.reorgs
            .lock()
            .expect("test observer reorg lock is healthy")
            .push(observation);
    }
}

#[derive(Clone)]
struct TestRepository {
    state: Arc<Mutex<TestState>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredBundle {
    block: BlockRef,
    transaction_ids: Vec<CanonicalTransactionId>,
}

struct TestState {
    scope: indexing::IndexScope,
    confirmation_policy: ConfirmationPolicy,
    checkpoint: Option<BlockRef>,
    canonical: BTreeMap<u64, BlockRef>,
    bundles: BTreeMap<u64, StoredBundle>,
    observations: BTreeMap<CanonicalTransactionId, ObservedTransaction>,
    observation_watches: BTreeMap<CanonicalTransactionId, Vec<WatchId>>,
    events: Vec<ObservationEvent>,
    watches: BTreeMap<String, WatchTarget<()>>,
    watch_version: WatchVersion,
    status: SyncStatus,
    rebuild: Option<RebuildState>,
    commit_order: Vec<u64>,
    revert_count: u64,
    fail_after_next_commit: bool,
}

impl TestRepository {
    fn new() -> Self {
        let scope = scope();
        let confirmation_policy = policy();
        Self {
            state: Arc::new(Mutex::new(TestState {
                scope: scope.clone(),
                confirmation_policy,
                checkpoint: None,
                canonical: BTreeMap::new(),
                bundles: BTreeMap::new(),
                observations: BTreeMap::new(),
                observation_watches: BTreeMap::new(),
                events: Vec::new(),
                watches: BTreeMap::new(),
                watch_version: WatchVersion(0),
                status: SyncStatus::starting(scope, confirmation_policy),
                rebuild: None,
                commit_order: Vec::new(),
                revert_count: 0,
                fail_after_next_commit: false,
            })),
        }
    }

    fn fail_after_next_commit(&self) {
        self.state
            .lock()
            .expect("test repository lock is healthy")
            .fail_after_next_commit = true;
    }

    fn snapshot(&self) -> TestSnapshot {
        let state = self.state.lock().expect("test repository lock is healthy");
        TestSnapshot {
            checkpoint: state.checkpoint.clone(),
            canonical: state.canonical.clone(),
            bundles: state.bundles.clone(),
            events: state.events.clone(),
            commit_order: state.commit_order.clone(),
            revert_count: state.revert_count,
            observations: state.observations.clone(),
        }
    }

    fn check_scope(state: &TestState, scope: &indexing::IndexScope) -> Result<(), IndexError> {
        if &state.scope != scope {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "test repository scope mismatch",
                false,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TestSnapshot {
    checkpoint: Option<BlockRef>,
    canonical: BTreeMap<u64, BlockRef>,
    bundles: BTreeMap<u64, StoredBundle>,
    events: Vec<ObservationEvent>,
    commit_order: Vec<u64>,
    revert_count: u64,
    observations: BTreeMap<CanonicalTransactionId, ObservedTransaction>,
}

impl TestState {
    fn append_transition(
        &mut self,
        transaction_id: CanonicalTransactionId,
        status: TransactionStatus,
        draft: Option<&ObservationDraft>,
        observed_at: u64,
    ) {
        let previous = self.observations.get(&transaction_id).cloned();
        let revision = ObservationRevision(
            previous
                .as_ref()
                .map_or(1, |transaction| transaction.revision.0 + 1),
        );
        let watch_ids = draft.map_or_else(
            || {
                self.observation_watches
                    .get(&transaction_id)
                    .cloned()
                    .unwrap_or_default()
            },
            |draft| draft.watch_ids.clone(),
        );
        let transaction = ObservedTransaction {
            scope: self.scope.clone(),
            transaction_id: transaction_id.clone(),
            revision,
            status: status.clone(),
            movements: draft.map_or_else(
                || {
                    previous
                        .as_ref()
                        .map(|transaction| transaction.movements.clone())
                        .unwrap_or_default()
                },
                |draft| draft.movements.clone(),
            ),
            fee: draft.map_or_else(
                || {
                    previous
                        .as_ref()
                        .and_then(|transaction| transaction.fee.clone())
                },
                |draft| draft.fee.clone(),
            ),
            first_seen_at: draft.map_or_else(
                || {
                    previous
                        .as_ref()
                        .map_or(observed_at, |transaction| transaction.first_seen_at)
                },
                |draft| {
                    previous
                        .as_ref()
                        .map_or(draft.first_seen_at, |transaction| transaction.first_seen_at)
                },
            ),
            observed_at,
        };
        self.observation_watches
            .insert(transaction_id.clone(), watch_ids.clone());
        self.observations
            .insert(transaction_id.clone(), transaction.clone());
        let cursor = EventCursor(self.events.len() as u64 + 1);
        self.events.push(ObservationEvent {
            id: ObservationEventId(format!(
                "{}:{}:{}",
                self.scope.network, transaction_id.value, revision.0
            )),
            cursor,
            watch_ids,
            previous_status: previous.map(|transaction| transaction.status),
            transaction,
        });
    }

    fn advance_confirmations(&mut self, tip: &BlockRef) {
        let transitions: Vec<_> = self
            .observations
            .iter()
            .filter_map(|(transaction_id, transaction)| {
                let (block, previous_depth) = match &transaction.status {
                    TransactionStatus::Included {
                        block,
                        confirmations,
                    } => (block, Some(*confirmations)),
                    TransactionStatus::Confirmed { block, .. } => (block, None),
                    _ => return None,
                };
                let depth = tip.height.0.checked_sub(block.height.0)? + 1;
                let status = if depth >= self.confirmation_policy.minimum_confirmations {
                    previous_depth?;
                    TransactionStatus::Confirmed {
                        block: block.clone(),
                        proof: ConfirmationProof::Depth {
                            required: self.confirmation_policy.minimum_confirmations,
                            observed: depth,
                        },
                    }
                } else if previous_depth == Some(depth) {
                    return None;
                } else {
                    TransactionStatus::Included {
                        block: block.clone(),
                        confirmations: depth,
                    }
                };
                Some((transaction_id.clone(), status))
            })
            .collect();
        for (transaction_id, status) in transitions {
            self.append_transition(
                transaction_id,
                status,
                None,
                tip.timestamp.unwrap_or(tip.height.0),
            );
        }
    }

    fn correct_confirmations_after_revert(&mut self, tip: Option<&BlockRef>) {
        let Some(tip) = tip else {
            return;
        };
        let transitions: Vec<_> = self
            .observations
            .iter()
            .filter_map(|(transaction_id, transaction)| {
                let block = match &transaction.status {
                    TransactionStatus::Included { block, .. }
                    | TransactionStatus::Confirmed { block, .. } => block,
                    _ => return None,
                };
                let depth = tip.height.0.checked_sub(block.height.0)? + 1;
                if depth >= self.confirmation_policy.minimum_confirmations {
                    return None;
                }
                match &transaction.status {
                    TransactionStatus::Included { confirmations, .. }
                        if *confirmations == depth =>
                    {
                        None
                    }
                    _ => Some((
                        transaction_id.clone(),
                        TransactionStatus::Included {
                            block: block.clone(),
                            confirmations: depth,
                        },
                    )),
                }
            })
            .collect();
        for (transaction_id, status) in transitions {
            self.append_transition(
                transaction_id,
                status,
                None,
                tip.timestamp.unwrap_or(tip.height.0),
            );
        }
    }

    fn commit(
        &mut self,
        command: CommitBlockCommand<()>,
    ) -> Result<CommitBlockOutcome, IndexError> {
        TestRepository::check_scope(self, &command.scope)?;
        let block = &command.block.block;
        if self
            .canonical
            .get(&block.height.0)
            .is_some_and(|canonical| canonical.hash == block.hash)
        {
            return Ok(CommitBlockOutcome::AlreadyApplied);
        }
        if self.checkpoint != command.expected_checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "checkpoint condition failed",
                true,
            ));
        }
        if self.watch_version != command.expected_watch_version {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "watch version condition failed",
                true,
            ));
        }
        if self.confirmation_policy != command.confirmation_policy {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "confirmation policy changed",
                false,
            ));
        }
        if self
            .checkpoint
            .as_ref()
            .is_some_and(|checkpoint| block.parent_hash.as_ref() != Some(&checkpoint.hash))
        {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "block does not connect",
                true,
            ));
        }

        self.advance_confirmations(block);
        let mut transaction_ids = Vec::with_capacity(command.block.drafts.len());
        let mut unique = BTreeSet::new();
        for draft in &command.block.drafts {
            if !unique.insert(draft.transaction_id.clone()) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "duplicate transaction draft",
                    false,
                ));
            }
            let status = match &draft.status {
                ObservationDraftStatus::Included => TransactionStatus::Included {
                    block: block.clone(),
                    confirmations: 1,
                },
                ObservationDraftStatus::Failed { reason } => TransactionStatus::Failed {
                    block: Some(block.clone()),
                    reason: reason.clone(),
                },
            };
            self.append_transition(
                draft.transaction_id.clone(),
                status,
                Some(draft),
                draft.observed_at,
            );
            transaction_ids.push(draft.transaction_id.clone());
        }

        self.canonical.insert(block.height.0, block.clone());
        self.bundles.insert(
            block.height.0,
            StoredBundle {
                block: block.clone(),
                transaction_ids,
            },
        );
        self.checkpoint = Some(block.clone());
        self.commit_order.push(block.height.0);
        let anchor = block.height.0.saturating_sub(command.reorg_retention);
        self.canonical.retain(|height, _| *height >= anchor);
        self.bundles.retain(|height, _| *height > anchor);
        Ok(CommitBlockOutcome::Applied)
    }
}

impl IndexRepository for TestRepository {
    type Target = ();
    type Undo = ();

    fn checkpoint<'a>(
        &'a self,
        scope: &'a indexing::IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, scope).map(|()| state.checkpoint.clone())
        };
        Box::pin(async move { result })
    }

    fn canonical_block<'a>(
        &'a self,
        scope: &'a indexing::IndexScope,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, scope)
                .map(|()| state.canonical.get(&height.0).cloned())
        };
        Box::pin(async move { result })
    }

    fn watches_at<'a>(
        &'a self,
        scope: &'a indexing::IndexScope,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<WatchSnapshot<Self::Target>, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, scope).map(|()| WatchSnapshot {
                version: state.watch_version,
                watches: state
                    .watches
                    .values()
                    .filter(|watch| watch.is_active_at(height))
                    .cloned()
                    .collect(),
            })
        };
        Box::pin(async move { result })
    }

    fn register_watch<'a>(
        &'a self,
        command: RegisterWatchCommand<Self::Target>,
    ) -> BoxFuture<'a, Result<RegisterWatchOutcome, IndexError>> {
        let result = {
            let mut state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &command.request.scope).and_then(|()| {
                if let Some(existing) = state.watches.get(&command.request.idempotency_key) {
                    if existing.selector != command.request.selector
                        || existing.start_height != command.request.start_height
                    {
                        return Err(IndexError::new(
                            IndexErrorKind::Conflict,
                            "idempotency key payload changed",
                            false,
                        ));
                    }
                    return Ok(RegisterWatchOutcome::Existing(WatchReceipt {
                        id: existing.id.clone(),
                        scope: existing.scope.clone(),
                        selector: existing.selector.clone(),
                        start_height: existing.start_height,
                        registered_at: existing.registered_at.clone(),
                        inactive_from: existing.inactive_from,
                        confirmation_policy: state.confirmation_policy,
                    }));
                }
                let id = WatchId(format!("watch-{}", state.watches.len() + 1));
                let target = WatchTarget {
                    id: id.clone(),
                    scope: command.request.scope.clone(),
                    selector: command.request.selector.clone(),
                    target: command.target,
                    idempotency_key: command.request.idempotency_key.clone(),
                    start_height: command.request.start_height,
                    registered_at: command.registered_at.clone(),
                    inactive_from: None,
                };
                state
                    .watches
                    .insert(command.request.idempotency_key, target);
                state.watch_version.0 += 1;
                Ok(RegisterWatchOutcome::Registered(WatchReceipt {
                    id,
                    scope: command.request.scope,
                    selector: command.request.selector,
                    start_height: command.request.start_height,
                    registered_at: command.registered_at,
                    inactive_from: None,
                    confirmation_policy: state.confirmation_policy,
                }))
            })
        };
        Box::pin(async move { result })
    }

    fn pending_watch_backfills<'a>(
        &'a self,
        _scope: &'a indexing::IndexScope,
        _limit: usize,
    ) -> BoxFuture<'a, Result<Vec<WatchBackfill>, IndexError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn commit_watch_backfill<'a>(
        &'a self,
        _command: CommitWatchBackfillCommand,
    ) -> BoxFuture<'a, Result<CommitWatchBackfillOutcome, IndexError>> {
        Box::pin(async {
            Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "test repository has no historical backfill worker",
                false,
            ))
        })
    }

    fn unwatch<'a>(
        &'a self,
        command: UnwatchCommand,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        let result = {
            let mut state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &command.scope).and_then(|()| {
                let watch = state
                    .watches
                    .values_mut()
                    .find(|watch| watch.id == command.watch_id)
                    .ok_or_else(|| {
                        IndexError::new(IndexErrorKind::InvalidWatch, "unknown watch", false)
                    })?;
                if watch.inactive_from.is_some() {
                    return Ok(UnwatchOutcome::AlreadyInactive);
                }
                watch.inactive_from = Some(command.inactive_from);
                state.watch_version.0 += 1;
                Ok(UnwatchOutcome::Deactivated)
            })
        };
        Box::pin(async move { result })
    }

    fn commit_block<'a>(
        &'a self,
        command: CommitBlockCommand<Self::Undo>,
    ) -> BoxFuture<'a, Result<CommitBlockOutcome, IndexError>> {
        let result = {
            let mut state = self.state.lock().expect("test repository lock is healthy");
            let result = state.commit(command);
            if result.is_ok() && state.fail_after_next_commit {
                state.fail_after_next_commit = false;
                Err(IndexError::new(
                    IndexErrorKind::Storage,
                    "simulated lost commit acknowledgement",
                    true,
                ))
            } else {
                result
            }
        };
        Box::pin(async move { result })
    }

    fn revert_tip<'a>(
        &'a self,
        command: RevertTipCommand,
    ) -> BoxFuture<'a, Result<RevertTipOutcome, IndexError>> {
        let result = {
            let mut state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &command.scope).and_then(|()| {
                if state.checkpoint.as_ref() != Some(&command.expected_tip) {
                    if state
                        .checkpoint
                        .as_ref()
                        .is_none_or(|tip| tip.height < command.expected_tip.height)
                    {
                        return Ok(RevertTipOutcome::AlreadyReverted {
                            checkpoint: state.checkpoint.clone(),
                        });
                    }
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "revert tip condition failed",
                        true,
                    ));
                }
                let bundle = state
                    .bundles
                    .remove(&command.expected_tip.height.0)
                    .ok_or_else(|| {
                        IndexError::new(
                            IndexErrorKind::ReorgBeyondRetention,
                            "tip undo bundle is no longer retained",
                            false,
                        )
                    })?;
                if bundle.block != command.expected_tip {
                    return Err(IndexError::new(
                        IndexErrorKind::Storage,
                        "undo bundle block mismatch",
                        false,
                    ));
                }
                state.canonical.remove(&command.expected_tip.height.0);
                let new_checkpoint = command
                    .expected_tip
                    .height
                    .0
                    .checked_sub(1)
                    .and_then(|height| state.canonical.get(&height).cloned());
                state.checkpoint = new_checkpoint.clone();
                for transaction_id in bundle.transaction_ids {
                    let previous_block = command.expected_tip.clone();
                    state.append_transition(
                        transaction_id,
                        TransactionStatus::Reorged { previous_block },
                        None,
                        new_checkpoint
                            .as_ref()
                            .and_then(|block| block.timestamp)
                            .unwrap_or(command.expected_tip.height.0),
                    );
                }
                state.correct_confirmations_after_revert(new_checkpoint.as_ref());
                state.revert_count += 1;
                Ok(RevertTipOutcome::Reverted {
                    checkpoint: new_checkpoint,
                })
            })
        };
        Box::pin(async move { result })
    }

    fn transaction<'a>(
        &'a self,
        request: TransactionRequest,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &request.scope)
                .map(|()| state.observations.get(&request.transaction_id).cloned())
        };
        Box::pin(async move { result })
    }

    fn transactions_by_address<'a>(
        &'a self,
        request: TransactionPageRequest,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &request.scope).map(|()| TransactionPage {
                transactions: state
                    .observations
                    .values()
                    .filter(|transaction| {
                        transaction.movements.iter().any(|movement| {
                            movement.from.as_ref() == Some(&request.address)
                                || movement.to.as_ref() == Some(&request.address)
                        })
                    })
                    .take(request.limit)
                    .cloned()
                    .collect(),
                next: None,
            })
        };
        Box::pin(async move { result })
    }

    fn watches_for_address<'a>(
        &'a self,
        request: AddressWatchRequest,
    ) -> BoxFuture<'a, Result<Vec<WatchReceipt>, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &request.scope).map(|()| {
                state
                    .watches
                    .values()
                    .filter(|watch| {
                        watch.selector == WatchSelector::Address(request.address.clone())
                    })
                    .map(|watch| WatchReceipt {
                        id: watch.id.clone(),
                        scope: watch.scope.clone(),
                        selector: watch.selector.clone(),
                        start_height: watch.start_height,
                        registered_at: watch.registered_at.clone(),
                        inactive_from: watch.inactive_from,
                        confirmation_policy: state.confirmation_policy,
                    })
                    .collect()
            })
        };
        Box::pin(async move { result })
    }

    fn events<'a>(
        &'a self,
        request: ObservationEventRequest,
    ) -> BoxFuture<'a, Result<ObservationEventPage, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &request.scope).map(|()| {
                let events: Vec<_> = state
                    .events
                    .iter()
                    .filter(|event| request.after.is_none_or(|after| event.cursor > after))
                    .take(request.limit)
                    .cloned()
                    .collect();
                ObservationEventPage {
                    next: events.last().map(|event| event.cursor),
                    events,
                }
            })
        };
        Box::pin(async move { result })
    }

    fn event_high_water<'a>(
        &'a self,
        scope: &'a indexing::IndexScope,
    ) -> BoxFuture<'a, Result<Option<EventCursor>, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, scope)
                .map(|()| state.events.last().map(|event| event.cursor))
        };
        Box::pin(async move { result })
    }

    fn status<'a>(
        &'a self,
        scope: &'a indexing::IndexScope,
    ) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, scope).map(|()| state.status.clone())
        };
        Box::pin(async move { result })
    }

    fn set_status<'a>(&'a self, status: SyncStatus) -> BoxFuture<'a, Result<(), IndexError>> {
        let result = {
            let mut state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &status.scope).map(|()| {
                state.status = status;
            })
        };
        Box::pin(async move { result })
    }

    fn migrate_policy<'a>(
        &'a self,
        _command: MigrateIndexPolicyCommand,
    ) -> BoxFuture<'a, Result<MigrateIndexPolicyOutcome, IndexError>> {
        Box::pin(async {
            Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "ordered-sync test repository does not persist policy migrations",
                false,
            ))
        })
    }

    fn rebuild_state<'a>(
        &'a self,
        scope: &'a indexing::IndexScope,
    ) -> BoxFuture<'a, Result<Option<RebuildState>, IndexError>> {
        let result = {
            let state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, scope).map(|()| state.rebuild.clone())
        };
        Box::pin(async move { result })
    }

    fn begin_rebuild<'a>(
        &'a self,
        command: BeginRebuildCommand,
    ) -> BoxFuture<'a, Result<RebuildState, IndexError>> {
        let result = {
            let mut state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &command.scope).map(|()| {
                let rebuild = RebuildState {
                    scope: command.scope,
                    generation: RebuildGeneration(1),
                    phase: RebuildPhase::Building,
                    bootstrap_height: command.bootstrap_height,
                    checkpoint: None,
                    published_event_high_water: EventCursor(state.events.len() as u64),
                };
                state.rebuild = Some(rebuild.clone());
                rebuild
            })
        };
        Box::pin(async move { result })
    }

    fn commit_rebuild_block<'a>(
        &'a self,
        _command: CommitRebuildBlockCommand<Self::Undo>,
    ) -> BoxFuture<'a, Result<CommitBlockOutcome, IndexError>> {
        Box::pin(async {
            Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "test repository does not build shadow generations",
                false,
            ))
        })
    }

    fn validate_rebuild<'a>(
        &'a self,
        command: ValidateRebuildCommand,
    ) -> BoxFuture<'a, Result<RebuildState, IndexError>> {
        let result = {
            let mut state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &command.scope).and_then(|()| {
                let rebuild = state.rebuild.as_mut().ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidRequest,
                        "no test rebuild is active",
                        false,
                    )
                })?;
                if rebuild.generation != command.generation
                    || rebuild.checkpoint.as_ref() != Some(&command.expected_checkpoint)
                {
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "test rebuild command does not match the manifest",
                        false,
                    ));
                }
                if rebuild.phase == RebuildPhase::Building {
                    rebuild.phase = RebuildPhase::Validating;
                }
                Ok(rebuild.clone())
            })
        };
        Box::pin(async move { result })
    }

    fn prepare_rebuild_activation<'a>(
        &'a self,
        command: PrepareRebuildActivationCommand,
    ) -> BoxFuture<'a, Result<RebuildState, IndexError>> {
        let result = {
            let mut state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &command.scope).and_then(|()| {
                let rebuild = state.rebuild.as_mut().ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidRequest,
                        "no test rebuild is active",
                        false,
                    )
                })?;
                if rebuild.generation != command.generation
                    || rebuild.checkpoint.as_ref() != Some(&command.expected_checkpoint)
                    || rebuild.phase == RebuildPhase::Building
                {
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "test rebuild is not validated for activation",
                        false,
                    ));
                }
                if rebuild.phase == RebuildPhase::Validating {
                    rebuild.phase = RebuildPhase::ReadyToActivate;
                }
                Ok(rebuild.clone())
            })
        };
        Box::pin(async move { result })
    }

    fn activate_rebuild<'a>(
        &'a self,
        _command: ActivateRebuildCommand,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async {
            Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "test repository does not activate shadow generations",
                false,
            ))
        })
    }

    fn abort_rebuild<'a>(
        &'a self,
        command: AbortRebuildCommand,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        let result = {
            let mut state = self.state.lock().expect("test repository lock is healthy");
            TestRepository::check_scope(&state, &command.scope).map(|()| {
                if state
                    .rebuild
                    .as_ref()
                    .is_some_and(|rebuild| rebuild.generation == command.generation)
                {
                    state.rebuild = None;
                }
            })
        };
        Box::pin(async move { result })
    }

    fn cleanup_generation<'a>(
        &'a self,
        _command: CleanupGenerationCommand,
    ) -> BoxFuture<'a, Result<CleanupGenerationOutcome, IndexError>> {
        Box::pin(async { Ok(CleanupGenerationOutcome::AlreadyAbsent) })
    }
}

fn worker(
    source: TestSource,
    repository: TestRepository,
) -> OrderedSyncWorker<TestSource, TestInterpreter, TestRepository> {
    OrderedSyncWorker::new(
        source,
        TestInterpreter,
        repository,
        OrderedSyncConfig::ethereum_v1(scope(), BlockHeight(1)),
    )
}

fn sync_all(
    worker: &OrderedSyncWorker<TestSource, TestInterpreter, TestRepository>,
) -> Result<SyncStatus, IndexError> {
    block_on(worker.sync(SyncRequest {
        scope: scope(),
        through: None,
        max_blocks: None,
    }))
}

#[test]
fn ordered_sync_fetches_every_height_when_heads_are_skipped() {
    let source = TestSource::linear(8, None);
    let repository = TestRepository::new();
    let worker = worker(source, repository.clone());

    let status = sync_all(&worker).expect("ordered synchronization succeeds");

    assert_eq!(status.phase, SyncPhase::Ready);
    assert_eq!(
        status.checkpoint.map(|block| block.height),
        Some(BlockHeight(8))
    );
    assert_eq!(
        repository.snapshot().commit_order,
        (1..=8).collect::<Vec<_>>()
    );
}

#[test]
fn restart_resumes_a_bounded_run_without_duplicate_commits() {
    let source = TestSource::linear(6, None);
    let repository = TestRepository::new();
    let first_worker = worker(source.clone(), repository.clone());
    let first = block_on(first_worker.sync(SyncRequest {
        scope: scope(),
        through: None,
        max_blocks: Some(2),
    }))
    .expect("bounded synchronization succeeds");
    assert_eq!(first.phase, SyncPhase::CatchingUp);

    let restarted = worker(source, repository.clone());
    assert_eq!(
        sync_all(&restarted).expect("restart catches up").phase,
        SyncPhase::Ready
    );
    let before_retry = repository.snapshot();
    assert_eq!(
        sync_all(&restarted).expect("ready retry succeeds").phase,
        SyncPhase::Ready
    );
    let after_retry = repository.snapshot();

    assert_eq!(before_retry.commit_order, (1..=6).collect::<Vec<_>>());
    assert_eq!(after_retry.commit_order, before_retry.commit_order);
    assert_eq!(after_retry.events, before_retry.events);
}

#[test]
fn unknown_commit_outcome_is_idempotent_after_restart() {
    let source = TestSource::linear(1, Some(1));
    let repository = TestRepository::new();
    repository.fail_after_next_commit();
    let first_worker = worker(source.clone(), repository.clone());

    let error = sync_all(&first_worker).expect_err("acknowledgement is intentionally lost");
    assert!(error.retryable);
    assert_eq!(repository.snapshot().events.len(), 1);

    let restarted = worker(source, repository.clone());
    assert_eq!(
        sync_all(&restarted).expect("restart reconciles").phase,
        SyncPhase::Ready
    );
    let snapshot = repository.snapshot();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.commit_order, vec![1]);
}

#[test]
fn observer_reports_each_repository_commit_attempt_with_its_outcome() {
    let source = TestSource::linear(2, None);
    let repository = TestRepository::new();
    repository.fail_after_next_commit();
    let observer = Arc::new(RecordingSyncObserver::default());
    let worker = worker(source, repository).with_observer(observer.clone());

    let error = sync_all(&worker).expect_err("the first commit acknowledgement is lost");
    assert_eq!(error.kind, IndexErrorKind::Storage);
    assert!(error.retryable);
    assert_eq!(
        sync_all(&worker)
            .expect("the next synchronization observes the committed checkpoint")
            .phase,
        SyncPhase::Ready
    );

    let commits = observer.commits();
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].scope, scope());
    assert_eq!(commits[0].block.height, BlockHeight(1));
    assert_eq!(
        commits[0].outcome,
        BlockCommitObservationOutcome::Failure {
            kind: IndexErrorKind::Storage,
            retryable: true,
        }
    );
    assert_eq!(commits[1].block.height, BlockHeight(2));
    assert_eq!(
        commits[1].outcome,
        BlockCommitObservationOutcome::Success(CommitBlockOutcome::Applied)
    );
}

#[test]
fn retryable_source_outages_do_not_mutate_canonical_state_or_prove_drop() {
    let source = TestSource::linear(2, Some(1));
    let repository = TestRepository::new();
    let worker = worker(source.clone(), repository.clone());

    let first = block_on(worker.sync(SyncRequest {
        scope: scope(),
        through: None,
        max_blocks: Some(1),
    }))
    .expect("the first block synchronizes");
    assert_eq!(first.phase, SyncPhase::CatchingUp);
    let baseline = repository.snapshot();

    for failure in [
        TestSourceFailure::Tip,
        TestSourceFailure::CanonicalHash(1),
        TestSourceFailure::BlockAt(2),
        TestSourceFailure::CanonicalHash(2),
    ] {
        source.fail_next(failure);
        let error = sync_all(&worker).expect_err("the injected source outage is returned");

        assert_eq!(error.kind, IndexErrorKind::Source, "failure {failure:?}");
        assert!(error.retryable, "failure {failure:?}");
        let after = repository.snapshot();
        assert_eq!(after, baseline, "failure {failure:?}");
        assert!(matches!(
            after
                .observations
                .get(&transaction_id("a-tx-1"))
                .map(|transaction| &transaction.status),
            Some(TransactionStatus::Included {
                confirmations: 1,
                ..
            })
        ));
    }

    let ready = sync_all(&worker).expect("synchronization resumes after the outages");
    assert_eq!(ready.phase, SyncPhase::Ready);
    let completed = repository.snapshot();
    assert_eq!(completed.commit_order, vec![1, 2]);
    assert!(matches!(
        completed
            .observations
            .get(&transaction_id("a-tx-1"))
            .map(|transaction| &transaction.status),
        Some(TransactionStatus::Included {
            confirmations: 2,
            ..
        })
    ));
}

#[test]
fn source_behind_persisted_checkpoint_fails_safely_until_it_catches_up() {
    let source = TestSource::linear(3, Some(1));
    let repository = TestRepository::new();
    let worker = worker(source.clone(), repository.clone());
    sync_all(&worker).expect("the original chain synchronizes");
    let baseline = repository.snapshot();

    let hidden_blocks = source.truncate_after(2);
    assert_eq!(hidden_blocks.len(), 1);
    let error = sync_all(&worker)
        .expect_err("a source that cannot expose the durable checkpoint must not reconcile");

    assert_eq!(error.kind, IndexErrorKind::Source);
    assert!(error.retryable);
    let after_error = repository.snapshot();
    assert_eq!(after_error, baseline);
    assert!(matches!(
        after_error
            .observations
            .get(&transaction_id("a-tx-1"))
            .map(|transaction| &transaction.status),
        Some(TransactionStatus::Included {
            confirmations: 3,
            ..
        })
    ));

    source.restore_blocks(hidden_blocks);
    let ready = sync_all(&worker).expect("synchronization resumes when the source catches up");
    assert_eq!(ready.phase, SyncPhase::Ready);
    assert_eq!(repository.snapshot(), baseline);
}

#[test]
fn empty_blocks_advance_every_depth_and_confirm_at_twelve() {
    let source = TestSource::linear(12, Some(1));
    let repository = TestRepository::new();
    let worker = worker(source, repository.clone());

    sync_all(&worker).expect("synchronization succeeds");
    let events = repository.snapshot().events;
    assert_eq!(events.len(), 12);
    for (offset, event) in events.iter().take(11).enumerate() {
        assert_eq!(
            event.transaction.revision,
            ObservationRevision(offset as u64 + 1)
        );
        assert!(matches!(
            event.transaction.status,
            TransactionStatus::Included { confirmations, .. }
                if confirmations == offset as u64 + 1
        ));
    }
    assert!(matches!(
        events.last().map(|event| &event.transaction.status),
        Some(TransactionStatus::Confirmed {
            proof: ConfirmationProof::Depth {
                required: 12,
                observed: 12
            },
            ..
        })
    ));
    let cursors: BTreeSet<_> = events.iter().map(|event| event.cursor).collect();
    assert_eq!(cursors.len(), events.len());
}

#[test]
fn reorg_depths_one_twelve_forty_nine_and_fifty_replay_from_common_ancestor() {
    for depth in [1_u64, 12, 49, 50] {
        let tip = 55;
        let common = tip - depth;
        let source = TestSource::linear(tip, Some(common + 1));
        let repository = TestRepository::new();
        let worker = worker(source.clone(), repository.clone());
        sync_all(&worker).expect("original chain synchronizes");

        source.replace_suffix(common, tip, "b");
        let status = sync_all(&worker).expect("retained reorg recovers");
        let snapshot = repository.snapshot();

        assert_eq!(status.phase, SyncPhase::Ready, "depth {depth}");
        assert_eq!(
            snapshot.checkpoint.as_ref().map(|block| &block.hash),
            Some(&hash("b", tip)),
            "depth {depth}"
        );
        assert_eq!(snapshot.revert_count, depth, "depth {depth}");
        let orphan_id = transaction_id(&format!("a-tx-{}", common + 1));
        assert!(matches!(
            snapshot
                .observations
                .get(&orphan_id)
                .map(|transaction| &transaction.status),
            Some(TransactionStatus::Reorged { .. })
        ));
        let cursors: BTreeSet<_> = snapshot.events.iter().map(|event| event.cursor).collect();
        assert_eq!(cursors.len(), snapshot.events.len(), "depth {depth}");
    }
}

#[test]
fn observer_reports_an_exact_reorg_depth_once_per_reconciliation() {
    let source = TestSource::linear(5, None);
    let repository = TestRepository::new();
    let observer = Arc::new(RecordingSyncObserver::default());
    let worker = worker(source.clone(), repository).with_observer(observer.clone());
    sync_all(&worker).expect("the original chain synchronizes");

    source.replace_suffix(2, 5, "b");
    sync_all(&worker).expect("the retained reorg reconciles");
    sync_all(&worker).expect("a repeated ready poll remains idempotent");

    let reorgs = observer.reorgs();
    assert_eq!(reorgs.len(), 1);
    assert_eq!(reorgs[0].scope, scope());
    assert_eq!(reorgs[0].previous_tip.height, BlockHeight(5));
    assert_eq!(reorgs[0].previous_tip.hash, hash("a", 5));
    assert!(matches!(
        &reorgs[0].depth,
        ReorgDepth::Exact {
            depth: 3,
            common_ancestor,
        } if common_ancestor.height == BlockHeight(2)
            && common_ancestor.hash == hash("a", 2)
    ));
}

#[test]
fn reorg_depth_fifty_one_requires_rebuild_without_deleting_canonical_state() {
    let tip = 60;
    let common = tip - 51;
    let source = TestSource::linear(tip, Some(common + 1));
    let repository = TestRepository::new();
    let worker = worker(source.clone(), repository.clone());
    sync_all(&worker).expect("original chain synchronizes");
    let original = repository.snapshot();

    source.replace_suffix(common, tip, "b");
    let status = sync_all(&worker).expect("deep reorg becomes an operational status");
    let after = repository.snapshot();

    assert_eq!(status.phase, SyncPhase::RebuildRequired);
    assert_eq!(
        status
            .rebuild_reason
            .as_ref()
            .map(|reason| reason.oldest_retained),
        Some(BlockHeight(10))
    );
    assert_eq!(after.revert_count, 0);
    assert_eq!(after.checkpoint, original.checkpoint);
    assert_eq!(after.events, original.events);

    let repeated = sync_all(&worker).expect("rebuild-required remains an observable status");
    let repeated_state = repository.snapshot();
    assert_eq!(repeated, status);
    assert_eq!(repeated_state.checkpoint, original.checkpoint);
    assert_eq!(repeated_state.events, original.events);
}

#[test]
fn observer_reports_beyond_retention_once_with_a_minimum_depth() {
    let tip = 60;
    let source = TestSource::linear(tip, None);
    let repository = TestRepository::new();
    let observer = Arc::new(RecordingSyncObserver::default());
    let worker = worker(source.clone(), repository).with_observer(observer.clone());
    sync_all(&worker).expect("the original chain synchronizes");

    source.replace_suffix(9, tip, "b");
    let status = sync_all(&worker).expect("the deep reorg becomes a rebuild status");
    assert_eq!(status.phase, SyncPhase::RebuildRequired);
    assert_eq!(
        sync_all(&worker)
            .expect("the rebuild status remains sticky")
            .phase,
        SyncPhase::RebuildRequired
    );

    let reorgs = observer.reorgs();
    assert_eq!(reorgs.len(), 1);
    assert_eq!(reorgs[0].previous_tip.height, BlockHeight(tip));
    assert_eq!(
        reorgs[0].depth,
        ReorgDepth::BeyondRetention {
            minimum_depth: 51,
            oldest_retained: BlockHeight(10),
        }
    );
}

#[test]
fn non_retryable_block_error_becomes_a_sticky_halted_status() {
    let source = TestSource::linear(1, None);
    source.corrupt_only_block_height(1);
    let repository = TestRepository::new();
    let worker = worker(source, repository.clone());

    let halted = sync_all(&worker).expect("invalid source data becomes an operational status");
    assert_eq!(halted.phase, SyncPhase::Halted);
    assert!(
        halted
            .halted_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("unexpected height"))
    );

    let repeated = sync_all(&worker).expect("halted status remains observable across polls");
    assert_eq!(repeated, halted);
    assert_eq!(repository.snapshot().checkpoint, None);
}

#[test]
fn watch_idempotency_is_scoped_and_changed_payload_conflicts() {
    let repository = TestRepository::new();
    let address = CanonicalAddress {
        chain: ChainId("ethereum".to_owned()),
        value: "0xabc".to_owned(),
    };
    let command = RegisterWatchCommand {
        request: indexing::WatchRequest {
            scope: scope(),
            selector: WatchSelector::Address(address),
            start_height: BlockHeight(3),
            idempotency_key: "deposit-1".to_owned(),
        },
        target: (),
        registered_at: None,
    };
    let first = block_on(repository.register_watch(command.clone()))
        .expect("first watch registration succeeds");
    let second = block_on(repository.register_watch(command.clone()))
        .expect("identical watch registration is idempotent");
    assert!(matches!(first, RegisterWatchOutcome::Registered(_)));
    assert!(matches!(second, RegisterWatchOutcome::Existing(_)));

    let mut changed = command;
    changed.request.start_height = BlockHeight(4);
    let error =
        block_on(repository.register_watch(changed)).expect_err("changed payload must conflict");
    assert_eq!(error.kind, IndexErrorKind::Conflict);
}
