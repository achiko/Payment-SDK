use base::Decimal;

use crate::{AssetId, CanonicalAddress, TransactionRef};
use crate::{BlockHeight, BlockRef, IndexScope, WatchId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObservationRevision(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MovementId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MovementKind {
    Transfer,
    Input,
    Output,
    InternalTransfer,
    Mint,
    Burn,
}

/// One independently meaningful value change within a transaction.
///
/// UTXO inputs and outputs are intentionally separate movements. Indexing never
/// invents a one-to-one transfer between them because a transaction can consume
/// and create any number of outputs. The variants also make impossible endpoint
/// combinations unrepresentable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueMovement {
    Transfer {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        from: CanonicalAddress,
        to: CanonicalAddress,
    },
    Input {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        owner: Option<CanonicalAddress>,
    },
    Output {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        owner: Option<CanonicalAddress>,
    },
    InternalTransfer {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        from: CanonicalAddress,
        to: CanonicalAddress,
    },
    Mint {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        to: CanonicalAddress,
    },
    Burn {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        from: CanonicalAddress,
    },
}

impl ValueMovement {
    #[must_use]
    pub fn id(&self) -> &MovementId {
        match self {
            Self::Transfer { id, .. }
            | Self::Input { id, .. }
            | Self::Output { id, .. }
            | Self::InternalTransfer { id, .. }
            | Self::Mint { id, .. }
            | Self::Burn { id, .. } => id,
        }
    }

    #[must_use]
    pub fn asset(&self) -> &AssetId {
        match self {
            Self::Transfer { asset, .. }
            | Self::Input { asset, .. }
            | Self::Output { asset, .. }
            | Self::InternalTransfer { asset, .. }
            | Self::Mint { asset, .. }
            | Self::Burn { asset, .. } => asset,
        }
    }

    #[must_use]
    pub fn amount(&self) -> &Decimal {
        match self {
            Self::Transfer { amount, .. }
            | Self::Input { amount, .. }
            | Self::Output { amount, .. }
            | Self::InternalTransfer { amount, .. }
            | Self::Mint { amount, .. }
            | Self::Burn { amount, .. } => amount,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> MovementKind {
        match self {
            Self::Transfer { .. } => MovementKind::Transfer,
            Self::Input { .. } => MovementKind::Input,
            Self::Output { .. } => MovementKind::Output,
            Self::InternalTransfer { .. } => MovementKind::InternalTransfer,
            Self::Mint { .. } => MovementKind::Mint,
            Self::Burn { .. } => MovementKind::Burn,
        }
    }

    #[must_use]
    pub fn from(&self) -> Option<&CanonicalAddress> {
        match self {
            Self::Transfer { from, .. }
            | Self::InternalTransfer { from, .. }
            | Self::Burn { from, .. } => Some(from),
            Self::Input { owner, .. } => owner.as_ref(),
            Self::Output { .. } | Self::Mint { .. } => None,
        }
    }

    #[must_use]
    pub fn to(&self) -> Option<&CanonicalAddress> {
        match self {
            Self::Transfer { to, .. }
            | Self::InternalTransfer { to, .. }
            | Self::Mint { to, .. } => Some(to),
            Self::Output { owner, .. } => owner.as_ref(),
            Self::Input { .. } | Self::Burn { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkFee {
    pub asset: AssetId,
    pub amount: Decimal,
    pub payer: Option<CanonicalAddress>,
}

/// IX configuration for one chain/network scope. A transaction cannot become
/// `Confirmed` until this policy is proven against the persisted canonical tip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfirmationPolicy {
    pub minimum_confirmations: u64,
    /// For chains exposing a finalized checkpoint, depth alone is insufficient.
    pub require_chain_finality: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmationProof {
    Depth { required: u64, observed: u64 },
    ChainFinalized,
    DepthAndChainFinalized { required: u64, observed: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    Pending,
    /// Canonically included, but not yet deep/final enough for business accounting.
    Included {
        block: BlockRef,
        confirmations: u64,
    },
    Confirmed {
        block: BlockRef,
        proof: ConfirmationProof,
    },
    Failed {
        block: Option<BlockRef>,
        reason: Option<String>,
    },
    Replaced {
        by: TransactionRef,
    },
    Dropped,
    Reorged {
        previous_block: BlockRef,
    },
}

/// IX fact only. It deliberately contains no deposit, user, incoming, or sweep label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedTransaction {
    pub scope: IndexScope,
    pub transaction_id: TransactionRef,
    pub revision: ObservationRevision,
    pub status: TransactionStatus,
    pub movements: Vec<ValueMovement>,
    pub fee: Option<NetworkFee>,
    pub first_seen_at: u64,
    pub observed_at: u64,
}

/// Chain-interpreted state before the repository assigns durable identity.
///
/// Revisions, event IDs, cursors, and previous state are deliberately absent:
/// the repository allocates all four in the same atomic block commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationDraft {
    pub scope: IndexScope,
    pub transaction_id: TransactionRef,
    pub status: ObservationDraftStatus,
    pub movements: Vec<ValueMovement>,
    pub fee: Option<NetworkFee>,
    pub watch_ids: Vec<WatchId>,
    pub first_seen_at: u64,
    pub observed_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationDraftStatus {
    Included,
    /// A canonical failed receipt. Interpreters must emit no movements for it;
    /// the network fee may still be present.
    Failed {
        reason: Option<String>,
    },
}

pub type WatchSelector = CanonicalAddress;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchRequest {
    pub scope: IndexScope,
    pub selector: WatchSelector,
    /// First block that can contain relevant history.
    pub start_height: BlockHeight,
    /// Caller idempotency key, distinct from the IX-assigned watch ID.
    pub idempotency_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchReceipt {
    pub id: WatchId,
    pub scope: IndexScope,
    pub selector: WatchSelector,
    pub start_height: BlockHeight,
    pub registered_at: Option<BlockRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterWatch<T> {
    pub request: WatchRequest,
    pub target: T,
    pub registered_at: Option<BlockRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionQuery {
    pub scope: IndexScope,
    pub transaction_id: TransactionRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryQuery {
    pub scope: IndexScope,
    pub address: CanonicalAddress,
    pub after: Option<TransactionRef>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionPage {
    pub transactions: Vec<ObservedTransaction>,
    pub next: Option<TransactionRef>,
}
