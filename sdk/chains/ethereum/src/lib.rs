//! Ethereum-owned contracts and chain-native types.

mod address;
mod batch;
mod erc20;
mod error;
mod indexer;
mod rpc;
mod transaction;
mod wallet;

pub use address::Address as EthereumAddress;
pub use address::{Address, AddressParseError};
pub use error::{ChainError, ChainErrorKind};
pub use indexer::{
    Block, BlockClient, BlockInterpreter, Indexer, IndexerSettings, Network, Source, SourceConfig,
};
pub use rpc::{
    AccountClient, Accounts, BuildError, BuildErrorKind, Client as RpcClient, HttpAccounts,
    HttpConfig, HttpTransactions, Limits, TransactionClient, Transactions,
};
pub use transaction::{
    BuildContext, FeeInspection, IdError, InspectionError, SignedError, SignedTransaction,
    TransactionBuilder, TransactionId, TransferIntent, TransferRequest, UnsignedTransaction,
};
pub use wallet::{WalletConfig, WalletProvider};

use base::{Asset, Decimal, DecimalError};
use base::{Chain, ChainCollection, NetworkId, NetworkKind};
use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Canonical chain key shared by metadata, indexing scopes, and persistence.
pub const CHAIN: &str = "ethereum";

const MAINNET: Chain = Chain::new(NetworkId::new("1", NetworkKind::Mainnet), CHAIN, "ETH");
const TESTNET: &[(&str, Chain)] = &[(
    "sepolia",
    Chain::new(
        NetworkId::new("11155111", NetworkKind::Testnet),
        CHAIN,
        "ETH",
    ),
)];

pub const CHAINS: ChainCollection = ChainCollection::new(MAINNET, TESTNET);
pub const ETH: Asset = Asset::new(MAINNET, "Ethereum", "ETH", 18);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetKind {
    Native,
    Erc20(Address),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Wei(pub [u8; 32]);

impl Wei {
    pub const ZERO: Self = Self([0; 32]);

    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        Self(alloy_primitives::U256::from(value).to_be_bytes())
    }

    pub fn from_decimal(value: &Decimal) -> Result<Self, DecimalError> {
        value.to_atomic_be_bytes(ETH.decimals).map(Self)
    }

    #[must_use]
    pub fn decimal(&self) -> Decimal {
        Decimal::from_atomic(num_bigint::BigUint::from_bytes_be(&self.0), ETH.decimals)
    }

    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|byte| *byte == 0)
    }

    pub fn checked_to_u128(&self) -> Option<u128> {
        u128::try_from(alloy_primitives::U256::from_be_bytes(self.0)).ok()
    }

    pub fn checked_add(&self, other: &Self) -> Option<Self> {
        alloy_primitives::U256::from_be_bytes(self.0)
            .checked_add(alloy_primitives::U256::from_be_bytes(other.0))
            .map(|value| Self(value.to_be_bytes()))
    }

    pub fn checked_sub(&self, other: &Self) -> Option<Self> {
        alloy_primitives::U256::from_be_bytes(self.0)
            .checked_sub(alloy_primitives::U256::from_be_bytes(other.0))
            .map(|value| Self(value.to_be_bytes()))
    }

    pub fn checked_mul_u64(&self, multiplier: u64) -> Option<Self> {
        alloy_primitives::U256::from_be_bytes(self.0)
            .checked_mul(alloy_primitives::U256::from(multiplier))
            .map(|value| Self(value.to_be_bytes()))
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
