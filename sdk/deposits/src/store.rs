use crate::{
    BoxFuture, Collection, CollectionId, CollectionLegId, CollectionLegState, CreateDeposit,
    Deposit, DepositError, DepositId, DepositState, IdempotencyKey, LedgerEntry,
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

    fn set_state<'a>(
        &'a self,
        id: &'a DepositId,
        state: DepositState,
    ) -> BoxFuture<'a, Result<(), DepositError>>;

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

pub trait CollectionStore: Send + Sync {
    fn create_collection<'a>(
        &'a self,
        collection: Collection,
    ) -> BoxFuture<'a, Result<(), DepositError>>;

    fn collection<'a>(
        &'a self,
        id: &'a CollectionId,
    ) -> BoxFuture<'a, Result<Option<Collection>, DepositError>>;

    fn set_leg_state<'a>(
        &'a self,
        collection_id: &'a CollectionId,
        leg_id: &'a CollectionLegId,
        state: CollectionLegState,
    ) -> BoxFuture<'a, Result<(), DepositError>>;
}
