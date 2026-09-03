use std::sync::Arc;

use wallets::{Error, ErrorKind, MAX_TRANSFERS, SendError, SendFuture, Sender, Transfer};

use crate::{Address, Lamport, NativeDestination};

use super::{NativeSender, NativeTransfer, provider::Keys};

pub(super) struct BatchSender {
    sender: Arc<dyn NativeSender>,
    keys: Arc<Keys>,
}

impl BatchSender {
    pub(super) fn new(sender: Arc<dyn NativeSender>, keys: Arc<Keys>) -> Self {
        Self { sender, keys }
    }

    fn transfer(&self, index: usize, transfer: Transfer) -> Result<NativeTransfer, SendError> {
        let source = Address::try_from(&transfer.wallet.address()).map_err(|_| {
            item(
                index,
                ErrorKind::Unsupported,
                "wallet is not a Solana wallet",
            )
        })?;
        let destination = transfer
            .wallet
            .parse_address(&transfer.to)
            .and_then(|address| Address::try_from(&address).map_err(|_| invalid_address()))
            .map_err(|error| SendError::item(index, Vec::new(), error))?;
        let destination = NativeDestination::try_from(destination).map_err(|_| {
            item(
                index,
                ErrorKind::Unsupported,
                "unsupported Solana destination",
            )
        })?;
        if destination.address() == &source {
            return Err(item(
                index,
                ErrorKind::AddressMismatch,
                "Solana source and destination must differ",
            ));
        }
        let amount = Lamport::from_decimal(&transfer.amount).map_err(|_| {
            item(
                index,
                ErrorKind::InvalidAmount,
                "native SOL amount is invalid",
            )
        })?;
        let signer = self.keys.get(&source).ok_or_else(|| {
            item(
                index,
                ErrorKind::NotFound,
                "Solana wallet signer is unavailable",
            )
        })?;
        NativeTransfer::new(source, signer, destination.address().clone(), amount)
            .map_err(|error| SendError::item(index, Vec::new(), error))
    }
}

impl Sender for BatchSender {
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
            let native = transfers
                .into_iter()
                .enumerate()
                .map(|(index, transfer)| self.transfer(index, transfer))
                .collect::<Result<Vec<_>, _>>()?;
            self.sender.send(native).await
        })
    }
}

fn item(index: usize, kind: ErrorKind, message: &'static str) -> SendError {
    SendError::item(index, Vec::new(), Error::new(kind, message))
}

