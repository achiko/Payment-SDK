use std::collections::{BTreeMap, BTreeSet};

mod access;
mod backfill;
mod backfill_state;
mod commit;
mod confirmation;
mod mechanics;
mod prepare;
mod projection_state;
mod publish;
mod query;
mod rebuild;
mod revert;
mod transition;
mod watch;

use crate::{CanonicalAddress, TransactionRef};
use bincode::{Decode, Encode, config};
use storage::{
    Condition, Error, ErrorKind, Key, Operation, ScanRequest, Store, Value, Version, WriteBatch,
};

use super::{
    BASE_GENERATION, IndexRecordCodec, Repository, keys,
    record::{
        self, BackfillMarker, BackfillRecord, BackfillRollback, BlockRecord, BundleChange,
        BundleRecord, CounterRecord, CurrentObservation, EventPointer, EventRecord, HeightMarker,
        ObservationRecord, PendingConfirmation, RebuildRecord, RepositoryMeta, ScopedValue,
        SyncRecord, WatchIdentity, WatchRecord,
    },
};
use crate::{
    AbortRebuild, AddressQuery, BackfillOutcome, BackfillReader, BackfillWriter, BeginRebuild,
    BlockHeight, BlockOutcome, BlockRef, CanonicalReader, ChainWriter, CleanupGeneration,
    CleanupOutcome, CommitBackfill, CommitBlock, ConfirmationProof, DeactivateWatch, EventCursor,
    EventPage, EventQuery, EventReader, HistoryQuery, IndexError, IndexErrorKind, IndexScope,
    IndexTypes, ObservationDraft, ObservationDraftStatus, ObservationRevision, ObservedTransaction,
    PrepareActivation, ProjectionBatch, ProjectionCursor, ProjectionEntry, ProjectionGet,
    ProjectionMutation, ProjectionPage, ProjectionQuery, ProjectionResult, ProjectionScan,
    ProjectionSnapshot, RebuildActivation, RebuildAdmin, RebuildBlock, RebuildBuilder,
    RebuildGeneration, RebuildPhase, RebuildPublisher, RebuildReader, RebuildState,
    RebuildValidation, RegisterWatch, RevertOutcome, RevertTip, StatusStore, SyncPhase, SyncStatus,
    TransactionPage, TransactionQuery, TransactionReader, TransactionStatus, UnwatchOutcome,
    WatchBackfill, WatchId, WatchLookup, WatchOutcome, WatchReader, WatchReceipt, WatchSelector,
    WatchSnapshot, WatchStore, WatchTarget, WatchVersion,
};

// This identifies the only repository layout this greenfield adapter accepts.
// It is an incompatibility guard, not a supported schema-generation API.
const REPOSITORY_FORMAT: u16 = 1;
const SCAN_CHUNK: usize = 512;
const MAX_QUERY_PAGE: usize = 1_000;

pub(super) struct StoredRecord<T> {
    value: T,
    version: Version,
}

struct Transition {
    prior: Option<CurrentObservation>,
    prior_version: Option<Version>,
    next: CurrentObservation,
    included_here: bool,
    // Rebuild corrections can use an active-generation observation as their
    // semantic prior even when no corresponding shadow index exists yet.
    prior_indexed_in_generation: bool,
}

type PreviousMarker = (
    Key,
    StoredRecord<BackfillMarker>,
    Key,
    StoredRecord<HeightMarker>,
);
