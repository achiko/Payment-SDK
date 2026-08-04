use indexing::{
    BlockChanges, BlockRef, IndexError, IndexedBlock, ObservedTransaction, WatchTarget,
};

use crate::{EthereumAddress, EthereumTransactionId, Wei};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumBlock {
    pub reference: BlockRef,
    pub transactions: Vec<Vec<u8>>,
    pub receipts: Vec<Vec<u8>>,
    pub traces: Option<Vec<Vec<u8>>>,
}

impl IndexedBlock for EthereumBlock {
    fn block_ref(&self) -> BlockRef {
        self.reference.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EthereumWatchTarget {
    Address(EthereumAddress),
    Transaction(EthereumTransactionId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EthereumIndexEvent {
    NativeTransfer {
        transaction_id: EthereumTransactionId,
        from: EthereumAddress,
        to: EthereumAddress,
        value: Wei,
        internal: bool,
    },
    TokenTransfer {
        transaction_id: EthereumTransactionId,
        token: EthereumAddress,
        from: EthereumAddress,
        to: EthereumAddress,
        value: Wei,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EthereumUndo {
    pub affected_transactions: Vec<EthereumTransactionId>,
}

pub trait EthereumBlockInterpreter: Send + Sync {
    fn inspect(
        &self,
        block: &EthereumBlock,
        watches: &[WatchTarget<EthereumWatchTarget>],
    ) -> Result<BlockChanges<EthereumIndexEvent, EthereumUndo>, IndexError>;

    fn observations(
        &self,
        block: &EthereumBlock,
        watches: &[WatchTarget<EthereumWatchTarget>],
    ) -> Result<Vec<ObservedTransaction>, IndexError>;
}
