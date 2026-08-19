use crate::{
    BlockHash, BlockHeight, BlockRef, BoxFuture, CanonicalAddress, IndexError, InterpretedBlock,
    SourceError,
};

/// Gives generic synchronization access to a chain-native block identity.
pub trait IndexedBlock: Clone + Send + Sync + 'static {
    fn block_ref(&self) -> BlockRef;
}

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

/// Converts a chain-native block into relevant canonical changes.
pub trait BlockInterpreter: Send + Sync {
    type Block: IndexedBlock;

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[CanonicalAddress],
    ) -> Result<InterpretedBlock, IndexError>;
}
