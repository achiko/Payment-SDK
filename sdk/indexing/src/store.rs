use crate::{
    BlockHeight, BlockRef, BoxFuture, CommitBlock, CommitContext, CommitPlan, HistoryQuery,
    IndexChanges, IndexError, IndexScope, IndexUndo, ObservedTransaction, RegisterWatch,
    RevertContext, RevertPlan, SyncStatus, TransactionPage, TransactionQuery, WatchContext,
    WatchPlan, WatchSelector, WatchSnapshot,
};

pub trait CanonicalStore: Send + Sync {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;

    fn canonical_block<'a>(
        &'a self,
        scope: &'a IndexScope,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;

    fn load_commit<'a>(
        &'a self,
        command: &'a CommitBlock<IndexChanges, IndexUndo>,
    ) -> BoxFuture<'a, Result<CommitContext, IndexError>>;
}

pub trait WatchStore: Send + Sync {
    fn watches_at<'a>(
        &'a self,
        scope: &'a IndexScope,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<WatchSnapshot<WatchSelector>, IndexError>>;
    fn load_watch<'a>(
        &'a self,
        command: &'a RegisterWatch<WatchSelector>,
    ) -> BoxFuture<'a, Result<WatchContext<WatchSelector>, IndexError>>;
    fn save_watch<'a>(
        &'a self,
        plan: WatchPlan<WatchSelector>,
    ) -> BoxFuture<'a, Result<(), IndexError>>;
}

pub trait BlockStore: Send + Sync {
    fn commit_block<'a>(
        &'a self,
        plan: CommitPlan<IndexChanges, IndexUndo>,
    ) -> BoxFuture<'a, Result<BlockOutcome, IndexError>>;

    fn load_revert<'a>(
        &'a self,
        command: &'a RevertTip,
    ) -> BoxFuture<'a, Result<RevertContext<IndexUndo>, IndexError>>;

    fn save_revert<'a>(
        &'a self,
        plan: RevertPlan<IndexUndo>,
    ) -> BoxFuture<'a, Result<(), IndexError>>;
}

pub trait HistoryStore: Send + Sync {
    fn transaction<'a>(
        &'a self,
        request: TransactionQuery,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>>;

    fn transactions_by_address<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;
}

pub trait StatusStore: Send + Sync {
    fn status<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<SyncStatus>, IndexError>>;

    fn set_status<'a>(&'a self, status: SyncStatus) -> BoxFuture<'a, Result<(), IndexError>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockOutcome {
    Applied,
    AlreadyApplied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertTip {
    pub scope: IndexScope,
    pub expected_tip: BlockRef,
}
