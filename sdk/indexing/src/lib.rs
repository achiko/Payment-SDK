//! Reorg-safe, chain-independent block synchronization contracts.

mod changes;
mod error;
mod indexer;
mod observation;
mod observer;
mod output;
mod service;
mod source;
mod store;
mod value;
mod watch;
mod worker;

pub use base::{BlockHash, BlockHeight, BlockRef};
pub use changes::{CommitBlock, InterpretedBlock, RawBlock};
pub use error::{IndexError, IndexErrorKind, SourceError};
pub use indexer::{Checkpoint, Composer, History, Indexer, Observer, Watcher};
pub use observation::{
    AddressQuery, ConfirmationPolicy, ConfirmationProof, DeactivateWatch, EventCursor, EventId,
    EventPage, EventQuery, HistoryQuery, MovementId, MovementKind, NetworkFee, ObservationDraft,
    ObservationDraftStatus, ObservationEvent, ObservationRevision, ObservedTransaction,
    RegisterWatch, TransactionPage, TransactionQuery, TransactionStatus, UnwatchOutcome,
    UnwatchRequest, ValueMovement, WatchOutcome, WatchReceipt, WatchRequest, WatchSelector,
};
pub use observer::{
    CommitObservation, CommitStatus, NoopWorkerObserver, ReorgDepth, ReorgObservation,
    WorkerObserver,
};
pub use output::{
    IndexChanges, IndexUndo, IndexedOutput, OutputChanges, OutputCursor, OutputId, OutputKey,
    OutputPage, OutputQuery, OutputRequest, OutputSnapshot,
};
pub use service::{RebuildReason, SyncPhase, SyncRequest, SyncStatus, Worker};
pub use source::{BlockInterpreter, BlockSource, IndexedBlock};
pub use store::{
    AbortRebuild, BackfillOutcome, BackfillReader, BackfillWriter, BeginRebuild, BlockOutcome,
    CanonicalReader, ChainWriter, CleanupGeneration, CleanupOutcome, CommitBackfill, EventReader,
    IndexTypes, PrepareActivation, RebuildActivation, RebuildAdmin, RebuildBlock, RebuildBuilder,
    RebuildGeneration, RebuildPhase, RebuildPublisher, RebuildReader, RebuildState,
    RebuildValidation, RevertOutcome, RevertTip, StatusStore, TransactionReader, WatchLookup,
    WatchReader, WatchStore,
};
pub use value::{AssetId, CanonicalAddress, ChainId, TransactionRef};
pub use watch::{IndexScope, WatchBackfill, WatchId, WatchSnapshot, WatchTarget, WatchVersion};
pub use worker::{SyncConfig, SyncWorker, V1_CONFIRMATION_DEPTH, V1_REORG_RETENTION};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
