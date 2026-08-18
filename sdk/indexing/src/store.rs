use crate::{
    AddressQuery, BlockHeight, BlockRef, BoxFuture, CommitBlock, DeactivateWatch, EventCursor,
    EventPage, EventQuery, HistoryQuery, IndexError, IndexScope, ObservationDraft,
    ObservedTransaction, RegisterWatch, SyncStatus, TransactionPage, TransactionQuery,
    UnwatchOutcome, WatchBackfill, WatchId, WatchOutcome, WatchReceipt, WatchSnapshot,
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
pub struct BeginRebuild {
    pub scope: IndexScope,
    pub bootstrap_height: BlockHeight,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildBlock<E, U> {
    pub generation: RebuildGeneration,
    pub command: CommitBlock<E, U>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildActivation {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
    pub expected_checkpoint: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildValidation {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
    pub expected_checkpoint: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareActivation {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
    pub expected_checkpoint: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbortRebuild {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupGeneration {
    pub scope: IndexScope,
    pub generation: RebuildGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupOutcome {
    Removed { records: u64 },
    AlreadyAbsent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitBackfill {
    pub scope: IndexScope,
    pub watch_id: WatchId,
    pub expected_next_height: BlockHeight,
    pub expected_checkpoint: BlockRef,
    pub block: BlockRef,
    pub drafts: Vec<ObservationDraft>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackfillOutcome {
    Applied { next_height: Option<BlockHeight> },
    AlreadyApplied { next_height: Option<BlockHeight> },
}

/// Composite semantic repository for one Indexer Service database.
///
/// Implementations must commit a block's raw payload, undo bundle, opaque
/// chain projection, current observations, immutable revisions, feed events,
/// confirmation transitions, checkpoint, and retention pruning atomically.
/// Revision and cursor allocation therefore never occurs in the interpreter
/// or worker.
pub trait IndexTypes: Send + Sync {
    type Target: Clone + Send + Sync + 'static;
    type Effect: Clone + Send + Sync + 'static;
    type Undo: Clone + Send + Sync + 'static;
}

pub trait CanonicalReader: IndexTypes {
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
}

pub trait WatchReader: IndexTypes {
    fn watches_at<'a>(
        &'a self,
        scope: &'a IndexScope,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<WatchSnapshot<Self::Target>, IndexError>>;
}

pub trait BackfillReader: IndexTypes {
    /// Lists durable historical work created by watches registered behind the
    /// current canonical checkpoint.
    fn pending_watch_backfills<'a>(
        &'a self,
        scope: &'a IndexScope,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<WatchBackfill>, IndexError>>;
}

pub trait BackfillWriter: IndexTypes {
    /// Applies one canonical historical height for one watch and advances its
    /// durable cursor without moving the live canonical checkpoint. Empty
    /// historical blocks must still be committed.
    fn commit_watch_backfill<'a>(
        &'a self,
        command: CommitBackfill,
    ) -> BoxFuture<'a, Result<BackfillOutcome, IndexError>>;

    /// Commits order-independent chain projection facts discovered by one
    /// historical watch alongside the ordinary observation backfill.
    ///
    /// The default preserves repositories that do not materialize a chain
    /// projection. Persistent repositories override this method so newly
    /// discovered projection keys and their retained rollback data share the
    /// same atomic commit as the backfill cursor and observation revisions.
    fn commit_watch_backfill_effect<'a>(
        &'a self,
        command: CommitBackfill,
        effect: Self::Effect,
    ) -> BoxFuture<'a, Result<BackfillOutcome, IndexError>>;
}

pub trait WatchStore: IndexTypes {
    /// `(scope, idempotency_key)` is unique. The same payload returns
    /// `Existing`; a changed payload returns `IndexErrorKind::Conflict`.
    fn register_watch<'a>(
        &'a self,
        command: RegisterWatch<Self::Target>,
    ) -> BoxFuture<'a, Result<WatchOutcome, IndexError>>;
    /// Soft-deactivates the watch while retaining its activation history.
    fn deactivate<'a>(
        &'a self,
        command: DeactivateWatch,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>>;
}

pub trait ChainWriter: IndexTypes {
    /// A retry after an unknown acknowledgement must return `AlreadyApplied`
    /// without allocating another revision or cursor.
    fn commit_block<'a>(
        &'a self,
        command: CommitBlock<Self::Effect, Self::Undo>,
    ) -> BoxFuture<'a, Result<BlockOutcome, IndexError>>;

    /// Reverts exactly one expected canonical tip. The undo, new observation
    /// revisions, correction events, and checkpoint movement are one commit.
    fn revert_tip<'a>(
        &'a self,
        command: RevertTip,
    ) -> BoxFuture<'a, Result<RevertOutcome, IndexError>>;
}

pub trait TransactionReader: IndexTypes {
    fn transaction<'a>(
        &'a self,
        request: TransactionQuery,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>>;

    fn transactions_by_address<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;
}

pub trait WatchLookup: IndexTypes {
    fn watches_for_address<'a>(
        &'a self,
        request: AddressQuery,
    ) -> BoxFuture<'a, Result<Vec<WatchReceipt>, IndexError>>;
}

pub trait EventReader: IndexTypes {
    fn events<'a>(&'a self, request: EventQuery) -> BoxFuture<'a, Result<EventPage, IndexError>>;

    /// Returns the last published event cursor without allocating a cursor or
    /// exposing hidden staged-rebuild events.
    fn event_high_water<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<EventCursor>, IndexError>>;
}

pub trait StatusStore: IndexTypes {
    /// Operational phase changes are durable so restart can resume a revert or
    /// expose a fail-closed rebuild requirement.
    fn status<'a>(&'a self, scope: &'a IndexScope)
    -> BoxFuture<'a, Result<SyncStatus, IndexError>>;

    fn set_status<'a>(&'a self, status: SyncStatus) -> BoxFuture<'a, Result<(), IndexError>>;
}

pub trait RebuildReader: IndexTypes {
    fn rebuild_state<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<RebuildState>, IndexError>>;
}

pub trait RebuildBuilder: IndexTypes {
    fn begin_rebuild<'a>(
        &'a self,
        command: BeginRebuild,
    ) -> BoxFuture<'a, Result<RebuildState, IndexError>>;

    fn commit_rebuild_block<'a>(
        &'a self,
        command: RebuildBlock<Self::Effect, Self::Undo>,
    ) -> BoxFuture<'a, Result<BlockOutcome, IndexError>>;

    /// Stops block ingestion for the staged generation after its checkpoint
    /// has been independently validated. Retrying the same transition is
    /// idempotent.
    fn validate_rebuild<'a>(
        &'a self,
        command: RebuildValidation,
    ) -> BoxFuture<'a, Result<RebuildState, IndexError>>;
}

