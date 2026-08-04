use crate::{
    BlockHeight, BlockRef, BoxFuture, ConfirmationPolicy, IndexError, IndexScope,
    ObservationEventPage, ObservationEventRequest, ObservedTransaction, TransactionPage,
    TransactionPageRequest, WatchId, WatchReceipt, WatchRequest,
};
use chain_identity::{CanonicalAddress, CanonicalTransactionId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncRequest {
    pub scope: IndexScope,
    /// `None` means follow the source's observed tip.
    pub through: Option<BlockHeight>,
    /// Allows a worker to bound one invocation and yield fairly.
    pub max_blocks: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    pub scope: IndexScope,
    pub checkpoint: Option<BlockRef>,
    pub observed_tip: Option<BlockRef>,
    pub confirmation_policy: ConfirmationPolicy,
    pub running: bool,
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

    fn unwatch<'a>(&'a self, watch_id: &'a WatchId) -> BoxFuture<'a, Result<(), IndexError>>;
}

pub trait ObservationQuery: Send + Sync {
    fn transaction<'a>(
        &'a self,
        transaction_id: &'a CanonicalTransactionId,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>>;

    fn transactions_by_address<'a>(
        &'a self,
        request: TransactionPageRequest,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;

    fn watches_for_address<'a>(
        &'a self,
        address: &'a CanonicalAddress,
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
