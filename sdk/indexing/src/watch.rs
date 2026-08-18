use crate::ChainId;
use crate::{BlockHeight, BlockRef, WatchReceipt, WatchSelector};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexScope {
    pub chain: ChainId,
    /// Chain-owned canonical network name, such as mainnet, sepolia, or regtest.
    pub network: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatchId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatchVersion(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchTarget<T> {
    pub id: WatchId,
    pub scope: IndexScope,
    pub selector: WatchSelector,
    pub target: T,
    /// Caller-provided idempotency key. It is unique only within `scope`.
    pub idempotency_key: String,
    /// First height at which this target can have relevant activity.
    pub start_height: BlockHeight,
    /// The observed tip when the watch was registered, if available.
    pub registered_at: Option<BlockRef>,
}

impl<T> WatchTarget<T> {
    #[must_use]
    pub fn is_active_at(&self, height: BlockHeight) -> bool {
        self.start_height <= height
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchSnapshot<T> {
    pub version: WatchVersion,
    pub watches: Vec<WatchTarget<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchContext<T> {
    pub checkpoint: Option<BlockRef>,
    pub version: WatchVersion,
    pub next_id: u64,
    pub existing: Option<WatchTarget<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchPlan<T> {
    pub watch: WatchTarget<T>,
    pub expected_checkpoint: Option<BlockRef>,
    pub expected_version: WatchVersion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchDecision<T> {
    pub receipt: WatchReceipt,
    pub plan: Option<WatchPlan<T>>,
}
