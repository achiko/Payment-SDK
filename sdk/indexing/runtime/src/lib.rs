//! Keeps indexers current until shutdown or a terminal failure.
//!
//! `sdk/indexing` deliberately owns no async runtime, so the loop that
//! actually drives `Indexer::sync` lives here instead. Any binary embedding
//! the SDK can reuse it rather than reimplementing the readiness and
//! shutdown semantics.

use std::{error::Error, io, marker::PhantomData, sync::Arc, time::Duration};

use indexing::{AddressFilter, FilterSource, IndexError, IndexErrorKind, Indexer, SyncPhase};
use tokio::sync::watch;

/// Adapts the caller's filter closure to the selection synchronization reads.
///
/// The closure is handed down rather than called here, because reading it
/// before `sync` observes the source tip is exactly the ordering that loses a
/// newly registered address. See [`indexing::FilterSource`].
struct Selection<F, E> {
    filters: F,
    marker: PhantomData<fn() -> E>,
}

impl<F, E> FilterSource for Selection<F, E>
where
    F: Fn() -> Result<Vec<AddressFilter>, E> + Send + Sync,
    E: Error + Send + Sync + 'static,
{
    fn filters(&self) -> Result<Vec<AddressFilter>, IndexError> {
        // A selection that cannot be read is a caller fault, not a transient
        // one, so it stops the loop instead of retrying forever.
        (self.filters)().map_err(|error| {
            IndexError::new(IndexErrorKind::InvalidRequest, error.to_string(), false)
        })
    }
}

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
pub async fn run<F, E>(
    indexer: Arc<dyn Indexer>,
    filters: F,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    state: watch::Sender<SyncState>,
) -> Result<(), TaskError>
where
    F: Fn() -> Result<Vec<AddressFilter>, E> + Send + Sync + 'static,
    E: Error + Send + Sync + 'static,
{
    let selection = Selection {
        filters,
        marker: PhantomData,
    };
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }

        let result = tokio::select! {
            result = indexer.sync(&selection) => result,
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
