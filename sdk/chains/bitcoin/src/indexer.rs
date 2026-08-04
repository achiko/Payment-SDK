use indexing::{
    BlockChanges, BlockRef, IndexError, IndexedBlock, ObservedTransaction, WatchTarget,
};

use crate::{BitcoinAddress, BitcoinTransactionId, Satoshi};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinBlock {
    pub reference: BlockRef,
    pub transactions: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitcoinWatchTarget {
    Address(BitcoinAddress),
    Transaction(BitcoinTransactionId),
}

impl IndexedBlock for BitcoinBlock {
    fn block_ref(&self) -> BlockRef {
        self.reference.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitcoinIndexEvent {
    Received {
        transaction_id: BitcoinTransactionId,
        output_index: u32,
        address: BitcoinAddress,
        value: Satoshi,
    },
    Spent {
        transaction_id: BitcoinTransactionId,
        input_index: u32,
        previous_transaction_id: BitcoinTransactionId,
        previous_output_index: u32,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitcoinUndo {
    pub created_outputs: Vec<([u8; 32], u32)>,
    pub restored_outputs: Vec<([u8; 32], u32)>,
}

pub trait BitcoinBlockInterpreter: Send + Sync {
    fn inspect(
        &self,
        block: &BitcoinBlock,
        watches: &[WatchTarget<BitcoinWatchTarget>],
    ) -> Result<BlockChanges<BitcoinIndexEvent, BitcoinUndo>, IndexError>;

    /// Converts chain-native inputs/outputs and status into IX facts. Classification
    /// as incoming or collection is intentionally impossible at this layer.
    fn observations(
        &self,
        block: &BitcoinBlock,
        watches: &[WatchTarget<BitcoinWatchTarget>],
    ) -> Result<Vec<ObservedTransaction>, IndexError>;
}
