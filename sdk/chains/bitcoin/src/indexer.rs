//! Bitcoin-owned canonical block ingestion, interpretation, and UTXO projection.

mod codec;
mod interpreter;
mod model;
mod source;

pub use codec::BitcoinIndexRecordCodec;
pub use interpreter::BitcoinBlockInterpreter;
pub use source::{BitcoinCoreBlockSource, BitcoinIndexSourceConfig};

use indexing::{
    BlockHash, BlockHeight, BlockRef, BlockSource, BoxFuture, IndexedBlock, SourceError,
};

use crate::{BitcoinAddress, BitcoinTransactionId, Satoshi};

/// Bitcoin Core verbosity-2 result enriched with bounded external prevout
/// value/address facts and retained for replay and audit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinBlock {
    pub reference: BlockRef,
    pub raw_block: Vec<u8>,
}

impl IndexedBlock for BitcoinBlock {
    fn block_ref(&self) -> BlockRef {
        self.reference.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitcoinWatchTarget {
    Address(BitcoinAddress),
    Transaction(BitcoinTransactionId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BitcoinOutPoint {
    pub transaction_id: BitcoinTransactionId,
    pub output_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitcoinProjectionKey {
    Utxo {
        address: BitcoinAddress,
        outpoint: BitcoinOutPoint,
    },
    SpentMarker {
        address: BitcoinAddress,
        outpoint: BitcoinOutPoint,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BitcoinUtxoKey {
    pub address: BitcoinAddress,
    pub outpoint: BitcoinOutPoint,
}

/// Immutable creation fact used to serve IX UTXO queries. Spends are recorded
/// separately as order-independent marker facts keyed by the same outpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinIndexedOutput {
    pub outpoint: BitcoinOutPoint,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
    pub address: BitcoinAddress,
    pub created_height: BlockHeight,
    pub coinbase: bool,
}

/// Chain-native forward state. The codec converts this into opaque, versioned
/// repository projection mutations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitcoinUtxoProjection {
    pub creates: Vec<BitcoinIndexedOutput>,
    /// Unconditional spent-marker keys for inputs matching an active watch.
    /// Creation-only value, script, height, and coinbase metadata are not
    /// duplicated into spend facts.
    pub spends: Vec<BitcoinUtxoKey>,
    /// Input-bounded spend candidates for addresses without an active watch.
    /// The repository materializes each marker only when the corresponding
    /// creation fact already exists in the same canonical projection snapshot.
    pub conditional_spends: Vec<BitcoinUtxoKey>,
}

/// Merge-friendly inverse of one block's projection facts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitcoinUndo {
    pub remove_created: Vec<BitcoinUtxoKey>,
    pub remove_spent_markers: Vec<BitcoinUtxoKey>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitcoinIndexingCapabilities {
    pub canonical_blocks: bool,
    pub resolved_prevouts: bool,
    pub watched_utxos: bool,
    pub mempool: bool,
}

impl BitcoinIndexingCapabilities {
    pub const V1: Self = Self {
        canonical_blocks: true,
        resolved_prevouts: true,
        watched_utxos: true,
        mempool: false,
    };
}

pub trait BitcoinIndexRpc: BlockSource<Block = BitcoinBlock> {
    fn indexing_capabilities(&self) -> BitcoinIndexingCapabilities;

    fn block_by_hash<'a>(
        &'a self,
        hash: BlockHash,
    ) -> BoxFuture<'a, Result<Option<BitcoinBlock>, SourceError>>;
}
