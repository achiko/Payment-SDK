use std::sync::Arc;

use base::{Address, SignedTransaction};
use deposits::{
    AcceptBroadcast, AttachWatch, Collection, CollectionId, CollectionLegState, Collections,
    Deposit, DepositError, DepositErrorKind, DepositReader, RecordSignature, SignedBytes,
    TransitionGuard,
};
use indexing::{
    BlockHeight, IndexScope, OutputId, TransactionRef, WatchRequest, WatchSelector, Watcher,
};
use wallets::{AddressText, SelectedOutput, Wallet};

use crate::allocation::allocate;
use crate::token::{self, GasWallet};

/// Resolves an application-owned deposit to its already-composed wallet.
///
/// Custody, key lookup, and `wallets::Wallets` provider selection stay behind
/// this application boundary. Neither private bytes nor key locators cross
/// the collection executor API.
pub trait DepositWallets: Send + Sync {
    fn wallet<'a>(&'a self, deposit: &'a Deposit) -> wallets::FutureResult<'a, Arc<dyn Wallet>>;
}

/// Durable persistence needed to execute a collection.
pub trait CollectionStore: Collections + DepositReader {}

impl<T> CollectionStore for T where T: Collections + DepositReader {}

/// Executes one already-reserved collection through wallet abstractions.
pub struct Sweeps {
    store: Arc<dyn CollectionStore>,
    wallets: Arc<dyn DepositWallets>,
    gas_wallet: Option<Arc<dyn GasWallet>>,
    indexer: Arc<dyn Watcher>,
    scope: IndexScope,
    asset: Option<indexing::AssetId>,
    mode: Option<deposits::CollectionMode>,
}

impl Sweeps {
    #[must_use]
    pub fn new(
        store: Arc<dyn CollectionStore>,
        wallets: Arc<dyn DepositWallets>,
        indexer: Arc<dyn Watcher>,
        scope: IndexScope,
    ) -> Self {
        Self {
            store,
            wallets,
            gas_wallet: None,
            indexer,
            scope,
            asset: None,
            mode: None,
        }
    }

    /// Binds a production executor to one configured asset and transaction model.
    #[must_use]
    pub fn for_asset(mut self, asset: indexing::AssetId, mode: deposits::CollectionMode) -> Self {
        self.asset = Some(asset);
        self.mode = Some(mode);
        self
    }

    /// Supplies the application-owned native-asset wallet used by token
    /// collections that require a gas-funding leg.
    #[must_use]
    pub fn with_gas_wallet(mut self, wallet: Arc<dyn GasWallet>) -> Self {
        self.gas_wallet = Some(wallet);
        self
    }

