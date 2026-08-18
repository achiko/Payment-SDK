use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    BlockInterpreter, BlockSource, CanonicalReader, ChainWriter, IndexError, StatusStore,
    SyncRequest, SyncStatus, WatchReader, Worker,
};

use super::SyncWorker;

pub(super) struct RunningGuard<'a>(pub(super) &'a AtomicBool);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<S, I, R> Worker for SyncWorker<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: CanonicalReader<Target = I::Target, Effect = I::Effect, Undo = I::Undo>
        + WatchReader
        + ChainWriter
        + StatusStore,
{
    fn sync<'a>(
        &'a self,
        request: SyncRequest,
    ) -> crate::BoxFuture<'a, Result<SyncStatus, IndexError>> {
        Box::pin(async move {
            let _guard = self.enter()?;
            self.sync_inner(request).await
        })
    }

    fn status<'a>(
        &'a self,
        scope: &'a crate::IndexScope,
    ) -> crate::BoxFuture<'a, Result<SyncStatus, IndexError>> {
        Box::pin(async move {
            self.validate_scope(scope)?;
            self.repository.status(scope).await
        })
    }
}
