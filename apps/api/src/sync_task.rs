use super::{AnyError, Duration};

pub(super) async fn run<S, I>(
    synchronizer: indexing::Synchronizer<S, I, indexing_rocksdb::Repository>,
    interval: Duration,
    batch_size: usize,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), AnyError>
where
    S: indexing::BlockSource<Block = I::Block> + 'static,
    I: indexing::BlockInterpreter<
            Target = indexing::WatchSelector,
            Effect = indexing::IndexChanges,
            Undo = indexing::IndexUndo,
        > + 'static,
{
    let mut wait = false;
    loop {
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
        if *shutdown.borrow() {
            return Ok(());
        }
        match synchronizer
            .sync(indexing::SyncRequest {
                scope: synchronizer.repository().scope().clone(),
                through: None,
                max_blocks: Some(batch_size),
            })
            .await
        {
            Ok(status) => wait = status.phase != indexing::SyncPhase::CatchingUp,
            Err(error) if error.retryable => wait = true,
            Err(error) => return Err(error.into()),
        }
    }
}

pub(super) fn stopped(
    result: Option<Result<Result<(), AnyError>, tokio::task::JoinError>>,
) -> Result<(), AnyError> {
    match result {
        Some(Ok(Err(error))) => Err(error),
        Some(Ok(Ok(()))) => Err("an indexing synchronizer stopped unexpectedly".into()),
        Some(Err(error)) => Err(error.into()),
        None => Err("no indexing tasks are running".into()),
    }
}
