use crate::{BoxFuture, DepositError, DepositId, IdempotencyKey};
use chain_identity::AtomicAmount;
use indexing::{MovementId, ObservationEventId, ObservationRevision, TransactionStatus};

/// Absolute balances after one ledger transition. Every ledger row stores the
/// complete snapshot; these are not deltas or mutable columns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DepositBalances {
    /// Canonically included incoming value, even if not deep enough yet.
    pub received: AtomicAmount,
    /// Subset of `received` that has satisfied IX confirmation/finality policy.
    pub confirmed: AtomicAmount,
    /// Current canonical on-chain value at the deposit address for this asset.
    pub balance: AtomicAmount,
    /// Confirmed gross value removed from the deposit by PS-owned collections.
    pub collected: AtomicAmount,
    /// Value credited to the user's business account by an explicit PS decision.
    pub accounted: AtomicAmount,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectionId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LedgerEntryId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerObservationKind {
    Incoming,
    Collection,
    GasFunding,
    OtherBalanceChange,
    Reorg,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerEntryCause {
    Opened {
        idempotency_key: IdempotencyKey,
    },
    Observation {
        projection_id: ProjectionId,
        event_id: ObservationEventId,
        observation_revision: ObservationRevision,
        status: TransactionStatus,
        kind: LedgerObservationKind,
        /// Stable pointers into the mirrored IX fact that changed this deposit.
        movement_ids: Vec<MovementId>,
    },
    Accounting {
        idempotency_key: IdempotencyKey,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenLedger {
    pub idempotency_key: IdempotencyKey,
    pub deposit_id: DepositId,
    pub recorded_at: u64,
}

/// Immutable PS ledger row. `previous` makes the per-deposit journal a
/// verifiable sequence and supplies an optimistic-concurrency boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    pub id: LedgerEntryId,
    pub deposit_id: DepositId,
    pub previous: Option<LedgerEntryId>,
    pub cause: LedgerEntryCause,
    pub balances: DepositBalances,
    pub recorded_at: u64,
}

/// Projection result calculated from a classified IX event. The store appends
/// `next_balances` only if `expected_head` is still the current ledger row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordObservationBalance {
    pub projection_id: ProjectionId,
    pub event_id: ObservationEventId,
    pub observation_revision: ObservationRevision,
    pub status: TransactionStatus,
    pub kind: LedgerObservationKind,
    pub movement_ids: Vec<MovementId>,
    pub deposit_id: DepositId,
    pub expected_head: Option<LedgerEntryId>,
    pub next_balances: DepositBalances,
    pub recorded_at: u64,
}

/// The ledger copies all on-chain balances from the current head and changes
/// only `accounted`, then appends a new absolute snapshot row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountingCommand {
    pub idempotency_key: IdempotencyKey,
    pub deposit_id: DepositId,
    pub expected_head: Option<LedgerEntryId>,
    pub next_accounted: AtomicAmount,
    pub recorded_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyResult {
    Appended { entry: LedgerEntry },
    AlreadyPresent { entry: LedgerEntry },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerPageRequest {
    pub deposit_id: DepositId,
    pub after: Option<LedgerEntryId>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerPage {
    pub entries: Vec<LedgerEntry>,
    pub next: Option<LedgerEntryId>,
}

pub trait DepositLedger: Send + Sync {
    /// Idempotently creates the zero-balance first row for a persisted deposit.
    fn open<'a>(&'a self, command: OpenLedger) -> BoxFuture<'a, Result<ApplyResult, DepositError>>;

    fn current<'a>(
        &'a self,
        deposit_id: &'a DepositId,
    ) -> BoxFuture<'a, Result<Option<LedgerEntry>, DepositError>>;

    fn entries<'a>(
        &'a self,
        request: LedgerPageRequest,
    ) -> BoxFuture<'a, Result<LedgerPage, DepositError>>;

    /// Appends the absolute snapshot. Implementations must preserve `accounted`
    /// for observation rows, apply optimistic head matching, and reject changes
    /// inconsistent with the supplied IX status and observation kind.
    fn record_observation<'a>(
        &'a self,
        command: RecordObservationBalance,
    ) -> BoxFuture<'a, Result<ApplyResult, DepositError>>;

    /// Appends a new absolute row by copying on-chain fields from the current
    /// head and changing only `accounted`. A positive value must not exceed the
    /// confirmation-qualified amount at authorization time.
    fn record_accounting<'a>(
        &'a self,
        command: AccountingCommand,
    ) -> BoxFuture<'a, Result<ApplyResult, DepositError>>;
}
