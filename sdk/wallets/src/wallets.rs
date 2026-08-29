use std::{
    borrow::Borrow,
    collections::BTreeMap,
    fmt,
    sync::{Arc, RwLock},
};

use base::{BlockHeight, Decimal, TransactionId};
use indexing::{
    AddressFilter, CanonicalAddress, Checkpoint, IndexScope, RegisteredAddress, Registry,
};

use crate::{
    AddressText, Balance, Error, ErrorKind, FutureResult, History, HistoryRequest, Provider,
    SecretBytes, SendError, SendFuture, Sender, Transfer, Wallet,
};

/// Non-secret facts retained for one registered wallet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletInfo<I, F> {
    pub id: I,
    pub family: F,
    pub scope: IndexScope,
    pub address: AddressText,
}

/// Maximum authored transfer occurrences accepted by one batch.
pub const MAX_TRANSFERS: usize = 50;

/// One batch item referencing a wallet already held by [`Wallets`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletTransfer<I> {
    pub wallet: I,
    pub to: AddressText,
    pub amount: Decimal,
}

#[derive(Clone)]
struct Family {
    scope: IndexScope,
    provider: Arc<dyn Provider>,
    sender: Arc<dyn Sender>,
    /// Durable address selection for this family's scope. A registry is bound
    /// to one scope, so it belongs beside the scope rather than on the whole
    /// collection, which spans every chain.
    registry: Option<Arc<dyn Registry>>,
}

#[derive(Clone)]
struct Entry<I, F> {
    info: WalletInfo<I, F>,
    wallet: Arc<dyn Wallet>,
    filter: AddressFilter,
}

/// Configured wallet families and the abstract wallet instances they create.
// design-lint: allow package-name-prefix -- Wallets is the public domain collection
pub struct Wallets<I: Ord, F: Ord> {
    checkpoint: Arc<dyn Checkpoint>,
    families: BTreeMap<F, Family>,
    values: RwLock<BTreeMap<I, Entry<I, F>>>,
}

