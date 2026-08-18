use std::collections::{BTreeMap, BTreeSet};

mod access;
mod commit;
mod mechanics;
mod projection_state;
mod query;
mod revert;
mod transition;
mod watch;

use crate::{CanonicalAddress, TransactionRef};
use bincode::{Decode, Encode, config};
use storage::{
    Condition, Error, ErrorKind, Key, Operation, ScanRequest, Store, Value, Version, WriteBatch,
};

use super::{
    Repository, index_record, keys,
    record::{
        self, BlockRecord, BundleChange, BundleRecord, CounterRecord, CurrentObservation,
        PendingConfirmation, RepositoryMeta, ScopedValue, SyncRecord, WatchIdentity, WatchRecord,
    },
};
use crate::{
    BlockHeight, BlockOutcome, BlockRef, BlockStore, CanonicalStore, CommitBlock, CommitContext,
    CommitPlan, HistoryQuery, HistoryStore, IndexChanges, IndexError, IndexErrorKind, IndexScope,
    IndexUndo, ObservedTransaction, PendingChange, ProjectionBatch, ProjectionCursor,
    ProjectionEntry, ProjectionGet, ProjectionMutation, ProjectionPage, ProjectionResult,
    ProjectionScan, ProjectionSnapshot, RegisterWatch, RevertBlock, RevertContext,
    RevertObservation, RevertPlan, RevertTip, StatusStore, StoredObservation, SyncPhase,
    SyncStatus, TransactionPage, TransactionQuery, WatchContext, WatchId, WatchPlan, WatchSelector,
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
    prior_addresses: BTreeSet<CanonicalAddress>,
    next_addresses: BTreeSet<CanonicalAddress>,
    pending: PendingChange,
}
