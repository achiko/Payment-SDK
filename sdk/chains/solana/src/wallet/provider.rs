use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock, Weak},
};

use base::{Address as BaseAddress, Addresser, SignRequest};
use indexing::History as IndexHistory;
use wallets::{
    AddressFormat, AddressText, Balance, BalanceReader, Error as WalletError,
    ErrorKind as WalletErrorKind, FutureResult, Provider, SecretBytes, SingleSender,
    Wallet as WalletContract,
};

use crate::{Address, Lamport, NativeAsset, NativeDestination, RpcClient};

use super::{Key, NativeSender, NativeTransfer, batch::BatchSender};

pub struct WalletProvider<C> {
    asset: NativeAsset,
    rpc: RpcClient<C>,
    history: Arc<dyn IndexHistory>,
    sender: Arc<dyn NativeSender>,
    keys: Arc<Keys>,
}

impl<C> WalletProvider<C> {
    #[must_use]
    pub fn new(
        asset: NativeAsset,
        rpc: RpcClient<C>,
        history: Arc<dyn IndexHistory>,
        sender: Arc<dyn NativeSender>,
    ) -> Self {
        Self {
            asset,
            rpc,
            history,
            sender,
            keys: Arc::new(Keys::default()),
        }
    }

    #[must_use]
    pub fn transactions(&self) -> Arc<dyn wallets::Sender> {
        Arc::new(BatchSender::new(
            Arc::clone(&self.sender),
            Arc::clone(&self.keys),
        ))
    }

    fn generate_with(
        &self,
        generator: impl FnOnce() -> Result<SecretBytes, crate::Error>,
    ) -> FutureResult<'_, Arc<dyn WalletContract>>
    where
        C: json_rpc::Client + 'static,
    {
        match generator() {
            Ok(secret) => self.create(secret),
            Err(_) => Box::pin(async {
                Err(WalletError::new(
                    WalletErrorKind::Generation,
                    "Solana wallet generation failed",
                ))
            }),
        }
    }
}

impl<C> Provider for WalletProvider<C>
where
    C: json_rpc::Client + 'static,
{
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn WalletContract>> {
        Box::pin(async move {
            let key = Key::from_secret(secret).map_err(|_| {
                WalletError::new(
                    WalletErrorKind::InvalidSecret,
                    "Solana wallet secret is invalid",
                )
            })?;
            let key = Arc::new(key);
            self.keys.insert(Arc::clone(&key));
            Ok(Arc::new(Wallet {
                asset: self.asset.clone(),
                key,
                rpc: self.rpc.clone(),
                history: Arc::clone(&self.history),
                sender: Arc::clone(&self.sender),
            }) as Arc<dyn WalletContract>)
        })
    }

    fn generate(&self) -> FutureResult<'_, Arc<dyn WalletContract>> {
        self.generate_with(Key::generate_secret)
    }
}

pub(super) struct Wallet<C> {
    pub(super) asset: NativeAsset,
    pub(super) key: Arc<Key>,
    pub(super) rpc: RpcClient<C>,
    pub(super) history: Arc<dyn IndexHistory>,
    sender: Arc<dyn NativeSender>,
}

impl<C> Addresser for Wallet<C>
where
    C: json_rpc::Client + 'static,
{
    fn address(&self) -> BaseAddress {
        self.key.address().address()
    }
}

impl<C> base::Signer for Wallet<C>
where
    C: json_rpc::Client + 'static,
{
    fn sign<'a>(&'a self, request: SignRequest) -> base::SignFuture<'a> {
        self.key.sign(request)
    }
}

impl<C> AddressFormat for Wallet<C>
where
    C: json_rpc::Client + 'static,
{
    fn address_text(&self, address: &BaseAddress) -> Result<AddressText, WalletError> {
        Address::try_from(address)
            .map(|value| AddressText::from(&value))
            .map_err(|_| invalid_address())
    }

    fn parse_address(&self, address: &AddressText) -> Result<BaseAddress, WalletError> {
        Address::try_from(address)
            .map(|value| value.address())
            .map_err(|_| invalid_address())
    }
}

impl<C> BalanceReader for Wallet<C>
where
    C: json_rpc::Client + 'static,
{
    fn balance<'a>(&'a self) -> FutureResult<'a, Balance> {
        Box::pin(async move {
            let balance = self
                .rpc
                .balance(self.key.address(), None)
                .await
                .map_err(|_| {
                    WalletError::new(
                        WalletErrorKind::Balance,
                        "finalized SOL balance is unavailable",
                    )
                })?;
            Ok(Balance {
                amount: self.asset.display(balance.value),
                // A contextual slot is not a canonical block checkpoint.
                observed_at: None,
            })
        })
    }
}

