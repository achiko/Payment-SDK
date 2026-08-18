//! Bitcoin-owned canonical block ingestion, interpretation, and UTXO facts.

mod interpreter;
mod model;
mod source;
mod transaction;

pub use interpreter::BlockInterpreter;
pub use source::{Blocks, Config as SourceConfig};

/// Standard chain-owned block source name used by embedded indexer composition.
pub type Source<C> = Blocks<C>;

use indexing::{BlockHash, BlockHeight, BlockRef, IndexedBlock};

use crate::{Address, ChainError, ChainErrorKind, Network, Satoshi, TransactionId};
use model::Transaction;

/// Bitcoin Core verbosity-2 result enriched with bounded external prevout
/// value/address facts and retained for replay and audit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub reference: BlockRef,
    transactions: Vec<Transaction>,
    raw: Vec<u8>,
}

impl Block {
    /// Parses and validates one complete Bitcoin Core verbosity-2 block.
    pub fn parse(
        raw: Vec<u8>,
        expected_height: Option<BlockHeight>,
        expected_hash: Option<&BlockHash>,
        network: Network,
    ) -> Result<Self, ChainError> {
        let parsed = model::BlockData::parse(&raw, expected_height, expected_hash, network)
            .map_err(|error| ChainError {
                kind: ChainErrorKind::InvalidTransaction,
                message: error.to_string(),
            })?;
        Ok(Self {
            reference: parsed.reference,
            transactions: parsed.transactions,
            raw,
        })
    }

    pub(in crate::indexer) fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    pub(in crate::indexer) fn raw(&self) -> &[u8] {
        &self.raw
    }
}

impl IndexedBlock for Block {
    fn block_ref(&self) -> BlockRef {
        self.reference.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Outpoint {
    pub transaction_id: TransactionId,
    pub output_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::indexer) struct UtxoKey {
    pub(in crate::indexer) address: Address,
    pub(in crate::indexer) outpoint: Outpoint,
}

/// Immutable creation fact used to serve indexed UTXO queries. Spend facts are
/// identified by the same address and outpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::indexer) struct IndexedOutput {
    pub(in crate::indexer) outpoint: Outpoint,
    pub(in crate::indexer) value: Satoshi,
    pub(in crate::indexer) script_pubkey: Vec<u8>,
    pub(in crate::indexer) address: Address,
    pub(in crate::indexer) created_height: BlockHeight,
    pub(in crate::indexer) coinbase: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexEvent {
    Received {
        transaction_id: TransactionId,
        output_index: u32,
        address: Address,
        value: Satoshi,
    },
    Spent {
        transaction_id: TransactionId,
        input_index: u32,
        previous_transaction_id: TransactionId,
        previous_output_index: u32,
    },
}
