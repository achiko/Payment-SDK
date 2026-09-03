use indexing::{BoxFuture, SourceError};

use crate::{FeeRate, SignedTransaction, TransactionId};

use super::{Client, NodeStatus, Preflight, transport::Client as Transport};

/// Node identity and canonical-chain reads.
pub struct Node<C> {
    client: Client<C>,
}

/// Fee estimation calls.
pub struct FeeClient<C> {
    client: Client<C>,
}

/// Transaction validation and submission calls.
pub struct TransactionClient<C> {
    client: Client<C>,
}

impl<C> Clone for Node<C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
        }
    }
}

impl<C> Clone for FeeClient<C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
        }
    }
}

impl<C> Clone for TransactionClient<C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
        }
    }
}

impl<C> Client<C>
where
    C: Transport,
{
    #[must_use]
    pub fn node(&self) -> Node<C> {
        Node {
            client: self.clone(),
        }
    }

    #[must_use]
    pub fn fees(&self) -> FeeClient<C> {
        FeeClient {
            client: self.clone(),
        }
    }

    #[must_use]
    pub fn transactions(&self) -> TransactionClient<C> {
        TransactionClient {
            client: self.clone(),
        }
    }
}

impl<C> Node<C>
where
    C: Transport,
{
    pub async fn status(&self) -> Result<NodeStatus, SourceError> {
        self.client.readiness().await
    }

    pub async fn canonical_hash(
        &self,
        height: indexing::BlockHeight,
    ) -> Result<Option<indexing::BlockHash>, SourceError> {
        self.client.canonical_hash(height).await
    }
}

pub trait Fees: Send + Sync {
    fn estimate<'a>(&'a self, target_blocks: u16) -> BoxFuture<'a, Result<FeeRate, SourceError>>;
}

pub trait Transactions: Send + Sync {
    fn preflight<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> BoxFuture<'a, Result<Preflight, SourceError>>;

    /// Submits one exact signed envelope using one visible transport execution.
    ///
    /// Definite local or provider rejection remains ID-free. If execution may
    /// have reached the node without a reliable acknowledgement, the concrete
    /// transaction adapter attaches only the ID derived from `transaction`.
    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> BoxFuture<'a, Result<TransactionId, base::TransactionError>>;
}

impl<C> Fees for Client<C>
where
    C: Transport,
{
    fn estimate<'a>(&'a self, target_blocks: u16) -> BoxFuture<'a, Result<FeeRate, SourceError>> {
        Box::pin(async move { self.estimate_fee_rate(target_blocks).await })
    }
}

impl<C> Fees for FeeClient<C>
where
    C: Transport,
{
    fn estimate<'a>(&'a self, target_blocks: u16) -> BoxFuture<'a, Result<FeeRate, SourceError>> {
        Box::pin(async move { self.client.estimate_fee_rate(target_blocks).await })
    }
}

impl<C> Transactions for Client<C>
where
    C: Transport,
{
    fn preflight<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> BoxFuture<'a, Result<Preflight, SourceError>> {
        Box::pin(async move { self.preflight(transaction, max_fee_rate).await })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> BoxFuture<'a, Result<TransactionId, base::TransactionError>> {
        Box::pin(async move { self.broadcast(transaction, max_fee_rate).await })
    }
}

impl<C> Transactions for TransactionClient<C>
where
    C: Transport,
{
    fn preflight<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> BoxFuture<'a, Result<Preflight, SourceError>> {
        Box::pin(async move { self.client.preflight(transaction, max_fee_rate).await })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> BoxFuture<'a, Result<TransactionId, base::TransactionError>> {
        Box::pin(async move { self.client.broadcast(transaction, max_fee_rate).await })
    }
}
