use crate::{BoxFuture, Chain, ChainError};
use signer::Signer;

pub trait TransferBuilder<C: Chain>: Send + Sync {
    fn build_transfer<'a>(
        &'a self,
        request: C::TransferRequest,
    ) -> BoxFuture<'a, Result<C::UnsignedTransaction, ChainError>>;
}

/// The chain computes its signing payload and inserts returned signatures.
/// The injected signer remains unaware of the chain transaction type.
pub trait TransactionSigner<C: Chain>: Send + Sync {
    fn sign_transaction<'a>(
        &'a self,
        transaction: C::UnsignedTransaction,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<C::SignedTransaction, ChainError>>;
}

pub trait Broadcaster<C: Chain>: Send + Sync {
    fn broadcast<'a>(
        &'a self,
        transaction: C::SignedTransaction,
    ) -> BoxFuture<'a, Result<C::TransactionId, ChainError>>;
}

pub trait TransactionReader<C: Chain>: Send + Sync {
    fn transaction<'a>(
        &'a self,
        id: &'a C::TransactionId,
    ) -> BoxFuture<'a, Result<Option<C::Receipt>, ChainError>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionSubmission<T, A> {
    pub transaction_id: T,
    /// Per-deposit input/debit attribution. This is required for batched UTXO sweeps.
    pub attribution: Vec<A>,
}

pub type CollectionResult<C> = Result<
    CollectionSubmission<<C as Chain>::TransactionId, <C as Chain>::CollectionAttribution>,
    ChainError,
>;

/// One stateless collection attempt. Durable waiting, retries, and multi-leg token
/// workflows are owned by the calling application.
pub trait Collector<C: Chain>: Send + Sync {
    /// Returns factual prerequisites such as a token address's native gas deficit.
    fn requirements<'a>(
        &'a self,
        request: &'a C::CollectionRequest,
    ) -> BoxFuture<'a, Result<Vec<C::CollectionRequirement>, ChainError>>;

    /// Builds, signs, broadcasts one sweep transaction and returns its attribution.
    fn collect<'a>(
        &'a self,
        request: C::CollectionRequest,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, CollectionResult<C>>;
}