    /// Advances one collection through prepare, durable recording, watch, and
    /// broadcast. Repeating the call never signs a second transaction: a
    /// `Signed` leg is reconstructed from its stored exact transaction and
    /// submitted unchanged.
    pub async fn execute(&self, id: &CollectionId, now: u64) -> Result<Collection, DepositError> {
        let collection = self
            .store
            .collection(id)
            .await?
            .ok_or_else(|| failure(DepositErrorKind::NotFound, "collection does not exist"))?;
        self.validate(&collection)?;
        let position = collection
            .legs
            .iter()
            .position(|leg| !matches!(leg.state, CollectionLegState::Confirmed { .. }));
        let Some(position) = position else {
            return Ok(collection);
        };
        let leg = &collection.legs[position];

        match &leg.state {
            CollectionLegState::Required => {
                let deposits = self.deposits(&collection).await?;
                let (wallet, transaction, allocations, fee_limit) = match collection.mode {
                    deposits::CollectionMode::UtxoBatch => {
                        let (wallet, prepared) = self.prepare_utxo(&collection, &deposits).await?;
                        let wallets::PreparedFee::Exact(fee) = &prepared.fee else {
                            return Err(invalid("UTXO collector returned an account fee limit"));
                        };
                        let allocations = allocate(&collection, fee)?;
                        (wallet, prepared.transaction, allocations, None)
                    }
                    deposits::CollectionMode::AccountTransfer => {
                        let wallet = self.first_wallet(&collection).await?;
                        let prepared = self.prepare_account(&collection, &wallet).await?;
                        let wallets::PreparedFee::Limit(limit) = prepared.fee else {
                            return Err(invalid("account wallet returned an exact UTXO fee"));
                        };
                        (wallet, prepared.transaction, Vec::new(), Some(limit))
                    }
                    deposits::CollectionMode::TokenWithGas => {
                        let deposit = deposits
                            .first()
                            .ok_or_else(|| invalid("token collection has no deposit"))?;
                        let deposit_wallet =
                            self.wallets.wallet(deposit).await.map_err(wallet_error)?;
                        let (wallet, transaction, fee_limit) = token::prepare(
                            &collection,
                            leg,
                            deposit_wallet,
                            self.gas_wallet.as_deref(),
                        )
                        .await?;
                        (wallet, transaction, Vec::new(), fee_limit)
                    }
                };
                let transaction_id = transaction_ref(&collection, &transaction);
                let envelope = serde_json::to_vec(&transaction)
                    .map_err(|_| invalid("signed collection transaction could not be encoded"))?;
                let signed = self
                    .store
                    .record_signed(RecordSignature {
                        collection_id: collection.id.clone(),
                        leg_id: leg.id.clone(),
                        expected: guard(&collection, position),
                        expected_transaction_id: transaction_id,
                        envelope: SignedBytes::new(envelope)?,
                        allocations,
                        fee_limit,
                        signed_at: now,
                        expires_at: u64::MAX,
                    })
                    .await?;
                self.submit(signed, position, wallet, transaction, now)
                    .await
            }
            CollectionLegState::Signed { transaction_id } => {
                let wallet = self.wallet_for_leg(&collection, position).await?;
                let transaction = self.stored_transaction(&collection, position).await?;
                if transaction.id().as_str() != transaction_id.value {
                    return Err(invalid(
                        "stored collection transaction ID does not match its leg",
                    ));
                }
                self.submit(collection, position, wallet, transaction, now)
                    .await
            }
            CollectionLegState::Broadcast { transaction_id } if leg.watch_id.is_none() => {
                let receipt = self.watch(&collection, position, transaction_id).await?;
                self.store
                    .attach_watch(AttachWatch {
                        collection_id: collection.id.clone(),
                        leg_id: leg.id.clone(),
                        expected: guard(&collection, position),
                        watch_id: receipt.id,
                        updated_at: now,
                    })
                    .await
            }
            CollectionLegState::Broadcast { .. }
            | CollectionLegState::Confirmed { .. }
            | CollectionLegState::Failed { .. }
            | CollectionLegState::Reorged { .. } => Ok(collection),
        }
    }

    /// Reads the durable collection state without exposing its signed envelope.
    pub async fn get(&self, id: &CollectionId) -> Result<Option<Collection>, DepositError> {
        self.store.collection(id).await
    }

    fn validate(&self, collection: &Collection) -> Result<(), DepositError> {
        let kinds = collection
            .legs
            .iter()
            .map(|leg| leg.kind)
            .collect::<Vec<_>>();
        let valid_legs = match collection.mode {
            deposits::CollectionMode::UtxoBatch | deposits::CollectionMode::AccountTransfer => {
                kinds == [deposits::CollectionLegKind::Sweep]
            }
            deposits::CollectionMode::TokenWithGas => {
                kinds == [deposits::CollectionLegKind::Sweep]
                    || kinds
                        == [
                            deposits::CollectionLegKind::GasFunding,
                            deposits::CollectionLegKind::Sweep,
                        ]
            }
        };
        if !valid_legs
            || collection.asset.chain != self.scope.chain
            || collection.destination.scope != self.scope
            || collection.participants.is_empty()
            || (collection.mode != deposits::CollectionMode::UtxoBatch
                && collection.participants.len() != 1)
            || self
                .asset
                .as_ref()
                .is_some_and(|asset| asset != &collection.asset)
            || self.mode.is_some_and(|mode| mode != collection.mode)
        {
            return Err(invalid(
                "collection mode, participants, or chain do not match the configured scope",
            ));
        }
        Ok(())
    }

