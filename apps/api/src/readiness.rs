//! Bridges synchronization state onto the boolean the HTTP layer serves.

use indexing_runtime::SyncState;
use tokio::sync::watch;

/// Publishes readiness and reports why synchronization is retrying.
///
/// A retryable failure repeats indefinitely, so without this the only signal
/// would be a readiness flag stuck at `false` with no stated reason.
pub(crate) async fn publish(mut state: watch::Receiver<SyncState>, ready: watch::Sender<bool>) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishes_catching_up_ready_retrying_and_closure() {
        let (state, state_rx) = watch::channel(SyncState::CatchingUp);
        let (ready, mut ready_rx) = watch::channel(true);
        let task = tokio::spawn(publish(state_rx, ready));

        ready_rx.changed().await.expect("initial not-ready state");
        assert!(!*ready_rx.borrow_and_update());

        state.send_replace(SyncState::Ready);
        ready_rx.changed().await.expect("ready state");
        assert!(*ready_rx.borrow_and_update());

        state.send_replace(SyncState::Retrying {
            error: "temporary outage".to_owned(),
        });
        ready_rx.changed().await.expect("retrying state");
        assert!(!*ready_rx.borrow_and_update());

        drop(state);
        task.await.expect("tracked readiness task");
    }
}
