use std::{collections::BTreeMap, sync::Arc};

use axum::Router;
use tokio::sync::RwLock;
use wallets::HistoryRequest;

use crate::{Balance, BatchError, Chain, Error, ErrorKind, Wallet};

pub struct WalletSend {
    pub wallet_id: String,
    pub destination: wallets::AddressText,
    pub amount: base::Decimal,
}

#[derive(Clone)]
pub struct WalletFamily {
    pub chain: Chain,
    pub network: String,
    pub scope: indexing::IndexScope,
    pub watcher: Arc<dyn indexing::Watcher>,
    pub checkpoint: Arc<dyn indexing::Checkpoint>,
    pub transactions: Arc<dyn wallets::Sender>,
}

#[derive(Clone)]
pub struct Gateway {
    providers: Arc<wallets::Providers<Chain>>,
    families: BTreeMap<Chain, WalletFamily>,
    wallets: Arc<RwLock<wallets::Wallets<String>>>,
    summaries: Arc<RwLock<BTreeMap<String, Wallet>>>,
}

impl Gateway {
    #[must_use]
    pub fn new(providers: wallets::Providers<Chain>) -> Self {
        Self {
            providers: Arc::new(providers),
            families: BTreeMap::new(),
            wallets: Arc::new(RwLock::new(wallets::Wallets::new())),
            summaries: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    pub async fn initialize(
        &self,
        id: String,
        chain: Chain,
        secret: wallets::SecretBytes,
    ) -> Result<Wallet, Error> {
        let wallet = self
            .providers
            .create(&chain, secret)
            .await
            .map_err(|error| Error::new(ErrorKind::InvalidRequest, error.to_string()))?;
        self.store(id, chain, wallet, Some(indexing::BlockHeight(0)))
            .await
    }

    pub async fn generate(&self, chain: Chain) -> Result<Wallet, Error> {
        let wallet = self
            .providers
            .generate(&chain)
            .await
            .map_err(wallet_error)?;
        self.store(uuid::Uuid::now_v7().to_string(), chain, wallet, None)
            .await
    }

    pub async fn wallet(&self, id: &str) -> Result<Wallet, Error> {
        self.summaries
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "wallet does not exist"))
    }

    pub async fn balance(&self, id: &str) -> Result<Balance, Error> {
        let balance = self
            .instance(id)
            .await?
            .balance()
            .await
            .map_err(wallet_error)?;
        Ok(Balance {
            amount: balance.amount.to_string(),
            observed_height: balance.observed_at.map(|block| block.height.0),
        })
    }

    pub async fn history(
        &self,
        id: &str,
        request: HistoryRequest,
    ) -> Result<wallets::History, Error> {
        self.instance(id)
            .await?
            .history(request)
            .await
            .map_err(wallet_error)
    }

