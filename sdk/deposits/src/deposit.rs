use base::Decimal;
use base::DerivationPath;
use indexing::{AssetId, BlockHeight, CanonicalAddress, WatchId};

/// Opaque application-owned reference to key material.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyId {
    Identifier(String),
    DerivationPath(DerivationPath),
}

/// Explicit marker assigned when decoding a version-1 deposit row that
/// predates durable key-purpose storage. It is metadata only and must never be
/// interpreted as a custody instruction for a new operation.

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
    /// The IX watch remains active so late payments stay observable.
    Expired {
        watch_id: WatchId,
    },
    Closed,
}

impl DepositState {
    /// Returns the durable IX watch while the deposit remains observable.
    #[must_use]
    pub const fn watch_id(&self) -> Option<&WatchId> {
        match self {
            Self::Active { watch_id } | Self::Expired { watch_id } => Some(watch_id),
            Self::AwaitingWatch | Self::Closed => None,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> DepositStateKind {
        match self {
            Self::AwaitingWatch => DepositStateKind::AwaitingWatch,
            Self::Active { .. } => DepositStateKind::Active,
            Self::Expired { .. } => DepositStateKind::Expired,
            Self::Closed => DepositStateKind::Closed,
        }
    }
}

/// Watch-ID-independent lifecycle discriminator used by durable list filters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepositStateKind {
    AwaitingWatch,
    Active,
    Expired,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deposit {
    pub id: DepositId,
    pub idempotency_key: IdempotencyKey,
    pub user_id: UserId,
    pub asset: AssetId,
    pub address: CanonicalAddress,
    pub key: KeyId,
    /// Opaque custody/provisioning purpose metadata. Never secret material.
    pub key_purpose: String,
    pub expected: Decimal,
    pub birthday: BlockHeight,
    pub expires_at: u64,
    pub state: DepositState,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositPlan {
    pub id: DepositId,
    pub idempotency_key: IdempotencyKey,
    pub user_id: UserId,
    pub asset: AssetId,
    pub address: CanonicalAddress,
    pub key: KeyId,
    /// Opaque custody/provisioning purpose metadata. Never secret material.
    pub key_purpose: String,
    pub expected: Decimal,
    pub birthday: BlockHeight,
    pub expires_at: u64,
    pub created_at: u64,
}