impl<I, F> Wallets<I, F>
where
    I: Clone + Ord + Send + Sync + 'static,
    F: Clone + Ord + Send + Sync + 'static,
{
    #[must_use]
    pub fn new(checkpoint: Arc<dyn Checkpoint>) -> Self {
        Self {
            checkpoint,
            families: BTreeMap::new(),
            values: RwLock::new(BTreeMap::new()),
        }
    }

    /// Registers one concrete constructor and sender at the composition root.
    pub fn register(
        &mut self,
        family: F,
        scope: IndexScope,
        provider: impl Provider + 'static,
        sender: Arc<dyn Sender>,
        registry: Option<Arc<dyn Registry>>,
    ) -> Result<(), Error> {
        if self.families.contains_key(&family) {
            return Err(Error::new(
                ErrorKind::Duplicate,
                "a wallet family is already registered for this key",
            ));
        }
        self.families.insert(
            family,
            Family {
                scope,
                provider: Arc::new(provider),
                sender,
                registry,
            },
        );
        Ok(())
    }

    /// Generates secret material without returning it and starts indexing at
    /// the first block after the current persisted checkpoint.
    pub fn generate<'a>(&'a self, id: I, family: &F) -> FutureResult<'a, WalletInfo<I, F>> {
        let family_key = family.clone();
        let configured = self.family(family);
        Box::pin(async move {
            let configured = configured?;
            let wallet = configured.provider.generate().await?;
            let (info, _) = self.store(id, family_key, configured, wallet, None).await?;
            Ok(info)
        })
    }

    /// Registers a wallet from caller-held secret material while running.
    ///
    /// Unlike [`Wallets::import`] this takes `&self`, so it stays available
    /// after synchronization has started. That is safe because the address is
    /// anchored at the current checkpoint exactly like a generated one: no
    /// historical selection is introduced, so no rescan is implied.
    ///
    /// Use it when the application generates and durably stores its own key
    /// material — a deposit address whose custody belongs in the application's
    /// database rather than in this in-memory registry.
    pub fn adopt<'a>(
        &'a self,
        id: I,
        family: &F,
        secret: SecretBytes,
    ) -> FutureResult<'a, WalletInfo<I, F>>
    where
        I: fmt::Display,
    {
        let family_key = family.clone();
        let configured = self.family(family);
        Box::pin(async move {
            let configured = configured?;
            // Copy the material before the provider consumes the secret: a
            // registry has to store what the caller supplied, not a derivation.
            let material = secret.as_bytes().to_vec();
            let registry = configured.registry.clone();
            let wallet = configured.provider.create(secret).await?;
            let (info, filter) = self
                .store(id.clone(), family_key, configured, wallet, None)
                .await?;
            if let Some(registry) = &registry
                && let Err(error) = registry
                    .register(RegisteredAddress {
                        id: id.to_string(),
                        filter,
                        material,
                    })
                    .await
            {
                // A wallet observed in memory but absent from the registry
                // would vanish on restart while appearing registered now.
                self.forget(&id);
                return Err(error.into());
            }
            Ok(info)
        })
    }

    /// Re-registers every address this family previously adopted.
    ///
    /// Startup-only, for the same reason as [`Wallets::import`]: a stored
    /// birthday is usually below the current checkpoint, which is historical
    /// selection and must not be introduced once synchronization is running.
    /// Taking `&mut self` makes that impossible after the collection is shared.
    ///
    /// Returns how many addresses were restored. A family with no registry
    /// restores nothing and is not an error — that collection is simply
    /// memory-only.
    pub fn restore<'a>(&'a mut self, family: &F) -> FutureResult<'a, usize>
    where
        I: From<String>,
    {
        let family_key = family.clone();
        let configured = self.family(family);
        Box::pin(async move {
            let configured = configured?;
            let Some(registry) = configured.registry.clone() else {
                return Ok(0);
            };
            let entries = registry.registered(&configured.scope).await?;
            let mut restored = 0;
            for entry in entries {
                let wallet = configured
                    .provider
                    .create(SecretBytes::new(entry.material))
                    .await?;
                self.store(
                    I::from(entry.id),
                    family_key.clone(),
                    configured.clone(),
                    wallet,
                    Some(entry.filter.start_height),
                )
                .await?;
                restored += 1;
            }
            Ok(restored)
        })
    }

    /// Imports secret material and indexes from its explicit birthday.
    ///
    /// This startup-only operation requires exclusive access so historical
    /// selection cannot be introduced after synchronization begins.
    pub fn import<'a>(
        &'a mut self,
        id: I,
        family: &F,
        secret: SecretBytes,
        start_height: BlockHeight,
    ) -> FutureResult<'a, WalletInfo<I, F>> {
        let family_key = family.clone();
        let configured = self.family(family);
        Box::pin(async move {
            let configured = configured?;
            let wallet = configured.provider.create(secret).await?;
            let (info, _) = self
                .store(id, family_key, configured, wallet, Some(start_height))
                .await?;
            Ok(info)
        })
    }

    pub fn get<Q>(&self, id: &Q) -> Result<WalletInfo<I, F>, Error>
    where
        I: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let values = self.values.read().map_err(|_| lock_error())?;
        match values.get(id) {
            Some(entry) => Ok(entry.info.clone()),
            None => Err(Error::new(ErrorKind::NotFound, "wallet does not exist")),
        }
    }

    pub fn balance<'a, Q>(&'a self, id: &Q) -> FutureResult<'a, Balance>
    where
        I: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let wallet = self.instance(id);
        Box::pin(async move { wallet?.balance().await })
    }

    pub fn history<'a, Q>(&'a self, id: &Q, request: HistoryRequest) -> FutureResult<'a, History>
    where
        I: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let wallet = self.instance(id);
        Box::pin(async move { wallet?.history(request).await })
    }

    pub fn send<'a, Q>(
        &'a self,
        id: &Q,
        destination: AddressText,
        amount: Decimal,
    ) -> FutureResult<'a, TransactionId>
    where
        I: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let wallet = self.instance(id);
        Box::pin(async move { wallet?.send(destination, amount).await })
    }

    pub fn send_all<'a>(&'a self, requests: Vec<WalletTransfer<I>>) -> SendFuture<'a> {
        let prepared = self.transfers(requests);
        Box::pin(async move {
            let (sender, transfers) = prepared?;
            sender.send(transfers).await
        })
    }

    /// Returns the authoritative address/birthday snapshot for one index run.
    /// Wallets sharing an address contribute the earliest birthday.
    pub fn filters(&self) -> Result<Vec<AddressFilter>, Error> {
        let values = self.values.read().map_err(|_| lock_error())?;
        let mut filters = BTreeMap::<CanonicalAddress, BlockHeight>::new();
        for entry in values.values() {
            filters
                .entry(entry.filter.address.clone())
                .and_modify(|height| *height = (*height).min(entry.filter.start_height))
                .or_insert(entry.filter.start_height);
        }
        Ok(filters
            .into_iter()
            .map(|(address, start_height)| AddressFilter {
                address,
                start_height,
            })
            .collect())
    }

    fn family(&self, family: &F) -> Result<Family, Error> {
        self.families.get(family).cloned().ok_or_else(|| {
            Error::new(
                ErrorKind::Unsupported,
                "no wallet family is registered for this key",
            )
        })
    }

    async fn store(
        &self,
        id: I,
        family_key: F,
        family: Family,
        wallet: Arc<dyn Wallet>,
        start_height: Option<BlockHeight>,
    ) -> Result<(WalletInfo<I, F>, AddressFilter), Error> {
        let entry = self
            .activate(id.clone(), family_key, family, wallet, start_height)
            .await?;
        let info = entry.info.clone();
        let filter = entry.filter.clone();
        let mut values = self.values.write().map_err(|_| lock_error())?;
        if values.contains_key(&id) {
            return Err(Error::new(
                ErrorKind::Duplicate,
                "a wallet is already registered for this key",
            ));
        }
        values.insert(id, entry);
        Ok((info, filter))
    }

    /// Drops an in-memory wallet again after a durable write failed, so the two
    /// never disagree about which addresses are registered.
    fn forget(&self, id: &I) {
        if let Ok(mut values) = self.values.write() {
            values.remove(id);
        }
    }

    async fn activate(
        &self,
        id: I,
        family_key: F,
        family: Family,
        wallet: Arc<dyn Wallet>,
        start_height: Option<BlockHeight>,
    ) -> Result<Entry<I, F>, Error> {
        let address = wallet.address_text(&wallet.address())?;
        let start_height = match start_height {
            Some(height) => height,
            None => self
                .checkpoint
                .checkpoint(&family.scope)
                .await?
                .map_or(BlockHeight(0), |block| {
                    BlockHeight(block.height.0.saturating_add(1))
                }),
        };
        let filter = AddressFilter {
            address: CanonicalAddress {
                scope: family.scope.clone(),
                value: address.text.clone(),
            },
            start_height,
        };
        Ok(Entry {
            info: WalletInfo {
                id,
                family: family_key,
                scope: family.scope,
                address,
            },
            wallet,
            filter,
        })
    }

    fn instance<Q>(&self, id: &Q) -> Result<Arc<dyn Wallet>, Error>
    where
        I: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let values = self.values.read().map_err(|_| lock_error())?;
        match values.get(id) {
            Some(entry) => Ok(entry.wallet.clone()),
            None => Err(Error::new(ErrorKind::NotFound, "wallet does not exist")),
        }
    }

    fn transfers(
        &self,
        requests: Vec<WalletTransfer<I>>,
    ) -> Result<(Arc<dyn Sender>, Vec<Transfer>), SendError> {
        if requests.is_empty() {
            return Err(SendError::collection(
                ErrorKind::InvalidBatch,
                "at least one transfer is required",
            ));
        }
        if requests.len() > MAX_TRANSFERS {
            return Err(SendError::collection(
                ErrorKind::InvalidBatch,
                "at most 50 transfers are allowed",
            ));
        }
        let mut family = None;
        let mut transfers = Vec::with_capacity(requests.len());
        for (index, request) in requests.into_iter().enumerate() {
            if request.amount <= Decimal::zero() {
                return Err(SendError::item(
                    index,
                    Vec::new(),
                    Error::new(ErrorKind::InvalidAmount, "amount must be positive"),
                ));
            }
            let entry = self
                .entry(&request.wallet)
                .map_err(|error| SendError::item(index, Vec::new(), error))?;
            if family
                .as_ref()
                .is_some_and(|expected| expected != &entry.info.family)
            {
                return Err(SendError::item(
                    index,
                    Vec::new(),
                    Error::new(
                        ErrorKind::Unsupported,
                        "all transfers must use the same wallet family",
                    ),
                ));
            }
            family = Some(entry.info.family.clone());
            transfers.push(Transfer {
                wallet: entry.wallet,
                to: request.to,
                amount: request.amount,
            });
        }
        let family = family.ok_or_else(|| {
            SendError::operation(ErrorKind::Unsupported, "transfer family is missing")
        })?;
        let sender = self
            .families
            .get(&family)
            .map(|configured| configured.sender.clone())
            .ok_or_else(|| {
                SendError::operation(ErrorKind::Unsupported, "wallet family is not configured")
            })?;
        Ok((sender, transfers))
    }

    fn entry(&self, id: &I) -> Result<Entry<I, F>, Error> {
        let values = self.values.read().map_err(|_| lock_error())?;
        match values.get(id) {
            Some(entry) => Ok(entry.clone()),
            None => Err(Error::new(ErrorKind::NotFound, "wallet does not exist")),
        }
    }
}