    pub async fn send(
        &self,
        id: &str,
        destination: wallets::AddressText,
        amount: base::Decimal,
    ) -> Result<base::TransactionId, Error> {
        if amount <= base::Decimal::zero() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "amount must be positive",
            ));
        }
        let wallet = self.instance(id).await?;
        let destination = wallet
            .parse_address(&destination)
            .map_err(|error| Error::new(ErrorKind::InvalidRequest, error.to_string()))?;
        let mut transaction = wallet.transaction();
        transaction
            .transfer(destination, amount)
            .map_err(transaction_error)?;
        let signed = transaction.prepare().await.map_err(transaction_error)?;
        let submitted = wallet
            .broadcaster()
            .broadcast(&signed)
            .await
            .map_err(transaction_error)?;
        if submitted.id != *signed.id() {
            return Err(Error::new(
                ErrorKind::Transaction,
                "broadcaster returned a different transaction ID",
            ));
        }
        Ok(submitted.id)
    }

    pub async fn send_all(
        &self,
        requests: Vec<WalletSend>,
    ) -> Result<Vec<base::TransactionId>, BatchError> {
        if requests.is_empty() {
            return Err(batch_error(0, "at least one transfer is required"));
        }
        let mut chain = None;
        let mut transfers = Vec::with_capacity(requests.len());
        for (index, request) in requests.into_iter().enumerate() {
            if request.amount <= base::Decimal::zero() {
                return Err(batch_error(index, "amount must be positive"));
            }
            let summary = self
                .wallet(&request.wallet_id)
                .await
                .map_err(|error| BatchError {
                    transaction_ids: Vec::new(),
                    failed_index: index,
                    error,
                })?;
            if chain.is_some_and(|expected| expected != summary.chain) {
                return Err(batch_error(
                    index,
                    "all transfers must use the same chain and network",
                ));
            }
            chain = Some(summary.chain);
            let wallet = self
                .instance(&request.wallet_id)
                .await
                .map_err(|error| BatchError {
                    transaction_ids: Vec::new(),
                    failed_index: index,
                    error,
                })?;
            transfers.push(wallets::Transfer {
                wallet,
                to: request.destination,
                amount: request.amount,
            });
        }
        let chain = chain.ok_or_else(|| batch_error(0, "transfer chain is missing"))?;
        let transactions = &self
            .families
            .get(&chain)
            .ok_or_else(|| batch_error(0, "transfer chain is not configured"))?
            .transactions;
        transactions
            .send(transfers)
            .await
            .map_err(|error| BatchError {
                transaction_ids: error
                    .accepted
                    .into_iter()
                    .map(|id| id.to_string())
                    .collect(),
                failed_index: error.failed_index,
                error: wallet_error(error.source),
            })
    }

    pub fn register(&mut self, family: WalletFamily) -> Result<(), Error> {
        if family.scope.chain.0 != family.chain.name() || family.scope.network != family.network {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "wallet family and index scope must agree",
            ));
        }
        if self.families.insert(family.chain, family).is_some() {
            return Err(Error::new(
                ErrorKind::Conflict,
                "wallet chain is already configured",
            ));
        }
        Ok(())
    }

    pub fn router(
        self,
        config: &http_support::server::Config,
    ) -> Result<Router, http_support::server::ConfigError> {
        crate::api::router(self, config)
    }

    async fn instance(&self, id: &str) -> Result<Arc<dyn wallets::Wallet>, Error> {
        self.wallets
            .read()
            .await
            .get(id)
            .map_err(|_| Error::new(ErrorKind::NotFound, "wallet does not exist"))
    }

    async fn store(
        &self,
        id: String,
        chain: Chain,
        wallet: Arc<dyn wallets::Wallet>,
        configured_start: Option<indexing::BlockHeight>,
    ) -> Result<Wallet, Error> {
        let family = self
            .families
            .get(&chain)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "chain is not configured"))?;
        let address = wallet
            .address_text(&wallet.address())
            .map_err(|error| Error::new(ErrorKind::InvalidRequest, error.to_string()))?;
        let checkpoint = family
            .checkpoint
            .checkpoint(&family.scope)
            .await
            .map_err(|error| Error::new(ErrorKind::Unavailable, error.to_string()))?;
        let start_height = configured_start.unwrap_or_else(|| {
            checkpoint.map_or(indexing::BlockHeight(0), |block| {
                indexing::BlockHeight(block.height.0.saturating_add(1))
            })
        });
        family
            .watcher
            .watch(indexing::WatchRequest {
                scope: family.scope.clone(),
                selector: indexing::CanonicalAddress {
                    scope: family.scope.clone(),
                    value: address.text.clone(),
                },
                start_height,
                idempotency_key: id.clone(),
            })
            .await
            .map_err(|error| Error::new(ErrorKind::Unavailable, error.to_string()))?;
        let summary = Wallet {
            id: id.clone(),
            chain,
            network: family.network.clone(),
            address: address.text,
        };
        self.wallets
            .write()
            .await
            .insert(id.clone(), wallet)
            .map_err(|error| Error::new(ErrorKind::Conflict, error.to_string()))?;
        self.summaries.write().await.insert(id, summary.clone());
        Ok(summary)
    }
}

fn wallet_error(error: wallets::Error) -> Error {
    let kind = match error.kind {
        wallets::ErrorKind::Unsupported
        | wallets::ErrorKind::InvalidSecret
        | wallets::ErrorKind::InvalidAddress
        | wallets::ErrorKind::InvalidAmount
        | wallets::ErrorKind::AddressMismatch => ErrorKind::InvalidRequest,
        wallets::ErrorKind::Duplicate => ErrorKind::Conflict,
        wallets::ErrorKind::Transaction => ErrorKind::Transaction,
        wallets::ErrorKind::Generation
        | wallets::ErrorKind::Balance
        | wallets::ErrorKind::History => ErrorKind::Unavailable,
    };
    Error::new(kind, error.to_string())
}

fn transaction_error(error: base::TransactionError) -> Error {
    Error::new(ErrorKind::Transaction, error.to_string())
}

fn batch_error(failed_index: usize, message: impl Into<String>) -> BatchError {
    BatchError {
        transaction_ids: Vec::new(),
        failed_index,
        error: Error::new(ErrorKind::InvalidRequest, message),
    }
}

#[cfg(test)]
#[path = "server_test.rs"]
mod tests;