impl<C> SingleSender for Wallet<C>
where
    C: json_rpc::Client + 'static,
{
    fn send<'a>(
        &'a self,
        destination: AddressText,
        amount: base::Decimal,
    ) -> FutureResult<'a, base::TransactionId> {
        Box::pin(async move {
            let destination = Address::try_from(&destination).map_err(|_| invalid_address())?;
            let destination = NativeDestination::try_from(destination).map_err(|_| {
                WalletError::new(
                    WalletErrorKind::Unsupported,
                    "unsupported Solana native destination",
                )
            })?;
            if destination.address() == self.key.address() {
                return Err(WalletError::new(
                    WalletErrorKind::AddressMismatch,
                    "Solana source and destination must differ",
                ));
            }
            let amount = Lamport::from_decimal(&amount).map_err(|_| {
                WalletError::new(
                    WalletErrorKind::InvalidAmount,
                    "native SOL amount is invalid",
                )
            })?;
            let transfer = NativeTransfer::new(
                self.key.address().clone(),
                Arc::clone(&self.key),
                destination.address().clone(),
                amount,
            )?;
            let mut submitted = self
                .sender
                .send(vec![transfer])
                .await
                .map_err(single_error)?;
            if submitted.len() != 1 {
                return Err(WalletError::new(
                    WalletErrorKind::Transaction,
                    "Solana single submission returned an invalid result count",
                ));
            }
            Ok(submitted.remove(0))
        })
    }
}

#[derive(Default)]
pub(super) struct Keys(RwLock<BTreeMap<Address, Weak<Key>>>);

impl Keys {
    fn insert(&self, key: Arc<Key>) {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.address().clone(), Arc::downgrade(&key));
    }

    pub(super) fn get(&self, address: &Address) -> Option<Arc<Key>> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(address)
            .and_then(Weak::upgrade)
    }
}

fn single_error(error: wallets::SendError) -> WalletError {
    let mut source = error.source;
    source.ambiguous_transaction_id = error.ambiguous_transaction_id;
    source
}

