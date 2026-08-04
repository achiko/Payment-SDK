use crate::{
    AddressWatchRequest, BlockHeight, BlockRef, BoxFuture, ConfirmationPolicy, IndexError,
    IndexScope, ObservationEventPage, ObservationEventRequest, ObservedTransaction,
    TransactionPage, TransactionPageRequest, TransactionRequest, UnwatchCommand, UnwatchOutcome,
    WatchReceipt, WatchRequest,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncRequest {
    pub scope: IndexScope,
    /// `None` means follow the source's observed tip.
    pub through: Option<BlockHeight>,
    /// Allows a worker to bound one invocation and yield fairly.
    pub max_blocks: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPhase {
    Starting,
    Reconciling,
    CatchingUp,
    Ready,
    Reverting,
    Replaying,
    RebuildRequired,
    Halted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildReason {
    pub checkpoint: BlockRef,
    pub oldest_retained: BlockHeight,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    pub scope: IndexScope,
    pub checkpoint: Option<BlockRef>,
    pub observed_tip: Option<BlockRef>,
    pub confirmation_policy: ConfirmationPolicy,
    pub phase: SyncPhase,
    pub rebuild_reason: Option<RebuildReason>,
    pub halted_reason: Option<String>,
}

impl SyncStatus {
    #[must_use]
    pub fn starting(scope: IndexScope, confirmation_policy: ConfirmationPolicy) -> Self {
        Self {
            scope,
            checkpoint: None,
            observed_tip: None,
            confirmation_policy,
            phase: SyncPhase::Starting,
            rebuild_reason: None,
            halted_reason: None,
        }
    }
}

/// Internal reorg-safe synchronization loop. It is intentionally separate from
/// the public observation registration/query surface.
pub trait IndexingWorker: Send + Sync {
    fn sync<'a>(&'a self, request: SyncRequest) -> BoxFuture<'a, Result<SyncStatus, IndexError>>;

    fn status<'a>(&'a self, scope: &'a IndexScope)
    -> BoxFuture<'a, Result<SyncStatus, IndexError>>;
}

/// Public IX command surface. Transport (in-process, HTTP, queue) is an adapter choice.
pub trait ObservationRegistry: Send + Sync {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>>;

    fn unwatch<'a>(
        &'a self,
        command: UnwatchCommand,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>>;
}

pub trait ObservationQuery: Send + Sync {
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
}

/// Durable at-least-once event feed. Push transports acknowledge a cursor only
/// after the consumer has durably mirrored the event.
pub trait ObservationEventSource: Send + Sync {
    fn events<'a>(
        &'a self,
        request: ObservationEventRequest,
    ) -> BoxFuture<'a, Result<ObservationEventPage, IndexError>>;
}
