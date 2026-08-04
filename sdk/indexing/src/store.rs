use crate::{
    AddressWatchRequest, BlockHeight, BlockRef, BoxFuture, CommitBlockCommand, ConfirmationPolicy,
    EventCursor, IndexError, IndexScope, ObservationDraft, ObservationEventPage,
    ObservationEventRequest, ObservedTransaction, RegisterWatchCommand, RegisterWatchOutcome,
    SyncStatus, TransactionPage, TransactionPageRequest, TransactionRequest, UnwatchCommand,
    UnwatchOutcome, WatchBackfill, WatchId, WatchReceipt, WatchSnapshot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RebuildGeneration(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildPhase {
    Building,
    Validating,
    ReadyToActivate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildState {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
    pub phase: RebuildPhase,
    pub bootstrap_height: BlockHeight,
    pub checkpoint: Option<BlockRef>,
    /// Staged correction events remain invisible above this cursor until the
    /// generation is activated.
    pub published_event_high_water: EventCursor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeginRebuildCommand {
    pub scope: IndexScope,
    pub bootstrap_height: BlockHeight,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRebuildBlockCommand<U> {
    pub generation: RebuildGeneration,
    pub command: CommitBlockCommand<U>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivateRebuildCommand {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
    pub expected_checkpoint: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidateRebuildCommand {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
    pub expected_checkpoint: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareRebuildActivationCommand {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
    pub expected_checkpoint: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbortRebuildCommand {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupGenerationCommand {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupGenerationOutcome {
    Removed { records: u64 },
    AlreadyAbsent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitWatchBackfillCommand {
    pub scope: IndexScope,
    pub watch_id: WatchId,
    pub expected_next_height: BlockHeight,
    pub expected_checkpoint: BlockRef,
    pub block: BlockRef,
    pub drafts: Vec<ObservationDraft>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitWatchBackfillOutcome {
    Applied { next_height: Option<BlockHeight> },
    AlreadyApplied { next_height: Option<BlockHeight> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PolicyMigrationVersion(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrateIndexPolicyCommand {
    pub scope: IndexScope,
    pub bootstrap_height: BlockHeight,
    pub expected_confirmation_policy: ConfirmationPolicy,
    pub expected_reorg_retention: u64,
    pub target_confirmation_policy: ConfirmationPolicy,
    pub target_reorg_retention: u64,
    pub idempotency_key: String,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrateIndexPolicyOutcome {
    Applied { version: PolicyMigrationVersion },
    AlreadyApplied { version: PolicyMigrationVersion },
}

/// Composite semantic repository for one Indexer Service database.
///
/// Implementations must commit a block's raw payload, undo bundle, current
/// observations, immutable revisions, feed events, confirmation transitions,
/// checkpoint, and retention pruning atomically. Revision and cursor allocation
/// therefore never occurs in the interpreter or worker.
pub trait IndexRepository: Send + Sync {
    type Target: Clone + Send + Sync + 'static;
    type Undo: Clone + Send + Sync + 'static;

    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;

    /// Returns a retained canonical reference, including the predecessor anchor.
    fn canonical_block<'a>(
        &'a self,
        scope: &'a IndexScope,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;

    fn watches_at<'a>(
        &'a self,
        scope: &'a IndexScope,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<WatchSnapshot<Self::Target>, IndexError>>;

    /// Lists durable historical work created by watches registered behind the
    /// current canonical checkpoint. Applying that work remains a worker
    /// responsibility and cannot be acknowledged by watch registration.
    fn pending_watch_backfills<'a>(
        &'a self,
        scope: &'a IndexScope,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<WatchBackfill>, IndexError>>;

    /// Applies one canonical historical height for one watch and advances its
    /// durable cursor without moving the live canonical checkpoint. Empty
    /// historical blocks must still be committed.
    fn commit_watch_backfill<'a>(
        &'a self,
        command: CommitWatchBackfillCommand,
    ) -> BoxFuture<'a, Result<CommitWatchBackfillOutcome, IndexError>>;

    /// `(scope, idempotency_key)` is unique. The same payload returns
    /// `Existing`; a changed payload returns `IndexErrorKind::Conflict`.
    fn register_watch<'a>(
        &'a self,
        command: RegisterWatchCommand<Self::Target>,
    ) -> BoxFuture<'a, Result<RegisterWatchOutcome, IndexError>>;

    /// Soft-deactivates the watch while retaining its activation history.
    fn unwatch<'a>(
        &'a self,
        command: UnwatchCommand,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>>;

    /// A retry after an unknown acknowledgement must return `AlreadyApplied`
    /// without allocating another revision or cursor.
    fn commit_block<'a>(
        &'a self,
        command: CommitBlockCommand<Self::Undo>,
    ) -> BoxFuture<'a, Result<CommitBlockOutcome, IndexError>>;

    /// Reverts exactly one expected canonical tip. The undo, new observation
    /// revisions, correction events, and checkpoint movement are one commit.
    fn revert_tip<'a>(
        &'a self,
        command: RevertTipCommand,
    ) -> BoxFuture<'a, Result<RevertTipOutcome, IndexError>>;

    fn transaction<'a>(
        &'a self,
        request: TransactionRequest,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>>;

    fn transactions_by_address<'a>(
        &'a self,
        request: TransactionPageRequest,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;

    fn watches_for_address<'a>(
        &'a self,
        request: AddressWatchRequest,
    ) -> BoxFuture<'a, Result<Vec<WatchReceipt>, IndexError>>;

    fn events<'a>(
        &'a self,
        request: ObservationEventRequest,
    ) -> BoxFuture<'a, Result<ObservationEventPage, IndexError>>;

    /// Returns the last published event cursor without allocating a cursor or
    /// exposing hidden staged-rebuild events.
    fn event_high_water<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<EventCursor>, IndexError>>;

    /// Operational phase changes are durable so restart can resume a revert or
    /// expose a fail-closed rebuild requirement.
    fn status<'a>(&'a self, scope: &'a IndexScope)
    -> BoxFuture<'a, Result<SyncStatus, IndexError>>;

    fn set_status<'a>(&'a self, status: SyncStatus) -> BoxFuture<'a, Result<(), IndexError>>;

    /// Atomically records and applies one explicit confirmation/retention
    /// policy migration. Existing canonical projections remain unpublished to
    /// semantic readers until a staged rebuild under the target policy is
    /// activated.
    fn migrate_policy<'a>(
        &'a self,
        command: MigrateIndexPolicyCommand,
    ) -> BoxFuture<'a, Result<MigrateIndexPolicyOutcome, IndexError>>;

    fn rebuild_state<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<RebuildState>, IndexError>>;

    fn begin_rebuild<'a>(
        &'a self,
        command: BeginRebuildCommand,
    ) -> BoxFuture<'a, Result<RebuildState, IndexError>>;

    fn commit_rebuild_block<'a>(
        &'a self,
        command: CommitRebuildBlockCommand<Self::Undo>,
    ) -> BoxFuture<'a, Result<CommitBlockOutcome, IndexError>>;

    /// Stops block ingestion for the staged generation after its checkpoint
    /// has been independently validated. Retrying the same transition is
    /// idempotent.
    fn validate_rebuild<'a>(
        &'a self,
        command: ValidateRebuildCommand,
    ) -> BoxFuture<'a, Result<RebuildState, IndexError>>;

    /// Materializes correction projections and events inside the hidden
    /// generation, then durably marks it ready for atomic publication.
    /// Retrying after an unknown acknowledgement is idempotent.
    fn prepare_rebuild_activation<'a>(
        &'a self,
        command: PrepareRebuildActivationCommand,
    ) -> BoxFuture<'a, Result<RebuildState, IndexError>>;

    /// Atomically publishes the prepared generation, checkpoint, and event
    /// high-water mark. No partially built generation may become queryable.
    fn activate_rebuild<'a>(
        &'a self,
        command: ActivateRebuildCommand,
    ) -> BoxFuture<'a, Result<(), IndexError>>;

    fn abort_rebuild<'a>(
        &'a self,
        command: AbortRebuildCommand,
    ) -> BoxFuture<'a, Result<(), IndexError>>;

    /// Removes only inactive generation-prefixed projection, canonical, raw,
    /// undo, and confirmation state. Published event and revision journals are
    /// never cleanup targets.
    fn cleanup_generation<'a>(
        &'a self,
        command: CleanupGenerationCommand,
    ) -> BoxFuture<'a, Result<CleanupGenerationOutcome, IndexError>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitBlockOutcome {
    Applied,
    AlreadyApplied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertTipCommand {
    pub scope: IndexScope,
    pub expected_tip: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevertTipOutcome {
    Reverted { checkpoint: Option<BlockRef> },
    AlreadyReverted { checkpoint: Option<BlockRef> },
}
