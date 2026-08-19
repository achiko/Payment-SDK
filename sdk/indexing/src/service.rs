use std::sync::Arc;

use crate::{BlockHeight, BlockRef, CanonicalAddress, IndexScope, Observer};

use crate::{
    BlockInterpreter, BlockSource, Blocks, Checkpoint, History, HistoryQuery, IndexError, Indexer,
    SyncConfig, TransactionPage, Transactions, indexer::Index, synchronizer::Synchronizer,
};

/// Caller-owned selection of one address and the first block worth inspecting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressFilter {
    pub address: CanonicalAddress,
    pub start_height: BlockHeight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPhase {
    CatchingUp,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    pub scope: IndexScope,
    pub checkpoint: Option<BlockRef>,
    pub observed_tip: Option<BlockRef>,
    pub phase: SyncPhase,
}

/// One chain's reusable synchronization and query implementation.
pub struct Service<S, I, R> {
    scope: IndexScope,
    synchronizer: Synchronizer<S, I, R>,
    index: Index<R>,
}

impl<S, I, R> Service<S, I, R>
where
    R: Clone,
{
    #[must_use]
    pub fn new(source: S, interpreter: I, repository: R, config: SyncConfig) -> Self {
        let scope = config.scope.clone();
        let index = Index::new(repository.clone(), config.minimum_confirmations);
        Self {
            scope,
            synchronizer: Synchronizer::new(source, interpreter, repository, config),
            index,
        }
    }

    /// Notifies `observer` after each block this service commits.
    ///
    /// Set before synchronization starts; blocks committed earlier are not
    /// replayed.
    pub fn observe(&mut self, observer: Arc<dyn Observer>) {
        self.synchronizer.observe(observer);
    }
}

impl<S, I, R> Checkpoint for Service<S, I, R>
where
    S: Send + Sync,
    I: Send + Sync,
    R: Blocks,
{
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> crate::BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.index.checkpoint(scope)
    }
}

impl<S, I, R> History for Service<S, I, R>
where
    S: Send + Sync,
    I: Send + Sync,
    R: Transactions,
{
    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> crate::BoxFuture<'a, Result<TransactionPage, IndexError>> {
        self.index.history(request)
    }
}

impl<S, I, R> Indexer for Service<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: Blocks + Transactions,
{
    fn scopes(&self) -> &[IndexScope] {
        std::slice::from_ref(&self.scope)
    }

    fn sync<'a>(
        &'a self,
        filters: Vec<AddressFilter>,
    ) -> crate::BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>> {
        Box::pin(async move {
            let status = self.synchronizer.sync(filters).await?;
            Ok(vec![status])
        })
    }
}
