//! Ethereum-owned contracts and chain-native types.

mod address;
mod collection;
mod indexer;
mod rpc;
mod transaction;
mod wallet;

pub use address::EthereumAddress;
pub use collection::{
    EthereumCollectionAttribution, EthereumCollectionRequest, EthereumCollectionRequirement,
};
pub use indexer::{
    EthereumBlock, EthereumBlockInterpreter, EthereumHeadWake, EthereumHttpBlockSource,
    EthereumIndexRecordCodec, EthereumIndexRpc, EthereumIndexSourceConfig,
    EthereumIndexingCapabilities, EthereumNewHeadsClient, EthereumNewHeadsConfig,
    EthereumNewHeadsConnection, EthereumNewHeadsConnectionEvent, EthereumNewHeadsConnector,
    EthereumUndo, EthereumWatchTarget, TokioTungsteniteNewHeadsConnector, parse_new_heads_wake,
};
pub use rpc::EthereumRpc;
pub use transaction::{
    EthereumBuildContext, EthereumReceipt, EthereumSignedTransaction, EthereumTransactionBuilder,
    EthereumTransactionCodec, EthereumTransactionId, EthereumTransactionSigning,
    EthereumTransferRequest, UnsignedEthereumTransaction,
};
pub use wallet::{EthereumAddressGenerator, EthereumGenerateAddress, EthereumWallet};

use chain_contract::Chain;
use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug)]
pub struct Ethereum;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EthereumAsset {
    Native,
    Erc20(EthereumAddress),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wei(pub [u8; 32]);

impl Wei {
    pub const ZERO: Self = Self([0; 32]);

    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        Self(alloy_primitives::U256::from(value).to_be_bytes())
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

impl Chain for Ethereum {
    const NAME: &'static str = "ethereum";

    type Asset = EthereumAsset;
    type Address = EthereumAddress;
    type Amount = Wei;
    type TransactionId = EthereumTransactionId;
    type GenerateAddressRequest = EthereumGenerateAddress;
    type TransferRequest = EthereumTransferRequest;
    type CollectionRequest = EthereumCollectionRequest;
    type CollectionRequirement = EthereumCollectionRequirement;
    type CollectionAttribution = EthereumCollectionAttribution;
    type UnsignedTransaction = UnsignedEthereumTransaction;
    type SignedTransaction = EthereumSignedTransaction;
    type Receipt = EthereumReceipt;
}
