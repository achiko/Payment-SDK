use std::sync::Arc;

use deposits::{
    Deposit, DepositAddressSource, DepositCreator, DepositError, DepositIndexerClient, DepositPage,
    DepositQuery, DepositReader, DepositRegistration, LedgerEntry, LedgerPage, LedgerQuery,
    LedgerReader, WatchCoordinator, WatchQueue,
};
use indexing::IndexScope;

/// Durable deposit records needed by the Payment Service address workflow.
///
/// The concrete implementation normally is `deposits::PaymentStore` over the
/// storage backend selected by the application composition root.
pub trait DepositStore: DepositCreator + DepositReader + LedgerReader + WatchQueue {}

impl<T> DepositStore for T where T: DepositCreator + DepositReader + LedgerReader + WatchQueue {}

/// Payment Service facade for issuing and querying deposit addresses.
///
/// `open` returns an address only after the deposit and its zero ledger row are
/// durable and IX has acknowledged the address watch. A lost IX response leaves
/// an `AwaitingWatch` record that `open` or `resume` completes idempotently.
pub struct Deposits {
    store: Arc<dyn DepositStore>,
    indexer: Arc<dyn DepositIndexerClient>,
    addresses: Arc<dyn DepositAddressSource>,
    scope: IndexScope,
}

impl Deposits {
    #[must_use]
    pub fn new(
        store: Arc<dyn DepositStore>,
        indexer: Arc<dyn DepositIndexerClient>,
        addresses: Arc<dyn DepositAddressSource>,
        scope: IndexScope,
    ) -> Self {
        Self {
            store,
            indexer,
            addresses,
            scope,
        }
    }

    pub async fn open(&self, request: DepositRegistration) -> Result<Deposit, DepositError> {
        self.coordinator().register(request).await
    }

    pub async fn resume(&self, limit: usize) -> Result<usize, DepositError> {
        self.coordinator().resume_awaiting(limit).await
    }

    pub async fn get(&self, id: &deposits::DepositId) -> Result<Option<Deposit>, DepositError> {
        self.store.deposit(id).await
    }

    pub async fn list(&self, request: DepositQuery) -> Result<DepositPage, DepositError> {
        self.store.deposits(request).await
    }

    /// Reads the latest complete absolute balance snapshot.
    pub async fn head(
        &self,
        id: &deposits::DepositId,
    ) -> Result<Option<LedgerEntry>, DepositError> {
        self.store.current(id).await
    }

    /// Reads the immutable ledger journal in stable cursor order.
    pub async fn history(&self, request: LedgerQuery) -> Result<LedgerPage, DepositError> {
        self.store.entries(request).await
    }

    /// Returns the single chain/network scope owned by this facade.
    ///
    /// HTTP callers do not select this value: the application composition root
    /// binds routes to an already-configured deposit capability.
    #[must_use]
    pub const fn scope(&self) -> &IndexScope {
        &self.scope
    }

    fn coordinator(
        &self,
    ) -> WatchCoordinator<'_, dyn DepositStore, dyn DepositIndexerClient, dyn DepositAddressSource>
    {
        WatchCoordinator::new(
            self.store.as_ref(),
            self.indexer.as_ref(),
            self.addresses.as_ref(),
            self.scope.clone(),
        )
    }
}
