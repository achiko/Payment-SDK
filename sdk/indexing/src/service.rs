use crate::{BlockHeight, BlockRef, BoxFuture, ConfirmationPolicy, IndexError, IndexScope};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncRequest {
    pub scope: IndexScope,
    /// `None` means follow the source's observed tip.
    pub through: Option<BlockHeight>,
    /// Allows a worker to bound one invocation and yield fairly.
    pub max_blocks: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPhase {
    Starting,
    Reconciling,
    CatchingUp,
    Ready,
    Reverting,
    Replaying,
    RebuildRequired,
    Halted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildReason {
    pub checkpoint: BlockRef,
    pub oldest_retained: BlockHeight,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    pub scope: IndexScope,
    pub checkpoint: Option<BlockRef>,
    pub observed_tip: Option<BlockRef>,
    pub confirmation_policy: ConfirmationPolicy,
    pub phase: SyncPhase,
    pub rebuild_reason: Option<RebuildReason>,
    pub halted_reason: Option<String>,
}

impl SyncStatus {
    #[must_use]
    pub fn starting(scope: IndexScope, confirmation_policy: ConfirmationPolicy) -> Self {
        Self {
            scope,
            checkpoint: None,
            observed_tip: None,
            confirmation_policy,
            phase: SyncPhase::Starting,
            rebuild_reason: None,
            halted_reason: None,
        }
    }
}

/// Internal reorg-safe synchronization loop. It is intentionally separate from
/// the public observation registration/query surface.
pub trait Worker: Send + Sync {
    fn sync<'a>(&'a self, request: SyncRequest) -> BoxFuture<'a, Result<SyncStatus, IndexError>>;

    fn status<'a>(&'a self, scope: &'a IndexScope)
    -> BoxFuture<'a, Result<SyncStatus, IndexError>>;
}
