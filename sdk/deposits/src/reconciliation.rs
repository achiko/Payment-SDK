use crate::{BoxFuture, CollectionId, CommandIdentity, DepositError, DepositId, EntryId};
use base::Decimal;
use indexing::EventId;
use indexing::TransactionRef;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CaseId(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconciliationReason {
    /// A canonical correction reduced confirmation-qualified value after PS had
    /// already credited the user. `accounted` remains an audit fact until an
    /// explicit operator command resolves the business liability.
    PostCreditReorg {
        accounted: Decimal,
        corrected_confirmed: Decimal,
    },
    /// A canonical debit conflicts with a PS-owned exact spend-resource
    /// reservation but is not the retained collection transaction. Automatic
    /// collection/accounting stays blocked until an operator resolves the
    /// ownership conflict.
    ReservedSpendConflict {
        collection_id: CollectionId,
        transaction_id: TransactionRef,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconciliationState {
    Open,
    /// Resolution written by the typed, idempotent PS command path.
    Resolved {
        resolution: ReconciliationResolution,
        resolved_at: u64,
    },
}

/// Explicit business decision for a post-credit reconciliation case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconciliationDecision {
    /// Reverse only the excess business credit. Persistence copies the current
    /// absolute ledger head and sets `accounted` to `min(accounted, confirmed)`.
    ReverseCredit {
        expected_head: EntryId,
        reason: String,
    },
    /// Preserve the current absolute balances and accept the liability inside
    /// the payment business.
    AcceptLiability { reason: String },
    /// Preserve balances because the excess was recorded in an external debt
    /// system. The reference is opaque to PS.
    ExternalDebtRecorded {
        external_reference: String,
        reason: String,
    },
}

/// Auditable result retained on a resolved reconciliation case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationResolution {
    pub command: CommandIdentity,
    pub decision: ReconciliationDecision,
    /// Present exactly when `decision` is `ReverseCredit`.
    pub ledger_entry_id: Option<EntryId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveReconciliation {
    /// Must use [`crate::CommandOperation::ResolveReconciliation`].
    pub command: CommandIdentity,
    pub case_id: CaseId,
    pub decision: ReconciliationDecision,
    pub resolved_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationCase {
    pub id: CaseId,
    pub deposit_id: DepositId,
    pub triggering_event_id: EventId,
    pub reason: ReconciliationReason,
    pub state: ReconciliationState,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseQuery {
    pub deposit_id: Option<DepositId>,
    pub after: Option<CaseId>,
    pub limit: usize,
    pub open_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationPage {
    pub cases: Vec<ReconciliationCase>,
    pub next: Option<CaseId>,
}

pub trait CaseOpener: Send + Sync {
    /// Idempotently creates a case by case ID. A different payload for an
    /// existing ID is a conflict.
    fn open_case<'a>(
        &'a self,
        case: ReconciliationCase,
    ) -> BoxFuture<'a, Result<ReconciliationCase, DepositError>>;
}

pub trait CaseReader: Send + Sync {
    fn case<'a>(
        &'a self,
        id: &'a CaseId,
    ) -> BoxFuture<'a, Result<Option<ReconciliationCase>, DepositError>>;

    fn cases<'a>(
        &'a self,
        request: CaseQuery,
    ) -> BoxFuture<'a, Result<ReconciliationPage, DepositError>>;
}

pub trait CaseResolver: Send + Sync {
    /// Resolves a case and, for `ReverseCredit`, appends the corrected absolute
    /// ledger row in the same atomic storage commit. Exact command replay
    /// returns the original case; scoped idempotency-key reuse with a different
    /// request hash conflicts.
    fn resolve_case<'a>(
        &'a self,
        command: ResolveReconciliation,
    ) -> BoxFuture<'a, Result<ReconciliationCase, DepositError>>;
}

pub trait ActionGuard: Send + Sync {
    /// Automatic accounting and collection are blocked while this is true.
    fn automatic_actions_blocked<'a>(
        &'a self,
        deposit_id: &'a DepositId,
    ) -> BoxFuture<'a, Result<bool, DepositError>>;
}

pub trait Reconciliations: CaseOpener + CaseReader + CaseResolver + ActionGuard {}

impl<T> Reconciliations for T where T: CaseOpener + CaseReader + CaseResolver + ActionGuard {}
