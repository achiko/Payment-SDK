use crate::{BoxFuture, DepositError};
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
