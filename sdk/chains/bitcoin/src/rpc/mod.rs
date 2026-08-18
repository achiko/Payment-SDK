mod blocks;
mod capability;
mod client;
mod config;
mod error;
mod fees;
mod node;
mod transactions;
mod transport;
mod types;
mod wire;

pub use capability::{FeeClient, Fees, Node, TransactionClient, Transactions};
pub use client::Client;
pub use config::CoreConfig;
pub(crate) use error::source_error;
pub use types::{NodeStatus, Preflight, UnspentOutput, UtxoSet};
pub(crate) use wire::parse_header;
pub use wire::{format_bitcoin_block_hash, parse_bitcoin_block_hash};

const SATOSHIS_PER_BITCOIN: u64 = 100_000_000;

/// Bitcoin Core's maximum accepted `maxfeerate` value, expressed in sat/kvB.
pub const BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB: u64 = SATOSHIS_PER_BITCOIN;

#[cfg(test)]
mod test;
