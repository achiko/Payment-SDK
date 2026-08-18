use std::{future::Future, time::Duration};

use http::server::HealthState;
use indexing::{SourceError, SyncPhase};
use tokio::sync::watch;

use super::{AppError, AppResult, OperationalSnapshot, SharedOperationalSnapshot, failure};

pub(super) async fn readiness_loop(
    health: HealthState,
    snapshot: SharedOperationalSnapshot,
    ready_max_lag: u64,
    ready_max_age: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> AppResult<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    health.set_ready(false);
                    return Ok(());
                }
            }
            _ = interval.tick() => {
                let ready = {
                    let guard = snapshot
                        .lock()
                        .map_err(|_| failure("operational snapshot lock is poisoned"))?;
                    readiness(&guard, ready_max_lag, ready_max_age).0
                };
                health.set_ready(ready);
            }
        }
    }
}

pub(super) fn readiness(
    snapshot: &OperationalSnapshot,
    max_lag: u64,
    max_age: Duration,
) -> (bool, Option<Duration>) {
    let age = snapshot.last_reconciled.map(|at| at.elapsed());
    let lag = snapshot.status.as_ref().and_then(|status| {
        Some(
            status
                .observed_tip
                .as_ref()?
                .height
                .0
                .saturating_sub(status.checkpoint.as_ref()?.height.0),
        )
    });
    let ready = snapshot
        .status
        .as_ref()
        .is_some_and(|status| status.phase == SyncPhase::Ready)
        && lag.is_some_and(|lag| lag <= max_lag)
        && age.is_some_and(|age| age <= max_age);
    (ready, age)
}

pub(super) async fn supervise_websocket<F>(
    websocket: F,
    shutdown: watch::Receiver<bool>,
) -> AppResult<()>
where
    F: Future<Output = Result<(), SourceError>> + Send,
{
    tokio::pin!(websocket);
    tokio::select! {
        result = &mut websocket => result.map_err(|error| Box::new(error) as AppError),
        _ = shutdown_signal(shutdown) => Ok(()),
    }
}

pub(super) async fn shutdown_signal(mut shutdown: watch::Receiver<bool>) {
    if !*shutdown.borrow() {
        drop(shutdown.changed().await);
    }
}
