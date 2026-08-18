use std::{collections::BTreeMap, sync::Arc};

use crate::{
    BlockRef, BoxFuture, EventPage, EventQuery, HistoryQuery, IndexError, IndexErrorKind,
    IndexScope, ObservedTransaction, TransactionPage, TransactionQuery, UnwatchOutcome,
    UnwatchRequest, WatchReceipt, WatchRequest,
};

/// Reads the current canonical indexing boundary for a chain and network.
///
/// Callers use this boundary as the birthday for watches created before an
/// external action, so the action cannot race ahead of observation.
pub trait Checkpoint: Send + Sync {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;
}

/// Registers and removes durable transaction watches.
pub trait Watcher: Send + Sync {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>>;

    fn unwatch<'a>(
        &'a self,
        request: UnwatchRequest,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>>;
}

/// Reads normalized transaction facts without exposing synchronization or storage.
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

/// Reads the durable, cursor-based transaction change feed.
pub trait Observer: Send + Sync {
    fn events<'a>(&'a self, request: EventQuery) -> BoxFuture<'a, Result<EventPage, IndexError>>;
}

/// Complete consumer-facing indexing API after application composition.
pub trait Indexer: Checkpoint + Watcher + History + Observer {}

impl<T> Indexer for T where T: Checkpoint + Watcher + History + Observer {}

/// Routes independently owned chain/network indexers by exact scope.
#[derive(Default)]
pub struct Composer {
    children: BTreeMap<IndexScope, Arc<dyn Indexer>>,
}

impl Composer {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            children: BTreeMap::new(),
        }
    }

    pub fn with(
        mut self,
        scope: IndexScope,
        indexer: impl Indexer + 'static,
    ) -> Result<Self, IndexError> {
        self.add(scope, indexer)?;
        Ok(self)
    }

    pub fn add(
        &mut self,
        scope: IndexScope,
        indexer: impl Indexer + 'static,
    ) -> Result<(), IndexError> {
        use std::collections::btree_map::Entry;

        match self.children.entry(scope) {
            Entry::Vacant(entry) => {
                entry.insert(Arc::new(indexer));
                Ok(())
            }
            Entry::Occupied(entry) => Err(IndexError::new(
                IndexErrorKind::Conflict,
                format!(
                    "an indexer is already registered for scope {:?}",
                    entry.key()
                ),
                false,
            )),
        }
    }

    fn child(&self, scope: &IndexScope) -> Result<&Arc<dyn Indexer>, IndexError> {
        self.children.get(scope).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "no indexer is registered for the requested scope",
                false,
            )
        })
    }

    fn validate_selector(
        scope: &IndexScope,
        selector: &crate::WatchSelector,
    ) -> Result<(), IndexError> {
        let matches = match selector {
            crate::WatchSelector::Address(address) => address.belongs_to(scope),
            crate::WatchSelector::Transaction(transaction) => transaction.belongs_to(scope),
        };
        if matches {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "watch identity does not belong to the requested scope",
                false,
            ))
        }
    }
}

impl Watcher for Composer {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            Self::validate_selector(&request.scope, &request.selector)?;
            self.child(&request.scope)?.watch(request).await
        })
    }

    fn unwatch<'a>(
        &'a self,
        request: UnwatchRequest,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Box::pin(async move { self.child(&request.scope)?.unwatch(request).await })
    }
}

impl Checkpoint for Composer {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { self.child(scope)?.checkpoint(scope).await })
    }
}

impl History for Composer {
    fn transaction<'a>(
        &'a self,
        request: TransactionQuery,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        Box::pin(async move {
            if !request.transaction_id.belongs_to(&request.scope) {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "transaction identity does not belong to the requested scope",
                    false,
                ));
            }
            self.child(&request.scope)?.transaction(request).await
        })
    }

    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        Box::pin(async move {
            if !request.address.belongs_to(&request.scope)
                || request
                    .after
                    .as_ref()
                    .is_some_and(|transaction| !transaction.belongs_to(&request.scope))
            {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "history identity does not belong to the requested scope",
                    false,
                ));
            }
            self.child(&request.scope)?.history(request).await
        })
    }
}

