//! Keeps indexers current until shutdown or a terminal failure.
//!
//! `sdk/indexing` deliberately owns no async runtime, so the loop that
//! actually drives `Indexer::sync` lives here instead. Any binary embedding
//! the SDK can reuse it rather than reimplementing the readiness and
//! shutdown semantics.

use std::{error::Error, io, sync::Arc, time::Duration};

use indexing::{FilterSource, Indexer, SyncPhase};
use tokio::sync::watch;

pub type TaskError = Box<dyn Error + Send + Sync>;

/// What synchronization is currently doing.
///
/// Published on every pass. `Retrying` carries the reason: a transient source
/// or store failure is retried indefinitely, and without the message a wedged
/// indexer is indistinguishable from an idle one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncState {
    /// Behind the source tip and still applying blocks.
    CatchingUp,
    /// Every configured scope has reached the source tip.
    Ready,
    /// The last pass failed with a retryable error and will be attempted again.
    Retrying { error: String },
}

/// Keeps the composed index current until shutdown or a terminal failure.
pub async fn run(
    indexer: Arc<dyn Indexer>,
    selection: Arc<dyn FilterSource>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    state: watch::Sender<SyncState>,
) -> Result<(), TaskError> {
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }

        let result = tokio::select! {
            result = indexer.sync(selection.as_ref()) => result,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
        };

        let wait = match result {
            Ok(statuses) => {
                let complete = !indexer.scopes().is_empty()
                    && statuses.len() == indexer.scopes().len()
                    && indexer.scopes().iter().all(|scope| {
                        statuses
                            .iter()
                            .filter(|status| status.scope == *scope)
                            .count()
                            == 1
                    });
                if !complete
                    || statuses.iter().any(|status| {
                        status.phase == SyncPhase::Ready && status.checkpoint.is_none()
                    })
                {
                    state.send_replace(SyncState::Retrying {
                        error: "incomplete synchronization status".to_owned(),
                    });
                    return Err(io::Error::other(
                        "indexer returned incomplete or inconsistent synchronization status",
                    )
                    .into());
                }
                let caught_up = statuses
                    .iter()
                    .all(|status| status.phase == SyncPhase::Ready);
                let next = if caught_up {
                    SyncState::Ready
                } else {
                    SyncState::CatchingUp
                };
                state.send_if_modified(|current| {
                    let changed = *current != next;
                    *current = next;
                    changed
                });
                caught_up
            }
            Err(error) if error.retryable => {
                // The reason must reach the caller: this loop will keep going
                // forever, and silence here is indistinguishable from health.
                let next = SyncState::Retrying {
                    error: error.message.clone(),
                };
                state.send_if_modified(|current| {
                    let changed = *current != next;
                    *current = next;
                    changed
                });
                true
            }
            Err(error) => {
                state.send_replace(SyncState::Retrying {
                    error: error.message.clone(),
                });
                return Err(error.into());
            }
        };

        if wait {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                () = tokio::time::sleep(interval) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use indexing::{
        BlockHash, BlockHeight, BlockPosition, BlockRef, BoxFuture, ChainId, Checkpoint, History,
        HistoryQuery, IndexError, IndexErrorKind, IndexScope, SyncStatus, TransactionPage,
    };

    use super::*;

    struct Index {
        scope: IndexScope,
        results: Mutex<VecDeque<Result<Vec<SyncStatus>, IndexError>>>,
        pending: bool,
        calls: AtomicUsize,
        continue_after_first: Option<Arc<tokio::sync::Notify>>,
    }

    impl Checkpoint for Index {
        fn checkpoint<'a>(
            &'a self,
            _scope: &'a IndexScope,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl History for Index {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async {
                Ok(TransactionPage {
                    checkpoint: None,
                    transactions: Vec::new(),
                    next: None,
                })
            })
        }
    }

    impl Indexer for Index {
        fn scopes(&self) -> &[IndexScope] {
            std::slice::from_ref(&self.scope)
        }

        fn sync<'a>(
            &'a self,
            _selection: &'a dyn FilterSource,
        ) -> BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>> {
            Box::pin(async move {
                if self.pending {
                    future::pending().await
                } else {
                    let call = self.calls.fetch_add(1, Ordering::Relaxed);
                    if call == 1
                        && let Some(gate) = &self.continue_after_first
                    {
                        gate.notified().await;
                    }
                    self.results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pop_front()
                        .unwrap_or_else(|| Ok(vec![status(&self.scope, SyncPhase::Ready)]))
                }
            })
        }
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("fixture".to_owned()),
            network: "owned".to_owned(),
        }
    }

    fn block() -> BlockRef {
        BlockRef {
            position: BlockPosition(4),
            height: BlockHeight(4),
            hash: BlockHash(vec![4; 32]),
            parent: None,
            timestamp: None,
        }
    }

    fn status(scope: &IndexScope, phase: SyncPhase) -> SyncStatus {
        SyncStatus {
            scope: scope.clone(),
            checkpoint: Some(block()),
            observed_tip: Some(block()),
            phase,
        }
    }

    fn index(results: impl IntoIterator<Item = Result<Vec<SyncStatus>, IndexError>>) -> Arc<Index> {
        Arc::new(Index {
            scope: scope(),
            results: Mutex::new(results.into_iter().collect()),
            pending: false,
            calls: AtomicUsize::new(0),
            continue_after_first: None,
        })
    }

    async fn next(state: &mut watch::Receiver<SyncState>) -> SyncState {
        state.changed().await.expect("runtime state");
        state.borrow_and_update().clone()
    }

    #[tokio::test]
    async fn publishes_catch_up_then_ready_with_a_persisted_checkpoint() {
        let scope = scope();
        let gate = Arc::new(tokio::sync::Notify::new());
        let mut index = index([
            Ok(vec![status(&scope, SyncPhase::CatchingUp)]),
            Ok(vec![status(&scope, SyncPhase::Ready)]),
        ]);
        Arc::get_mut(&mut index)
            .expect("unshared fixture")
            .continue_after_first = Some(Arc::clone(&gate));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (state, mut state_rx) = watch::channel(SyncState::Retrying {
            error: "initial".to_owned(),
        });
        let task = tokio::spawn(run(
            index,
            Arc::new(Vec::new()),
            Duration::from_millis(1),
            shutdown_rx,
            state,
        ));

        assert_eq!(next(&mut state_rx).await, SyncState::CatchingUp);
        gate.notify_one();
        assert_eq!(next(&mut state_rx).await, SyncState::Ready);
        shutdown.send_replace(true);
        task.await.expect("runtime task").expect("clean shutdown");
    }

    #[tokio::test]
    async fn retryable_failure_recovers_without_terminating() {
        let scope = scope();
        let index = index([
            Err(IndexError::new(IndexErrorKind::Source, "offline", true)),
            Ok(vec![status(&scope, SyncPhase::Ready)]),
        ]);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (state, mut state_rx) = watch::channel(SyncState::CatchingUp);
        let task = tokio::spawn(run(
            index,
            Arc::new(Vec::new()),
            Duration::from_millis(1),
            shutdown_rx,
            state,
        ));

        assert_eq!(
            next(&mut state_rx).await,
            SyncState::Retrying {
                error: "offline".to_owned()
            }
        );
        assert_eq!(next(&mut state_rx).await, SyncState::Ready);
        shutdown.send_replace(true);
        task.await.expect("runtime task").expect("clean shutdown");
    }

    #[tokio::test]
    async fn fatal_failure_is_returned_and_published_not_ready() {
        let index = index([Err(IndexError::new(
            IndexErrorKind::InvalidBlock,
            "fatal",
            false,
        ))]);
        let (_shutdown, shutdown_rx) = watch::channel(false);
        let (state, mut state_rx) = watch::channel(SyncState::Ready);

        let error = run(
            index,
            Arc::new(Vec::new()),
            Duration::from_millis(1),
            shutdown_rx,
            state,
        )
        .await
        .expect_err("fatal result");
        assert_eq!(error.to_string(), "fatal");
        assert_eq!(
            state_rx.borrow_and_update().clone(),
            SyncState::Retrying {
                error: "fatal".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_an_inflight_sync() {
        let index = Arc::new(Index {
            scope: scope(),
            results: Mutex::new(VecDeque::new()),
            pending: true,
            calls: AtomicUsize::new(0),
            continue_after_first: None,
        });
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (state, _state_rx) = watch::channel(SyncState::CatchingUp);
        let task = tokio::spawn(run(
            index,
            Arc::new(Vec::new()),
            Duration::from_secs(1),
            shutdown_rx,
            state,
        ));
        tokio::task::yield_now().await;

        shutdown.send_replace(true);
        task.await.expect("runtime task").expect("clean shutdown");
    }

    #[tokio::test]
    async fn ready_without_a_checkpoint_is_a_fatal_contract_violation() {
        let scope = scope();
        let index = index([Ok(vec![SyncStatus {
            scope,
            checkpoint: None,
            observed_tip: Some(block()),
            phase: SyncPhase::Ready,
        }])]);
        let (_shutdown, shutdown_rx) = watch::channel(false);
        let (state, _state_rx) = watch::channel(SyncState::CatchingUp);

        let error = run(
            index,
            Arc::new(Vec::new()),
            Duration::from_millis(1),
            shutdown_rx,
            state,
        )
        .await
        .expect_err("missing persisted checkpoint");
        assert!(error.to_string().contains("incomplete or inconsistent"));
    }
}
