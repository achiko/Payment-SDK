use crate::{
    BoxFuture, CreateDeposit, Deposit, DepositError, DepositId, DepositState, DepositStateKind,
    IdempotencyKey, LedgerEntry, LedgerEntryId, UserId,
};
use chain_identity::CanonicalAddress;
use indexing::WatchId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDepositWithLedger {
    pub deposit: CreateDeposit,
    /// Normally equal to the deposit creation time, but explicit so imported
    /// records and deterministic replay do not read the wall clock in storage.
    pub ledger_recorded_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedDeposit {
    pub deposit: Deposit,
    pub ledger: LedgerEntry,
}

/// Optimistic close command tied to the exact zero-balance ledger snapshot
/// used for the business eligibility decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseDeposit {
    pub deposit_id: DepositId,
    pub expected_state: DepositState,
    pub expected_ledger_head: LedgerEntryId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwaitingWatchPageRequest {
    pub after: Option<DepositId>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwaitingWatchPage {
    pub deposits: Vec<Deposit>,
    pub next: Option<DepositId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositPageRequest {
    pub after: Option<DepositId>,
    pub limit: usize,
    pub user_id: Option<UserId>,
    pub state: Option<DepositStateKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositPage {
    pub deposits: Vec<Deposit>,
    pub next: Option<DepositId>,
}

/// Bounded, restart-safe request for backfilling association indexes from
/// authoritative deposit rows created by older repository versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositIndexRebuildRequest {
    pub after: Option<DepositId>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositIndexRebuild {
    pub scanned: usize,
    pub next: Option<DepositId>,
    pub complete: bool,
}

/// Backend-independent PS persistence contract. No database engine is selected here.
pub trait DepositStore: Send + Sync {
    /// Atomically persists `AwaitingWatch` and the zero-balance first ledger
    /// row. The command idempotency key covers both records.
    fn create_with_ledger<'a>(
        &'a self,
        command: CreateDepositWithLedger,
    ) -> BoxFuture<'a, Result<CreatedDeposit, DepositError>>;

    /// Kept for callers that deliberately manage ledger creation separately.
    /// Deposit-address issuance must use `create_with_ledger`.
    fn create<'a>(&'a self, command: CreateDeposit)
    -> BoxFuture<'a, Result<Deposit, DepositError>>;

    fn deposit<'a>(
        &'a self,
        id: &'a DepositId,
    ) -> BoxFuture<'a, Result<Option<Deposit>, DepositError>>;

    fn by_address<'a>(
        &'a self,
        address: &'a CanonicalAddress,
    ) -> BoxFuture<'a, Result<Option<Deposit>, DepositError>>;

    /// Lists deposits in stable deposit-ID order. Optional user and lifecycle
    /// filters use durable association indexes and share the same cursor.
    fn deposits<'a>(
        &'a self,
        request: DepositPageRequest,
    ) -> BoxFuture<'a, Result<DepositPage, DepositError>>;

    /// Idempotently backfills all deposit association indexes. Until a full
    /// rebuild completes, filtered listing falls back to authoritative rows so
    /// legacy deposits are never silently hidden.
    fn rebuild_deposit_indexes<'a>(
        &'a self,
        request: DepositIndexRebuildRequest,
    ) -> BoxFuture<'a, Result<DepositIndexRebuild, DepositError>>;

    fn set_state<'a>(
        &'a self,
        id: &'a DepositId,
        state: DepositState,
    ) -> BoxFuture<'a, Result<(), DepositError>>;

    /// The only supported path to `Closed`. Atomically closes a zero-balance
    /// deposit while invalidating concurrent ledger projections and rejecting
    /// active reservations. The IX address watch is deliberately retained so
    /// late payments cannot become invisible.
    fn close<'a>(&'a self, command: CloseDeposit) -> BoxFuture<'a, Result<(), DepositError>>;

    fn awaiting_watch<'a>(
        &'a self,
        request: AwaitingWatchPageRequest,
    ) -> BoxFuture<'a, Result<AwaitingWatchPage, DepositError>>;

    /// Idempotently changes `AwaitingWatch` to `Active`. Repeating the same
    /// watch ID succeeds; a different watch ID conflicts.
    fn activate_watch<'a>(
        &'a self,
        id: &'a DepositId,
        idempotency_key: &'a IdempotencyKey,
        watch_id: WatchId,
    ) -> BoxFuture<'a, Result<Deposit, DepositError>>;
}
