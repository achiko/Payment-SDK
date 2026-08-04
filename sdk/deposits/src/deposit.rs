use chain_identity::{AssetId, AtomicAmount, CanonicalAddress};
use indexing::{BlockHeight, WatchId};
use signer::KeyLocator;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DepositId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdempotencyKey(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepositState {
    /// Persisted, but IX watch registration has not completed yet.
    AwaitingWatch,
    Active {
        watch_id: WatchId,
    },
    Expired,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deposit {
    pub id: DepositId,
    pub user_id: UserId,
    pub asset: AssetId,
    pub address: CanonicalAddress,
    pub key: KeyLocator,
    pub expected: AtomicAmount,
    pub birthday: BlockHeight,
    pub expires_at: u64,
    pub state: DepositState,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDeposit {
    pub id: DepositId,
    pub idempotency_key: IdempotencyKey,
    pub user_id: UserId,
    pub asset: AssetId,
    pub address: CanonicalAddress,
    pub key: KeyLocator,
    pub expected: AtomicAmount,
    pub birthday: BlockHeight,
    pub expires_at: u64,
    pub created_at: u64,
}
