use crate::{
    ApplyResult, BoxFuture, Collection, CollectionId, DepositError, DepositId, LegId,
    ReconciliationCase, RecordObservation, TransitionGuard, UtxoBatchProjectionTransition,
};
use indexing::TransactionRef;
use indexing::{EventCursor, EventId, ObservationEvent};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredObservation {
    pub event: ObservationEvent,
    pub received_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendObservation {
    pub observation: MirroredObservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended,
    AlreadyPresent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogQuery {
    pub after: Option<EventCursor>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogPage {
    pub observations: Vec<MirroredObservation>,
    pub next: Option<EventCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositFilter {
    pub deposit_id: DepositId,
    pub after: Option<EventCursor>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositEvents {
    pub observations: Vec<MirroredObservation>,
    pub next: Option<EventCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConsumerCheckpointName {
    IxIngestion,
    IxProjection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsumerCheckpoint {
    pub name: ConsumerCheckpointName,
    pub cursor: Option<EventCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorObservation {
    /// The expected durable ingestion cursor. A stale value conflicts without
    /// appending the mirror row or moving the cursor.
    pub expected_cursor: Option<EventCursor>,
    pub observation: MirroredObservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MirrorOutcome {
    Appended { cursor: EventCursor },
    AlreadyPresent { cursor: EventCursor },
}

/// Determines whether a mirrored transaction fee is an independent debit or
/// is already contained in an input-based movement effect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProjectionFeeTreatment {
    /// Apply the factual fee separately when the mirrored payer and asset
    /// identify the projected deposit.
    #[default]
    Separate,
    /// Do not apply a second fee debit because the factual input or net-input
    /// movement amount already includes it. Persistence validates that this is
    /// used only with an input-derived debit for any fee-paying deposit.
    IncludedInMovementEffect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectObservation {
    pub expected_cursor: Option<EventCursor>,
    pub through: EventCursor,
    /// Every deposit for which this IX fact is relevant, including facts such
    /// as token gas funding that intentionally do not change the token ledger.
    pub affected_deposits: Vec<DepositId>,
    /// One mirrored IX event may affect multiple deposits. Every ledger append
    /// and reconciliation case must commit with the projection cursor.
    pub ledger_updates: Vec<RecordObservation>,
    pub reconciliation_cases: Vec<ReconciliationCase>,
    pub fee_treatment: ProjectionFeeTreatment,
    /// Optional collection aggregate mutation committed in the exact same
    /// storage batch as ledger rows, deposit-event indexes, reconciliation,
    /// and cursor movement. Generic/account-model projection leaves this
    /// empty; callers should prefer `project_utxo_batch_and_advance` when set.
    pub utxo_batch_transition: Option<BatchMutation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchMutation {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected: TransitionGuard,
    pub transaction_id: TransactionRef,
    pub transition: UtxoBatchProjectionTransition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionOutcome {
    pub checkpoint: ConsumerCheckpoint,
    pub ledger_results: Vec<ApplyResult>,
    pub reconciliation_cases: Vec<ReconciliationCase>,
}

/// Couples a UTXO collection lifecycle transition to every PS semantic
/// effect of the same mirrored IX fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectBatch {
    pub projection: ProjectObservation,
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected: TransitionGuard,
    pub transaction_id: TransactionRef,
    pub transition: UtxoBatchProjectionTransition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchOutcome {
    pub projection: ProjectionOutcome,
    pub collection: Collection,
}

/// PS-owned append-only mirror. IX owns the source facts; this log owns what PS received.
pub trait EventWriter: Send + Sync {
    fn append<'a>(
        &'a self,
        command: AppendObservation,
    ) -> BoxFuture<'a, Result<AppendOutcome, DepositError>>;
}

pub trait EventReader: Send + Sync {
    fn observation<'a>(
        &'a self,
        event_id: &'a EventId,
    ) -> BoxFuture<'a, Result<Option<MirroredObservation>, DepositError>>;

    fn observations<'a>(
        &'a self,
        request: LogQuery,
    ) -> BoxFuture<'a, Result<LogPage, DepositError>>;

    fn observations_for_deposit<'a>(
        &'a self,
        request: DepositFilter,
    ) -> BoxFuture<'a, Result<DepositEvents, DepositError>>;
}

pub trait EventLog: EventWriter + EventReader {}

impl<T> EventLog for T where T: EventWriter + EventReader {}

/// Durable two-stage PS consumer progress. Mirroring and its cursor movement
/// are one transaction. Projection advances independently only after all
/// semantic ledger effects for the mirrored event commit.
pub trait ProgressReader: Send + Sync {
    fn consumer_checkpoint<'a>(
        &'a self,
        name: ConsumerCheckpointName,
    ) -> BoxFuture<'a, Result<ConsumerCheckpoint, DepositError>>;
}

pub trait EventProjector: Send + Sync {
    fn mirror_and_advance<'a>(
        &'a self,
        command: MirrorObservation,
    ) -> BoxFuture<'a, Result<MirrorOutcome, DepositError>>;

    fn project_and_advance<'a>(
        &'a self,
        command: ProjectObservation,
    ) -> BoxFuture<'a, Result<ProjectionOutcome, DepositError>>;

    fn project_utxo_batch_and_advance<'a>(
        &'a self,
        command: ProjectBatch,
    ) -> BoxFuture<'a, Result<BatchOutcome, DepositError>>;
}

pub trait ConsumerProgress: ProgressReader + EventProjector {}

impl<T> ConsumerProgress for T where T: ProgressReader + EventProjector {}
