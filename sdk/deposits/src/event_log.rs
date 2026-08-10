use crate::{
    ApplyResult, BoxFuture, Collection, CollectionId, CollectionLegId, CollectionTransitionGuard,
    DepositError, DepositId, ReconciliationCase, RecordObservation, UtxoBatchProjectionTransition,
};
use chain_identity::CanonicalTransactionId;
use indexing::{EventCursor, ObservationEvent, ObservationEventId};

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
pub struct ObservationLogRequest {
    pub after: Option<EventCursor>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationLogPage {
    pub observations: Vec<MirroredObservation>,
    pub next: Option<EventCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositObservationLogRequest {
    pub deposit_id: DepositId,
    pub after: Option<EventCursor>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositObservationLogPage {
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
    pub utxo_batch_transition: Option<UtxoBatchProjectionMutation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtxoBatchProjectionMutation {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected: CollectionTransitionGuard,
    pub transaction_id: CanonicalTransactionId,
    pub transition: UtxoBatchProjectionTransition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionOutcome {
    pub checkpoint: ConsumerCheckpoint,
    pub ledger_results: Vec<ApplyResult>,
    pub reconciliation_cases: Vec<ReconciliationCase>,
}

/// Couples a Bitcoin collection lifecycle transition to every PS semantic
/// effect of the same mirrored IX fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectUtxoBatchCollection {
    pub projection: ProjectObservation,
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected: CollectionTransitionGuard,
    pub transaction_id: CanonicalTransactionId,
    pub transition: UtxoBatchProjectionTransition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtxoBatchProjectionOutcome {
    pub projection: ProjectionOutcome,
    pub collection: Collection,
}

/// PS-owned append-only mirror. IX owns the source facts; this log owns what PS received.
pub trait ObservationEventLog: Send + Sync {
    fn append<'a>(
        &'a self,
        command: AppendObservation,
    ) -> BoxFuture<'a, Result<AppendOutcome, DepositError>>;

    fn observation<'a>(
        &'a self,
        event_id: &'a ObservationEventId,
    ) -> BoxFuture<'a, Result<Option<MirroredObservation>, DepositError>>;

    fn observations<'a>(
        &'a self,
        request: ObservationLogRequest,
    ) -> BoxFuture<'a, Result<ObservationLogPage, DepositError>>;

    fn observations_for_deposit<'a>(
        &'a self,
        request: DepositObservationLogRequest,
    ) -> BoxFuture<'a, Result<DepositObservationLogPage, DepositError>>;
}

/// Durable two-stage PS consumer progress. Mirroring and its cursor movement
/// are one transaction. Projection advances independently only after all
/// semantic ledger effects for the mirrored event commit.
pub trait ObservationConsumerCheckpoints: Send + Sync {
    fn consumer_checkpoint<'a>(
        &'a self,
        name: ConsumerCheckpointName,
    ) -> BoxFuture<'a, Result<ConsumerCheckpoint, DepositError>>;

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
        command: ProjectUtxoBatchCollection,
    ) -> BoxFuture<'a, Result<UtxoBatchProjectionOutcome, DepositError>>;
}
