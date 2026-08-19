//! Caller-supplied notification for blocks that were just committed.

use crate::{BlockRef, BoxFuture, CanonicalTransaction, IndexScope};

/// Facts from one canonical block, delivered after it was durably committed.
///
/// Only blocks that actually changed the index are reported. Re-applying a
/// block that is already the checkpoint produces no observation, so a restart
/// that replays the tip does not re-notify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockObservation {
    pub scope: IndexScope,
    pub block: BlockRef,
    /// Transactions in this block that touched a supplied address filter.
    /// Empty when the block was indexed but matched nothing.
    pub transactions: Vec<CanonicalTransaction>,
}

/// Receives observations as synchronization commits blocks.
///
/// Called from inside the synchronization loop, so a slow implementation slows
/// indexing. Hand work to a queue rather than blocking here.
///
/// This reports inclusion, not confirmation depth: a transaction is announced
/// once, in the block that contained it. Depth thresholds are derived by
/// reading `History`, which recomputes confirmations against the checkpoint.
pub trait Observer: Send + Sync {
    fn observed<'a>(&'a self, observation: BlockObservation) -> BoxFuture<'a, ()>;
}
