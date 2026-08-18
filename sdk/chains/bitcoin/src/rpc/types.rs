use indexing::{BlockHash, BlockHeight, BlockRef};

use crate::{Network, Satoshi};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnspentOutput {
    pub transaction_id: [u8; 32],
    pub output_index: u32,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
    pub confirmations: u64,
    pub coinbase: bool,
}

/// One canonically fenced index UTXO read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtxoSet {
    pub checkpoint: BlockRef,
    pub outputs: Vec<UnspentOutput>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeStatus {
    pub version: u64,
    pub network: Network,
    pub height: BlockHeight,
    pub best_block_hash: BlockHash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preflight {
    pub allowed: bool,
    pub reject_reason: Option<String>,
    pub virtual_size: Option<u64>,
    pub base_fee: Option<Satoshi>,
}
