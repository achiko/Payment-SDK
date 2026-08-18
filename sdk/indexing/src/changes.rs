use std::collections::{BTreeMap, BTreeSet};

use crate::{
    BlockHeight, BlockRef, CanonicalAddress, ConfirmationPolicy, IndexScope, ObservationDraft,
    ObservedTransaction, TransactionRef, WatchId, WatchVersion,
};

/// Complete semantic input to one atomic repository commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterpretedBlock<E, U> {
    pub block: BlockRef,
    pub drafts: Vec<ObservationDraft>,
    /// Typed chain semantics; only a repository adapter may encode them.
    pub effect: E,
    /// Chain-owned information required to reverse the block.
    pub undo: U,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitBlock<E, U> {
    pub scope: IndexScope,
    pub expected_checkpoint: Option<BlockRef>,
    pub expected_watch_version: WatchVersion,
    pub confirmation_policy: ConfirmationPolicy,
    pub reorg_retention: u64,
    pub block: InterpretedBlock<E, U>,
}

/// Storage-neutral state required to decide one block transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitContext {
    pub checkpoint: Option<BlockRef>,
    pub watch_version: WatchVersion,
    pub active_watches: BTreeSet<WatchId>,
    pub observations: BTreeMap<TransactionRef, StoredObservation>,
    pub pending_confirmations: BTreeSet<TransactionRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredObservation {
    pub transaction: ObservedTransaction,
    pub watch_ids: Vec<WatchId>,
}

/// A semantic transition already decided by indexing and ready for atomic storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationTransition {
    pub prior: Option<StoredObservation>,
    pub next: StoredObservation,
    pub prior_addresses: BTreeSet<CanonicalAddress>,
    pub next_addresses: BTreeSet<CanonicalAddress>,
    pub included_here: bool,
    pub pending: PendingChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingChange {
    None,
    Add { inclusion: BlockHeight },
    Remove { inclusion: BlockHeight },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitPlan<E, U> {
    pub scope: IndexScope,
    pub expected_checkpoint: Option<BlockRef>,
    pub expected_watch_version: WatchVersion,
    pub block: BlockRef,
    pub transitions: BTreeMap<TransactionRef, ObservationTransition>,
    pub effect: E,
    pub undo: U,
    pub prune_before: Option<crate::BlockHeight>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertContext<U> {
    pub checkpoint: Option<BlockRef>,
    pub block: Option<RevertBlock<U>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertBlock<U> {
    pub block: BlockRef,
    pub prior_checkpoint: Option<BlockRef>,
    pub undo: U,
    pub observations: Vec<RevertObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertObservation {
    pub current: StoredObservation,
    pub prior: Option<StoredObservation>,
    pub included_here: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertPlan<U> {
    pub scope: IndexScope,
    pub expected_tip: BlockRef,
    pub checkpoint: Option<BlockRef>,
    pub undo: U,
    pub transitions: BTreeMap<TransactionRef, ObservationTransition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertDecision<U> {
    pub checkpoint: Option<BlockRef>,
    pub plan: Option<RevertPlan<U>>,
}
