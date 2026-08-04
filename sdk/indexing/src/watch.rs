use crate::{BlockHeight, BlockRef, WatchSelector};
use chain_identity::ChainId;

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
    /// First height at which this watch is inactive. Historical scans below it
    /// continue to see the watch, which makes soft unwatch reorg-safe.
    pub inactive_from: Option<BlockHeight>,
}

impl<T> WatchTarget<T> {
    #[must_use]
    pub fn is_active_at(&self, height: BlockHeight) -> bool {
        self.start_height <= height
            && self
                .inactive_from
                .is_none_or(|inactive_from| height < inactive_from)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchSnapshot<T> {
    pub version: WatchVersion,
    pub watches: Vec<WatchTarget<T>>,
}

/// Durable historical scan work created when a watch birthday precedes the
/// canonical checkpoint observed during registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchBackfill {
    pub scope: IndexScope,
    pub watch_id: WatchId,
    pub from_height: BlockHeight,
    pub next_height: BlockHeight,
    pub through: BlockRef,
}
