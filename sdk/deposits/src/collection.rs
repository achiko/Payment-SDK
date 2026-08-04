use crate::DepositId;
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId};
use indexing::WatchId;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollectionId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollectionLegId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionMode {
    AccountTransfer,
    UtxoBatch,
    TokenWithGas,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionLegKind {
    GasFunding,
    Sweep,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CollectionLegState {
    Required,
    /// Broadcast is recorded before IX registration so a crash can be reconciled.
    Broadcast {
        transaction_id: CanonicalTransactionId,
        watch_id: Option<WatchId>,
    },
    Confirmed {
        transaction_id: CanonicalTransactionId,
    },
    Failed {
        transaction_id: Option<CanonicalTransactionId>,
        reason: Option<String>,
    },
    Reorged {
        transaction_id: CanonicalTransactionId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionLeg {
    pub id: CollectionLegId,
    pub kind: CollectionLegKind,
    pub state: CollectionLegState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionReservation {
    pub deposit_id: DepositId,
    pub amount: AtomicAmount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionAllocation {
    pub deposit_id: DepositId,
    /// Gross amount removed from the deposit.
    pub gross_debit: AtomicAmount,
    /// Net amount the master destination actually received.
    pub master_credit: AtomicAmount,
    pub allocated_fee: AtomicAmount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub id: CollectionId,
    pub mode: CollectionMode,
    pub asset: AssetId,
    pub destination: CanonicalAddress,
    pub reservations: Vec<CollectionReservation>,
    pub legs: Vec<CollectionLeg>,
    pub allocations: Vec<CollectionAllocation>,
    pub created_at: u64,
}
