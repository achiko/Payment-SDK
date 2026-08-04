//! Reorg-safe, chain-independent block synchronization contracts.

mod block;
mod changes;
mod error;
mod observation;
mod observer;
mod persistent;
mod service;
mod source;
mod store;
mod watch;
mod worker;

pub use block::{BlockHash, BlockHeight, BlockRef, IndexedBlock};
pub use changes::{BlockChanges, CommitBlockCommand, IndexedEvent, InterpretedBlock, RawBlockData};
pub use error::{IndexError, IndexErrorKind, SourceError};
pub use observation::{
    AddressWatchRequest, ConfirmationPolicy, ConfirmationProof, EventCursor, FinalityScanPage,
    FinalityScanRequest, MovementId, MovementKind, NetworkFee, ObservationDraft,
    ObservationDraftStatus, ObservationEvent, ObservationEventId, ObservationEventPage,
    ObservationEventRequest, ObservationRevision, ObservedTransaction, RegisterWatchCommand,
    RegisterWatchOutcome, TransactionPage, TransactionPageRequest, TransactionRequest,
    TransactionStatus, UnwatchCommand, UnwatchOutcome, ValueMovement, WatchReceipt, WatchRequest,
    WatchSelector,
};
pub use observer::{
    BlockCommitObservation, BlockCommitObservationOutcome, NoopSyncObserver, ReorgDepth,
    ReorgObservation, SyncObserver,
};
pub use persistent::{
    IndexRecordCodec, PersistentIndexConfig, PersistentIndexRepository, RawBytesIndexCodec,
};
pub use service::{
    IndexingWorker, ObservationEventSource, ObservationQuery, ObservationRegistry, RebuildReason,
    SyncPhase, SyncRequest, SyncStatus,
};
pub use source::{BlockInterpreter, BlockSource, MempoolSource};
pub use store::{
    AbortRebuildCommand, ActivateRebuildCommand, BeginRebuildCommand, CleanupGenerationCommand,
    CleanupGenerationOutcome, CommitBlockOutcome, CommitRebuildBlockCommand,
    CommitWatchBackfillCommand, CommitWatchBackfillOutcome, IndexRepository,
    MigrateIndexPolicyCommand, MigrateIndexPolicyOutcome, PolicyMigrationVersion,
    PrepareRebuildActivationCommand, RebuildGeneration, RebuildPhase, RebuildState,
    RevertTipCommand, RevertTipOutcome, ValidateRebuildCommand,
};
pub use watch::{IndexScope, WatchBackfill, WatchId, WatchSnapshot, WatchTarget, WatchVersion};
pub use worker::{OrderedSyncConfig, OrderedSyncWorker, V1_CONFIRMATION_DEPTH, V1_REORG_RETENTION};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
