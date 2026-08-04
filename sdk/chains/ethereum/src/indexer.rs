//! Ethereum-owned canonical block ingestion and interpretation.

mod codec;
mod interpreter;
mod model;
mod source;
mod websocket;

pub use codec::EthereumIndexRecordCodec;
pub use interpreter::EthereumBlockInterpreter;
pub use source::{
    EthereumHeadWake, EthereumHttpBlockSource, EthereumIndexSourceConfig, parse_new_heads_wake,
};
pub use websocket::{
    EthereumNewHeadsClient, EthereumNewHeadsConfig, EthereumNewHeadsConnection,
    EthereumNewHeadsConnectionEvent, EthereumNewHeadsConnector, TokioTungsteniteNewHeadsConnector,
};

use indexing::{BlockHash, BlockRef, BlockSource, BoxFuture, IndexedBlock, SourceError};

use crate::{EthereumAddress, EthereumTransactionId};

/// Canonical source payload retained by IX for replay and reorg recovery.
///
/// The bytes are the JSON-RPC `result` values, before conversion into domain
/// facts. This keeps source evidence independent of the parser's Rust layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumBlock {
    pub reference: BlockRef,
    pub raw_block: Vec<u8>,
    pub raw_receipts: Vec<Vec<u8>>,
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

/// Chain-owned reversible metadata. Repository-owned observation identities
/// remain outside this value and are allocated atomically at commit time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EthereumUndo {
    pub affected_transactions: Vec<EthereumTransactionId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EthereumIndexingCapabilities {
    pub block_transactions: bool,
    pub receipts: bool,
    pub logs: bool,
    pub traces: bool,
    pub internal_transfers: bool,
}

impl EthereumIndexingCapabilities {
    pub const V1: Self = Self {
        block_transactions: true,
        receipts: true,
        logs: true,
        traces: false,
        internal_transfers: false,
    };
}

/// Ethereum-specific capability boundary layered over generic ordered sync.
///
/// Implementations must use numbered canonical blocks. `safe`, `finalized`,
/// pending, mempool, and trace methods are outside the v1 contract.
pub trait EthereumIndexRpc: BlockSource<Block = EthereumBlock> {
    fn indexing_capabilities(&self) -> EthereumIndexingCapabilities;

    /// Reads one explicit block hash for audit/replay workflows. Numbered reads
    /// remain authoritative for ordered canonical synchronization.
    fn block_by_hash<'a>(
        &'a self,
        hash: BlockHash,
    ) -> BoxFuture<'a, Result<Option<EthereumBlock>, SourceError>>;
}
