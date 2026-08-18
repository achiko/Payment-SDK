use crate::{
    BlockRef, BoxFuture, CanonicalStore, HistoryQuery, HistoryStore, IndexError, IndexErrorKind,
    IndexScope, ObservedTransaction, RegisterWatch, TransactionPage, TransactionQuery,
    WatchReceipt, WatchRequest, WatchStore,
};

#[derive(Clone)]
pub struct Index<R>(R);

impl<R> Index<R> {
    #[must_use]
    pub const fn new(repository: R) -> Self {
        Self(repository)
    }

    #[must_use]
    pub const fn repository(&self) -> &R {
        &self.0
    }
}

impl<R> Checkpoint for Index<R>
where
    R: CanonicalStore,
{
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.0.checkpoint(scope)
    }
}

impl<R> Watcher for Index<R>
where
    R: CanonicalStore + WatchStore,
{
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            for attempt in 0..8 {
                let registered_at = self.0.checkpoint(&request.scope).await?;
                let command = RegisterWatch {
                    request: request.clone(),
                    target: request.selector.clone(),
                    registered_at,
                };
                let result: Result<WatchReceipt, IndexError> = async {
                    let context = self.0.load_watch(&command).await?;
                    let decision = crate::plan_watch(&command, &context)?;
                    if let Some(plan) = decision.plan {
                        self.0.save_watch(plan).await?;
                    }
                    Ok(decision.receipt)
                }
                .await;
                match result {
                    Ok(value) => return Ok(value),
                    Err(error)
                        if error.kind == IndexErrorKind::Conflict
                            && error.retryable
                            && attempt < 7 => {}
                    Err(error) => return Err(error),
                }
            }
            unreachable!("bounded registration loop returns")
        })
    }
}

impl<R> History for Index<R>
where
    R: HistoryStore,
{
    fn transaction<'a>(
        &'a self,
        request: TransactionQuery,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        self.0.transaction(request)
    }

    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        self.0.transactions_by_address(request)
    }
}

/// Reads the current canonical indexing boundary for one chain and network.
pub trait Checkpoint: Send + Sync {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;
}

/// Registers a durable address watch.
pub trait Watcher: Send + Sync {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>>;
}

/// Reads normalized transaction facts without exposing storage mechanics.
pub trait History: Send + Sync {
    fn transaction<'a>(
        &'a self,
        request: TransactionQuery,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>>;

    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;
}
