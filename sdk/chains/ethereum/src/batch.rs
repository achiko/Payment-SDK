use std::sync::Arc;

use base::Address as BaseAddress;
use wallets::{Error, ErrorKind, MAX_TRANSFERS, SendError, SendFuture, Sender, Transfer};

use crate::transaction::{Preparation, PreparationError};
use crate::wallet::{WalletConfig, preparation_error};
use crate::{Address, TransactionCoordinator};

pub(crate) struct Batch {
    config: WalletConfig,
    coordinator: Arc<TransactionCoordinator>,
}

impl Batch {
    pub(crate) fn new(config: WalletConfig, coordinator: Arc<TransactionCoordinator>) -> Self {
        Self {
            config,
            coordinator,
        }
    }

    fn preparation<'a>(&self, transfer: &'a Transfer) -> Result<Preparation<'a>, Error> {
        let from = ethereum_address(&transfer.wallet.address())?;
        let destination = transfer.wallet.parse_address(&transfer.to)?;
        let destination = ethereum_address(&destination)?;
        let request = self
            .config
            .transfer_request(from, destination, &transfer.amount)
            .map_err(Error::from)?;
        Ok(Preparation::wallet(
            request,
            self.config.chain_id,
            transfer.wallet.as_ref(),
        ))
    }
}