fn invalid_address() -> WalletError {
    WalletError::new(
        WalletErrorKind::InvalidAddress,
        "invalid canonical Solana address",
    )
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Mutex};

    use base::{BlockHash, BlockHeight, BlockPosition, BlockRef, Decimal, TransactionId};
    use indexing::{
        BoxFuture, CanonicalAddress, History as IndexHistory, HistoryQuery, IndexScope,
        TransactionPage,
    };
    use wallets::{AddressEncoding, HistoryRequest, Provider as _};

    use crate::{Seed, rpc::test_support::Scripted};

    use super::*;

    #[derive(Default)]
    struct HistoryFixture {
        requests: Mutex<Vec<HistoryQuery>>,
        page: Mutex<Option<TransactionPage>>,
    }

    impl HistoryFixture {
        fn with_page(page: TransactionPage) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                page: Mutex::new(Some(page)),
            }
        }
    }

    impl IndexHistory for HistoryFixture {
        fn history<'a>(
            &'a self,
            request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, indexing::IndexError>> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request);
                Ok(self
                    .page
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .unwrap_or(TransactionPage {
                        checkpoint: None,
                        transactions: Vec::new(),
                        next: None,
                    }))
            })
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Sent {
        source: Address,
        signer: Address,
        destination: Address,
        amount: Lamport,
    }

    #[derive(Default)]
    struct SenderFixture {
        calls: Mutex<Vec<Sent>>,
    }

    impl NativeSender for SenderFixture {
        fn send<'a>(&'a self, transfers: Vec<NativeTransfer>) -> wallets::SendFuture<'a> {
            Box::pin(async move {
                let mut calls = self
                    .calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                calls.extend(transfers.iter().map(|transfer| Sent {
                    source: transfer.source().clone(),
                    signer: transfer.signer().address().clone(),
                    destination: transfer.destination().clone(),
                    amount: transfer.amount(),
                }));
                Ok(vec![TransactionId::new("local-first-signature")])
            })
        }
    }

    fn key(value: u8) -> Key {
        Key::from_seed(
            hex::encode([value; 32])
                .parse::<Seed>()
                .expect("fixture seed"),
        )
        .expect("fixture key")
    }

    fn checkpoint() -> BlockRef {
        BlockRef {
            position: BlockPosition(17),
            height: BlockHeight(9),
            hash: BlockHash(vec![7; 32]),
            parent: None,
            timestamp: None,
        }
    }

    fn provider(
        rpc: Scripted,
        history: Arc<HistoryFixture>,
        sender: Arc<SenderFixture>,
    ) -> WalletProvider<Scripted> {
        let history_contract: Arc<dyn IndexHistory> = history;
        let sender_contract: Arc<dyn NativeSender> = sender;
        WalletProvider::new(
            NativeAsset::new("mainnet").expect("scope"),
            RpcClient::new(rpc),
            history_contract,
            sender_contract,
        )
    }

    #[tokio::test]
    async fn imported_and_generated_wallets_share_canonical_public_capabilities() {
        let provider = provider(
            Scripted::new([]),
            Arc::new(HistoryFixture::default()),
            Arc::new(SenderFixture::default()),
        );
        let imported = provider
            .create(SecretBytes::new([7; 32]))
            .await
            .expect("imported wallet");
        let expected = key(7).address().clone();
        assert_eq!(imported.address().as_bytes(), expected.as_bytes());
        assert_eq!(
            imported.address_text(&imported.address()).expect("text"),
            AddressText::new(AddressEncoding::Base58, expected.to_string())
        );

        let generated = provider.generate().await.expect("generated wallet");
        assert_eq!(generated.address().as_bytes().len(), 32);
        assert_ne!(generated.address(), imported.address());
    }

    #[tokio::test]
    async fn rejects_invalid_import_and_maps_rng_failure_without_secret_output() {
        let provider = provider(
            Scripted::new([]),
            Arc::new(HistoryFixture::default()),
            Arc::new(SenderFixture::default()),
        );
        let invalid = provider
            .create(SecretBytes::new([3; 31]))
            .await
            .err()
            .expect("short seed");
        assert_eq!(invalid.kind, WalletErrorKind::InvalidSecret);

        let failure = provider
            .generate_with(|| {
                Err(crate::Error::new(
                    crate::ErrorKind::Generation,
                    "injected secret material",
                ))
            })
            .await
            .err()
            .expect("injected RNG failure");
        assert_eq!(failure.kind, WalletErrorKind::Generation);
        assert!(!failure.to_string().contains("secret material"));
    }

    #[tokio::test]
    async fn reads_exact_finalized_balance_without_inventing_a_checkpoint() {
        let address = key(7).address().clone();
        let rpc = Scripted::one(
            "getBalance",
            serde_json::json!([address.to_string(), {"commitment":"finalized"}]),
            serde_json::json!({"context":{"slot":41},"value":u64::MAX}),
        );
        let provider = provider(
            rpc.clone(),
            Arc::new(HistoryFixture::default()),
            Arc::new(SenderFixture::default()),
        );
        let wallet = provider
            .create(SecretBytes::new([7; 32]))
            .await
            .expect("wallet");
        let balance = wallet.balance().await.expect("finalized balance");

        assert_eq!(balance.amount, Lamport::from_atomic(u64::MAX).decimal());
        assert_eq!(balance.observed_at, None);
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn history_query_and_result_remain_bound_to_the_wallet_scope_and_checkpoint() {
        let checkpoint = checkpoint();
        let history = Arc::new(HistoryFixture::with_page(TransactionPage {
            checkpoint: Some(checkpoint.clone()),
            transactions: Vec::new(),
            next: None,
        }));
        let provider = provider(
            Scripted::new([]),
            Arc::clone(&history),
            Arc::new(SenderFixture::default()),
        );
        let wallet = provider
            .create(SecretBytes::new([7; 32]))
            .await
            .expect("wallet");
        let page = wallet
            .history(HistoryRequest::first(37))
            .await
            .expect("history");
        assert_eq!(page.checkpoint, Some(checkpoint));

        let requests = history
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].limit, 37);
        assert_eq!(
            requests[0].scope,
            IndexScope {
                chain: indexing::ChainId(crate::CHAIN.to_owned()),
                network: "mainnet".to_owned(),
            }
        );
        assert_eq!(
            requests[0].address,
            CanonicalAddress {
                scope: requests[0].scope.clone(),
                value: key(7).address().to_string(),
            }
        );
    }

    #[tokio::test]
    async fn validates_public_inputs_then_delegates_once_to_the_native_sender() {
        let sender = Arc::new(SenderFixture::default());
        let provider = provider(
            Scripted::new([]),
            Arc::new(HistoryFixture::default()),
            Arc::clone(&sender),
        );
        let wallet = provider
            .create(SecretBytes::new([7; 32]))
            .await
            .expect("wallet");
        let source = key(7).address().clone();
        let destination = key(8).address().clone();
        let id = wallet
            .send(
                AddressText::from(&destination),
                Decimal::from_str("1.000000001").expect("amount"),
            )
            .await
            .expect("native send");
        assert_eq!(id.as_str(), "local-first-signature");
        assert_eq!(
            sender
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[Sent {
                source: source.clone(),
                signer: source.clone(),
                destination: destination.clone(),
                amount: Lamport::from_atomic(1_000_000_001),
            }]
        );

        for (address, amount) in [
            (AddressText::from(&source), Decimal::from(1_u64)),
            (AddressText::from(&destination), Decimal::zero()),
            (
                AddressText::new(AddressEncoding::Hex, destination.to_string()),
                Decimal::from(1_u64),
            ),
        ] {
            assert!(wallet.send(address, amount).await.is_err());
        }
        assert_eq!(
            sender
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }
}
