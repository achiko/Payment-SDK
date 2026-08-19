use std::{error::Error, io, sync::Arc, time::Duration};

use indexing::{AddressFilter, Indexer, SyncPhase};
use tokio::sync::watch;

pub(crate) type TaskError = Box<dyn Error + Send + Sync>;

/// Keeps the composed index current until shutdown or a terminal failure.
pub(crate) async fn run<F, E>(
    indexer: Arc<dyn Indexer>,
    filters: F,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    ready: watch::Sender<bool>,
) -> Result<(), TaskError>
where
    F: Fn() -> Result<Vec<AddressFilter>, E> + Send + Sync + 'static,
    E: Error + Send + Sync + 'static,
{
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }

        let filters = match filters() {
            Ok(filters) => filters,
            Err(error) => {
                ready.send_replace(false);
                return Err(TaskError::from(error));
            }
        };
        let result = tokio::select! {
            result = indexer.sync(filters) => result,
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
                    ready.send_replace(false);
                    return Err(io::Error::other(
                        "indexer returned incomplete or inconsistent synchronization status",
                    )
                    .into());
                }
                let caught_up = statuses
                    .iter()
                    .all(|status| status.phase == SyncPhase::Ready);
                ready.send_if_modified(|current| {
                    let changed = *current != caught_up;
                    *current = caught_up;
                    changed
                });
                caught_up
            }
            Err(error) if error.retryable => {
                ready.send_if_modified(|current| {
                    let changed = *current;
                    *current = false;
                    changed
                });
                true
            }
            Err(error) => {
                ready.send_replace(false);
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
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use base::BlockHeight;
    use indexing::{
        BlockHash, BlockRef, BoxFuture, ChainId, Checkpoint, History, HistoryQuery, IndexError,
        IndexErrorKind, IndexScope, SyncStatus, TransactionPage,
    };

    use super::*;

    struct Fixture {
        scopes: Vec<IndexScope>,
        results: Mutex<VecDeque<Result<Vec<SyncStatus>, IndexError>>>,
    }

    impl Fixture {
        fn new(results: Vec<Result<Vec<SyncStatus>, IndexError>>) -> Self {
            Self {
                scopes: vec![scope()],
                results: Mutex::new(results.into()),
            }
        }
    }

    impl Checkpoint for Fixture {
        fn checkpoint<'a>(
            &'a self,
            _scope: &'a IndexScope,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl History for Fixture {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async {
                Err(IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "fixture has no history",
                    false,
                ))
            })
        }
    }

    impl Indexer for Fixture {
        fn scopes(&self) -> &[IndexScope] {
            &self.scopes
        }

        fn sync<'a>(
            &'a self,
            _filters: Vec<AddressFilter>,
        ) -> BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>> {
            Box::pin(async move {
                let result = self.results.lock().expect("fixture lock").pop_front();
                match result {
                    Some(result) => result,
                    None => std::future::pending().await,
                }
            })
        }
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("fixture".into()),
            network: "mainnet".into(),
        }
    }

    fn status(phase: SyncPhase) -> SyncStatus {
        let block = BlockRef {
            height: BlockHeight(7),
            hash: BlockHash(vec![7]),
            parent_hash: None,
            timestamp: None,
        };
        SyncStatus {
            scope: scope(),
            checkpoint: Some(block.clone()),
            observed_tip: Some(block),
            phase,
        }
    }

    #[tokio::test]
    async fn refreshes_filters_until_every_scope_is_ready() {
        let indexer: Arc<dyn Indexer> = Arc::new(Fixture::new(vec![
            Ok(vec![status(SyncPhase::CatchingUp)]),
            Ok(vec![status(SyncPhase::Ready)]),
        ]));
        let calls = Arc::new(AtomicUsize::new(0));
        let sampled = calls.clone();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (ready, mut ready_rx) = watch::channel(false);
        let task = tokio::spawn(run(
            indexer,
            move || {
                sampled.fetch_add(1, Ordering::Relaxed);
                Ok::<_, io::Error>(Vec::new())
            },
            Duration::from_secs(60),
            shutdown_rx,
            ready,
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while !*ready_rx.borrow() {
                ready_rx.changed().await.expect("readiness sender");
            }
        })
        .await
        .expect("index becomes ready");
        shutdown.send(true).expect("shutdown receiver");
        task.await.expect("task join").expect("clean shutdown");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn retryable_failure_clears_readiness_without_stopping_task() {
        let error = IndexError::new(IndexErrorKind::Source, "offline", true);
        let indexer: Arc<dyn Indexer> = Arc::new(Fixture::new(vec![Err(error)]));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (ready, mut ready_rx) = watch::channel(true);
        let task = tokio::spawn(run(
            indexer,
            || Ok::<_, io::Error>(Vec::new()),
            Duration::from_secs(60),
            shutdown_rx,
            ready,
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while *ready_rx.borrow() {
                ready_rx.changed().await.expect("readiness sender");
            }
        })
        .await
        .expect("retry clears readiness");
        shutdown.send(true).expect("shutdown receiver");
        task.await.expect("task join").expect("clean shutdown");
    }

    #[tokio::test]
    async fn terminal_failure_is_returned_and_clears_readiness() {
        let error = IndexError::new(IndexErrorKind::InvalidBlock, "invalid block", false);
        let indexer: Arc<dyn Indexer> = Arc::new(Fixture::new(vec![Err(error)]));
        let (_shutdown, shutdown_rx) = watch::channel(false);
        let (ready, ready_rx) = watch::channel(true);

        let error = run(
            indexer,
            || Ok::<_, io::Error>(Vec::new()),
            Duration::from_secs(60),
            shutdown_rx,
            ready,
        )
        .await
        .expect_err("terminal failure");

        assert_eq!(error.to_string(), "invalid block");
        assert!(!*ready_rx.borrow());
    }

    #[tokio::test]
    async fn shutdown_cancels_an_in_flight_sync() {
        let indexer: Arc<dyn Indexer> = Arc::new(Fixture::new(Vec::new()));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (ready, _ready_rx) = watch::channel(false);
        let task = tokio::spawn(run(
            indexer,
            || Ok::<_, io::Error>(Vec::new()),
            Duration::from_secs(60),
            shutdown_rx,
            ready,
        ));

        tokio::task::yield_now().await;
        shutdown.send(true).expect("shutdown receiver");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("task stops")
            .expect("task join")
            .expect("clean shutdown");
    }
}