impl Sender for Batch {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        Box::pin(async move {
            if transfers.is_empty() {
                return Err(SendError::collection(
                    ErrorKind::InvalidBatch,
                    "at least one transfer is required",
                ));
            }
            if transfers.len() > MAX_TRANSFERS {
                return Err(SendError::collection(
                    ErrorKind::InvalidBatch,
                    "at most 50 transfers are allowed",
                ));
            }
            let preparations = transfers
                .iter()
                .enumerate()
                .map(|(index, transfer)| {
                    self.preparation(transfer)
                        .map_err(|error| SendError::item(index, Vec::new(), error))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut prepared = self
                .coordinator
                .prepare_batch(preparations)
                .await
                .map_err(preparation_failure)?;
            let mut accepted = Vec::with_capacity(prepared.len());
            loop {
                let id = prepared.next().await.map_err(|error| {
                    SendError::item(accepted.len(), accepted.clone(), error.into())
                })?;
                let Some(id) = id else {
                    return Ok(accepted);
                };
                accepted.push(base::Id::new(id.to_string()));
            }
        })
    }
}

fn ethereum_address(address: &BaseAddress) -> Result<Address, Error> {
    let bytes: [u8; 20] = address.as_bytes().try_into().map_err(|_| {
        Error::new(
            ErrorKind::InvalidAddress,
            "Ethereum address must contain exactly 20 bytes",
        )
    })?;
    Ok(Address(bytes))
}

fn preparation_failure(error: PreparationError) -> SendError {
    SendError::item(
        error.index,
        Vec::new(),
        preparation_error(error.source).into(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use futures_executor::block_on;
    use indexing::{
        BlockRef, BoxFuture, ChainId, History, HistoryQuery, IndexError, IndexScope, SourceError,
        TransactionPage,
    };
    use wallets::{AddressText, Provider, SecretBytes};

    use super::*;
    use crate::{
        Accounts, AssetKind, BuildContext, ChainError, ChainErrorKind, SignedTransaction,
        TransactionId, Transactions, WalletProvider, Wei,
    };

    #[derive(Default)]
    struct Dependencies {
        balance_calls: AtomicUsize,
        nonce_calls: AtomicUsize,
        contexts: Mutex<Vec<u64>>,
        broadcasts: Mutex<Vec<SignedTransaction>>,
    }

    impl Accounts for Dependencies {
        fn balance<'a>(
            &'a self,
            _address: Address,
            _asset: &'a AssetKind,
            _at: Option<BlockRef>,
        ) -> BoxFuture<'a, Result<Wei, SourceError>> {
            Box::pin(async move {
                self.balance_calls.fetch_add(1, Ordering::Relaxed);
                Ok(Wei::from_u128(1_000_000))
            })
        }

        fn nonce<'a>(&'a self, _address: Address) -> BoxFuture<'a, Result<u64, SourceError>> {
            Box::pin(async move {
                self.nonce_calls.fetch_add(1, Ordering::Relaxed);
                Ok(0)
            })
        }
    }

    impl Transactions for Dependencies {
        fn build_context<'a>(
            &'a self,
            _request: &'a crate::TransferRequest,
            nonce: u64,
        ) -> BoxFuture<'a, Result<BuildContext, ChainError>> {
            Box::pin(async move {
                self.contexts
                    .lock()
                    .expect("context lock must be healthy")
                    .push(nonce);
                Ok(BuildContext {
                    chain_id: 31_337,
                    nonce,
                    gas_limit: 1,
                    max_fee_per_gas: Wei::from_u128(1),
                    max_priority_fee_per_gas: Wei::from_u128(1),
                })
            })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: SignedTransaction,
        ) -> BoxFuture<'a, Result<TransactionId, base::TransactionError>> {
            Box::pin(async move {
                let id = transaction.id.clone();
                let index = {
                    let mut broadcasts = self
                        .broadcasts
                        .lock()
                        .expect("broadcast lock must be healthy");
                    let index = broadcasts.len();
                    broadcasts.push(transaction);
                    index
                };
                if index == 1 {
                    return Err(base::TransactionError::new(
                        base::TransactionErrorKind::Unavailable,
                        "provider claimed transaction provider-candidate",
                    )
                    .with_ambiguous_transaction_id(base::TransactionId::new(
                        "provider-candidate",
                    )));
                }
                Ok(id)
            })
        }

        fn known<'a>(
            &'a self,
            _transaction: &'a TransactionId,
        ) -> BoxFuture<'a, Result<bool, SourceError>> {
            Box::pin(async { unreachable!("an initial batch must not reconcile old submissions") })
        }
    }

    impl History for Dependencies {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async { unreachable!("batch submission must not read indexed history") })
        }
    }

    fn direct_sender() -> (Arc<dyn Sender>, Arc<dyn wallets::Wallet>, Arc<Dependencies>) {
        let dependencies = Arc::new(Dependencies::default());
        let accounts: Arc<dyn Accounts> = dependencies.clone();
        let transactions: Arc<dyn Transactions> = dependencies.clone();
        let history: Arc<dyn History> = dependencies.clone();
        let coordinator = Arc::new(TransactionCoordinator::new(accounts.clone(), transactions));
        let provider = WalletProvider::new(
            WalletConfig {
                scope: IndexScope {
                    chain: ChainId(crate::CHAIN.to_owned()),
                    network: "local".to_owned(),
                },
                chain_id: 31_337,
                asset: AssetKind::Native,
                decimals: crate::ETH.decimals,
            },
            accounts,
            coordinator,
            history,
        );
        let wallet = block_on(provider.create(SecretBytes::new([1_u8; 32])))
            .expect("fixed valid secret must create an Ethereum wallet");
        (provider.transactions(), wallet, dependencies)
    }

    fn transfer(wallet: Arc<dyn wallets::Wallet>) -> Transfer {
        let destination = wallet
            .address_text(&wallet.address())
            .expect("fixture wallet address must format");
        Transfer {
            wallet,
            to: AddressText::new(destination.encoding, destination.text),
            amount: "0.000000000000000001"
                .parse()
                .expect("one wei must be a valid decimal"),
        }
    }

    fn assert_no_chain_io(dependencies: &Dependencies) {
        assert_eq!(dependencies.balance_calls.load(Ordering::Relaxed), 0);
        assert_eq!(dependencies.nonce_calls.load(Ordering::Relaxed), 0);
        assert!(
            dependencies
                .contexts
                .lock()
                .expect("context lock must be healthy")
                .is_empty()
        );
        assert!(
            dependencies
                .broadcasts
                .lock()
                .expect("broadcast lock must be healthy")
                .is_empty()
        );
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
    fn direct_sender_rejects_an_empty_batch_before_chain_io() {
        let (sender, _, dependencies) = direct_sender();

        let failure = block_on(sender.send(Vec::new()))
            .expect_err("the concrete Ethereum sender must reject an empty batch");

        assert_invalid_batch(failure, "at least one transfer is required");
        assert_no_chain_io(&dependencies);
    }

    #[test]
    fn direct_sender_rejects_51_items_before_chain_io() {
        let (sender, wallet, dependencies) = direct_sender();
        let transfers = (0..=MAX_TRANSFERS)
            .map(|_| transfer(wallet.clone()))
            .collect();

        let failure = block_on(sender.send(transfers))
            .expect_err("the concrete Ethereum sender must reject 51 items");

        assert_invalid_batch(failure, "at most 50 transfers are allowed");
        assert_no_chain_io(&dependencies);
    }

    #[test]
    fn duplicates_keep_authored_indices_and_stop_after_the_ambiguous_item() {
        let (sender, wallet, dependencies) = direct_sender();
        let transfers = (0..3).map(|_| transfer(wallet.clone())).collect();

        let failure = block_on(sender.send(transfers))
            .expect_err("the second exact occurrence must fail ambiguously");
        let contexts = dependencies
            .contexts
            .lock()
            .expect("context lock must be healthy")
            .clone();
        let broadcasts = dependencies
            .broadcasts
            .lock()
            .expect("broadcast lock must be healthy")
            .clone();

        assert_eq!(contexts, [0, 1, 2]);
        assert_eq!(broadcasts.len(), 2);
        assert_ne!(broadcasts[0].id, broadcasts[1].id);
        assert_eq!(
            failure.accepted,
            [base::TransactionId::new(broadcasts[0].id.to_string())]
        );
        assert_eq!(failure.failed_index, Some(1));
        assert_eq!(
            failure.ambiguous_transaction_id,
            Some(base::TransactionId::new(broadcasts[1].id.to_string()))
        );
        assert_ne!(
            failure
                .ambiguous_transaction_id
                .as_ref()
                .map(base::TransactionId::as_str),
            Some("provider-candidate")
        );
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_eq!(failure.source.kind, ErrorKind::Unavailable);
        assert_eq!(
            failure.source.message,
            "provider claimed transaction provider-candidate"
        );
    }

    #[test]
    fn rejects_non_ethereum_source_addresses_before_preparation() {
        let error = ethereum_address(&BaseAddress::new(vec![0_u8; 19]))
            .expect_err("a non-Ethereum address must fail before RPC");

        assert_eq!(error.kind, ErrorKind::InvalidAddress);
    }

    #[test]
    fn preparation_failure_preserves_index_with_zero_accepted() {
        let failure = preparation_failure(PreparationError {
            index: 2,
            source: ChainError {
                kind: ChainErrorKind::InsufficientFunds,
                message: "aggregate balance is insufficient".to_owned(),
            },
        });

        assert_eq!(failure.failed_index, Some(2));
        assert!(failure.accepted.is_empty());
        assert_eq!(failure.source.kind, ErrorKind::Transaction);
    }

    #[test]
    fn broadcast_failure_preserves_transaction_classification_for_http_mapping() {
        let unavailable = Error::from(base::TransactionError::new(
            base::TransactionErrorKind::Unavailable,
            "submission outcome is ambiguous",
        ));
        let rejected = Error::from(base::TransactionError::new(
            base::TransactionErrorKind::Rejected,
            "node rejected the transaction",
        ));

        assert_eq!(unavailable.kind, ErrorKind::Unavailable);
        assert_eq!(rejected.kind, ErrorKind::Transaction);
    }
}
