//! Ethereum-owned canonical block ingestion and interpretation.

mod interpreter;
mod model;
mod settings;
mod source;

pub use interpreter::BlockInterpreter;
pub use settings::{IndexerSettings, Network};
pub use source::{BlockClient, SourceConfig};

/// Standard chain-owned block source name used by embedded indexer composition.
pub type Source<C> = BlockClient<C>;

/// Ethereum implementation of the shared indexing service.
pub type Indexer<C, R> = indexing::Service<Source<C>, BlockInterpreter, R>;

use indexing::{BlockRef, IndexedBlock};

/// Canonical source payload retained by IX for replay and reorg recovery.
///
/// The bytes are the JSON-RPC `result` values, before conversion into domain
/// facts. This keeps source evidence independent of the parser's Rust layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub reference: BlockRef,
    pub raw_block: Vec<u8>,
    pub raw_receipts: Vec<Vec<u8>>,
}

impl IndexedBlock for Block {
    fn block_ref(&self) -> BlockRef {
        self.reference.clone()
    }
}
