use std::time::Instant;

use indexing::{BlockInterpreter, BlockSource, IndexChanges, IndexUndo, SyncPhase, WatchSelector};
use tokio::sync::watch;

use super::{AppResult, SyncRuntime, failure};

pub(super) async fn sync_loop<S, I>(
    runtime: SyncRuntime<S, I>,
    mut wakes: tokio::sync::mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
) -> AppResult<()>
where
    S: BlockSource,
    I: BlockInterpreter<
            Block = S::Block,
            Target = WatchSelector,
            Effect = IndexChanges,
            Undo = IndexUndo,
        > + Clone,
{
    let mut wait_before_sync = false;
    loop {
        if wait_before_sync {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(runtime.poll_interval) => {}
                wake = wakes.recv() => {
                    if wake.is_none() {
                        return Ok(());
                    }
                }
            }
        }
        if *shutdown.borrow() {
            return Ok(());
        }

        match runtime.indexer.sync(256).await {
            Ok(outcome) => {
                let status = outcome.status;
                let catching_up = status.phase == SyncPhase::CatchingUp;
                let mut guard = runtime
                    .snapshot
                    .lock()
                    .map_err(|_| failure("operational snapshot lock is poisoned"))?;
                guard.status = Some(status);
                guard.last_reconciled = Some(Instant::now());
                drop(guard);
                wait_before_sync = !catching_up && outcome.backfills == 0;
            }
            Err(error) => {
                tracing::warn!(kind = ?error.kind, retryable = error.retryable, "Indexer reconciliation failed");
                if !error.retryable {
                    return Err(Box::new(error));
                }
                wait_before_sync = true;
            }
        }
    }
}
