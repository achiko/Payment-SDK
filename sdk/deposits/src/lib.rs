//! Payment-facing deposit, accounting, and collection contracts.
//!
//! This package may classify IX facts because it owns business records. IX and
//! concrete chains must never depend on it.

mod accounting;
mod amount;
mod classification;
mod collection;
mod deposit;
mod error;
mod event_log;
mod job;
mod jobs;
mod metadata;
mod persistent;
mod reconciliation;
mod store;
mod user;
mod watch_registration;

pub use accounting::{
    AccountingCommand, ApplyResult, BalanceDirection, DepositBalances, DepositLedger, EntryId,
    LedgerArithmeticOperation, LedgerBalanceField, LedgerEffect, LedgerEntry, LedgerEntryCause,
    LedgerObservationKind, LedgerPage, LedgerQuery, LedgerReader, LedgerTransition,
    LedgerTransitionError, LedgerWriter, ObservationLedgerEffect, OpenLedger, ProjectionId,
    RecordObservation, apply_observation_transition,
};
pub use classification::{ClassifiedMovement, ObservationClassification, ObservationClassifier};
pub use collection::{
    AcceptBroadcast, AttachWatch, BatchParticipant, Collection, CollectionAllocation,
    CollectionCreator, CollectionError, CollectionHistory, CollectionId, CollectionLeg,
    CollectionLegKind, CollectionLegState, CollectionMode, CollectionPage, CollectionParticipant,
    CollectionPlan, CollectionQuery, CollectionReader, CollectionReservation,
    CollectionReservationState, CollectionRetry, CollectionState, Collections, ConfirmLeg,
    CreateBatch, CreateCollectionOutcome, CreateLeg, FailLeg, LegId, LegOutcome, LegRef,
    MAX_COLLECTION_PARTICIPANTS, MAX_COLLECTION_SPEND_RESOURCES, MAX_SIGNED_ENVELOPE_BYTES,
    MAX_SPEND_RESOURCE_EVIDENCE_BYTES, MAX_TOTAL_SPEND_RESOURCE_EVIDENCE_BYTES, RecordSignature,
    ReleaseReservation, ReorgLeg, ReservationReleaseReason, ResourceId, ResourceProof, RetryLeg,
    SignedBytes, SignedEnvelope, SpendResource, SubmissionWriter, TransitionGuard,
    UtxoBatchProjectionTransition,
};
pub use deposit::{
    Deposit, DepositId, DepositPlan, DepositState, DepositStateKind, IdempotencyKey, KeyId, UserId,
};
pub use error::{DepositError, DepositErrorKind};
pub use event_log::{
    AppendObservation, AppendOutcome, BatchMutation, BatchOutcome, ConsumerCheckpoint,
    ConsumerCheckpointName, ConsumerProgress, DepositEvents, DepositFilter, EventLog,
    EventProjector, EventReader, EventWriter, LogPage, LogQuery, MirrorObservation, MirrorOutcome,
    MirroredObservation, ProgressReader, ProjectBatch, ProjectObservation, ProjectionFeeTreatment,
    ProjectionOutcome,
};
pub use job::{
    BatchJob, ClaimJob, CloseJob, CollectionJob, CommandIdentity, CommandOperation,
    CommandPrincipal, CreateJobOutcome, DepositJob, Job, JobAssociations, JobCommands, JobError,
    JobId, JobKind, JobPage, JobPayload, JobPlan, JobQuery, JobReader, JobResource, JobRunner,
    JobState, JobStateKind, Jobs, RequestHash, RetryBatch, RetryJob, TransitionJob,
};
pub use metadata::{
    DatabaseIdentity, DatabaseInitializer, DatabaseMetadata, InitializeDatabase, MetadataReader,
    PAYMENT_DOMAIN_SCHEMA_VERSION, PAYMENT_SERVICE_OWNER, PolicyIdentity, PrincipalScopeMode,
};
pub use persistent::PaymentStore;
pub use reconciliation::{
    ActionGuard, CaseId, CaseOpener, CaseQuery, CaseReader, CaseResolver, ReconciliationCase,
    ReconciliationDecision, ReconciliationPage, ReconciliationReason, ReconciliationResolution,
    ReconciliationState, Reconciliations, ResolveReconciliation,
};
pub use store::{
    AwaitingPage, AwaitingQuery, CloseDeposit, CreatedDeposit, DepositCreator, DepositLifecycle,
    DepositPage, DepositQuery, DepositReader, IndexRebuild, IndexRebuilder, OpenDeposit,
    RebuildRequest, WatchQueue,
};
pub use user::{User, UserStore};
pub use watch_registration::{
    AddressRequest, DepositAddressSource, DepositIndexerClient, DepositRegistration,
    ProvisionedAddress, WatchCoordinator,
};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
