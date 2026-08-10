//! Bitcoin-owned contracts and chain-native types.

mod address;
mod collection;
mod indexer;
mod production_wallet;
mod rpc;
mod transaction;
mod wallet;

pub use address::BitcoinAddress;
pub use collection::{
    BitcoinBatchCollectionRequest, BitcoinCollectionAttribution, BitcoinCollectionRequirement,
    BitcoinCollectionSource,
};
pub use indexer::{
    BitcoinBlock, BitcoinBlockInterpreter, BitcoinCoreBlockSource, BitcoinIndexEvent,
    BitcoinIndexRecordCodec, BitcoinIndexRpc, BitcoinIndexSourceConfig, BitcoinIndexedOutput,
    BitcoinIndexingCapabilities, BitcoinOutPoint, BitcoinProjectionKey, BitcoinUndo,
    BitcoinUtxoKey, BitcoinUtxoProjection, BitcoinWatchTarget,
};
pub use production_wallet::{BitcoinNodePolicy, BitcoinProductionWallet};
pub use rpc::{
    BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB, BitcoinCoreClient, BitcoinCoreConfig,
    BitcoinCoreNodeStatus, BitcoinNodeRpc, BitcoinPreflight, BitcoinRpc, BitcoinRpcUtxo,
    BitcoinUtxoSet, BitcoinUtxoSource, format_bitcoin_block_hash, parse_bitcoin_block_hash,
};
pub use transaction::{
    BitcoinBuildRequest, BitcoinInput, BitcoinOutput, BitcoinReceipt, BitcoinSignedInputInspection,
    BitcoinSignedOutputInspection, BitcoinSignedTransaction, BitcoinSignedTransactionInspection,
    BitcoinTransactionBuilder, BitcoinTransactionCodec, BitcoinTransactionId,
    BitcoinTransactionSigning, BitcoinUtxo, SatoshisPerKvb, SighashType,
    UnsignedBitcoinTransaction,
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