fn invalid_address() -> Error {
    Error::new(
        ErrorKind::InvalidAddress,
        "invalid canonical Solana address",
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Mutex};

    use base::{Decimal, TransactionId};
    use indexing::{BoxFuture, History, HistoryQuery, IndexError, TransactionPage};
    use wallets::{AddressText, Provider, SecretBytes};

    use crate::{
        AssetKind, RpcClient, Seed, WalletConfig, WalletProvider, rpc::test_support::Scripted,
    };

    use super::*;

    #[derive(Default)]
    struct Index;

    impl History for Index {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async {
                Ok(TransactionPage {
                    checkpoint: None,
                    transactions: Vec::new(),
                    next: None,
                })
            })
        }
    }

    #[derive(Default)]
    struct Native {
        sizes: Mutex<Vec<usize>>,
    }

    #[derive(Default)]
    struct Guarded(Mutex<BTreeSet<Address>>);

    impl NativeSender for Guarded {
        fn send<'a>(&'a self, transfers: Vec<NativeTransfer>) -> SendFuture<'a> {
            Box::pin(async move {
                let mut guarded = self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some((index, _)) = transfers
                    .iter()
                    .enumerate()
                    .find(|(_, transfer)| guarded.contains(transfer.source()))
                {
                    return Err(item(
                        index,
                        ErrorKind::SourceBusy,
                        "Solana source is already in use",
                    ));
                }
                let transfer = &transfers[0];
                guarded.insert(transfer.source().clone());
                let id = TransactionId::new("ambiguous-signature");
                let mut error = Error::new(ErrorKind::Unavailable, "unknown Solana submission");
                error.ambiguous_transaction_id = Some(id);
                Err(SendError::item(0, Vec::new(), error))
            })
        }
    }

    impl NativeSender for Native {
        fn send<'a>(&'a self, transfers: Vec<NativeTransfer>) -> SendFuture<'a> {
            Box::pin(async move {
                self.sizes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(transfers.len());
                Ok((0..transfers.len())
                    .map(|index| TransactionId::new(format!("signature-{index}")))
                    .collect())
            })
        }
    }

    fn family() -> (WalletProvider<Scripted>, Arc<Native>) {
        let sender = Arc::new(Native::default());
        let provider = WalletProvider::new(
            WalletConfig::new("localnet", AssetKind::Native).expect("asset"),
            RpcClient::new(Scripted::new([])),
            Arc::new(Index),
            sender.clone(),
        );
        (provider, sender)
    }

    fn guarded_family() -> WalletProvider<Scripted> {
        WalletProvider::new(
            WalletConfig::new("localnet", AssetKind::Native).expect("asset"),
            RpcClient::new(Scripted::new([])),
            Arc::new(Index),
            Arc::new(Guarded::default()),
        )
    }

    fn transfers(
        wallet: Arc<dyn wallets::Wallet>,
        destination: AddressText,
        count: usize,
    ) -> Vec<Transfer> {
        (0..count)
            .map(|_| Transfer {
                wallet: Arc::clone(&wallet),
                to: destination.clone(),
                amount: Decimal::from(1_u64),
            })
            .collect()
    }

    #[test]
    fn routes_one_and_fifty_items_once_and_rejects_zero_and_fifty_one_first() {
        let (provider, native) = family();
        let wallet =
            futures_executor::block_on(provider.create(SecretBytes::new([7; 32]))).expect("wallet");
        let destination: AddressText =
            crate::Key::from_seed(hex::encode([8; 32]).parse::<Seed>().expect("seed"))
                .expect("key")
                .address()
                .into();
        let sender = provider.transactions();

        for count in [1, MAX_TRANSFERS] {
            let ids = futures_executor::block_on(sender.send(transfers(
                Arc::clone(&wallet),
                destination.clone(),
                count,
            )))
            .expect("valid batch");
            assert_eq!(ids.len(), count);
        }
        assert_eq!(
            futures_executor::block_on(sender.send(Vec::new()))
                .expect_err("empty")
                .source
                .kind,
            ErrorKind::InvalidBatch
        );
        assert_eq!(
            futures_executor::block_on(sender.send(transfers(
                wallet,
                destination,
                MAX_TRANSFERS + 1,
            )))
            .expect_err("above maximum")
            .source
            .kind,
            ErrorKind::InvalidBatch
        );
        assert_eq!(
            *native
                .sizes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [1, MAX_TRANSFERS]
        );
    }

    #[test]
    fn single_and_batch_paths_share_one_source_guard_in_both_directions() {
        for single_first in [true, false] {
            let provider = guarded_family();
            let wallet = futures_executor::block_on(provider.create(SecretBytes::new([7; 32])))
                .expect("wallet");
            let destination: AddressText =
                crate::Key::from_seed(hex::encode([8; 32]).parse::<Seed>().expect("seed"))
                    .expect("key")
                    .address()
                    .into();
            let batch = provider.transactions();
            let single = || {
                futures_executor::block_on(wallet.send(destination.clone(), Decimal::from(1_u64)))
            };
            let grouped = || {
                futures_executor::block_on(batch.send(transfers(
                    Arc::clone(&wallet),
                    destination.clone(),
                    1,
                )))
            };

            let first_ambiguous = if single_first {
                single()
                    .expect_err("single ambiguity")
                    .ambiguous_transaction_id
            } else {
                grouped()
                    .expect_err("batch ambiguity")
                    .ambiguous_transaction_id
            };
            assert!(first_ambiguous.is_some());
            let second = if single_first {
                grouped().expect_err("batch must see guard").source
            } else {
                single().expect_err("single must see guard")
            };
            assert_eq!(second.kind, ErrorKind::SourceBusy);
        }
    }
}
