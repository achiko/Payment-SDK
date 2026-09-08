use crate::{
    BlockPosition, BlockRef, BoxFuture, CanonicalAddress, IndexError, InterpretedBlock, SourceError,
};

/// Gives generic synchronization access to a chain-native block identity.
pub trait IndexedBlock: Clone + Send + Sync + 'static {
    fn block_ref(&self) -> BlockRef;
}

/// Fetches canonical chain data. Concrete RPC methods remain in the chain crate.
pub trait BlockSource: Send + Sync {
    type Block: IndexedBlock;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>>;

    /// Returns at most `limit` actual produced blocks in the inclusive range.
    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>>;

    /// Returns the complete canonical reference at one native position.
    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>>;
}

/// Converts a chain-native block into relevant canonical changes.
pub trait BlockInterpreter: Send + Sync {
    type Block: IndexedBlock;

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[CanonicalAddress],
    ) -> Result<InterpretedBlock, IndexError>;
}
