//! Bridges synchronization state onto the boolean the HTTP layer serves.

use indexing_runtime::SyncState;
use tokio::sync::watch;

/// Publishes readiness and reports why synchronization is retrying.
///
/// A retryable failure repeats indefinitely, so without this the only signal
/// would be a readiness flag stuck at `false` with no stated reason.
pub(crate) fn publish(mut state: watch::Receiver<SyncState>, ready: watch::Sender<bool>) {
    tokio::spawn(async move {
        loop {
            let current = state.borrow_and_update().clone();
            let _ = ready.send(current == SyncState::Ready);
            if let SyncState::Retrying { error } = &current {
                eprintln!("synchronization is retrying: {error}");
            }
            if state.changed().await.is_err() {
                return;
            }
        }
    });
}
