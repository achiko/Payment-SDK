//! Payment-facing deposit, accounting, and collection contracts.
//!
//! This package may classify IX facts because it owns business records. IX and
//! concrete chains must never depend on it.

mod accounting;
mod classification;
mod collection;
mod deposit;
mod error;
mod event_log;
mod persistent;
mod reconciliation;
mod store;
mod watch_registration;

pub use accounting::{
    AccountingCommand, ApplyResult, DepositBalances, DepositLedger, LedgerEntry, LedgerEntryCause,
    LedgerEntryId, LedgerObservationKind, LedgerPage, LedgerPageRequest, OpenLedger, ProjectionId,
    RecordObservationBalance,
};
pub use classification::{ClassifiedMovement, ObservationClassification, ObservationClassifier};
pub use collection::{
    Collection, CollectionAllocation, CollectionId, CollectionLeg, CollectionLegId,
    CollectionLegKind, CollectionLegState, CollectionMode, CollectionReservation,
};
pub use deposit::{CreateDeposit, Deposit, DepositId, DepositState, IdempotencyKey, UserId};
pub use error::{DepositError, DepositErrorKind};
pub use event_log::{
    AppendObservation, AppendOutcome, ConsumerCheckpoint, ConsumerCheckpointName,
    MirrorObservation, MirrorOutcome, MirroredObservation, ObservationConsumerCheckpoints,
    ObservationEventLog, ObservationLogPage, ObservationLogRequest, ProjectObservation,
    ProjectionOutcome,
};
pub use persistent::PersistentPaymentRepository;
pub use reconciliation::{
    ReconciliationCase, ReconciliationCaseId, ReconciliationPage, ReconciliationPageRequest,
    ReconciliationReason, ReconciliationState, ReconciliationStore,
};
pub use store::{
    AwaitingWatchPage, AwaitingWatchPageRequest, CollectionStore, CreateDepositWithLedger,
    CreatedDeposit, DepositStore,
};
pub use watch_registration::{
    DepositAddressRequest, DepositAddressSource, DepositIndexerClient, DepositWatchCoordinator,
    GeneratedDepositAddress, RegisterDeposit,
};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
