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
mod store;

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
    AppendObservation, AppendOutcome, MirroredObservation, ObservationEventLog, ObservationLogPage,
    ObservationLogRequest,
};
pub use store::{CollectionStore, DepositStore};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
