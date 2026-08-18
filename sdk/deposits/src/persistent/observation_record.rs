use super::*;

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct BlockRecord {
    pub(super) height: u64,
    pub(super) hash: Vec<u8>,
    pub(super) parent_hash: Option<Vec<u8>>,
    pub(super) timestamp: Option<u64>,
}

impl From<&BlockRef> for BlockRecord {
    fn from(value: &BlockRef) -> Self {
        Self {
            height: value.height.0,
            hash: value.hash.0.clone(),
            parent_hash: value.parent_hash.as_ref().map(|hash| hash.0.clone()),
            timestamp: value.timestamp,
        }
    }
}

impl From<BlockRecord> for BlockRef {
    fn from(value: BlockRecord) -> Self {
        Self {
            height: BlockHeight(value.height),
            hash: BlockHash(value.hash),
            parent_hash: value.parent_hash.map(BlockHash),
            timestamp: value.timestamp,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum StatusRecord {
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
        chain: String,
        network: String,
        transaction_id: String,
    },
    Dropped,
    Reorged {
        previous_block: BlockRecord,
    },
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum ConfirmationProofRecord {
    Depth { required: u64, observed: u64 },
    ChainFinalized,
    DepthAndChainFinalized { required: u64, observed: u64 },
}

impl From<&ConfirmationProof> for ConfirmationProofRecord {
    fn from(value: &ConfirmationProof) -> Self {
        match value {
            ConfirmationProof::Depth { required, observed } => Self::Depth {
                required: *required,
                observed: *observed,
            },
            ConfirmationProof::ChainFinalized => Self::ChainFinalized,
            ConfirmationProof::DepthAndChainFinalized { required, observed } => {
                Self::DepthAndChainFinalized {
                    required: *required,
                    observed: *observed,
                }
            }
        }
    }
}

impl From<ConfirmationProofRecord> for ConfirmationProof {
    fn from(value: ConfirmationProofRecord) -> Self {
        match value {
            ConfirmationProofRecord::Depth { required, observed } => {
                Self::Depth { required, observed }
            }
            ConfirmationProofRecord::ChainFinalized => Self::ChainFinalized,
            ConfirmationProofRecord::DepthAndChainFinalized { required, observed } => {
                Self::DepthAndChainFinalized { required, observed }
            }
        }
    }
}

impl From<&TransactionStatus> for StatusRecord {
    fn from(value: &TransactionStatus) -> Self {
        match value {
            TransactionStatus::Pending => Self::Pending,
            TransactionStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: block.into(),
                confirmations: *confirmations,
            },
            TransactionStatus::Confirmed { block, proof } => Self::Confirmed {
                block: block.into(),
                proof: proof.into(),
            },
            TransactionStatus::Failed { block, reason } => Self::Failed {
                block: block.as_ref().map(Into::into),
                reason: reason.clone(),
            },
            TransactionStatus::Replaced { by } => Self::Replaced {
                chain: by.scope.chain.0.clone(),
                network: by.scope.network.clone(),
                transaction_id: by.value.clone(),
            },
            TransactionStatus::Dropped => Self::Dropped,
            TransactionStatus::Reorged { previous_block } => Self::Reorged {
                previous_block: previous_block.into(),
            },
        }
    }
}

impl From<StatusRecord> for TransactionStatus {
    fn from(value: StatusRecord) -> Self {
        match value {
            StatusRecord::Pending => Self::Pending,
            StatusRecord::Included {
                block,
                confirmations,
            } => Self::Included {
                block: block.into(),
                confirmations,
            },
            StatusRecord::Confirmed { block, proof } => Self::Confirmed {
                block: block.into(),
                proof: proof.into(),
            },
            StatusRecord::Failed { block, reason } => Self::Failed {
                block: block.map(Into::into),
                reason,
            },
            StatusRecord::Replaced {
                chain,
                network,
                transaction_id,
            } => Self::Replaced {
                by: TransactionRef {
                    scope: IndexScope {
                        chain: ChainId(chain),
                        network,
                    },
                    value: transaction_id,
                },
            },
            StatusRecord::Dropped => Self::Dropped,
            StatusRecord::Reorged { previous_block } => Self::Reorged {
                previous_block: previous_block.into(),
            },
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct MovementRecord {
    pub(super) id: String,
    pub(super) asset_chain: String,
    pub(super) asset: String,
    pub(super) amount: [u8; 32],
    pub(super) from: Option<AddressRecord>,
    pub(super) to: Option<AddressRecord>,
    pub(super) kind: u8,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct AddressRecord {
    pub(super) chain: String,
    pub(super) network: String,
    pub(super) value: String,
}

impl From<&CanonicalAddress> for AddressRecord {
    fn from(value: &CanonicalAddress) -> Self {
        Self {
            chain: value.scope.chain.0.clone(),
            network: value.scope.network.clone(),
            value: value.value.clone(),
        }
    }
}

impl From<AddressRecord> for CanonicalAddress {
    fn from(value: AddressRecord) -> Self {
        Self {
            scope: IndexScope {
                chain: ChainId(value.chain),
                network: value.network,
            },
            value: value.value,
        }
    }
}

fn movement_kind_to_tag(kind: MovementKind) -> u8 {
    match kind {
        MovementKind::Transfer => 0,
        MovementKind::Input => 1,
        MovementKind::Output => 2,
        MovementKind::InternalTransfer => 3,
        MovementKind::Mint => 4,
        MovementKind::Burn => 5,
    }
}

fn movement_kind_from_tag(tag: u8) -> Result<MovementKind, DepositError> {
    match tag {
        0 => Ok(MovementKind::Transfer),
        1 => Ok(MovementKind::Input),
        2 => Ok(MovementKind::Output),
        3 => Ok(MovementKind::InternalTransfer),
        4 => Ok(MovementKind::Mint),
        5 => Ok(MovementKind::Burn),
        _ => Err(storage_error("PS movement record has an unknown kind")),
    }
}

impl From<&ValueMovement> for MovementRecord {
    fn from(value: &ValueMovement) -> Self {
        Self {
            id: value.id().0.clone(),
            asset_chain: value.asset().chain.0.clone(),
            asset: value.asset().asset.clone(),
            amount: amount::record_bytes(value.amount()),
            from: value.from().map(Into::into),
            to: value.to().map(Into::into),
            kind: movement_kind_to_tag(value.kind()),
        }
    }
}

impl TryFrom<MovementRecord> for ValueMovement {
    type Error = DepositError;

    fn try_from(value: MovementRecord) -> Result<Self, Self::Error> {
        let id = MovementId(value.id);
        let asset = AssetId {
            chain: ChainId(value.asset_chain),
            asset: value.asset,
        };
        let amount = amount::from_bytes(value.amount);
        let from = value.from.map(Into::into);
        let to = value.to.map(Into::into);
        let invalid = || storage_error("stored PS movement has invalid endpoints");
        match movement_kind_from_tag(value.kind)? {
            MovementKind::Transfer => Ok(Self::Transfer {
                id,
                asset,
                amount,
                from: from.ok_or_else(&invalid)?,
                to: to.ok_or_else(&invalid)?,
            }),
            MovementKind::Input => Ok(Self::Input {
                id,
                asset,
                amount,
                owner: from,
            }),
            MovementKind::Output => Ok(Self::Output {
                id,
                asset,
                amount,
                owner: to,
            }),
            MovementKind::InternalTransfer => Ok(Self::InternalTransfer {
                id,
                asset,
                amount,
                from: from.ok_or_else(&invalid)?,
                to: to.ok_or_else(&invalid)?,
            }),
            MovementKind::Mint => Ok(Self::Mint {
                id,
                asset,
                amount,
                to: to.ok_or_else(&invalid)?,
            }),
            MovementKind::Burn => Ok(Self::Burn {
                id,
                asset,
                amount,
                from: from.ok_or_else(&invalid)?,
            }),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct FeeRecord {
    pub(super) asset_chain: String,
    pub(super) asset: String,
    pub(super) amount: [u8; 32],
    pub(super) payer: Option<AddressRecord>,
}

impl From<&NetworkFee> for FeeRecord {
    fn from(value: &NetworkFee) -> Self {
        Self {
            asset_chain: value.asset.chain.0.clone(),
            asset: value.asset.asset.clone(),
            amount: amount::record_bytes(&value.amount),
            payer: value.payer.as_ref().map(Into::into),
        }
    }
}

impl From<FeeRecord> for NetworkFee {
    fn from(value: FeeRecord) -> Self {
        Self {
            asset: AssetId {
                chain: ChainId(value.asset_chain),
                asset: value.asset,
            },
            amount: amount::from_bytes(value.amount),
            payer: value.payer.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct TransactionData {
    pub(super) scope_chain: String,
    pub(super) scope_network: String,
    pub(super) transaction_chain: String,
    pub(super) transaction_id: String,
    pub(super) revision: u64,
    pub(super) status: StatusRecord,
    pub(super) movements: Vec<MovementRecord>,
    pub(super) fee: Option<FeeRecord>,
    pub(super) first_seen_at: u64,
    pub(super) observed_at: u64,
}

impl From<&ObservedTransaction> for TransactionData {
    fn from(value: &ObservedTransaction) -> Self {
        Self {
            scope_chain: value.scope.chain.0.clone(),
            scope_network: value.scope.network.clone(),
            transaction_chain: value.transaction_id.scope.chain.0.clone(),
            transaction_id: value.transaction_id.value.clone(),
            revision: value.revision.0,
            status: (&value.status).into(),
            movements: value.movements.iter().map(Into::into).collect(),
            fee: value.fee.as_ref().map(Into::into),
            first_seen_at: value.first_seen_at,
            observed_at: value.observed_at,
        }
    }
}

impl TryFrom<TransactionData> for ObservedTransaction {
    type Error = DepositError;

    fn try_from(value: TransactionData) -> Result<Self, Self::Error> {
        Ok(Self {
            scope: IndexScope {
                chain: ChainId(value.scope_chain),
                network: value.scope_network.clone(),
            },
            transaction_id: TransactionRef {
                scope: IndexScope {
                    chain: ChainId(value.transaction_chain),
                    network: value.scope_network,
                },
                value: value.transaction_id,
            },
            revision: ObservationRevision(value.revision),
            status: value.status.into(),
            movements: value
                .movements
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            fee: value.fee.map(Into::into),
            first_seen_at: value.first_seen_at,
            observed_at: value.observed_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct ObservationRecord {
    pub(super) version: u16,
    pub(super) id: String,
    pub(super) cursor: u64,
    pub(super) watch_ids: Vec<String>,
    pub(super) previous_status: Option<StatusRecord>,
    pub(super) transaction: TransactionData,
    pub(super) received_at: u64,
}

impl From<&MirroredObservation> for ObservationRecord {
    fn from(value: &MirroredObservation) -> Self {
        Self {
            version: OBSERVATION_RECORD_VERSION,
            id: value.event.id.0.clone(),
            cursor: value.event.cursor.0,
            watch_ids: value
                .event
                .watch_ids
                .iter()
                .map(|id| id.0.clone())
                .collect(),
            previous_status: value.event.previous_status.as_ref().map(Into::into),
            transaction: (&value.event.transaction).into(),
            received_at: value.received_at,
        }
    }
}

impl TryFrom<ObservationRecord> for MirroredObservation {
    type Error = DepositError;

    fn try_from(value: ObservationRecord) -> Result<Self, Self::Error> {
        if value.version != OBSERVATION_RECORD_VERSION {
            return Err(storage_error(format!(
                "unsupported PS observation record version {}",
                value.version
            )));
        }
        Ok(Self {
            event: ObservationEvent {
                id: EventId(value.id),
                cursor: EventCursor(value.cursor),
                watch_ids: value.watch_ids.into_iter().map(WatchId).collect(),
                previous_status: value.previous_status.map(Into::into),
                transaction: value.transaction.try_into()?,
            },
            received_at: value.received_at,
        })
    }
}
