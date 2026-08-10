use crate::{BlockHeight, BlockRef, IndexScope, WatchId};
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObservationRevision(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObservationEventId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventCursor(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MovementId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MovementKind {
    /// Account-model or explicitly parsed value transfer.
    Transfer,
    /// UTXO consumed by this transaction.
    Input,
    /// UTXO created by this transaction.
    Output,
    InternalTransfer,
    Mint,
    Burn,
}

/// A transaction can contain many movements. Optional endpoints avoid inventing
/// a false one-to-one `from -> to` relationship for UTXO transactions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueMovement {
    pub id: MovementId,
    pub asset: AssetId,
    pub amount: AtomicAmount,
    pub from: Option<CanonicalAddress>,
    pub to: Option<CanonicalAddress>,
    pub kind: MovementKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkFee {
    pub asset: AssetId,
    pub amount: AtomicAmount,
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
        by: CanonicalTransactionId,
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
    pub transaction_id: CanonicalTransactionId,
    pub revision: ObservationRevision,
    pub status: TransactionStatus,
    pub movements: Vec<ValueMovement>,
    pub fee: Option<NetworkFee>,
    pub first_seen_at: u64,
    pub observed_at: u64,
}

/// A revision, not `(txid, status)`, is the idempotency boundary. The same status
/// can legitimately occur again after a reorg and re-inclusion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationEvent {
    pub id: ObservationEventId,
    pub cursor: EventCursor,
    pub watch_ids: Vec<WatchId>,
    pub previous_status: Option<TransactionStatus>,
    pub transaction: ObservedTransaction,
}

/// Chain-interpreted state before the repository assigns durable identity.
///
/// Revisions, event IDs, cursors, and previous state are deliberately absent:
/// the repository allocates all four in the same atomic block commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationDraft {
    pub scope: IndexScope,
    pub transaction_id: CanonicalTransactionId,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchSelector {
    Address(CanonicalAddress),
    Transaction(CanonicalTransactionId),
}

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
    pub inactive_from: Option<BlockHeight>,
    pub confirmation_policy: ConfirmationPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterWatchCommand<T> {
    pub request: WatchRequest,
    pub target: T,
    pub registered_at: Option<BlockRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterWatchOutcome {
    Registered(WatchReceipt),
    Existing(WatchReceipt),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnwatchCommand {
    pub scope: IndexScope,
    pub watch_id: WatchId,
    pub inactive_from: BlockHeight,
    pub expected_checkpoint: Option<BlockRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnwatchOutcome {
    Deactivated,
    AlreadyInactive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionRequest {
    pub scope: IndexScope,
    pub transaction_id: CanonicalTransactionId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionPageRequest {
    pub scope: IndexScope,
    pub address: CanonicalAddress,
    pub after: Option<CanonicalTransactionId>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressWatchRequest {
    pub scope: IndexScope,
    pub address: CanonicalAddress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionPage {
    pub transactions: Vec<ObservedTransaction>,
    pub next: Option<CanonicalTransactionId>,
}

/// IX store query used after every checkpoint advance, including blocks with no
/// watched movements, to find inclusions that have just reached proof depth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalityScanRequest {
    pub scope: crate::IndexScope,
    pub included_through: BlockHeight,
    pub after: Option<CanonicalTransactionId>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalityScanPage {
    pub transactions: Vec<ObservedTransaction>,
    pub next: Option<CanonicalTransactionId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationEventRequest {
    pub scope: IndexScope,
    pub after: Option<EventCursor>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationEventPage {
    pub events: Vec<ObservationEvent>,
    pub next: Option<EventCursor>,
}
