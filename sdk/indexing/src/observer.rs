use std::time::Duration;

use crate::{BlockHeight, BlockRef, CommitBlockOutcome, IndexErrorKind, IndexScope};

/// Result of one repository block-commit attempt.
///
/// Failure observations intentionally omit the error message so observers do
/// not become an accidental sink for backend or RPC details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockCommitObservationOutcome {
    Success(CommitBlockOutcome),
    Failure {
        kind: IndexErrorKind,
        retryable: bool,
    },
}

/// Timing and result of one call to `IndexRepository::commit_block`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockCommitObservation {
    pub scope: IndexScope,
    pub block: BlockRef,
    pub elapsed: Duration,
    pub outcome: BlockCommitObservationOutcome,
}

/// Reorg depth found by canonical reconciliation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReorgDepth {
    Exact {
        depth: u64,
        common_ancestor: BlockRef,
    },
    /// No ancestor was found inside the retained canonical window.
    BeyondRetention {
        minimum_depth: u64,
        oldest_retained: BlockHeight,
    },
}

/// One reorg detected from a previously persisted canonical tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReorgObservation {
    pub scope: IndexScope,
    pub previous_tip: BlockRef,
    pub depth: ReorgDepth,
}

/// Backend-independent ordered-sync observation boundary.
///
/// Implementations should return quickly and must not perform blocking I/O on
/// the synchronization task. Both methods default to no-ops so consumers may
/// implement only the signals they need.
pub trait SyncObserver: Send + Sync + 'static {
    fn block_commit(&self, _observation: BlockCommitObservation) {}

    fn reorg_detected(&self, _observation: ReorgObservation) {}
}

/// Default observer used by `OrderedSyncWorker::new`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSyncObserver;

impl SyncObserver for NoopSyncObserver {}
