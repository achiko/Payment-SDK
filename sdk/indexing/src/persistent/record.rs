use bincode::{Decode, Encode};
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};

use crate::{
    BlockHash, BlockHeight, BlockRef, ConfirmationPolicy, ConfirmationProof, EventCursor,
    IndexError, IndexErrorKind, IndexScope, MovementId, MovementKind, NetworkFee, ObservationEvent,
    ObservationEventId, ObservationRevision, ObservedTransaction, RebuildGeneration, RebuildPhase,
    RebuildReason, RebuildState, SyncPhase, SyncStatus, TransactionStatus, ValueMovement, WatchId,
    WatchSelector,
};

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct RepositoryMetaRecordV1 {
    pub format_version: u16,
    pub scope: ScopeRecordV1,
    pub bootstrap_height: u64,
    pub confirmation_depth: u64,
    pub require_chain_finality: bool,
    pub reorg_retention: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct CounterRecordV1 {
    pub value: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct ScopeRecordV1 {
    pub chain: String,
    pub network: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct ChainValueRecordV1 {
    pub chain: String,
    pub value: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct BlockRefRecordV1 {
    pub height: u64,
    pub hash: Vec<u8>,
    pub parent_hash: Option<Vec<u8>>,
    pub timestamp: Option<u64>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct PolicyRecordV1 {
    pub minimum_confirmations: u64,
    pub require_chain_finality: bool,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct PolicyMigrationRecordV1 {
    pub version: u64,
    pub idempotency_key: String,
    pub scope: ScopeRecordV1,
    pub bootstrap_height: u64,
    pub from_confirmation_policy: PolicyRecordV1,
    pub from_reorg_retention: u64,
    pub to_confirmation_policy: PolicyRecordV1,
    pub to_reorg_retention: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct PolicyMigrationIdRecordV1 {
    pub version: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum SelectorRecordV1 {
    Address(ChainValueRecordV1),
    Transaction(ChainValueRecordV1),
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct WatchRecordV1 {
    pub id: String,
    pub scope: ScopeRecordV1,
    pub selector: SelectorRecordV1,
    pub encoded_target: Vec<u8>,
    pub idempotency_key: String,
    pub start_height: u64,
    pub registered_at: Option<BlockRefRecordV1>,
    pub inactive_from: Option<u64>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct WatchIdempotencyRecordV1 {
    pub watch_id: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct WatchBackfillRecordV1 {
    pub scope: ScopeRecordV1,
    pub watch_id: String,
    pub from_height: u64,
    pub next_height: u64,
    pub through: BlockRefRecordV1,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct WatchBackfillAppliedRecordV1 {
    pub block: BlockRefRecordV1,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct WatchBackfillAppliedHeightRecordV1 {
    pub watch_id: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum MovementKindRecordV1 {
    Transfer,
    Input,
    Output,
    InternalTransfer,
    Mint,
    Burn,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct MovementRecordV1 {
    pub id: String,
    pub asset: ChainValueRecordV1,
    pub amount: [u8; 32],
    pub from: Option<ChainValueRecordV1>,
    pub to: Option<ChainValueRecordV1>,
    pub kind: MovementKindRecordV1,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct FeeRecordV1 {
    pub asset: ChainValueRecordV1,
    pub amount: [u8; 32],
    pub payer: Option<ChainValueRecordV1>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum ConfirmationProofRecordV1 {
    Depth { required: u64, observed: u64 },
    ChainFinalized,
    DepthAndChainFinalized { required: u64, observed: u64 },
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum TransactionStatusRecordV1 {
    Pending,
    Included {
        block: BlockRefRecordV1,
        confirmations: u64,
    },
    Confirmed {
        block: BlockRefRecordV1,
        proof: ConfirmationProofRecordV1,
    },
    Failed {
        block: Option<BlockRefRecordV1>,
        reason: Option<String>,
    },
    Replaced {
        by: ChainValueRecordV1,
    },
    Dropped,
    Reorged {
        previous_block: BlockRefRecordV1,
    },
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct ObservationRecordV1 {
    pub scope: ScopeRecordV1,
    pub transaction_id: ChainValueRecordV1,
    pub revision: u64,
    pub status: TransactionStatusRecordV1,
    pub movements: Vec<MovementRecordV1>,
    pub fee: Option<FeeRecordV1>,
    pub first_seen_at: u64,
    pub observed_at: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct CurrentObservationRecordV1 {
    pub transaction: ObservationRecordV1,
    pub watch_ids: Vec<String>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct EventRecordV1 {
    pub id: String,
    pub cursor: u64,
    pub watch_ids: Vec<String>,
    pub previous_status: Option<TransactionStatusRecordV1>,
    pub transaction: ObservationRecordV1,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct EventIdRecordV1 {
    pub cursor: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct PendingConfirmationRecordV1 {
    pub transaction_id: ChainValueRecordV1,
    pub inclusion_height: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct BundleChangeRecordV1 {
    pub transaction_id: ChainValueRecordV1,
    pub prior: Option<CurrentObservationRecordV1>,
    pub included_here: bool,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct BundleRecordV1 {
    pub block: BlockRefRecordV1,
    pub prior_checkpoint: Option<BlockRefRecordV1>,
    pub encoded_undo: Vec<u8>,
    pub raw_block: Vec<u8>,
    pub raw_receipts: Vec<Vec<u8>>,
    pub changes: Vec<BundleChangeRecordV1>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum SyncPhaseRecordV1 {
    Starting,
    Reconciling,
    CatchingUp,
    Ready,
    Reverting,
    Replaying,
    RebuildRequired,
    Halted,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct RebuildReasonRecordV1 {
    pub checkpoint: BlockRefRecordV1,
    pub oldest_retained: u64,
    pub message: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct SyncStatusRecordV1 {
    pub scope: ScopeRecordV1,
    pub checkpoint: Option<BlockRefRecordV1>,
    pub observed_tip: Option<BlockRefRecordV1>,
    pub confirmation_policy: PolicyRecordV1,
    pub phase: SyncPhaseRecordV1,
    pub rebuild_reason: Option<RebuildReasonRecordV1>,
    pub halted_reason: Option<String>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum RebuildPhaseRecordV1 {
    Building,
    Validating,
    ReadyToActivate,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct RebuildStateRecordV1 {
    pub scope: ScopeRecordV1,
    pub generation: u64,
    pub phase: RebuildPhaseRecordV1,
    pub bootstrap_height: u64,
    pub checkpoint: Option<BlockRefRecordV1>,
    pub published_event_high_water: u64,
}

#[must_use]
pub(super) fn scope_to_record(value: &IndexScope) -> ScopeRecordV1 {
    ScopeRecordV1 {
        chain: value.chain.0.clone(),
        network: value.network.clone(),
    }
}

#[must_use]
pub(super) fn scope_from_record(value: ScopeRecordV1) -> IndexScope {
    IndexScope {
        chain: ChainId(value.chain),
        network: value.network,
    }
}

#[must_use]
pub(super) fn chain_value_from_transaction(value: &CanonicalTransactionId) -> ChainValueRecordV1 {
    ChainValueRecordV1 {
        chain: value.chain.0.clone(),
        value: value.value.clone(),
    }
}

#[must_use]
pub(super) fn transaction_from_chain_value(value: ChainValueRecordV1) -> CanonicalTransactionId {
    CanonicalTransactionId {
        chain: ChainId(value.chain),
        value: value.value,
    }
}

fn chain_value_from_address(value: &CanonicalAddress) -> ChainValueRecordV1 {
    ChainValueRecordV1 {
        chain: value.chain.0.clone(),
        value: value.value.clone(),
    }
}

fn address_from_chain_value(value: ChainValueRecordV1) -> CanonicalAddress {
    CanonicalAddress {
        chain: ChainId(value.chain),
        value: value.value,
    }
}

#[must_use]
pub(super) fn block_to_record(value: &BlockRef) -> BlockRefRecordV1 {
    BlockRefRecordV1 {
        height: value.height.0,
        hash: value.hash.0.clone(),
        parent_hash: value.parent_hash.as_ref().map(|hash| hash.0.clone()),
        timestamp: value.timestamp,
    }
}

#[must_use]
pub(super) fn block_from_record(value: BlockRefRecordV1) -> BlockRef {
    BlockRef {
        height: BlockHeight(value.height),
        hash: BlockHash(value.hash),
        parent_hash: value.parent_hash.map(BlockHash),
        timestamp: value.timestamp,
    }
}

#[must_use]
pub(super) fn policy_to_record(value: ConfirmationPolicy) -> PolicyRecordV1 {
    PolicyRecordV1 {
        minimum_confirmations: value.minimum_confirmations,
        require_chain_finality: value.require_chain_finality,
    }
}

#[must_use]
pub(super) fn policy_from_record(value: PolicyRecordV1) -> ConfirmationPolicy {
    ConfirmationPolicy {
        minimum_confirmations: value.minimum_confirmations,
        require_chain_finality: value.require_chain_finality,
    }
}

#[must_use]
pub(super) fn selector_to_record(value: &WatchSelector) -> SelectorRecordV1 {
    match value {
        WatchSelector::Address(address) => {
            SelectorRecordV1::Address(chain_value_from_address(address))
        }
        WatchSelector::Transaction(transaction) => {
            SelectorRecordV1::Transaction(chain_value_from_transaction(transaction))
        }
    }
}

#[must_use]
pub(super) fn selector_from_record(value: SelectorRecordV1) -> WatchSelector {
    match value {
        SelectorRecordV1::Address(address) => {
            WatchSelector::Address(address_from_chain_value(address))
        }
        SelectorRecordV1::Transaction(transaction) => {
            WatchSelector::Transaction(transaction_from_chain_value(transaction))
        }
    }
}

fn movement_kind_to_record(value: MovementKind) -> MovementKindRecordV1 {
    match value {
        MovementKind::Transfer => MovementKindRecordV1::Transfer,
        MovementKind::Input => MovementKindRecordV1::Input,
        MovementKind::Output => MovementKindRecordV1::Output,
        MovementKind::InternalTransfer => MovementKindRecordV1::InternalTransfer,
        MovementKind::Mint => MovementKindRecordV1::Mint,
        MovementKind::Burn => MovementKindRecordV1::Burn,
    }
}

fn movement_kind_from_record(value: MovementKindRecordV1) -> MovementKind {
    match value {
        MovementKindRecordV1::Transfer => MovementKind::Transfer,
        MovementKindRecordV1::Input => MovementKind::Input,
        MovementKindRecordV1::Output => MovementKind::Output,
        MovementKindRecordV1::InternalTransfer => MovementKind::InternalTransfer,
        MovementKindRecordV1::Mint => MovementKind::Mint,
        MovementKindRecordV1::Burn => MovementKind::Burn,
    }
}

fn movement_to_record(value: &ValueMovement) -> MovementRecordV1 {
    MovementRecordV1 {
        id: value.id.0.clone(),
        asset: ChainValueRecordV1 {
            chain: value.asset.chain.0.clone(),
            value: value.asset.asset.clone(),
        },
        amount: value.amount.0,
        from: value.from.as_ref().map(chain_value_from_address),
        to: value.to.as_ref().map(chain_value_from_address),
        kind: movement_kind_to_record(value.kind),
    }
}

fn movement_from_record(value: MovementRecordV1) -> ValueMovement {
    ValueMovement {
        id: MovementId(value.id),
        asset: AssetId {
            chain: ChainId(value.asset.chain),
            asset: value.asset.value,
        },
        amount: AtomicAmount(value.amount),
        from: value.from.map(address_from_chain_value),
        to: value.to.map(address_from_chain_value),
        kind: movement_kind_from_record(value.kind),
    }
}

fn fee_to_record(value: &NetworkFee) -> FeeRecordV1 {
    FeeRecordV1 {
        asset: ChainValueRecordV1 {
            chain: value.asset.chain.0.clone(),
            value: value.asset.asset.clone(),
        },
        amount: value.amount.0,
        payer: value.payer.as_ref().map(chain_value_from_address),
    }
}

fn fee_from_record(value: FeeRecordV1) -> NetworkFee {
    NetworkFee {
        asset: AssetId {
            chain: ChainId(value.asset.chain),
            asset: value.asset.value,
        },
        amount: AtomicAmount(value.amount),
        payer: value.payer.map(address_from_chain_value),
    }
}

#[must_use]
pub(super) fn status_to_record(value: &TransactionStatus) -> TransactionStatusRecordV1 {
    match value {
        TransactionStatus::Pending => TransactionStatusRecordV1::Pending,
        TransactionStatus::Included {
            block,
            confirmations,
        } => TransactionStatusRecordV1::Included {
            block: block_to_record(block),
            confirmations: *confirmations,
        },
        TransactionStatus::Confirmed { block, proof } => TransactionStatusRecordV1::Confirmed {
            block: block_to_record(block),
            proof: match proof {
                ConfirmationProof::Depth { required, observed } => {
                    ConfirmationProofRecordV1::Depth {
                        required: *required,
                        observed: *observed,
                    }
                }
                ConfirmationProof::ChainFinalized => ConfirmationProofRecordV1::ChainFinalized,
                ConfirmationProof::DepthAndChainFinalized { required, observed } => {
                    ConfirmationProofRecordV1::DepthAndChainFinalized {
                        required: *required,
                        observed: *observed,
                    }
                }
            },
        },
        TransactionStatus::Failed { block, reason } => TransactionStatusRecordV1::Failed {
            block: block.as_ref().map(block_to_record),
            reason: reason.clone(),
        },
        TransactionStatus::Replaced { by } => TransactionStatusRecordV1::Replaced {
            by: chain_value_from_transaction(by),
        },
        TransactionStatus::Dropped => TransactionStatusRecordV1::Dropped,
        TransactionStatus::Reorged { previous_block } => TransactionStatusRecordV1::Reorged {
            previous_block: block_to_record(previous_block),
        },
    }
}

#[must_use]
pub(super) fn status_from_record(value: TransactionStatusRecordV1) -> TransactionStatus {
    match value {
        TransactionStatusRecordV1::Pending => TransactionStatus::Pending,
        TransactionStatusRecordV1::Included {
            block,
            confirmations,
        } => TransactionStatus::Included {
            block: block_from_record(block),
            confirmations,
        },
        TransactionStatusRecordV1::Confirmed { block, proof } => TransactionStatus::Confirmed {
            block: block_from_record(block),
            proof: match proof {
                ConfirmationProofRecordV1::Depth { required, observed } => {
                    ConfirmationProof::Depth { required, observed }
                }
                ConfirmationProofRecordV1::ChainFinalized => ConfirmationProof::ChainFinalized,
                ConfirmationProofRecordV1::DepthAndChainFinalized { required, observed } => {
                    ConfirmationProof::DepthAndChainFinalized { required, observed }
                }
            },
        },
        TransactionStatusRecordV1::Failed { block, reason } => TransactionStatus::Failed {
            block: block.map(block_from_record),
            reason,
        },
        TransactionStatusRecordV1::Replaced { by } => TransactionStatus::Replaced {
            by: transaction_from_chain_value(by),
        },
        TransactionStatusRecordV1::Dropped => TransactionStatus::Dropped,
        TransactionStatusRecordV1::Reorged { previous_block } => TransactionStatus::Reorged {
            previous_block: block_from_record(previous_block),
        },
    }
}

#[must_use]
pub(super) fn observation_to_record(value: &ObservedTransaction) -> ObservationRecordV1 {
    ObservationRecordV1 {
        scope: scope_to_record(&value.scope),
        transaction_id: chain_value_from_transaction(&value.transaction_id),
        revision: value.revision.0,
        status: status_to_record(&value.status),
        movements: value.movements.iter().map(movement_to_record).collect(),
        fee: value.fee.as_ref().map(fee_to_record),
        first_seen_at: value.first_seen_at,
        observed_at: value.observed_at,
    }
}

#[must_use]
pub(super) fn observation_from_record(value: ObservationRecordV1) -> ObservedTransaction {
    ObservedTransaction {
        scope: scope_from_record(value.scope),
        transaction_id: transaction_from_chain_value(value.transaction_id),
        revision: ObservationRevision(value.revision),
        status: status_from_record(value.status),
        movements: value
            .movements
            .into_iter()
            .map(movement_from_record)
            .collect(),
        fee: value.fee.map(fee_from_record),
        first_seen_at: value.first_seen_at,
        observed_at: value.observed_at,
    }
}

#[must_use]
pub(super) fn event_from_record(value: EventRecordV1) -> ObservationEvent {
    ObservationEvent {
        id: ObservationEventId(value.id),
        cursor: EventCursor(value.cursor),
        watch_ids: value.watch_ids.into_iter().map(WatchId).collect(),
        previous_status: value.previous_status.map(status_from_record),
        transaction: observation_from_record(value.transaction),
    }
}

fn sync_phase_to_record(value: SyncPhase) -> SyncPhaseRecordV1 {
    match value {
        SyncPhase::Starting => SyncPhaseRecordV1::Starting,
        SyncPhase::Reconciling => SyncPhaseRecordV1::Reconciling,
        SyncPhase::CatchingUp => SyncPhaseRecordV1::CatchingUp,
        SyncPhase::Ready => SyncPhaseRecordV1::Ready,
        SyncPhase::Reverting => SyncPhaseRecordV1::Reverting,
        SyncPhase::Replaying => SyncPhaseRecordV1::Replaying,
        SyncPhase::RebuildRequired => SyncPhaseRecordV1::RebuildRequired,
        SyncPhase::Halted => SyncPhaseRecordV1::Halted,
    }
}

fn sync_phase_from_record(value: SyncPhaseRecordV1) -> SyncPhase {
    match value {
        SyncPhaseRecordV1::Starting => SyncPhase::Starting,
        SyncPhaseRecordV1::Reconciling => SyncPhase::Reconciling,
        SyncPhaseRecordV1::CatchingUp => SyncPhase::CatchingUp,
        SyncPhaseRecordV1::Ready => SyncPhase::Ready,
        SyncPhaseRecordV1::Reverting => SyncPhase::Reverting,
        SyncPhaseRecordV1::Replaying => SyncPhase::Replaying,
        SyncPhaseRecordV1::RebuildRequired => SyncPhase::RebuildRequired,
        SyncPhaseRecordV1::Halted => SyncPhase::Halted,
    }
}

#[must_use]
pub(super) fn sync_status_to_record(value: &SyncStatus) -> SyncStatusRecordV1 {
    SyncStatusRecordV1 {
        scope: scope_to_record(&value.scope),
        checkpoint: value.checkpoint.as_ref().map(block_to_record),
        observed_tip: value.observed_tip.as_ref().map(block_to_record),
        confirmation_policy: policy_to_record(value.confirmation_policy),
        phase: sync_phase_to_record(value.phase),
        rebuild_reason: value
            .rebuild_reason
            .as_ref()
            .map(|reason| RebuildReasonRecordV1 {
                checkpoint: block_to_record(&reason.checkpoint),
                oldest_retained: reason.oldest_retained.0,
                message: reason.message.clone(),
            }),
        halted_reason: value.halted_reason.clone(),
    }
}

#[must_use]
pub(super) fn sync_status_from_record(value: SyncStatusRecordV1) -> SyncStatus {
    SyncStatus {
        scope: scope_from_record(value.scope),
        checkpoint: value.checkpoint.map(block_from_record),
        observed_tip: value.observed_tip.map(block_from_record),
        confirmation_policy: policy_from_record(value.confirmation_policy),
        phase: sync_phase_from_record(value.phase),
        rebuild_reason: value.rebuild_reason.map(|reason| RebuildReason {
            checkpoint: block_from_record(reason.checkpoint),
            oldest_retained: BlockHeight(reason.oldest_retained),
            message: reason.message,
        }),
        halted_reason: value.halted_reason,
    }
}

fn rebuild_phase_to_record(value: RebuildPhase) -> RebuildPhaseRecordV1 {
    match value {
        RebuildPhase::Building => RebuildPhaseRecordV1::Building,
        RebuildPhase::Validating => RebuildPhaseRecordV1::Validating,
        RebuildPhase::ReadyToActivate => RebuildPhaseRecordV1::ReadyToActivate,
    }
}

fn rebuild_phase_from_record(value: RebuildPhaseRecordV1) -> RebuildPhase {
    match value {
        RebuildPhaseRecordV1::Building => RebuildPhase::Building,
        RebuildPhaseRecordV1::Validating => RebuildPhase::Validating,
        RebuildPhaseRecordV1::ReadyToActivate => RebuildPhase::ReadyToActivate,
    }
}

#[must_use]
pub(super) fn rebuild_state_to_record(value: &RebuildState) -> RebuildStateRecordV1 {
    RebuildStateRecordV1 {
        scope: scope_to_record(&value.scope),
        generation: value.generation.0,
        phase: rebuild_phase_to_record(value.phase),
        bootstrap_height: value.bootstrap_height.0,
        checkpoint: value.checkpoint.as_ref().map(block_to_record),
        published_event_high_water: value.published_event_high_water.0,
    }
}

#[must_use]
pub(super) fn rebuild_state_from_record(value: RebuildStateRecordV1) -> RebuildState {
    RebuildState {
        scope: scope_from_record(value.scope),
        generation: RebuildGeneration(value.generation),
        phase: rebuild_phase_from_record(value.phase),
        bootstrap_height: BlockHeight(value.bootstrap_height),
        checkpoint: value.checkpoint.map(block_from_record),
        published_event_high_water: EventCursor(value.published_event_high_water),
    }
}

pub(super) fn ensure_record_scope(
    expected: &IndexScope,
    actual: &IndexScope,
    record: &str,
) -> Result<(), IndexError> {
    if expected == actual {
        Ok(())
    } else {
        Err(IndexError::new(
            IndexErrorKind::Storage,
            format!("persisted {record} belongs to another scope"),
            false,
        ))
    }
}
