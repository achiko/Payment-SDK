use super::*;
use crate::amount;

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct AssetRecord {
    pub(super) chain: String,
    pub(super) asset: String,
}

impl From<&AssetId> for AssetRecord {
    fn from(value: &AssetId) -> Self {
        Self {
            chain: value.chain.0.clone(),
            asset: value.asset.clone(),
        }
    }
}

impl From<AssetRecord> for AssetId {
    fn from(value: AssetRecord) -> Self {
        Self {
            chain: ChainId(value.chain),
            asset: value.asset,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct TransactionRecord {
    pub(super) chain: String,
    pub(super) network: String,
    pub(super) value: String,
}

impl From<&TransactionRef> for TransactionRecord {
    fn from(value: &TransactionRef) -> Self {
        Self {
            chain: value.scope.chain.0.clone(),
            network: value.scope.network.clone(),
            value: value.value.clone(),
        }
    }
}

impl From<TransactionRecord> for TransactionRef {
    fn from(value: TransactionRecord) -> Self {
        Self {
            scope: IndexScope {
                chain: ChainId(value.chain),
                network: value.network,
            },
            value: value.value,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct PolicyRecord {
    pub(super) version: String,
    pub(super) digest: [u8; 32],
}

impl From<&PolicyIdentity> for PolicyRecord {
    fn from(value: &PolicyIdentity) -> Self {
        Self {
            version: value.version.clone(),
            digest: value.digest,
        }
    }
}

impl From<PolicyRecord> for PolicyIdentity {
    fn from(value: PolicyRecord) -> Self {
        Self {
            version: value.version,
            digest: value.digest,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct FailureRecord {
    pub(super) code: String,
    pub(super) message: String,
    pub(super) retryable: bool,
}

impl From<&CollectionError> for FailureRecord {
    fn from(value: &CollectionError) -> Self {
        Self {
            code: value.code.clone(),
            message: value.message.clone(),
            retryable: value.retryable,
        }
    }
}

impl From<FailureRecord> for CollectionError {
    fn from(value: FailureRecord) -> Self {
        Self {
            code: value.code,
            message: value.message,
            retryable: value.retryable,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum LegStateRecord {
    Required,
    Signed { transaction_id: TransactionRecord },
    Broadcast { transaction_id: TransactionRecord },
    Confirmed { transaction_id: TransactionRecord },
    Failed { transaction_id: TransactionRecord },
    Reorged { transaction_id: TransactionRecord },
}

impl From<&CollectionLegState> for LegStateRecord {
    fn from(value: &CollectionLegState) -> Self {
        match value {
            CollectionLegState::Required => Self::Required,
            CollectionLegState::Signed { transaction_id } => Self::Signed {
                transaction_id: transaction_id.into(),
            },
            CollectionLegState::Broadcast { transaction_id } => Self::Broadcast {
                transaction_id: transaction_id.into(),
            },
            CollectionLegState::Confirmed { transaction_id } => Self::Confirmed {
                transaction_id: transaction_id.into(),
            },
            CollectionLegState::Failed { transaction_id } => Self::Failed {
                transaction_id: transaction_id.into(),
            },
            CollectionLegState::Reorged { transaction_id } => Self::Reorged {
                transaction_id: transaction_id.into(),
            },
        }
    }
}

impl From<LegStateRecord> for CollectionLegState {
    fn from(value: LegStateRecord) -> Self {
        match value {
            LegStateRecord::Required => Self::Required,
            LegStateRecord::Signed { transaction_id } => Self::Signed {
                transaction_id: transaction_id.into(),
            },
            LegStateRecord::Broadcast { transaction_id } => Self::Broadcast {
                transaction_id: transaction_id.into(),
            },
            LegStateRecord::Confirmed { transaction_id } => Self::Confirmed {
                transaction_id: transaction_id.into(),
            },
            LegStateRecord::Failed { transaction_id } => Self::Failed {
                transaction_id: transaction_id.into(),
            },
            LegStateRecord::Reorged { transaction_id } => Self::Reorged {
                transaction_id: transaction_id.into(),
            },
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum ReservationStateRecord {
    Active,
    Consumed {
        transaction_id: TransactionRecord,
        consumed_at: u64,
    },
    Released {
        reason: u8,
        released_at: u64,
    },
}

impl From<&CollectionReservationState> for ReservationStateRecord {
    fn from(value: &CollectionReservationState) -> Self {
        match value {
            CollectionReservationState::Active => Self::Active,
            CollectionReservationState::Consumed {
                transaction_id,
                consumed_at,
            } => Self::Consumed {
                transaction_id: transaction_id.into(),
                consumed_at: *consumed_at,
            },
            CollectionReservationState::Released {
                reason,
                released_at,
            } => Self::Released {
                reason: release_reason_tag(*reason),
                released_at: *released_at,
            },
        }
    }
}

impl TryFrom<ReservationStateRecord> for CollectionReservationState {
    type Error = DepositError;

    fn try_from(value: ReservationStateRecord) -> Result<Self, Self::Error> {
        Ok(match value {
            ReservationStateRecord::Active => Self::Active,
            ReservationStateRecord::Consumed {
                transaction_id,
                consumed_at,
            } => Self::Consumed {
                transaction_id: transaction_id.into(),
                consumed_at,
            },
            ReservationStateRecord::Released {
                reason,
                released_at,
            } => Self::Released {
                reason: release_reason_from_tag(reason)?,
                released_at,
            },
        })
    }
}

// design-lint: allow unclassified-free-function -- preserves the frozen reservation wire tag
fn release_reason_tag(reason: ReservationReleaseReason) -> u8 {
    match reason {
        ReservationReleaseReason::TerminalFailure => 0,
        ReservationReleaseReason::Reorg => 1,
    }
}

fn release_reason_from_tag(tag: u8) -> Result<ReservationReleaseReason, DepositError> {
    match tag {
        0 => Ok(ReservationReleaseReason::TerminalFailure),
        1 => Ok(ReservationReleaseReason::Reorg),
        _ => Err(storage_error(
            "PS collection record has an unknown reservation release reason",
        )),
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct AllocationRecord {
    pub(super) deposit_id: String,
    pub(super) asset: AssetRecord,
    pub(super) gross_debit: [u8; 32],
    pub(super) master_credit: [u8; 32],
    pub(super) allocated_fee_asset: AssetRecord,
    pub(super) allocated_fee: [u8; 32],
}

impl From<&CollectionAllocation> for AllocationRecord {
    fn from(value: &CollectionAllocation) -> Self {
        Self {
            deposit_id: value.deposit_id.0.clone(),
            asset: (&value.asset).into(),
            gross_debit: amount::record_bytes(&value.gross_debit),
            master_credit: amount::record_bytes(&value.master_credit),
            allocated_fee_asset: (&value.allocated_fee_asset).into(),
            allocated_fee: amount::record_bytes(&value.allocated_fee),
        }
    }
}

impl From<AllocationRecord> for CollectionAllocation {
    fn from(value: AllocationRecord) -> Self {
        Self {
            deposit_id: DepositId(value.deposit_id),
            asset: value.asset.into(),
            gross_debit: amount::from_bytes(value.gross_debit),
            master_credit: amount::from_bytes(value.master_credit),
            allocated_fee_asset: value.allocated_fee_asset.into(),
            allocated_fee: amount::from_bytes(value.allocated_fee),
        }
    }
}
