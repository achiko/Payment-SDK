use crate::{
    BlockChanges, BlockHash, BlockHeight, BlockRef, BoxFuture, IndexError, IndexedBlock,
    SourceError, WatchTarget,
};

/// Fetches canonical chain data. Concrete RPC methods remain in the chain crate.
pub trait BlockSource: Send + Sync {
    type Block: IndexedBlock;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>>;

    fn block_at<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Self::Block, SourceError>>;

    /// Used to compare a persisted checkpoint with the current canonical chain.
    fn canonical_hash<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Option<BlockHash>, SourceError>>;
}

/// Converts a chain-native block into relevant, reversible index changes.
pub trait BlockInterpreter: Send + Sync {
    type Block: IndexedBlock;
    type Target: Clone + Send + Sync + 'static;
    type Event: Clone + Send + Sync + 'static;
    type Undo: Clone + Send + Sync + 'static;

    fn inspect(
        &self,
        block: &Self::Block,
        watches: &[WatchTarget<Self::Target>],
    ) -> Result<BlockChanges<Self::Event, Self::Undo>, IndexError>;
}

/// Mempool state is non-canonical and is intentionally separate from block sync.
pub trait MempoolSource: Send + Sync {
    type Transaction: Clone + Send + Sync + 'static;

    fn transactions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Self::Transaction>, SourceError>>;
}