impl Observer for Composer {
    fn events<'a>(&'a self, request: EventQuery) -> BoxFuture<'a, Result<EventPage, IndexError>> {
        Box::pin(async move { self.child(&request.scope)?.events(request).await })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_executor::block_on;

    use super::*;
    use crate::{CanonicalAddress, ChainId, EventCursor, EventQuery, HistoryQuery};

    struct FixtureIndexer(Arc<AtomicUsize>);

    impl Checkpoint for FixtureIndexer {
        fn checkpoint<'a>(
            &'a self,
            _scope: &'a IndexScope,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl Watcher for FixtureIndexer {
        fn watch<'a>(
            &'a self,
            _request: WatchRequest,
        ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
            Box::pin(async { unreachable!("fixture watch must not run") })
        }

        fn unwatch<'a>(
            &'a self,
            _request: UnwatchRequest,
        ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
            Box::pin(async { unreachable!("fixture unwatch must not run") })
        }
    }

    impl History for FixtureIndexer {
        fn transaction<'a>(
            &'a self,
            _request: TransactionQuery,
        ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
            Box::pin(async { Ok(None) })
        }

        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(TransactionPage {
                    transactions: Vec::new(),
                    next: None,
                })
            })
        }
    }

    impl Observer for FixtureIndexer {
        fn events<'a>(
            &'a self,
            _request: EventQuery,
        ) -> BoxFuture<'a, Result<EventPage, IndexError>> {
            Box::pin(async {
                Ok(EventPage {
                    events: Vec::new(),
                    next: Some(EventCursor(0)),
                })
            })
        }
    }

    fn scope(network: &str) -> IndexScope {
        IndexScope {
            chain: ChainId("fixture".to_owned()),
            network: network.to_owned(),
        }
    }

    fn history(scope: IndexScope) -> HistoryQuery {
        HistoryQuery {
            address: CanonicalAddress {
                scope: scope.clone(),
                value: "address".to_owned(),
            },
            scope,
            after: None,
            limit: 10,
        }
    }

    #[test]
    fn routes_history_by_exact_scope() {
        let local_calls = Arc::new(AtomicUsize::new(0));
        let remote_calls = Arc::new(AtomicUsize::new(0));
        let composer = Composer::new()
            .with(scope("local"), FixtureIndexer(local_calls.clone()))
            .expect("local scope is unique")
            .with(scope("remote"), FixtureIndexer(remote_calls.clone()))
            .expect("remote scope is unique");

        let result = block_on(composer.history(history(scope("local"))))
            .expect("registered scope must route");

        assert!(result.transactions.is_empty());
        assert_eq!(local_calls.load(Ordering::SeqCst), 1);
        assert_eq!(remote_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn rejects_history_identity_from_another_network_before_routing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let composer = Composer::new()
            .with(scope("mainnet"), FixtureIndexer(calls.clone()))
            .expect("scope is unique");
        let mut request = history(scope("mainnet"));
        request.address.scope = scope("testnet");

        let error = block_on(composer.history(request)).expect_err("scope mismatch must fail");

        assert_eq!(error.kind, IndexErrorKind::ScopeMismatch);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn rejects_a_duplicate_scope_without_replacing_the_child() {
        let original_calls = Arc::new(AtomicUsize::new(0));
        let replacement_calls = Arc::new(AtomicUsize::new(0));
        let mut composer = Composer::new();
        composer
            .add(scope("local"), FixtureIndexer(original_calls.clone()))
            .expect("first registration succeeds");

        let error = composer
            .add(scope("local"), FixtureIndexer(replacement_calls.clone()))
            .expect_err("duplicate registration must fail");
        assert_eq!(error.kind, IndexErrorKind::Conflict);

        block_on(composer.history(history(scope("local"))))
            .expect("the original child remains registered");
        assert_eq!(original_calls.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn rejects_an_unregistered_scope() {
        let composer = Composer::new();
        let error = block_on(composer.history(history(scope("missing"))))
            .expect_err("unregistered scope must fail");

        assert_eq!(error.kind, IndexErrorKind::ScopeMismatch);
    }
}
