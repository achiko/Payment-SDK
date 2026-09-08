//! Bridges synchronization state onto the boolean the HTTP layer serves.

use indexing_runtime::SyncState;
use tokio::sync::watch;

// design-lint: allow unclassified-free-function -- application-owned async bridge projects synchronization watch updates into HTTP readiness and retry diagnostics under tracked task supervision
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
    async fn publishes_current_state_even_when_upstream_already_closed() {
        let (state, state_rx) = watch::channel(SyncState::Ready);
        let (ready, ready_rx) = watch::channel(false);
        drop(state);

        publish(state_rx, ready).await;

        assert!(*ready_rx.borrow());
        assert!(ready_rx.has_changed().is_err());
    }

    #[tokio::test]
    async fn loss_of_readiness_consumers_does_not_end_the_upstream_bridge() {
        let (state, state_rx) = watch::channel(SyncState::CatchingUp);
        let (ready, ready_rx) = watch::channel(false);
        drop(ready_rx);
        let mut publisher = std::pin::pin!(publish(state_rx, ready));

        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(publisher.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(state.receiver_count(), 1);

        drop(state);
        publisher.await;
    }

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
