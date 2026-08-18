use crate::{
    AssetId, BlockHash, BlockHeight, BlockRef as DomainBlock, CanonicalAddress, ChainId,
    ConfirmationPolicy, ConfirmationProof, EventCursor, EventId as DomainEventId, IndexError,
    IndexErrorKind, IndexScope, NetworkFee, ObservationEvent, ObservationRevision,
    ObservedTransaction, RebuildGeneration, RebuildPhase, RebuildReason as DomainRebuildReason,
    RebuildState as DomainRebuild, SyncPhase, SyncStatus as DomainSync, TransactionRef,
    TransactionStatus, WatchId, WatchSelector,
};
use bincode::{Decode, Encode};

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct RepositoryMeta {
    pub format: u16,
    pub scope: ScopeRecord,
    pub bootstrap_height: u64,
    pub confirmation_depth: u64,
    pub require_chain_finality: bool,
    pub reorg_retention: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct CounterRecord {
    pub value: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct ScopeRecord {
    pub chain: String,
    pub network: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct ChainValue {
    pub chain: String,
    pub value: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct ScopedValue {
    pub scope: ScopeRecord,
    pub value: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct BlockRecord {
    pub height: u64,
    pub hash: Vec<u8>,
    pub parent_hash: Option<Vec<u8>>,
    pub timestamp: Option<u64>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct PolicyRecord {
    pub minimum_confirmations: u64,
    pub require_chain_finality: bool,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum SelectorRecord {
    Address(ScopedValue),
    Transaction(ScopedValue),
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct WatchRecord {
    pub id: String,
    pub scope: ScopeRecord,
    pub selector: SelectorRecord,
    pub encoded_target: Vec<u8>,
    pub idempotency_key: String,
    pub start_height: u64,
    pub registered_at: Option<BlockRecord>,
    pub inactive_from: Option<u64>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct WatchIdentity {
    pub watch_id: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct BackfillRecord {
    pub scope: ScopeRecord,
    pub watch_id: String,
    pub from_height: u64,
    pub next_height: u64,
    pub through: BlockRecord,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct BackfillMarker {
    pub block: BlockRecord,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct HeightMarker {
    pub watch_id: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum MovementKindRecord {
    Transfer,
    Input,
    Output,
    InternalTransfer,
    Mint,
    Burn,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct MovementRecord {
    pub id: String,
    pub asset: ChainValue,
    pub amount: String,
    pub from: Option<ScopedValue>,
    pub to: Option<ScopedValue>,
    pub kind: MovementKindRecord,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct FeeRecord {
    pub asset: ChainValue,
    pub amount: String,
    pub payer: Option<ScopedValue>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum ConfirmationProofRecord {
    Depth { required: u64, observed: u64 },
    ChainFinalized,
    DepthAndChainFinalized { required: u64, observed: u64 },
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum TransactionStatusRecord {
    Pending,
    Included {
        block: BlockRecord,
        confirmations: u64,
    },
    Confirmed {
        block: BlockRecord,
        proof: ConfirmationProofRecord,
    },
    Failed {
        block: Option<BlockRecord>,
        reason: Option<String>,
    },
    Replaced {
        by: ScopedValue,
    },
    Dropped,
    Reorged {
        previous_block: BlockRecord,
    },
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct ObservationRecord {
    pub scope: ScopeRecord,
    pub transaction_id: ScopedValue,
    pub revision: u64,
    pub status: TransactionStatusRecord,
    pub movements: Vec<MovementRecord>,
    pub fee: Option<FeeRecord>,
    pub first_seen_at: u64,
    pub observed_at: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct CurrentObservation {
    pub transaction: ObservationRecord,
    pub watch_ids: Vec<String>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct EventRecord {
    pub id: String,
    pub cursor: u64,
    pub watch_ids: Vec<String>,
    pub previous_status: Option<TransactionStatusRecord>,
    pub transaction: ObservationRecord,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct EventPointer {
    pub cursor: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct PendingConfirmation {
    pub transaction_id: ScopedValue,
    pub inclusion_height: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct BundleChange {
    pub transaction_id: ScopedValue,
    pub prior: Option<CurrentObservation>,
    pub included_here: bool,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct BundleRecord {
    pub block: BlockRecord,
    pub prior_checkpoint: Option<BlockRecord>,
    pub encoded_undo: Vec<u8>,
    pub raw_block: Vec<u8>,
    pub raw_receipts: Vec<Vec<u8>>,
    pub changes: Vec<BundleChange>,
}

/// Projection keys first materialized by historical watch backfill for one
/// retained canonical block. Reverting that block deletes these keys after
/// the chain-owned rollback has been decoded and validated.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct BackfillRollback {
    pub block: BlockRecord,
    pub relative_keys: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum SyncPhaseRecord {
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
pub(super) struct RebuildCause {
    pub checkpoint: BlockRecord,
    pub oldest_retained: u64,
    pub message: String,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct SyncRecord {
    pub scope: ScopeRecord,
    pub checkpoint: Option<BlockRecord>,
    pub observed_tip: Option<BlockRecord>,
    pub confirmation_policy: PolicyRecord,
    pub phase: SyncPhaseRecord,
    pub rebuild_reason: Option<RebuildCause>,
    pub halted_reason: Option<String>,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) enum RebuildPhaseRecord {
    Building,
    Validating,
    ReadyToActivate,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub(super) struct RebuildRecord {
    pub scope: ScopeRecord,
    pub generation: u64,
    pub phase: RebuildPhaseRecord,
    pub bootstrap_height: u64,
    pub checkpoint: Option<BlockRecord>,
    pub published_event_high_water: u64,
}

impl ScopeRecord {
    #[must_use]
    pub(super) fn from_domain(value: &IndexScope) -> Self {
        Self {
            chain: value.chain.0.clone(),
            network: value.network.clone(),
        }
    }

    #[must_use]
    pub(super) fn into_domain(self) -> IndexScope {
        IndexScope {
            chain: ChainId(self.chain),
            network: self.network,
        }
    }
}

impl ScopedValue {
    #[must_use]
    pub(super) fn from_transaction(value: &TransactionRef) -> Self {
        Self {
            scope: ScopeRecord::from_domain(&value.scope),
            value: value.value.clone(),
        }
    }

    #[must_use]
    pub(super) fn into_transaction(self) -> TransactionRef {
        TransactionRef {
            scope: self.scope.into_domain(),
            value: self.value,
        }
    }

    #[must_use]
    pub(super) fn from_address(value: &CanonicalAddress) -> Self {
        Self {
            scope: ScopeRecord::from_domain(&value.scope),
            value: value.value.clone(),
        }
    }

    #[must_use]
    pub(super) fn into_address(self) -> CanonicalAddress {
        CanonicalAddress {
            scope: self.scope.into_domain(),
            value: self.value,
        }
    }
}

impl BlockRecord {
    #[must_use]
    pub(super) fn from_domain(value: &DomainBlock) -> Self {
        Self {
            height: value.height.0,
            hash: value.hash.0.clone(),
            parent_hash: value.parent_hash.as_ref().map(|hash| hash.0.clone()),
            timestamp: value.timestamp,
        }
    }

    #[must_use]
    pub(super) fn into_domain(self) -> DomainBlock {
        DomainBlock {
            height: BlockHeight(self.height),
            hash: BlockHash(self.hash),
            parent_hash: self.parent_hash.map(BlockHash),
            timestamp: self.timestamp,
        }
    }
}

impl PolicyRecord {
    #[must_use]
    pub(super) fn from_domain(value: ConfirmationPolicy) -> Self {
        Self {
            minimum_confirmations: value.minimum_confirmations,
            require_chain_finality: value.require_chain_finality,
        }
    }

    #[must_use]
    pub(super) fn into_domain(self) -> ConfirmationPolicy {
        ConfirmationPolicy {
            minimum_confirmations: self.minimum_confirmations,
            require_chain_finality: self.require_chain_finality,
        }
    }
}

impl SelectorRecord {
    #[must_use]
    pub(super) fn from_domain(value: &WatchSelector) -> Self {
        match value {
            WatchSelector::Address(address) => Self::Address(ScopedValue::from_address(address)),
            WatchSelector::Transaction(transaction) => {
                Self::Transaction(ScopedValue::from_transaction(transaction))
            }
        }
    }

    #[must_use]
    pub(super) fn into_domain(self) -> WatchSelector {
        match self {
            Self::Address(address) => WatchSelector::Address(address.into_address()),
            Self::Transaction(transaction) => {
                WatchSelector::Transaction(transaction.into_transaction())
            }
        }
    }
}

impl FeeRecord {
    fn from_domain(value: &NetworkFee) -> Self {
        Self {
            asset: ChainValue {
                chain: value.asset.chain.0.clone(),
                value: value.asset.asset.clone(),
            },
            amount: crate::amount_record::encode(&value.amount),
            payer: value.payer.as_ref().map(ScopedValue::from_address),
        }
    }

    fn into_domain(self) -> Result<NetworkFee, IndexError> {
        Ok(NetworkFee {
            asset: AssetId {
                chain: ChainId(self.asset.chain),
                asset: self.asset.value,
            },
            amount: crate::amount_record::decode(&self.amount)?,
            payer: self.payer.map(ScopedValue::into_address),
        })
    }
}

impl TransactionStatusRecord {
    #[must_use]
    pub(super) fn from_domain(value: &TransactionStatus) -> Self {
        match value {
            TransactionStatus::Pending => Self::Pending,
            TransactionStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: BlockRecord::from_domain(block),
                confirmations: *confirmations,
            },
            TransactionStatus::Confirmed { block, proof } => Self::Confirmed {
                block: BlockRecord::from_domain(block),
                proof: match proof {
                    ConfirmationProof::Depth { required, observed } => {
                        ConfirmationProofRecord::Depth {
                            required: *required,
                            observed: *observed,
                        }
                    }
                    ConfirmationProof::ChainFinalized => ConfirmationProofRecord::ChainFinalized,
                    ConfirmationProof::DepthAndChainFinalized { required, observed } => {
                        ConfirmationProofRecord::DepthAndChainFinalized {
                            required: *required,
                            observed: *observed,
                        }
                    }
                },
            },
            TransactionStatus::Failed { block, reason } => Self::Failed {
                block: block.as_ref().map(BlockRecord::from_domain),
                reason: reason.clone(),
            },
            TransactionStatus::Replaced { by } => Self::Replaced {
                by: ScopedValue::from_transaction(by),
            },
            TransactionStatus::Dropped => Self::Dropped,
            TransactionStatus::Reorged { previous_block } => Self::Reorged {
                previous_block: BlockRecord::from_domain(previous_block),
            },
        }
    }

    #[must_use]
    pub(super) fn into_domain(self) -> TransactionStatus {
        match self {
            Self::Pending => TransactionStatus::Pending,
            Self::Included {
                block,
                confirmations,
            } => TransactionStatus::Included {
                block: BlockRecord::into_domain(block),
                confirmations,
            },
            Self::Confirmed { block, proof } => TransactionStatus::Confirmed {
                block: BlockRecord::into_domain(block),
                proof: match proof {
                    ConfirmationProofRecord::Depth { required, observed } => {
                        ConfirmationProof::Depth { required, observed }
                    }
                    ConfirmationProofRecord::ChainFinalized => ConfirmationProof::ChainFinalized,
                    ConfirmationProofRecord::DepthAndChainFinalized { required, observed } => {
                        ConfirmationProof::DepthAndChainFinalized { required, observed }
                    }
                },
            },
            Self::Failed { block, reason } => TransactionStatus::Failed {
                block: block.map(BlockRecord::into_domain),
                reason,
            },
            Self::Replaced { by } => TransactionStatus::Replaced {
                by: ScopedValue::into_transaction(by),
            },
            Self::Dropped => TransactionStatus::Dropped,
            Self::Reorged { previous_block } => TransactionStatus::Reorged {
                previous_block: BlockRecord::into_domain(previous_block),
            },
        }
    }
}

impl ObservationRecord {
    #[must_use]
    pub(super) fn from_domain(value: &ObservedTransaction) -> Self {
        Self {
            scope: ScopeRecord::from_domain(&value.scope),
            transaction_id: ScopedValue::from_transaction(&value.transaction_id),
            revision: value.revision.0,
            status: TransactionStatusRecord::from_domain(&value.status),
            movements: value
                .movements
                .iter()
                .map(crate::movement_record::to_record)
                .collect(),
            fee: value.fee.as_ref().map(FeeRecord::from_domain),
            first_seen_at: value.first_seen_at,
            observed_at: value.observed_at,
        }
    }

    pub(super) fn into_domain(self) -> Result<ObservedTransaction, IndexError> {
        Ok(ObservedTransaction {
            scope: self.scope.into_domain(),
            transaction_id: self.transaction_id.into_transaction(),
            revision: ObservationRevision(self.revision),
            status: self.status.into_domain(),
            movements: self
                .movements
                .into_iter()
                .map(crate::movement_record::from_record)
                .collect::<Result<Vec<_>, _>>()?,
            fee: self.fee.map(FeeRecord::into_domain).transpose()?,
            first_seen_at: self.first_seen_at,
            observed_at: self.observed_at,
        })
    }
}

impl EventRecord {
    pub(super) fn into_domain(self) -> Result<ObservationEvent, IndexError> {
        Ok(ObservationEvent {
            id: DomainEventId(self.id),
            cursor: EventCursor(self.cursor),
            watch_ids: self.watch_ids.into_iter().map(WatchId).collect(),
            previous_status: self
                .previous_status
                .map(TransactionStatusRecord::into_domain),
            transaction: self.transaction.into_domain()?,
        })
    }
}

impl SyncPhaseRecord {
    fn from_domain(value: SyncPhase) -> Self {
        match value {
            SyncPhase::Starting => Self::Starting,
            SyncPhase::Reconciling => Self::Reconciling,
            SyncPhase::CatchingUp => Self::CatchingUp,
            SyncPhase::Ready => Self::Ready,
            SyncPhase::Reverting => Self::Reverting,
            SyncPhase::Replaying => Self::Replaying,
            SyncPhase::RebuildRequired => Self::RebuildRequired,
            SyncPhase::Halted => Self::Halted,
        }
    }

    fn into_domain(self) -> SyncPhase {
        match self {
            Self::Starting => SyncPhase::Starting,
            Self::Reconciling => SyncPhase::Reconciling,
            Self::CatchingUp => SyncPhase::CatchingUp,
            Self::Ready => SyncPhase::Ready,
            Self::Reverting => SyncPhase::Reverting,
            Self::Replaying => SyncPhase::Replaying,
            Self::RebuildRequired => SyncPhase::RebuildRequired,
            Self::Halted => SyncPhase::Halted,
        }
    }
}

impl SyncRecord {
    #[must_use]
    pub(super) fn from_domain(value: &DomainSync) -> Self {
        Self {
            scope: ScopeRecord::from_domain(&value.scope),
            checkpoint: value.checkpoint.as_ref().map(BlockRecord::from_domain),
            observed_tip: value.observed_tip.as_ref().map(BlockRecord::from_domain),
            confirmation_policy: PolicyRecord::from_domain(value.confirmation_policy),
            phase: SyncPhaseRecord::from_domain(value.phase),
            rebuild_reason: value.rebuild_reason.as_ref().map(|reason| RebuildCause {
                checkpoint: BlockRecord::from_domain(&reason.checkpoint),
                oldest_retained: reason.oldest_retained.0,
                message: reason.message.clone(),
            }),
            halted_reason: value.halted_reason.clone(),
        }
    }

    #[must_use]
    pub(super) fn into_domain(self) -> DomainSync {
        DomainSync {
            scope: self.scope.into_domain(),
            checkpoint: self.checkpoint.map(BlockRecord::into_domain),
            observed_tip: self.observed_tip.map(BlockRecord::into_domain),
            confirmation_policy: self.confirmation_policy.into_domain(),
            phase: self.phase.into_domain(),
            rebuild_reason: self.rebuild_reason.map(|reason| DomainRebuildReason {
                checkpoint: BlockRecord::into_domain(reason.checkpoint),
                oldest_retained: BlockHeight(reason.oldest_retained),
                message: reason.message,
            }),
            halted_reason: self.halted_reason,
        }
    }
}

impl RebuildPhaseRecord {
    fn from_domain(value: RebuildPhase) -> Self {
        match value {
            RebuildPhase::Building => Self::Building,
            RebuildPhase::Validating => Self::Validating,
            RebuildPhase::ReadyToActivate => Self::ReadyToActivate,
        }
    }

    fn into_domain(self) -> RebuildPhase {
        match self {
            Self::Building => RebuildPhase::Building,
            Self::Validating => RebuildPhase::Validating,
            Self::ReadyToActivate => RebuildPhase::ReadyToActivate,
        }
    }
}

impl RebuildRecord {
    #[must_use]
    pub(super) fn from_domain(value: &DomainRebuild) -> Self {
        Self {
            scope: ScopeRecord::from_domain(&value.scope),
            generation: value.generation.0,
            phase: RebuildPhaseRecord::from_domain(value.phase),
            bootstrap_height: value.bootstrap_height.0,
            checkpoint: value.checkpoint.as_ref().map(BlockRecord::from_domain),
            published_event_high_water: value.published_event_high_water.0,
        }
    }

    #[must_use]
    pub(super) fn into_domain(self) -> DomainRebuild {
        DomainRebuild {
            scope: self.scope.into_domain(),
            generation: RebuildGeneration(self.generation),
            phase: self.phase.into_domain(),
            bootstrap_height: BlockHeight(self.bootstrap_height),
            checkpoint: self.checkpoint.map(BlockRecord::into_domain),
            published_event_high_water: EventCursor(self.published_event_high_water),
        }
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
            IndexErrorKind::Store,
            format!("persisted {record} belongs to another scope"),
            false,
        ))
    }
}
