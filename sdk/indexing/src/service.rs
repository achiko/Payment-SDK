use std::sync::Arc;

use crate::{BlockPosition, BlockRef, CanonicalAddress, IndexScope, Observer};

use crate::{
    BlockInterpreter, BlockSource, Blocks, Checkpoint, History, HistoryQuery, IndexError, Indexer,
    SyncConfig, SyncPlan, TransactionPage, Transactions, indexer::Index,
    synchronizer::Synchronizer,
};

/// Caller-owned selection of one address and the first block worth inspecting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressFilter {
    pub address: CanonicalAddress,
    pub start_position: BlockPosition,
}

/// The complete address selection, read on demand rather than handed over in
/// advance.
///
/// Synchronization reads this *after* it observes the source tip, and the
/// ordering is load-bearing. An address is registered with a birthday of the
/// current checkpoint plus one, which promises that every later block is
/// inspected for it. A selection captured before the tip was observed cannot
/// contain an address registered in between, so the blocks that tip admits —
/// blocks the new address's birthday already covers — would be indexed without
/// it, and nothing rescans them once the checkpoint moves past.
///
/// Implementing this for a fixed `Vec` is correct only when the selection
/// cannot change during the pass, which is why it is spelled out rather than
/// taken as a snapshot argument.
pub trait FilterSource: Send + Sync {
    fn filters(&self) -> Result<Vec<AddressFilter>, IndexError>;

    fn plan(
        &self,
        _scope: &IndexScope,
        checkpoint: Option<BlockRef>,
    ) -> Result<SyncPlan, IndexError> {
        self.filters()
            .map(|filters| SyncPlan::detached(filters, checkpoint))
    }
}

/// A selection that cannot change while a pass runs, such as a fixed
/// configuration or a test fixture.
impl FilterSource for Vec<AddressFilter> {
    fn filters(&self) -> Result<Vec<AddressFilter>, IndexError> {
        Ok(self.clone())
    }
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
        selection: &'a dyn FilterSource,
    ) -> crate::BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>> {
        Box::pin(async move {
            let status = self.synchronizer.sync(selection).await?;
            Ok(vec![status])
        })
    }
}
