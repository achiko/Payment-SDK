use crate::{BitcoinReceipt, BitcoinSignedTransaction, BitcoinTransactionId, BoxFuture, Satoshi};
use indexing::{BlockHeight, BlockRef, SourceError};
use transaction_utxo::FeeRate;

use crate::indexer::BitcoinBlock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinRpcUtxo {
    pub transaction_id: [u8; 32],
    pub output_index: u32,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
    pub confirmations: u64,
    pub coinbase: bool,
}

/// Concrete Bitcoin RPC surface; JSON-RPC framing remains in `packages/json-rpc`.
pub trait BitcoinRpc: Send + Sync {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>>;

    fn block_at<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<BitcoinBlock, SourceError>>;

    fn utxos<'a>(
        &'a self,
        scripts: Vec<Vec<u8>>,
    ) -> BoxFuture<'a, Result<Vec<BitcoinRpcUtxo>, SourceError>>;

    fn estimate_fee_rate<'a>(&'a self) -> BoxFuture<'a, Result<FeeRate, SourceError>>;

    fn broadcast<'a>(
        &'a self,
        transaction: BitcoinSignedTransaction,
    ) -> BoxFuture<'a, Result<BitcoinTransactionId, SourceError>>;

    fn receipt<'a>(
        &'a self,
        id: &'a BitcoinTransactionId,
    ) -> BoxFuture<'a, Result<Option<BitcoinReceipt>, SourceError>>;
}
