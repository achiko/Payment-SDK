use crate::{BlockHeight, BlockRef};
use chain_identity::ChainId;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexScope {
    pub chain: ChainId,
    /// Chain-owned canonical network name, such as mainnet, sepolia, or regtest.
    pub network: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatchId(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchTarget<T> {
    pub id: WatchId,
    pub scope: IndexScope,
    pub target: T,
    /// First height at which this target can have relevant activity.
    pub start_height: BlockHeight,
    /// The observed tip when the watch was registered, if available.
    pub registered_at: Option<BlockRef>,
}
