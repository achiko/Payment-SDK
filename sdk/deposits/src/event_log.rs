use crate::{ApplyResult, BoxFuture, DepositError, ReconciliationCase, RecordObservationBalance};
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectObservation {
    pub expected_cursor: Option<EventCursor>,
    pub through: EventCursor,
    /// One mirrored IX event may affect multiple deposits. Every ledger append
    /// and reconciliation case must commit with the projection cursor.
    pub ledger_updates: Vec<RecordObservationBalance>,
    pub reconciliation_cases: Vec<ReconciliationCase>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionOutcome {
    pub checkpoint: ConsumerCheckpoint,
    pub ledger_results: Vec<ApplyResult>,
    pub reconciliation_cases: Vec<ReconciliationCase>,
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
}
