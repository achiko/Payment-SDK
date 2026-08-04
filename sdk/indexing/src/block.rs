#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHeight(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHash(pub Vec<u8>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub height: BlockHeight,
    pub hash: BlockHash,
    pub parent_hash: Option<BlockHash>,
    pub timestamp: Option<u64>,
}

pub trait IndexedBlock: Clone + Send + Sync + 'static {
    fn block_ref(&self) -> BlockRef;
}
