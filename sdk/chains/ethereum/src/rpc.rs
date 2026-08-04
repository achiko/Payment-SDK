use crate::{
    BoxFuture, EthereumAddress, EthereumAsset, EthereumBlock, EthereumBuildContext,
    EthereumReceipt, EthereumSignedTransaction, EthereumTransactionId, EthereumTransferRequest,
    Wei,
};
use indexing::{BlockHeight, BlockRef, SourceError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EthereumIndexingCapabilities {
    pub block_transactions: bool,
    pub receipts: bool,
    pub logs: bool,
    /// Required if internal/native value transfers must be indexed.
    pub traces: bool,
}

/// Concrete Ethereum RPC surface; JSON-RPC framing remains generic.
pub trait EthereumRpc: Send + Sync {
    fn indexing_capabilities<'a>(
        &'a self,
    ) -> BoxFuture<'a, Result<EthereumIndexingCapabilities, SourceError>>;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>>;

    fn block_at<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<EthereumBlock, SourceError>>;

    fn balance<'a>(
        &'a self,
        address: EthereumAddress,
        asset: &'a EthereumAsset,
        at: Option<BlockRef>,
    ) -> BoxFuture<'a, Result<Wei, SourceError>>;

    fn nonce<'a>(&'a self, address: EthereumAddress) -> BoxFuture<'a, Result<u64, SourceError>>;

    /// Returns nonce, gas limit, and EIP-1559 fees for one concrete transfer.
    fn build_context<'a>(
        &'a self,
        request: &'a EthereumTransferRequest,
    ) -> BoxFuture<'a, Result<EthereumBuildContext, SourceError>>;

    fn receipt<'a>(
        &'a self,
        id: &'a EthereumTransactionId,
    ) -> BoxFuture<'a, Result<Option<EthereumReceipt>, SourceError>>;

    fn broadcast<'a>(
        &'a self,
        transaction: EthereumSignedTransaction,
    ) -> BoxFuture<'a, Result<EthereumTransactionId, SourceError>>;
}
