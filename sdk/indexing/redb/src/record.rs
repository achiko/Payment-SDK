use bincode::{Decode, Encode};
use indexing::{
    AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, CanonicalAddress,
    CanonicalStatus, CanonicalTransaction, ChainId, IndexError, IndexScope, IndexedOutput,
    NetworkFee, OutputId, OutputKey, TransactionRef,
};

#[derive(Clone, Debug, Encode, Decode)]
pub(super) struct BlockRecord {
    position: u64,
    height: u64,
    hash: Vec<u8>,
    parent: Option<ParentRecord>,
    timestamp: Option<u64>,
}

#[derive(Clone, Debug, Encode, Decode)]
struct ParentRecord {
    position: u64,
    hash: Vec<u8>,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(super) struct AddressRecord {
    chain: String,
    network: String,
    value: String,
}

#[derive(Clone, Debug, Encode, Decode)]
struct TransactionIdentity {
    chain: String,
    network: String,
    value: String,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(super) struct AssetRecord {
    chain: String,
    value: String,
}

#[derive(Clone, Copy, Debug, Encode, Decode)]
pub(super) enum MovementTag {
    Transfer,
    Input,
    Output,
    Mint,
    Burn,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(super) struct MovementRecord {
    pub(super) kind: MovementTag,
    pub(super) id: String,
    pub(super) asset: AssetRecord,
    pub(super) amount: String,
    pub(super) from: Option<AddressRecord>,
    pub(super) to: Option<AddressRecord>,
}

#[derive(Clone, Debug, Encode, Decode)]
struct FeeRecord {
    asset: AssetRecord,
    amount: String,
    payer: Option<AddressRecord>,
}

#[derive(Clone, Debug, Encode, Decode)]
enum StatusRecord {
    Included(BlockRecord),
    Failed(BlockRecord, Option<String>),
}

#[derive(Clone, Debug, Encode, Decode)]
pub(super) struct TransactionRecord {
    id: TransactionIdentity,
    status: StatusRecord,
    movements: Vec<MovementRecord>,
    fee: Option<FeeRecord>,
}

#[derive(Clone, Debug, Encode, Decode)]
struct OutputIdentity {
    address: AddressRecord,
    transaction: TransactionIdentity,
    index: u32,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(super) struct OutputRecord {
    id: OutputIdentity,
    asset: AssetRecord,
    amount: String,
    evidence: Vec<u8>,
    created_at: u64,
    coinbase: bool,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(super) struct JournalRecord {
    pub(super) block: BlockRecord,
    pub(super) previous_checkpoint: Option<BlockRecord>,
    pub(super) history_keys: Vec<Vec<u8>>,
    pub(super) remove_output_keys: Vec<Vec<u8>>,
    pub(super) restore_outputs: Vec<RestoredOutput>,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(super) struct RestoredOutput {
    pub(super) key: Vec<u8>,
    pub(super) value: OutputRecord,
}

impl BlockRecord {
    pub(super) fn from_domain(value: &BlockRef) -> Self {
        Self {
            position: value.position.0,
            height: value.height.0,
            hash: value.hash.0.clone(),
            parent: value.parent.as_ref().map(|parent| ParentRecord {
                position: parent.position.0,
                hash: parent.hash.0.clone(),
            }),
            timestamp: value.timestamp,
        }
    }

    pub(super) fn into_domain(self) -> BlockRef {
        BlockRef {
            position: BlockPosition(self.position),
            height: BlockHeight(self.height),
            hash: BlockHash(self.hash),
            parent: self.parent.map(|parent| BlockParent {
                position: BlockPosition(parent.position),
                hash: BlockHash(parent.hash),
            }),
            timestamp: self.timestamp,
        }
    }
}

impl AddressRecord {
    pub(super) fn from_domain(value: &CanonicalAddress) -> Self {
        Self {
            chain: value.scope.chain.0.clone(),
            network: value.scope.network.clone(),
            value: value.value.clone(),
        }
    }

    pub(super) fn into_domain(self) -> CanonicalAddress {
        CanonicalAddress {
            scope: IndexScope {
                chain: ChainId(self.chain),
                network: self.network,
            },
            value: self.value,
        }
    }
}

impl TransactionIdentity {
    fn from_domain(value: &TransactionRef) -> Self {
        Self {
            chain: value.scope.chain.0.clone(),
            network: value.scope.network.clone(),
            value: value.value.clone(),
        }
    }

    fn into_domain(self) -> TransactionRef {
        TransactionRef {
            scope: IndexScope {
                chain: ChainId(self.chain),
                network: self.network,
            },
            value: self.value,
        }
    }
}

impl AssetRecord {
    pub(super) fn from_domain(value: &AssetId) -> Self {
        Self {
            chain: value.chain.0.clone(),
            value: value.asset.clone(),
        }
    }

    pub(super) fn into_domain(self) -> AssetId {
        AssetId {
            chain: ChainId(self.chain),
            asset: self.value,
        }
    }
}

impl TransactionRecord {
    pub(super) fn from_domain(value: &CanonicalTransaction) -> Self {
        Self {
            id: TransactionIdentity::from_domain(&value.transaction_id),
            status: match &value.status {
                CanonicalStatus::Included { block } => {
                    StatusRecord::Included(BlockRecord::from_domain(block))
                }
                CanonicalStatus::Failed { block, reason } => {
                    StatusRecord::Failed(BlockRecord::from_domain(block), reason.clone())
                }
            },
            movements: value
                .movements
                .iter()
                .map(MovementRecord::from_domain)
                .collect(),
            fee: value.fee.as_ref().map(FeeRecord::from_domain),
        }
    }

    pub(super) fn into_domain(self) -> Result<CanonicalTransaction, IndexError> {
        let transaction_id = self.id.into_domain();
        Ok(CanonicalTransaction {
            scope: transaction_id.scope.clone(),
            transaction_id,
            status: match self.status {
                StatusRecord::Included(block) => CanonicalStatus::Included {
                    block: block.into_domain(),
                },
                StatusRecord::Failed(block, reason) => CanonicalStatus::Failed {
                    block: block.into_domain(),
                    reason,
                },
            },
            movements: self
                .movements
                .into_iter()
                .map(MovementRecord::into_domain)
                .collect::<Result<_, _>>()?,
            fee: self.fee.map(FeeRecord::into_domain).transpose()?,
        })
    }
}

impl FeeRecord {
    fn from_domain(value: &NetworkFee) -> Self {
        Self {
            asset: AssetRecord::from_domain(&value.asset),
            amount: crate::amount_record::encode(&value.amount),
            payer: value.payer.as_ref().map(AddressRecord::from_domain),
        }
    }

    fn into_domain(self) -> Result<NetworkFee, IndexError> {
        Ok(NetworkFee {
            asset: self.asset.into_domain(),
            amount: crate::amount_record::decode(&self.amount)?,
            payer: self.payer.map(AddressRecord::into_domain),
        })
    }
}

impl OutputIdentity {
    fn from_domain(value: &OutputKey) -> Self {
        Self {
            address: AddressRecord::from_domain(&value.address),
            transaction: TransactionIdentity::from_domain(&value.output.transaction),
            index: value.output.index,
        }
    }

    fn into_domain(self) -> OutputKey {
        OutputKey {
            address: self.address.into_domain(),
            output: OutputId {
                transaction: self.transaction.into_domain(),
                index: self.index,
            },
        }
    }
}

impl OutputRecord {
    pub(super) fn from_domain(value: &IndexedOutput) -> Self {
        Self {
            id: OutputIdentity::from_domain(&value.key()),
            asset: AssetRecord::from_domain(&value.asset),
            amount: crate::amount_record::encode(&value.amount),
            evidence: value.evidence.clone(),
            created_at: value.created_at.0,
            coinbase: value.coinbase,
        }
    }

    pub(super) fn into_domain(self) -> Result<IndexedOutput, IndexError> {
        let key = self.id.into_domain();
        Ok(IndexedOutput {
            id: key.output,
            address: key.address,
            asset: self.asset.into_domain(),
            amount: crate::amount_record::decode(&self.amount)?,
            evidence: self.evidence,
            created_at: BlockHeight(self.created_at),
            coinbase: self.coinbase,
        })
    }
}

impl JournalRecord {
    pub(super) fn block(&self) -> BlockRef {
        self.block.clone().into_domain()
    }
}

#[cfg(test)]
mod tests {
    use bincode::{Decode, Encode, config};

    use super::BlockRecord;

    #[derive(Encode, Decode)]
    struct HeightOnlyBlockRecord {
        height: u64,
        hash: Vec<u8>,
        parent_hash: Option<Vec<u8>>,
        timestamp: Option<u64>,
    }

    #[test]
    fn rejects_height_only_block_records() {
        let old = HeightOnlyBlockRecord {
            height: 42,
            hash: vec![0xab; 32],
            parent_hash: Some(vec![0xcd; 32]),
            timestamp: Some(1_000),
        };
        let bytes = bincode::encode_to_vec(old, config::standard()).expect("old record encodes");

        assert!(
            bincode::decode_from_slice::<BlockRecord, _>(&bytes, config::standard()).is_err(),
            "height-only redb records must not decode as complete block references"
        );
    }
}
