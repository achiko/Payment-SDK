use crate::{BlockHeight, BlockRef, ConfirmationPolicy, IndexScope};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncRequest {
    pub scope: IndexScope,
    /// `None` means follow the source's observed tip.
    pub through: Option<BlockHeight>,
    /// Allows a synchronizer to bound one invocation and yield fairly.
    pub max_blocks: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPhase {
    Starting,
    Reconciling,
    CatchingUp,
    Ready,
    Reverting,
    Halted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    pub scope: IndexScope,
    pub checkpoint: Option<BlockRef>,
    pub observed_tip: Option<BlockRef>,
    pub confirmation_policy: ConfirmationPolicy,
    pub phase: SyncPhase,
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
            halted_reason: None,
        }
    }
}
