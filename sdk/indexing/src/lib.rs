//! Reorg-safe, chain-independent block synchronization contracts.

mod changes;
mod error;
mod indexer;
mod observation;
mod output;
mod planning;
mod planning_revert;
mod planning_watch;
mod service;
mod source;
mod store;
mod synchronizer;
#[cfg(test)]
mod synchronizer_test;
mod value;
mod watch;

pub use base::{BlockHash, BlockHeight, BlockRef};
pub use changes::{
    CommitBlock, CommitContext, CommitPlan, InterpretedBlock, ObservationTransition, PendingChange,
    RevertBlock, RevertContext, RevertDecision, RevertObservation, RevertPlan, StoredObservation,
};
pub use error::{IndexError, IndexErrorKind, SourceError};
pub use indexer::{Checkpoint, History, Index, Watcher};
pub use observation::{
    ConfirmationPolicy, ConfirmationProof, HistoryQuery, MovementId, MovementKind, NetworkFee,
    ObservationDraft, ObservationDraftStatus, ObservationRevision, ObservedTransaction,
    RegisterWatch, TransactionPage, TransactionQuery, TransactionStatus, ValueMovement,
    WatchReceipt, WatchRequest, WatchSelector,
};
pub use output::{
    IndexChanges, IndexUndo, IndexedOutput, OutputChanges, OutputCursor, OutputId, OutputKey,
    OutputPage, OutputQuery, OutputRequest, OutputSnapshot,
};
#[doc(hidden)]
pub use planning::{
    addresses as observation_addresses, commit as plan_commit, confirmation as plan_confirmation,
    observation as plan_observation, validate_draft,
};
#[doc(hidden)]
pub use planning_revert::revert as plan_revert;
#[doc(hidden)]
pub use planning_watch::watch as plan_watch;
pub use service::{SyncPhase, SyncRequest, SyncStatus};
pub use source::{BlockInterpreter, BlockSource, IndexedBlock};
pub use store::{
    BlockOutcome, BlockStore, CanonicalStore, HistoryStore, RevertTip, StatusStore, WatchStore,
};
pub use synchronizer::{DEFAULT_CONFIRMATIONS, DEFAULT_REORG_RETENTION, SyncConfig, Synchronizer};
pub use value::{AssetId, CanonicalAddress, ChainId, TransactionRef};
pub use watch::{
    IndexScope, WatchContext, WatchDecision, WatchId, WatchPlan, WatchSnapshot, WatchTarget,
    WatchVersion,
};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
