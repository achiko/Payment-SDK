use crate::{BoxFuture, DepositError, DepositId};
use chain_identity::AtomicAmount;
use indexing::ObservationEventId;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReconciliationCaseId(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconciliationReason {
    /// A canonical correction reduced confirmation-qualified value after PS had
    /// already credited the user. `accounted` remains an audit fact until an
    /// explicit operator command resolves the business liability.
    PostCreditReorg {
        accounted: AtomicAmount,
        corrected_confirmed: AtomicAmount,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconciliationState {
    Open,
    Resolved {
        resolution: String,
        resolved_at: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationCase {
    pub id: ReconciliationCaseId,
    pub deposit_id: DepositId,
    pub triggering_event_id: ObservationEventId,
    pub reason: ReconciliationReason,
    pub state: ReconciliationState,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationPageRequest {
    pub deposit_id: Option<DepositId>,
    pub after: Option<ReconciliationCaseId>,
    pub limit: usize,
    pub open_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationPage {
    pub cases: Vec<ReconciliationCase>,
    pub next: Option<ReconciliationCaseId>,
}

pub trait ReconciliationStore: Send + Sync {
    /// Idempotently creates a case by case ID. A different payload for an
    /// existing ID is a conflict.
    fn open_case<'a>(
        &'a self,
        case: ReconciliationCase,
    ) -> BoxFuture<'a, Result<ReconciliationCase, DepositError>>;

    fn case<'a>(
        &'a self,
        id: &'a ReconciliationCaseId,
    ) -> BoxFuture<'a, Result<Option<ReconciliationCase>, DepositError>>;

    fn cases<'a>(
        &'a self,
        request: ReconciliationPageRequest,
    ) -> BoxFuture<'a, Result<ReconciliationPage, DepositError>>;

    fn resolve_case<'a>(
        &'a self,
        id: &'a ReconciliationCaseId,
        resolution: String,
        resolved_at: u64,
    ) -> BoxFuture<'a, Result<ReconciliationCase, DepositError>>;

    /// Automatic accounting and collection are blocked while this is true.
    fn automatic_actions_blocked<'a>(
        &'a self,
        deposit_id: &'a DepositId,
    ) -> BoxFuture<'a, Result<bool, DepositError>>;
}
