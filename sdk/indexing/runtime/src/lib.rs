//! Keeps indexers current until shutdown or a terminal failure.
//!
//! `sdk/indexing` deliberately owns no async runtime, so the loop that
//! actually drives `Indexer::sync` lives here instead. Any binary embedding
//! the SDK can reuse it rather than reimplementing the readiness and
//! shutdown semantics.

use std::{error::Error, io, sync::Arc, time::Duration};

use indexing::{AddressFilter, Indexer, SyncPhase};
use tokio::sync::watch;

pub type TaskError = Box<dyn Error + Send + Sync>;

/// Keeps the composed index current until shutdown or a terminal failure.
pub async fn run<F, E>(
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
