use indexing::{
    BlockHeight, BlockRef, CanonicalReader, Checkpoint, DeactivateWatch, EventPage, EventQuery,
    EventReader, History, HistoryQuery, IndexChanges, IndexError, IndexUndo, ObservedTransaction,
    Observer, RegisterWatch, TransactionPage, TransactionQuery, TransactionReader, UnwatchOutcome,
    UnwatchRequest, WatchOutcome, WatchReceipt, WatchRequest, WatchSelector, WatchStore, Watcher,
};
use storage::Store;

use crate::{RecordCodec, Repository};

impl<S, C> Checkpoint for Repository<S, C>
where
    S: Store,
    C: RecordCodec<Target = WatchSelector, Effect = IndexChanges, Undo = IndexUndo>,
{
    fn checkpoint<'a>(
        &'a self,
        scope: &'a indexing::IndexScope,
    ) -> indexing::BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        CanonicalReader::checkpoint(self, scope)
    }
}

impl<S, C> Watcher for Repository<S, C>
where
    S: Store,
    C: RecordCodec<Target = WatchSelector, Effect = IndexChanges, Undo = IndexUndo>,
{
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> indexing::BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            let registered_at = CanonicalReader::checkpoint(self, &request.scope).await?;
            let target = request.selector.clone();
            let outcome = WatchStore::register_watch(
                self,
                RegisterWatch {
                    request,
                    target,
                    registered_at,
                },
            )
            .await?;
            Ok(match outcome {
                WatchOutcome::Registered(receipt) | WatchOutcome::Existing(receipt) => receipt,
            })
        })
    }

    fn unwatch<'a>(
        &'a self,
        request: UnwatchRequest,
    ) -> indexing::BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Box::pin(async move {
            let expected_checkpoint = CanonicalReader::checkpoint(self, &request.scope).await?;
            let inactive_from = expected_checkpoint.as_ref().map_or(
                Ok(BlockHeight(self.config().bootstrap_height.0)),
                |checkpoint| {
                    checkpoint
                        .height
                        .0
                        .checked_add(1)
                        .map(BlockHeight)
                        .ok_or_else(|| {
                            IndexError::new(
                                indexing::IndexErrorKind::Conflict,
                                "watch cannot be deactivated after the maximum block height",
                                false,
                            )
                        })
                },
            )?;
            WatchStore::deactivate(
                self,
                DeactivateWatch {
                    scope: request.scope,
                    watch_id: request.watch_id,
                    inactive_from,
                    expected_checkpoint,
                },
            )
            .await
        })
    }
}

impl<S, C> History for Repository<S, C>
where
    S: Store,
    C: RecordCodec<Target = WatchSelector, Effect = IndexChanges, Undo = IndexUndo>,
{
    fn transaction<'a>(
        &'a self,
        request: TransactionQuery,
    ) -> indexing::BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        TransactionReader::transaction(self, request)
    }

    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> indexing::BoxFuture<'a, Result<TransactionPage, IndexError>> {
        TransactionReader::transactions_by_address(self, request)
    }
}

impl<S, C> Observer for Repository<S, C>
where
    S: Store,
    C: RecordCodec<Target = WatchSelector, Effect = IndexChanges, Undo = IndexUndo>,
{
    fn events<'a>(
        &'a self,
        request: EventQuery,
    ) -> indexing::BoxFuture<'a, Result<EventPage, IndexError>> {
        EventReader::events(self, request)
    }
}
