/// A chain position. Concrete RPC adapters validate whether their protocol can
/// represent the value before issuing a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHeight(pub u64);

/// Canonical block identity in the chain's native byte order.
///
/// The base layer deliberately does not impose a fixed length: common chains
/// use 32 bytes, while future protocols may use a different digest format.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHash(pub Vec<u8>);

/// Chain-independent reference required for canonical traversal and reorg
/// detection. Protocol-specific block fields stay in concrete chain crates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub height: BlockHeight,
    pub hash: BlockHash,
    pub parent_hash: Option<BlockHash>,
    pub timestamp: Option<u64>,
}
