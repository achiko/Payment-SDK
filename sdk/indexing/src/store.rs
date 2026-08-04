use crate::{
    BlockChanges, BlockHeight, BlockRef, BoxFuture, FinalityScanPage, FinalityScanRequest,
    IndexError, IndexScope, ObservationEvent, ObservationEventPage, ObservationEventRequest,
    ObservedTransaction, TransactionPage, TransactionPageRequest, WatchId, WatchTarget,
};
use chain_identity::CanonicalTransactionId;
use storage::Storage;

/// Semantic indexing operations implemented on top of a generic storage backend.
pub trait IndexStore<E, U>: Storage {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;

    /// Persists events, undo information, and the new checkpoint atomically.
    fn commit_block<'a>(
        &'a self,
        scope: IndexScope,
        changes: BlockChanges<E, U>,
    ) -> BoxFuture<'a, Result<(), IndexError>>;

    /// Reverts exactly the expected current tip and restores the previous checkpoint atomically.
    fn revert_tip<'a>(
        &'a self,
        scope: IndexScope,
        expected_tip: BlockRef,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;
}

pub trait WatchStore<T>: Storage {
    fn register_watch<'a>(&'a self, watch: WatchTarget<T>)
    -> BoxFuture<'a, Result<(), IndexError>>;

    fn remove_watch<'a>(&'a self, watch: WatchId) -> BoxFuture<'a, Result<(), IndexError>>;

    fn watches_at<'a>(
        &'a self,
        scope: &'a IndexScope,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Vec<WatchTarget<T>>, IndexError>>;
}

/// IX-owned persistence for normalized facts and its replayable event feed.
/// Recording a state transition and appending its event must be one atomic commit.
pub trait ObservationStore: Storage {
    fn record_transition<'a>(
        &'a self,
        event: ObservationEvent,
    ) -> BoxFuture<'a, Result<(), IndexError>>;

    fn transaction<'a>(
        &'a self,
        transaction_id: &'a CanonicalTransactionId,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>>;

    fn transactions_by_address<'a>(
        &'a self,
        request: TransactionPageRequest,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;

    fn awaiting_confirmation<'a>(
        &'a self,
        request: FinalityScanRequest,
    ) -> BoxFuture<'a, Result<FinalityScanPage, IndexError>>;

    fn events<'a>(
        &'a self,
        request: ObservationEventRequest,
    ) -> BoxFuture<'a, Result<ObservationEventPage, IndexError>>;
}
