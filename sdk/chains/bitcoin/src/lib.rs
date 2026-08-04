//! Bitcoin-owned contracts and chain-native types.

mod address;
mod collection;
mod indexer;
mod rpc;
mod transaction;
mod wallet;

pub use address::BitcoinAddress;
pub use collection::{
    BitcoinBatchCollectionRequest, BitcoinCollectionAttribution, BitcoinCollectionRequirement,
    BitcoinCollectionSource,
};
pub use indexer::{
    BitcoinBlock, BitcoinBlockInterpreter, BitcoinIndexEvent, BitcoinUndo, BitcoinWatchTarget,
};
pub use rpc::{BitcoinRpc, BitcoinRpcUtxo};
pub use transaction::{
    BitcoinBuildRequest, BitcoinInput, BitcoinOutput, BitcoinReceipt, BitcoinSignedTransaction,
    BitcoinTransactionBuilder, BitcoinTransactionCodec, BitcoinTransactionId,
    BitcoinTransactionSigning, BitcoinUtxo, SighashType, UnsignedBitcoinTransaction,
};
pub use wallet::{
    BitcoinAddressGenerator, BitcoinAddressKind, BitcoinGenerateAddress, BitcoinNetwork,
    BitcoinWallet,
};

use chain_contract::Chain;
use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug)]
pub struct Bitcoin;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitcoinAsset {
    Native,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Satoshi(pub u64);

impl Chain for Bitcoin {
    const NAME: &'static str = "bitcoin";

    type Asset = BitcoinAsset;
    type Address = BitcoinAddress;
    type Amount = Satoshi;
    type TransactionId = BitcoinTransactionId;
    type GenerateAddressRequest = BitcoinGenerateAddress;
    type TransferRequest = BitcoinBuildRequest;
    type CollectionRequest = BitcoinBatchCollectionRequest;
    type CollectionRequirement = BitcoinCollectionRequirement;
    type CollectionAttribution = BitcoinCollectionAttribution;
    type UnsignedTransaction = UnsignedBitcoinTransaction;
    type SignedTransaction = BitcoinSignedTransaction;
    type Receipt = BitcoinReceipt;
}