    async fn deposits(&self, collection: &Collection) -> Result<Vec<Deposit>, DepositError> {
        let mut values = Vec::with_capacity(collection.participants.len());
        for participant in &collection.participants {
            let deposit = self
                .store
                .deposit(&participant.reservation.deposit_id)
                .await?
                .ok_or_else(|| invalid("collection participant deposit does not exist"))?;
            if deposit.id != participant.reservation.deposit_id
                || deposit.user_id != participant.user_id
                || deposit.asset != collection.asset
                || deposit.address.scope != self.scope
            {
                return Err(invalid(
                    "collection participant does not match its durable deposit",
                ));
            }
            values.push(deposit);
        }
        Ok(values)
    }

    async fn first_wallet(&self, collection: &Collection) -> Result<Arc<dyn Wallet>, DepositError> {
        let deposit = self
            .deposits(collection)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| invalid("collection has no participant wallet"))?;
        self.wallets.wallet(&deposit).await.map_err(wallet_error)
    }

    async fn wallet_for_leg(
        &self,
        collection: &Collection,
        position: usize,
    ) -> Result<Arc<dyn Wallet>, DepositError> {
        if collection.legs[position].kind == deposits::CollectionLegKind::GasFunding {
            return self
                .gas_wallet
                .as_ref()
                .ok_or_else(|| invalid("token collection has no gas-funding wallet"))?
                .wallet(collection)
                .await
                .map_err(wallet_error);
        }
        self.first_wallet(collection).await
    }

    async fn prepare_utxo(
        &self,
        collection: &Collection,
        deposits: &[Deposit],
    ) -> Result<(Arc<dyn Wallet>, wallets::PreparedCollection), DepositError> {
        let mut sources = Vec::with_capacity(deposits.len());
        for (deposit, participant) in deposits.iter().zip(&collection.participants) {
            let wallet = self.wallets.wallet(deposit).await.map_err(wallet_error)?;
            let outputs = participant
                .spend_resources
                .iter()
                .map(|resource| SelectedOutput {
                    output: OutputId {
                        transaction: resource.id.transaction_id.clone(),
                        index: resource.id.output_index,
                    },
                    amount: resource.amount.clone(),
                })
                .collect();
            sources.push((wallet, outputs));
        }
        let owner = sources
            .first()
            .map(|(wallet, _)| wallet.clone())
            .ok_or_else(|| invalid("collection has no participant wallet"))?;
        let mut collector = owner
            .collector()
            .ok_or_else(|| invalid("wallet does not support UTXO collection"))?;
        for (wallet, outputs) in sources {
            collector.source(wallet, outputs).map_err(wallet_error)?;
        }
        collector
            .destination(Address::new(collection.destination.value.as_bytes()))
            .map_err(wallet_error)?;
        let prepared = collector.prepare().await.map_err(wallet_error)?;
        Ok((owner, prepared))
    }

    async fn prepare_account(
        &self,
        collection: &Collection,
        wallet: &Arc<dyn Wallet>,
    ) -> Result<wallets::PreparedCollection, DepositError> {
        let encoding = wallet
            .address_text(&wallet.address())
            .map_err(wallet_error)?
            .encoding;
        let destination = wallet
            .parse_address(&AddressText::new(
                encoding,
                collection.destination.value.clone(),
            ))
            .map_err(wallet_error)?;
        wallet.sweep(destination).await.map_err(wallet_error)
    }

    async fn stored_transaction(
        &self,
        collection: &Collection,
        position: usize,
    ) -> Result<SignedTransaction, DepositError> {
        let leg = &collection.legs[position];
        let envelope = self
            .store
            .signed_envelope(&collection.id, &leg.id)
            .await?
            .ok_or_else(|| invalid("signed collection leg has no durable transaction"))?;
        serde_json::from_slice(envelope.bytes.as_bytes())
            .map_err(|_| invalid("durable signed collection transaction is invalid"))
    }

    async fn submit(
        &self,
        collection: Collection,
        position: usize,
        wallet: Arc<dyn Wallet>,
        transaction: SignedTransaction,
        now: u64,
    ) -> Result<Collection, DepositError> {
        let transaction_id = transaction_ref(&collection, &transaction);
        let receipt = self.watch(&collection, position, &transaction_id).await?;
        let submission = wallet
            .broadcaster()
            .broadcast(&transaction)
            .await
            .map_err(transaction_error)?;
        if submission.id != *transaction.id() {
            return Err(invalid(
                "collection broadcaster returned a different transaction ID",
            ));
        }
        let broadcast = self
            .store
            .accept_broadcast(AcceptBroadcast {
                collection_id: collection.id.clone(),
                leg_id: collection.legs[position].id.clone(),
                expected: guard(&collection, position),
                transaction_id,
                accepted_at: now,
            })
            .await?;
        self.store
            .attach_watch(AttachWatch {
                collection_id: broadcast.id.clone(),
                leg_id: broadcast.legs[position].id.clone(),
                expected: guard(&broadcast, position),
                watch_id: receipt.id,
                updated_at: now,
            })
            .await
    }

    async fn watch(
        &self,
        collection: &Collection,
        position: usize,
        transaction_id: &TransactionRef,
    ) -> Result<indexing::WatchReceipt, DepositError> {
        let start_height = self
            .deposits(collection)
            .await?
            .iter()
            .map(|deposit| deposit.birthday)
            .min()
            .unwrap_or(BlockHeight(0));
        let selector = WatchSelector::Transaction(transaction_id.clone());
        let receipt = self
            .indexer
            .watch(WatchRequest {
                scope: self.scope.clone(),
                selector: selector.clone(),
                start_height,
                idempotency_key: watch_key(&collection.id, &collection.legs[position].id),
            })
            .await
            .map_err(index_error)?;
        if receipt.scope != self.scope || receipt.selector != selector {
            return Err(invalid("indexer returned a mismatched collection watch"));
        }
        Ok(receipt)
    }
}

