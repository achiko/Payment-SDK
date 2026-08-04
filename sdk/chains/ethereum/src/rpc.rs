use crate::{
    BoxFuture, EthereumAddress, EthereumAsset, EthereumBuildContext, EthereumReceipt,
    EthereumSignedTransaction, EthereumTransactionId, EthereumTransferRequest, Wei,
};
use indexing::{BlockRef, SourceError};

/// Wallet-facing Ethereum RPC surface.
///
/// Canonical block synchronization is deliberately exposed through the
/// separate `EthereumIndexRpc`/`BlockSource` boundary. Wallet composition must
/// not acquire Indexer Service ownership by implementing this trait.
pub trait EthereumRpc: Send + Sync {
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
