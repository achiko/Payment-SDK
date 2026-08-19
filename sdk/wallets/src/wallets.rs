use std::{
    borrow::Borrow,
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use base::{BlockHeight, Decimal, TransactionId};
use indexing::{AddressFilter, CanonicalAddress, Checkpoint, IndexScope};

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
            self.store(id, family_key, configured, wallet, None).await
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
            self.store(id, family_key, configured, wallet, Some(start_height))
                .await
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
    ) -> Result<WalletInfo<I, F>, Error> {
        let entry = self
            .activate(id.clone(), family_key, family, wallet, start_height)
            .await?;
        let info = entry.info.clone();
        let mut values = self.values.write().map_err(|_| lock_error())?;
        if values.contains_key(&id) {
            return Err(Error::new(
                ErrorKind::Duplicate,
                "a wallet is already registered for this key",
            ));
        }
        values.insert(id, entry);
        Ok(info)
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
            return Err(SendError::at(
                0,
                Vec::new(),
                Error::new(
                    ErrorKind::InvalidAmount,
                    "at least one transfer is required",
                ),
            ));
        }
        let mut family = None;
        let mut transfers = Vec::with_capacity(requests.len());
        for (index, request) in requests.into_iter().enumerate() {
            if request.amount <= Decimal::zero() {
                return Err(SendError::at(
                    index,
                    Vec::new(),
                    Error::new(ErrorKind::InvalidAmount, "amount must be positive"),
                ));
            }
            let entry = self
                .entry(&request.wallet)
                .map_err(|error| SendError::at(index, Vec::new(), error))?;
            if family
                .as_ref()
                .is_some_and(|expected| expected != &entry.info.family)
            {
                return Err(SendError::at(
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
            SendError::at(
                0,
                Vec::new(),
                Error::new(ErrorKind::Unsupported, "transfer family is missing"),
            )
        })?;
        let sender = self
            .families
            .get(&family)
            .map(|configured| configured.sender.clone())
            .ok_or_else(|| {
                SendError::at(
                    0,
                    Vec::new(),
                    Error::new(ErrorKind::Unsupported, "wallet family is not configured"),
                )
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
    use std::sync::Mutex;

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
    fn startup_imports_deduplicate_address_at_earliest_birthday() {
        let (sender, _) = sender();
        let mut wallets = Wallets::<String, String>::new(Arc::new(FixtureIndex(None)));
        wallets
            .register(
                "family".to_owned(),
                scope("mainnet"),
                FixtureProvider::Value,
                sender,
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
            )
            .expect("first family");
        wallets
            .register(
                "second".to_owned(),
                scope("secondnet"),
                FixtureProvider::Value,
                second_sender,
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

        assert_eq!(error.failed_index, 1);
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
        assert_eq!(unavailable.kind, ErrorKind::Unavailable);
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