fn transaction_ref(collection: &Collection, transaction: &SignedTransaction) -> TransactionRef {
    TransactionRef {
        scope: collection.destination.scope.clone(),
        value: transaction.id().as_str().to_owned(),
    }
}

fn guard(collection: &Collection, position: usize) -> TransitionGuard {
    TransitionGuard {
        collection_state: collection.state,
        leg_state: collection.legs[position].state.clone(),
    }
}

fn watch_key(collection: &CollectionId, leg: &deposits::LegId) -> String {
    format!(
        "collection:{}:{}:{}:{}",
        collection.0.len(),
        collection.0,
        leg.0.len(),
        leg.0
    )
}

pub(super) fn wallet_error(error: wallets::Error) -> DepositError {
    failure(DepositErrorKind::Other, error.message)
}

pub(super) fn transaction_error(error: base::TransactionError) -> DepositError {
    failure(DepositErrorKind::Other, error.message)
}

fn index_error(error: indexing::IndexError) -> DepositError {
    failure(DepositErrorKind::Other, error.message)
}

pub(super) fn invalid(message: impl Into<String>) -> DepositError {
    failure(DepositErrorKind::InvariantViolation, message)
}

fn failure(kind: DepositErrorKind, message: impl Into<String>) -> DepositError {
    DepositError {
        kind,
        message: message.into(),
    }
}
