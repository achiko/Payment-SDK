//! Bitcoin-owned contracts and chain-native types.

mod address;
mod batch;
mod error;
mod indexer;
mod network;
mod rpc;
mod transaction;
mod wallet;

pub use address::Address;
pub use address::Address as BitcoinAddress;
pub use error::{ChainError, ChainErrorKind};
pub use indexer::{Block, BlockInterpreter, Blocks, Indexer, Outpoint, Source, SourceConfig};
pub use network::Network;
pub use rpc::{
    BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB, Client as RpcClient, CoreConfig, FeeClient, Fees,
    Node, NodeStatus, Preflight, TransactionClient, Transactions, UnspentOutput, UtxoSet,
    format_bitcoin_block_hash, parse_bitcoin_block_hash,
};
pub use transaction::{
    BuildRequest, FeeRate, Input, InputInspection, Output, OutputInspection, SighashType,
    SignedTransaction, SpendSource, TransactionBuilder, TransactionId, TransactionInspection,
    UnsignedTransaction,
};
pub use wallet::{AddressType, Config as WalletConfig, Factory as WalletProvider, IndexUtxos};

use base::{Asset, Decimal, DecimalError};
use base::{Chain, ChainCollection};
use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Canonical chain key shared by metadata, indexing scopes, and persistence.
pub const CHAIN: &str = "bitcoin";

const MAINNET: Chain = Chain::new(Network::Mainnet.id(), CHAIN, "BTC");
const TESTNET: &[(&str, Chain)] = &[
    ("testnet3", Chain::new(Network::Testnet3.id(), CHAIN, "BTC")),
    ("testnet4", Chain::new(Network::Testnet4.id(), CHAIN, "BTC")),
    ("signet", Chain::new(Network::Signet.id(), CHAIN, "BTC")),
    ("regtest", Chain::new(Network::Regtest.id(), CHAIN, "BTC")),
];

pub const CHAINS: ChainCollection = ChainCollection::new(MAINNET, TESTNET);
pub const BTC: Asset = Asset::new(MAINNET, "Bitcoin", "BTC", 8);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Satoshi(pub u64);

impl Satoshi {
    pub fn from_decimal(value: &Decimal) -> Result<Self, DecimalError> {
        value.to_atomic_u64(BTC.decimals).map(Self)
    }

    #[must_use]
    pub fn decimal(self) -> Decimal {
        Decimal::from_atomic(self.0.into(), BTC.decimals)
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn metadata_uses_the_canonical_chain_key() {
        assert_eq!(CHAINS.mainnet.name, CHAIN);
        assert!(
            CHAINS
                .testnet
                .as_slice()
                .iter()
                .all(|(_, chain)| chain.name == CHAIN)
        );
    }
}
