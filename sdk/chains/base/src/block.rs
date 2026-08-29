/// Number of produced blocks through one canonical block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHeight(pub u64);

impl BlockHeight {
    /// Returns the next produced height, or `None` at the numeric boundary.
    #[must_use]
    pub const fn checked_successor(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl From<u64> for BlockHeight {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<BlockHeight> for u64 {
    fn from(value: BlockHeight) -> Self {
        value.0
    }
}

/// Native monotonic coordinate used by a concrete chain's RPC interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockPosition(pub u64);

impl BlockPosition {
    /// Returns the next native coordinate, or `None` at the numeric boundary.
    #[must_use]
    pub const fn checked_successor(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl From<u64> for BlockPosition {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<BlockPosition> for u64 {
    fn from(value: BlockPosition) -> Self {
        value.0
    }
}

/// Canonical block identity in the chain's native byte order.
///
/// The base layer deliberately does not impose a fixed length: common chains
/// use 32 bytes, while future protocols may use a different digest format.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHash(pub Vec<u8>);

/// Atomic identity of one canonical block's parent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockParent {
    pub position: BlockPosition,
    pub hash: BlockHash,
}

impl From<(BlockPosition, BlockHash)> for BlockParent {
    fn from((position, hash): (BlockPosition, BlockHash)) -> Self {
        Self { position, hash }
    }
}

impl From<BlockParent> for (BlockPosition, BlockHash) {
    fn from(parent: BlockParent) -> Self {
        (parent.position, parent.hash)
    }
}

/// Chain-independent reference required for canonical traversal and reorg
/// detection. Protocol-specific block fields stay in concrete chain crates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub position: BlockPosition,
    pub height: BlockHeight,
    pub hash: BlockHash,
    pub parent: Option<BlockParent>,
    pub timestamp: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_coordinates_convert_and_advance() {
        let position = BlockPosition::from(0);
        let height = BlockHeight::from(0);

        assert_eq!(u64::from(position), 0);
        assert_eq!(u64::from(height), 0);
        assert_eq!(position.checked_successor(), Some(BlockPosition(1)));
        assert_eq!(height.checked_successor(), Some(BlockHeight(1)));
    }

    #[test]
    fn maximum_coordinates_have_no_successor() {
        assert_eq!(BlockPosition(u64::MAX).checked_successor(), None);
        assert_eq!(BlockHeight(u64::MAX).checked_successor(), None);
    }

    #[test]
    fn parent_conversion_keeps_position_and_hash_atomic() {
        let pair = (BlockPosition(42), BlockHash(vec![1, 2, 3]));
        let parent = BlockParent::from(pair.clone());

        assert_eq!(parent.position, pair.0);
        assert_eq!(parent.hash, pair.1);
        assert_eq!(<(BlockPosition, BlockHash)>::from(parent), pair);
    }
}