fn lock_error() -> Error {
    Error::new(ErrorKind::Unavailable, "wallet registry lock is poisoned")
}

#[cfg(test)]
mod tests {
    use std::{
        cmp::Ordering,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
        },
    };

    use base::{
        Address, Addresser, Broadcaster, SignFuture, SignRequest, SignedTransaction, Signer,
        Submission, TransactionBuilder, TransactionEnvelope, TransactionError, TransactionFuture,
        TransactionSnapshot,
    };
    use indexing::{BlockHash, BlockRef, BoxFuture, ChainId, HistoryCursor, IndexError};

    use super::*;
    use crate::{AddressEncoding, AddressFormat, BalanceReader, HistoryReader, TransactionFactory};

    struct FixtureIndex(Option<BlockRef>);

    impl Checkpoint for FixtureIndex {
        fn checkpoint<'a>(
            &'a self,
            _scope: &'a IndexScope,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    #[derive(Clone, Debug)]
    struct CountedId {
        value: usize,
        comparisons: Arc<AtomicUsize>,
    }

    impl PartialEq for CountedId {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Eq for CountedId {}

    impl PartialOrd for CountedId {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for CountedId {
        fn cmp(&self, other: &Self) -> Ordering {
            self.comparisons.fetch_add(1, AtomicOrdering::Relaxed);
            self.value.cmp(&other.value)
        }
    }

    #[derive(Clone)]
    struct FixtureSender {
        calls: Arc<Mutex<usize>>,
    }

    impl Sender for FixtureSender {
        fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
            Box::pin(async move {
                *self.calls.lock().expect("fixture lock") += 1;
                Ok(vec![TransactionId::new(format!(
                    "batch-{}",
                    transfers.len()
                ))])
            })
        }
    }

    struct RecordingSender {
        batches: Arc<Mutex<Vec<Vec<Transfer>>>>,
        result: Vec<TransactionId>,
    }

    impl Sender for RecordingSender {
        fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
            Box::pin(async move {
                self.batches
                    .lock()
                    .expect("recording sender lock")
                    .push(transfers);
                Ok(self.result.clone())
            })
        }
    }

    enum FixtureProvider {
        Value,
    }

    impl Provider for FixtureProvider {
        fn create<'a>(&'a self, _secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
            Box::pin(async { Ok(Arc::new(FixtureWallet(false)) as Arc<dyn Wallet>) })
        }

        fn generate(&self) -> FutureResult<'_, Arc<dyn Wallet>> {
            self.create(SecretBytes::new([7; 32]))
        }
    }

    struct GenerationFailureProvider;

    impl Provider for GenerationFailureProvider {
        fn create<'a>(&'a self, _secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
            Box::pin(async { unreachable!("generation failure must not reach wallet creation") })
        }

        fn generate(&self) -> FutureResult<'_, Arc<dyn Wallet>> {
            Box::pin(async {
                Err(Error::new(
                    ErrorKind::Generation,
                    "fixture generation failed",
                ))
            })
        }
    }

    struct FixtureWallet(bool);

    impl Addresser for FixtureWallet {
        fn address(&self) -> Address {
            Address::from([1])
        }
    }

    impl Signer for FixtureWallet {
        fn sign<'a>(&'a self, _request: SignRequest) -> SignFuture<'a> {
            Box::pin(async { unreachable!("fixture builder does not call the signer") })
        }
    }

    impl AddressFormat for FixtureWallet {
        fn address_text(&self, _address: &Address) -> Result<AddressText, Error> {
            Ok(AddressText::new(AddressEncoding::Hex, "fixture-address"))
        }

        fn parse_address(&self, address: &AddressText) -> Result<Address, Error> {
            if address.encoding != AddressEncoding::Hex || address.text.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidAddress,
                    "invalid fixture address",
                ));
            }
            Ok(Address::new(address.text.as_bytes()))
        }
    }

    impl BalanceReader for FixtureWallet {
        fn balance<'a>(&'a self) -> FutureResult<'a, Balance> {
            Box::pin(async {
                Ok(Balance {
                    amount: Decimal::from(12_u64),
                    observed_at: None,
                })
            })
        }
    }

    impl HistoryReader for FixtureWallet {
        fn history<'a>(&'a self, _request: HistoryRequest) -> FutureResult<'a, History> {
            Box::pin(async {
                Ok(History {
                    checkpoint: None,
                    transactions: Vec::new(),
                    next: None::<HistoryCursor>,
                })
            })
        }
    }

    impl TransactionFactory for FixtureWallet {
        fn transaction(&self) -> Box<dyn TransactionBuilder> {
            Box::new(FixtureBuilder(false))
        }

        fn restore(
            &self,
            _snapshot: &TransactionSnapshot,
        ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
            Ok(self.transaction())
        }

        fn broadcaster(&self) -> &dyn Broadcaster {
            self
        }
    }

    impl Broadcaster for FixtureWallet {
        fn broadcast<'a>(
            &'a self,
            transaction: &'a SignedTransaction,
        ) -> TransactionFuture<'a, Result<Submission, TransactionError>> {
            Box::pin(async move {
                Ok(Submission {
                    id: if self.0 {
                        TransactionId::new("different")
                    } else {
                        transaction.id().clone()
                    },
                })
            })
        }
    }

    struct FixtureBuilder(bool);

    impl TransactionBuilder for FixtureBuilder {
        fn transfer(
            &mut self,
            _destination: Address,
            _amount: Decimal,
        ) -> Result<(), TransactionError> {
            self.0 = true;
            Ok(())
        }

        fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
            Ok(TransactionSnapshot::new("fixture", serde_json::json!({})))
        }

        fn prepare<'a>(
            &'a mut self,
        ) -> TransactionFuture<'a, Result<SignedTransaction, TransactionError>> {
            Box::pin(async move {
                assert!(self.0, "fixture transfer must be configured");
                Ok(SignedTransaction::new(
                    "fixture",
                    TransactionId::new("single"),
                    TransactionEnvelope::new(Vec::new()),
                ))
            })
        }
    }

    fn scope(network: &str) -> IndexScope {
        IndexScope {
            chain: ChainId("example".to_owned()),
            network: network.to_owned(),
        }
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent_hash: None,
            timestamp: None,
        }
    }

    fn sender() -> (Arc<FixtureSender>, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0));
        (
            Arc::new(FixtureSender {
                calls: calls.clone(),
            }),
            calls,
        )
    }

    fn batch_wallets() -> (
        Wallets<CountedId, String>,
        CountedId,
        Arc<AtomicUsize>,
        Arc<Mutex<usize>>,
    ) {
        let comparisons = Arc::new(AtomicUsize::new(0));
        let wallet_id = CountedId {
            value: 1,
            comparisons: comparisons.clone(),
        };
        let (sender, calls) = sender();
        let family = "family".to_owned();
        let mut wallets = Wallets::new(Arc::new(FixtureIndex(None)));
        wallets
            .register(
                family.clone(),
                scope("mainnet"),
                FixtureProvider::Value,
                sender,
                None,
            )
            .expect("family registration");
        futures_executor::block_on(wallets.import(
            wallet_id.clone(),
            &family,
            SecretBytes::new([1; 32]),
            BlockHeight(1),
        ))
        .expect("wallet import");
        comparisons.store(0, AtomicOrdering::Relaxed);
        (wallets, wallet_id, comparisons, calls)
    }

    fn batch_transfers(wallet: &CountedId, count: usize) -> Vec<WalletTransfer<CountedId>> {
        std::iter::repeat_with(|| WalletTransfer {
            wallet: wallet.clone(),
            to: AddressText::new(AddressEncoding::Hex, "destination"),
            amount: Decimal::from(1_u64),
        })
        .take(count)
        .collect()
    }

    fn assert_invalid_batch(failure: SendError, message: &str) {
        assert!(failure.accepted.is_empty());
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert_eq!(failure.source.kind, ErrorKind::InvalidBatch);
        assert_eq!(failure.source.message, message);
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_eq!(failure.to_string(), message);
    }

    #[test]
    fn generated_wallet_owns_lookup_reads_send_and_tip_birthday() {
        let (sender, _) = sender();
        let mut wallets = Wallets::<String, String>::new(Arc::new(FixtureIndex(Some(block(7)))));
        wallets
            .register(
                "family".to_owned(),
                scope("mainnet"),
                FixtureProvider::Value,
                sender,
                None,
            )
            .expect("family registration");

        let info =
            futures_executor::block_on(wallets.generate("alice".to_owned(), &"family".to_owned()))
                .expect("wallet generation");
        assert_eq!(info.address.text, "fixture-address");
        assert_eq!(wallets.get("alice").expect("wallet").family, "family");
        assert_eq!(
            futures_executor::block_on(wallets.balance("alice"))
                .expect("balance")
                .amount,
            Decimal::from(12_u64)
        );
        assert!(
            futures_executor::block_on(wallets.history("alice", HistoryRequest::first(10)))
                .expect("history")
                .transactions
                .is_empty()
        );
        assert_eq!(
            futures_executor::block_on(wallets.send(
                "alice",
                AddressText::new(AddressEncoding::Hex, "destination"),
                Decimal::from(1_u64),
            ))
            .expect("send")
            .as_str(),
            "single"
        );
        assert_eq!(
            wallets.filters().expect("filters")[0].start_height,
            BlockHeight(8)
        );
    }

    #[test]
    fn generation_failure_publishes_no_wallet_or_filter() {
        let (sender, _) = sender();
        let family = "family".to_owned();
        let mut wallets = Wallets::<String, String>::new(Arc::new(FixtureIndex(None)));
        wallets
            .register(
                family.clone(),
                scope("mainnet"),
                GenerationFailureProvider,
                sender,
                None,
            )
            .expect("family registration");

        let error = futures_executor::block_on(wallets.generate("alice".to_owned(), &family))
            .expect_err("generation must fail");

        assert_eq!(error.kind, ErrorKind::Generation);
        assert_eq!(
            wallets
                .get("alice")
                .expect_err("wallet must not be stored")
                .kind,
            ErrorKind::NotFound
        );
        assert!(wallets.filters().expect("filters").is_empty());
    }

    #[test]
    fn startup_imports_deduplicate_address_at_earliest_birthday() {
        let (sender, _) = sender();
        let mut wallets = Wallets::<String, String>::new(Arc::new(FixtureIndex(None)));
        wallets
            .register(
                "family".to_owned(),
                scope("mainnet"),
                FixtureProvider::Value,
                sender,
                None,
            )
            .expect("family registration");
        futures_executor::block_on(wallets.import(
            "newer".to_owned(),
            &"family".to_owned(),
            SecretBytes::new([1; 32]),
            BlockHeight(20),
        ))
        .expect("newer wallet");
        futures_executor::block_on(wallets.import(
            "older".to_owned(),
            &"family".to_owned(),
            SecretBytes::new([2; 32]),
            BlockHeight(5),
        ))
        .expect("older wallet");

        let filters = wallets.filters().expect("filters");
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].start_height, BlockHeight(5));
    }

    #[test]
    fn batch_bounds_reject_zero_and_fifty_one_before_lookup_or_sender() {
        assert_eq!(crate::MAX_TRANSFERS, 50);
        let (wallets, wallet_id, comparisons, calls) = batch_wallets();

        let empty =
            futures_executor::block_on(wallets.send_all(Vec::new())).expect_err("empty batch");
        assert_invalid_batch(empty, "at least one transfer is required");
        assert_eq!(comparisons.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(*calls.lock().expect("sender calls"), 0);

        let oversized = futures_executor::block_on(
            wallets.send_all(batch_transfers(&wallet_id, MAX_TRANSFERS + 1)),
        )
        .expect_err("oversized batch");
        assert_invalid_batch(oversized, "at most 50 transfers are allowed");
        assert_eq!(comparisons.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(*calls.lock().expect("sender calls"), 0);
    }

    #[test]
    fn batch_bounds_admit_one_and_fifty() {
        let (wallets, wallet_id, comparisons, calls) = batch_wallets();

        let one = futures_executor::block_on(wallets.send_all(batch_transfers(&wallet_id, 1)))
            .expect("one transfer");
        assert_eq!(one, vec![TransactionId::new("batch-1")]);
        assert!(comparisons.load(AtomicOrdering::Relaxed) > 0);
        assert_eq!(*calls.lock().expect("sender calls"), 1);

        comparisons.store(0, AtomicOrdering::Relaxed);
        let fifty = futures_executor::block_on(
            wallets.send_all(batch_transfers(&wallet_id, MAX_TRANSFERS)),
        )
        .expect("fifty transfers");
        assert_eq!(fifty, vec![TransactionId::new("batch-50")]);
        assert!(comparisons.load(AtomicOrdering::Relaxed) > 0);
        assert_eq!(*calls.lock().expect("sender calls"), 2);
    }

    #[test]
    fn batch_preserves_authored_occurrences_and_sender_result() {
        let batches = Arc::new(Mutex::new(Vec::new()));
        let sender_result = vec![
            TransactionId::new("sender-result-two"),
            TransactionId::new("sender-result-one"),
        ];
        let sender = Arc::new(RecordingSender {
            batches: batches.clone(),
            result: sender_result.clone(),
        });
        let family = "family".to_owned();
        let primary = "primary".to_owned();
        let alias = "alias".to_owned();
        let mut wallets = Wallets::<String, String>::new(Arc::new(FixtureIndex(None)));
        wallets
            .register(
                family.clone(),
                scope("mainnet"),
                FixtureProvider::Value,
                sender,
                None,
            )
            .expect("family registration");
        let primary_info = futures_executor::block_on(wallets.import(
            primary.clone(),
            &family,
            SecretBytes::new([1; 32]),
            BlockHeight(1),
        ))
        .expect("primary wallet");
        let alias_info = futures_executor::block_on(wallets.import(
            alias.clone(),
            &family,
            SecretBytes::new([2; 32]),
            BlockHeight(1),
        ))
        .expect("alias wallet");
        assert_eq!(primary_info.address, alias_info.address);

        let first = AddressText::new(AddressEncoding::Hex, "first");
        let second = AddressText::new(AddressEncoding::Hex, "second");
        let one = Decimal::from(1_u64);
        let two = Decimal::from(2_u64);
        let requests = vec![
            WalletTransfer {
                wallet: primary.clone(),
                to: first.clone(),
                amount: one.clone(),
            },
            WalletTransfer {
                wallet: primary.clone(),
                to: first.clone(),
                amount: one.clone(),
            },
            WalletTransfer {
                wallet: alias.clone(),
                to: first.clone(),
                amount: one.clone(),
            },
            WalletTransfer {
                wallet: primary.clone(),
                to: second.clone(),
                amount: two.clone(),
            },
            WalletTransfer {
                wallet: alias,
                to: second.clone(),
                amount: two.clone(),
            },
            WalletTransfer {
                wallet: primary,
                to: first.clone(),
                amount: two.clone(),
            },
        ];

        let result = futures_executor::block_on(wallets.send_all(requests)).expect("batch send");
        assert_eq!(result, sender_result);

        let recorded = batches.lock().expect("recorded batches");
        assert_eq!(recorded.len(), 1);
        let transfers = &recorded[0];
        assert_eq!(transfers.len(), 6);
        assert!(Arc::ptr_eq(&transfers[0].wallet, &transfers[1].wallet));
        assert!(Arc::ptr_eq(&transfers[0].wallet, &transfers[3].wallet));
        assert!(Arc::ptr_eq(&transfers[0].wallet, &transfers[5].wallet));
        assert!(Arc::ptr_eq(&transfers[2].wallet, &transfers[4].wallet));
        assert!(!Arc::ptr_eq(&transfers[0].wallet, &transfers[2].wallet));
        assert_eq!(
            transfers
                .iter()
                .map(|transfer| (
                    transfer.wallet.address(),
                    transfer.to.clone(),
                    transfer.amount.clone(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (Address::from([1]), first.clone(), one.clone()),
                (Address::from([1]), first.clone(), one.clone()),
                (Address::from([1]), first.clone(), one.clone()),
                (Address::from([1]), second.clone(), two.clone()),
                (Address::from([1]), second, two.clone()),
                (Address::from([1]), first, two),
            ]
        );
    }

    #[test]
    fn batch_failure_keeps_original_authored_index() {
        let batches = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(RecordingSender {
            batches: batches.clone(),
            result: Vec::new(),
        });
        let family = "family".to_owned();
        let primary = "primary".to_owned();
        let alias = "alias".to_owned();
        let mut wallets = Wallets::<String, String>::new(Arc::new(FixtureIndex(None)));
        wallets
            .register(
                family.clone(),
                scope("mainnet"),
                FixtureProvider::Value,
                sender,
                None,
            )
            .expect("family registration");
        futures_executor::block_on(wallets.import(
            primary.clone(),
            &family,
            SecretBytes::new([1; 32]),
            BlockHeight(1),
        ))
        .expect("primary wallet");
        futures_executor::block_on(wallets.import(
            alias.clone(),
            &family,
            SecretBytes::new([2; 32]),
            BlockHeight(1),
        ))
        .expect("alias wallet");

        let transfer = |wallet: String| WalletTransfer {
            wallet,
            to: AddressText::new(AddressEncoding::Hex, "destination"),
            amount: Decimal::from(1_u64),
        };
        let failure = futures_executor::block_on(wallets.send_all(vec![
            transfer(primary.clone()),
            transfer(alias.clone()),
            transfer(primary.clone()),
            transfer(alias),
            transfer("missing".to_owned()),
            transfer(primary),
        ]))
        .expect_err("missing wallet");

        assert!(failure.accepted.is_empty());
        assert_eq!(failure.failed_index, Some(4));
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert_eq!(failure.source.kind, ErrorKind::NotFound);
        assert_eq!(failure.source.message, "wallet does not exist");
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert!(batches.lock().expect("recorded batches").is_empty());
    }

    #[test]
    fn batch_common_validation_uses_authored_itemwise_precedence() {
        let (first_sender, first_calls) = sender();
        let (second_sender, second_calls) = sender();
        let first_family = "first".to_owned();
        let second_family = "second".to_owned();
        let mut wallets = Wallets::<String, String>::new(Arc::new(FixtureIndex(None)));
        wallets
            .register(
                first_family.clone(),
                scope("firstnet"),
                FixtureProvider::Value,
                first_sender,
                None,
            )
            .expect("first family");
        wallets
            .register(
                second_family.clone(),
                scope("secondnet"),
                FixtureProvider::Value,
                second_sender,
                None,
            )
            .expect("second family");
        futures_executor::block_on(wallets.import(
            "first".to_owned(),
            &first_family,
            SecretBytes::new([1; 32]),
            BlockHeight(1),
        ))
        .expect("first wallet");
        futures_executor::block_on(wallets.import(
            "second".to_owned(),
            &second_family,
            SecretBytes::new([2; 32]),
            BlockHeight(1),
        ))
        .expect("second wallet");

        let transfer = |wallet: &str, amount: u64| WalletTransfer {
            wallet: wallet.to_owned(),
            to: AddressText::new(AddressEncoding::Hex, "destination"),
            amount: Decimal::from(amount),
        };
        let cases = [
            (
                "amount before same-item lookup",
                vec![transfer("first", 1), transfer("missing", 0)],
                ErrorKind::InvalidAmount,
                "amount must be positive",
            ),
            (
                "amount before same-item family compatibility",
                vec![transfer("first", 1), transfer("second", 0)],
                ErrorKind::InvalidAmount,
                "amount must be positive",
            ),
            (
                "lookup before later amount and family defects",
                vec![
                    transfer("first", 1),
                    transfer("missing", 1),
                    transfer("second", 0),
                ],
                ErrorKind::NotFound,
                "wallet does not exist",
            ),
            (
                "family compatibility before later amount and lookup defects",
                vec![
                    transfer("first", 1),
                    transfer("second", 1),
                    transfer("missing", 0),
                ],
                ErrorKind::Unsupported,
                "all transfers must use the same wallet family",
            ),
        ];

        for (name, requests, kind, message) in cases {
            let failure = futures_executor::block_on(wallets.send_all(requests)).expect_err(name);

            assert!(failure.accepted.is_empty(), "{name}");
            assert_eq!(failure.failed_index, Some(1), "{name}");
            assert_eq!(failure.ambiguous_transaction_id, None, "{name}");
            assert_eq!(failure.source.kind, kind, "{name}");
            assert_eq!(failure.source.message, message, "{name}");
            assert_eq!(failure.source.ambiguous_transaction_id, None, "{name}");
        }
        assert_eq!(*first_calls.lock().expect("first calls"), 0);
        assert_eq!(*second_calls.lock().expect("second calls"), 0);
    }

    #[test]
    fn batch_resolves_wallets_and_rejects_mixed_families_before_sending() {
        let (first_sender, first_calls) = sender();
        let (second_sender, second_calls) = sender();
        let mut wallets = Wallets::<String, String>::new(Arc::new(FixtureIndex(None)));
        wallets
            .register(
                "first".to_owned(),
                scope("firstnet"),
                FixtureProvider::Value,
                first_sender,
                None,
            )
            .expect("first family");
        wallets
            .register(
                "second".to_owned(),
                scope("secondnet"),
                FixtureProvider::Value,
                second_sender,
                None,
            )
            .expect("second family");
        futures_executor::block_on(wallets.import(
            "alice".to_owned(),
            &"first".to_owned(),
            SecretBytes::new([1; 32]),
            BlockHeight(1),
        ))
        .expect("alice");
        futures_executor::block_on(wallets.import(
            "bob".to_owned(),
            &"second".to_owned(),
            SecretBytes::new([2; 32]),
            BlockHeight(1),
        ))
        .expect("bob");

        let error = futures_executor::block_on(wallets.send_all(vec![
            WalletTransfer {
                wallet: "alice".to_owned(),
                to: AddressText::new(AddressEncoding::Hex, "one"),
                amount: Decimal::from(1_u64),
            },
            WalletTransfer {
                wallet: "bob".to_owned(),
                to: AddressText::new(AddressEncoding::Hex, "two"),
                amount: Decimal::from(1_u64),
            },
        ]))
        .expect_err("mixed family batch");

        assert_eq!(error.failed_index, Some(1));
        assert_eq!(*first_calls.lock().expect("first calls"), 0);
        assert_eq!(*second_calls.lock().expect("second calls"), 0);
    }

    #[test]
    fn error_conversion_preserves_index_conflict_and_unavailability() {
        let conflict = Error::from(IndexError::new(
            indexing::IndexErrorKind::Conflict,
            "history changed",
            true,
        ));
        let unavailable = Error::from(IndexError::new(
            indexing::IndexErrorKind::Store,
            "database unavailable",
            true,
        ));

        assert_eq!(conflict.kind, ErrorKind::Conflict);
        assert_eq!(conflict.ambiguous_transaction_id, None);
        assert_eq!(unavailable.kind, ErrorKind::Unavailable);
        assert_eq!(unavailable.ambiguous_transaction_id, None);
    }

    #[test]
    fn wallet_send_rejects_non_positive_amounts_and_divergent_ids() {
        let invalid = futures_executor::block_on(FixtureWallet(false).send(
            AddressText::new(AddressEncoding::Hex, "destination"),
            Decimal::zero(),
        ))
        .expect_err("zero amount");
        let divergent = futures_executor::block_on(FixtureWallet(true).send(
            AddressText::new(AddressEncoding::Hex, "destination"),
            Decimal::from(1_u64),
        ))
        .expect_err("different broadcast ID");

        assert_eq!(invalid.kind, ErrorKind::InvalidAmount);
        assert_eq!(divergent.kind, ErrorKind::Transaction);
    }
}