pub trait RebuildPublisher: IndexTypes {
    /// Materializes correction projections and events inside the hidden
    /// generation, then durably marks it ready for atomic publication.
    /// Retrying after an unknown acknowledgement is idempotent.
    fn prepare_rebuild_activation<'a>(
        &'a self,
        command: PrepareActivation,
    ) -> BoxFuture<'a, Result<RebuildState, IndexError>>;

    /// Atomically publishes the prepared generation, checkpoint, and event
    /// high-water mark. No partially built generation may become queryable.
    fn activate_rebuild<'a>(
        &'a self,
        command: RebuildActivation,
    ) -> BoxFuture<'a, Result<(), IndexError>>;
}

pub trait RebuildAdmin: IndexTypes {
    fn abort_rebuild<'a>(&'a self, command: AbortRebuild) -> BoxFuture<'a, Result<(), IndexError>>;

    /// Removes only inactive generation-prefixed projection, canonical, raw,
    /// undo, and confirmation state. Published event and revision journals are
    /// never cleanup targets.
    fn cleanup_generation<'a>(
        &'a self,
        command: CleanupGeneration,
    ) -> BoxFuture<'a, Result<CleanupOutcome, IndexError>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockOutcome {
    Applied,
    AlreadyApplied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertTip {
    pub scope: IndexScope,
    pub expected_tip: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevertOutcome {
    Reverted { checkpoint: Option<BlockRef> },
    AlreadyReverted { checkpoint: Option<BlockRef> },
}
