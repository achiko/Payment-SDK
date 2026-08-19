//! Bitcoin-owned canonical block ingestion, interpretation, and UTXO facts.

mod interpreter;
mod model;
mod source;
mod transaction;

mod settings;

pub use interpreter::BlockInterpreter;
pub use settings::{Credentials, IndexerSettings};
pub use source::{Blocks, Config as SourceConfig};

/// Standard chain-owned block source name used by embedded indexer composition.
pub type Source<C> = Blocks<C>;

/// Bitcoin implementation of the shared indexing service.
pub type Indexer<C, R> = indexing::Service<Source<C>, BlockInterpreter, R>;

use indexing::{BlockHash, BlockHeight, BlockRef, IndexedBlock};

use crate::{Address, ChainError, ChainErrorKind, Network, Satoshi, TransactionId};
use model::Transaction;

/// Validated Bitcoin block with the previous-output facts required for
/// deterministic interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub reference: BlockRef,
    transactions: Vec<Transaction>,
}

impl Block {
    /// Parses and validates one complete Bitcoin Core verbosity-2 block.
    pub fn parse(
        raw: &[u8],
        expected_height: Option<BlockHeight>,
        expected_hash: Option<&BlockHash>,
        network: Network,
    ) -> Result<Self, ChainError> {
        let parsed = model::BlockData::parse(raw, expected_height, expected_hash, network)
            .map_err(|error| ChainError {
                kind: ChainErrorKind::InvalidTransaction,
                message: error.to_string(),
            })?;
        Ok(Self {
            reference: parsed.reference,
            transactions: parsed.transactions,
        })
    }

    pub(in crate::indexer) fn transactions(&self) -> &[Transaction] {
        &self.transactions
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
