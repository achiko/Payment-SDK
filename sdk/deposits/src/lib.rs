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
mod job;
mod metadata;
mod migration;
mod persistent;
mod persistent_collections;
mod persistent_jobs;
mod reconciliation;
mod store;
mod user;
mod watch_registration;

pub use accounting::{
    AccountingCommand, ApplyResult, BalanceDirection, DepositBalances, DepositLedger,
    LedgerArithmeticOperation, LedgerBalanceField, LedgerEffect, LedgerEntry, LedgerEntryCause,
    LedgerEntryId, LedgerObservationKind, LedgerObservationTransition, LedgerPage,
    LedgerPageRequest, LedgerTransitionError, ObservationLedgerEffect, OpenLedger, ProjectionId,
    RecordObservation, apply_observation_transition,
};
pub use classification::{ClassifiedMovement, ObservationClassification, ObservationClassifier};
pub use collection::{
    AcceptCollectionBroadcast, AttachCollectionWatch, Collection, CollectionAllocation,
    CollectionId, CollectionLeg, CollectionLegId, CollectionLegKind, CollectionLegReference,
    CollectionLegState, CollectionMode, CollectionPage, CollectionPageRequest,
    CollectionReservation, CollectionReservationState, CollectionState, CollectionStore,
    CollectionTransitionGuard, ConfirmCollectionLeg, CreateCollection, CreateCollectionLeg,
    CreateCollectionOutcome, FailCollectionLeg, MAX_SIGNED_ENVELOPE_BYTES,
    RecordSignedCollectionLeg, ReleaseCollectionReservation, ReorgCollectionLeg,
    ReservationReleaseReason, RetryCollectionLeg, SafeCollectionError, SignedCollectionEnvelope,
    SignedEnvelopeBytes,
};
pub use deposit::{
    CreateDeposit, Deposit, DepositId, DepositState, DepositStateKind, IdempotencyKey,
    LEGACY_DEPOSIT_KEY_PURPOSE, UserId,
};
pub use error::{DepositError, DepositErrorKind};
pub use event_log::{
    AppendObservation, AppendOutcome, ConsumerCheckpoint, ConsumerCheckpointName,
    DepositObservationLogPage, DepositObservationLogRequest, MirrorObservation, MirrorOutcome,
    MirroredObservation, ObservationConsumerCheckpoints, ObservationEventLog, ObservationLogPage,
    ObservationLogRequest, ProjectObservation, ProjectionOutcome,
};
pub use job::{
    ClaimJob, CloseDepositJob, CommandIdentity, CommandOperation, CommandPrincipal,
    CreateCollectionJob, CreateDepositJob, CreateJob, CreateJobOutcome, Job, JobError, JobId,
    JobKind, JobPage, JobPageRequest, JobPayload, JobResource, JobState, JobStateKind, JobStore,
    RequestHash, RetryCollectionJob, TransitionJob,
};
pub use metadata::{
    InitializePaymentDatabase, MigratePaymentDatabase, PAYMENT_DOMAIN_SCHEMA_VERSION,
    PAYMENT_SERVICE_OWNER, PaymentDatabaseMetadata, PaymentDatabaseMetadataStore,
    PaymentDatabaseMigrationReport, PolicyIdentity,
};
pub use persistent::PersistentPaymentRepository;
pub use reconciliation::{
    ReconciliationCase, ReconciliationCaseId, ReconciliationDecision, ReconciliationPage,
    ReconciliationPageRequest, ReconciliationReason, ReconciliationResolution, ReconciliationState,
    ReconciliationStore, ResolveReconciliation,
};
pub use store::{
    AwaitingWatchPage, AwaitingWatchPageRequest, CloseDeposit, CreateDepositWithLedger,
    CreatedDeposit, DepositIndexRebuild, DepositIndexRebuildRequest, DepositPage,
    DepositPageRequest, DepositStore,
};
pub use user::{EnsureUser, User, UserStore};
pub use watch_registration::{
    DepositAddressRequest, DepositAddressSource, DepositIndexerClient, DepositWatchCoordinator,
    GeneratedDepositAddress, RegisterDeposit,
};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
