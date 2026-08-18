use std::{collections::BTreeMap, error, fmt, sync::Arc};

use base::{Decimal, SignedTransaction, TransactionId, TransactionSnapshot};
use indexing::{
    ConfirmationProof, EventQuery, IndexScope, Indexer, ObservationEvent, TransactionRef,
    TransactionStatus, WatchRequest, WatchSelector,
};
use serde::{Deserialize, Serialize};
use storage::ErrorKind as StorageErrorKind;
use wallets::{AddressText, Wallet};

use crate::{ReconcileBatch, Storage};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub wallet: String,
    pub destination: AddressText,
    pub amount: String,
    pub confirmations: u64,
    #[serde(default)]
    pub require_finality: bool,
}

impl Request {
    fn validate(&self) -> Result<Decimal, Error> {
        if self.id.is_empty() || self.id.len() > 256 {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "payment ID must contain between 1 and 256 bytes",
            ));
        }
        if self.wallet.is_empty() || self.destination.text.is_empty() || self.confirmations == 0 {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "wallet and destination must not be empty and confirmations must be positive",
            ));
        }
        let amount = self.amount.parse::<Decimal>().map_err(|_| {
            Error::new(
                ErrorKind::InvalidRequest,
                "amount must be an exact base-10 decimal",
            )
        })?;
        if amount.to_string() != self.amount {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "amount must use canonical base-10 notation",
            ));
        }
        Ok(amount)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub chain: String,
    pub network: String,
}

impl From<&IndexScope> for Scope {
    fn from(value: &IndexScope) -> Self {
        Self {
            chain: value.chain.0.clone(),
            network: value.network.clone(),
        }
    }
}

impl From<&Scope> for IndexScope {
    fn from(value: &Scope) -> Self {
        Self {
            chain: indexing::ChainId(value.chain.clone()),
            network: value.network.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub id: String,
    pub minimum_confirmations: u64,
    pub require_chain_finality: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceStatus {
    Pending,
    Included { confirmations: u64 },
    Confirmed,
    Failed,
    Replaced,
    Dropped,
    Reorged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub event_id: String,
    pub cursor: u64,
    pub revision: u64,
    pub status: EvidenceStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Requested,
    Prepared {
        prepared: SignedTransaction,
        snapshot: TransactionSnapshot,
    },
    Watched {
        prepared: SignedTransaction,
        snapshot: TransactionSnapshot,
        watch: Watch,
    },
    Submitted {
        transaction_id: TransactionId,
        snapshot: TransactionSnapshot,
        watch: Watch,
    },
    Confirmed {
        transaction_id: TransactionId,
        confirmations: u64,
        snapshot: TransactionSnapshot,
        watch: Watch,
    },
}

impl Stage {
    fn prepared(&self) -> (SignedTransaction, TransactionSnapshot) {
        match self {
            Self::Prepared { prepared, snapshot } => (prepared.clone(), snapshot.clone()),
            _ => unreachable!("prepared payment stage was checked"),
        }
    }

    fn watched(&self) -> (SignedTransaction, TransactionSnapshot, Watch) {
        match self {
            Self::Watched {
                prepared,
                snapshot,
                watch,
            } => (prepared.clone(), snapshot.clone(), watch.clone()),
            _ => unreachable!("watched payment stage was persisted"),
        }
    }

    fn transaction_id(&self) -> Option<&str> {
        match self {
            Self::Prepared { prepared, .. } | Self::Watched { prepared, .. } => {
                Some(prepared.id().as_str())
            }
            Self::Submitted { transaction_id, .. } | Self::Confirmed { transaction_id, .. } => {
                Some(transaction_id.as_str())
            }
            Self::Requested => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// design-lint: allow package-name-prefix -- Payment is the application domain entity
pub struct Payment {
    pub id: String,
    pub request: Request,
    pub scope: Scope,
    pub stage: Stage,
    pub evidence: Vec<Evidence>,
}

struct WalletEntry {
    wallet: Arc<dyn Wallet>,
    scope: IndexScope,
}

pub struct Payments {
    store: Arc<dyn Storage>,
    indexer: Arc<dyn Indexer>,
    wallets: BTreeMap<String, WalletEntry>,
}

impl Payments {
    #[must_use]
    pub fn new(store: Arc<dyn Storage>, indexer: Arc<dyn Indexer>) -> Self {
        Self {
            store,
            indexer,
            wallets: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with(
        mut self,
        id: impl Into<String>,
        scope: IndexScope,
        wallet: Arc<dyn Wallet>,
    ) -> Self {
        self.wallets
            .insert(id.into(), WalletEntry { wallet, scope });
        self
    }

    pub async fn pay(&self, mut request: Request) -> Result<Payment, Error> {
        let amount = request.validate()?;
        let entry = self.wallets.get(&request.wallet).ok_or_else(|| {
            Error::new(ErrorKind::UnknownWallet, "payment wallet is not configured")
        })?;
        let wallet = entry.wallet.clone();
        let destination = wallet
            .parse_address(&request.destination)
            .map_err(Error::address)?;
        request.destination = wallet.address_text(&destination).map_err(Error::address)?;
        let mut stored = self.load_or_create(&request, &entry.scope).await?;
        if stored.payment.request != request {
            return Err(Error::new(
                ErrorKind::Conflict,
                "payment ID was already used with a different request",
            ));
        }
        if stored.payment.scope != Scope::from(&entry.scope) {
            return Err(Error::new(
                ErrorKind::Conflict,
                "payment wallet scope changed after the payment was created",
            ));
        }
        if matches!(
            stored.payment.stage,
            Stage::Submitted { .. } | Stage::Confirmed { .. }
        ) {
            return Ok(stored.payment);
        }

        if matches!(stored.payment.stage, Stage::Requested) {
            let mut builder = wallet.transaction();
            builder
                .transfer(destination, amount)
                .map_err(Error::transaction)?;
            let snapshot = builder.snapshot().map_err(Error::transaction)?;
            let prepared = builder.prepare().await.map_err(Error::transaction)?;
            stored.payment.stage = Stage::Prepared { prepared, snapshot };
            stored = self
                .store
                .update(stored.payment, stored.version)
                .await
                .map_err(Error::storage)?;
        }

        if matches!(stored.payment.stage, Stage::Prepared { .. }) {
            let (prepared, snapshot) = stored.payment.stage.prepared();
            let transaction_id = prepared.id().clone();
            let checkpoint = self
                .indexer
                .checkpoint(&entry.scope)
                .await
                .map_err(Error::indexer)?
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Indexer,
                        "indexer has no canonical checkpoint for the payment scope",
                    )
                })?;
            let selector = WatchSelector::Transaction(TransactionRef {
                scope: entry.scope.clone(),
                value: transaction_id.to_string(),
            });
            let receipt = self
                .indexer
                .watch(WatchRequest {
                    scope: entry.scope.clone(),
                    selector: selector.clone(),
                    start_height: checkpoint.height,
                    idempotency_key: watch_key(&request.id, &transaction_id),
                })
                .await
                .map_err(Error::indexer)?;
            if receipt.scope != entry.scope
                || receipt.selector != selector
                || receipt.start_height != checkpoint.height
            {
                return Err(Error::new(
                    ErrorKind::Indexer,
                    "indexer returned a watch with different transaction boundaries",
                ));
            }
            if receipt.confirmation_policy.minimum_confirmations < request.confirmations
                || (request.require_finality && !receipt.confirmation_policy.require_chain_finality)
            {
                return Err(Error::new(
                    ErrorKind::Indexer,
                    "indexer confirmation policy is weaker than the payment request",
                ));
            }
            stored.payment.stage = Stage::Watched {
                prepared,
                snapshot,
                watch: Watch {
                    id: receipt.id.0,
                    minimum_confirmations: receipt.confirmation_policy.minimum_confirmations,
                    require_chain_finality: receipt.confirmation_policy.require_chain_finality,
                },
            };
            stored = self
                .store
                .update(stored.payment, stored.version)
                .await
                .map_err(Error::storage)?;
        }

        let (prepared, snapshot, watch) = stored.payment.stage.watched();
        let transaction_id = prepared.id().clone();
        let submission = wallet
            .broadcaster()
            .broadcast(&prepared)
            .await
            .map_err(Error::transaction)?;
        if submission.id != transaction_id {
            return Err(Error::new(
                ErrorKind::Transaction,
                "broadcast returned a different transaction ID",
            ));
        }
        stored.payment.stage = Stage::Submitted {
            transaction_id,
            snapshot,
            watch,
        };
        self.store
            .update(stored.payment, stored.version)
            .await
            .map(|stored| stored.payment)
            .map_err(Error::storage)
    }

    async fn load_or_create(
        &self,
        request: &Request,
        scope: &IndexScope,
    ) -> Result<crate::StoredPayment, Error> {
        if let Some(stored) = self.store.load(&request.id).await.map_err(Error::storage)? {
            return Ok(stored);
        }
        let payment = Payment {
            id: request.id.clone(),
            request: request.clone(),
            scope: Scope::from(scope),
            stage: Stage::Requested,
            evidence: Vec::new(),
        };
        match self.store.create(payment).await {
            Ok(stored) => Ok(stored),
            Err(error) if error.kind == StorageErrorKind::Conflict => self
                .store
                .load(&request.id)
                .await
                .map_err(Error::storage)?
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Store,
                        "payment create conflicted but no record exists",
                    )
                }),
            Err(error) => Err(Error::storage(error)),
        }
    }

    /// Applies one durable observer page. It never waits for a transaction and
    /// is safe to call again after a process interruption.
    pub async fn reconcile(&self, scope: IndexScope, limit: usize) -> Result<usize, Error> {
        if limit == 0 {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "reconciliation page limit must be positive",
            ));
        }
        let state = self
            .store
            .reconcile_state(&scope)
            .await
            .map_err(Error::storage)?;
        let page = self
            .indexer
            .events(EventQuery {
                scope: scope.clone(),
                after: state.cursor.as_ref().map(|cursor| cursor.cursor),
                limit,
            })
            .await
            .map_err(Error::indexer)?;
        let event_ids = page
            .events
            .iter()
            .map(|event| event.id.0.as_str())
            .collect::<Vec<_>>();
        let mut payments = state.payments;
        for event in &page.events {
            for stored in &mut payments {
                apply_event(&mut stored.payment, event);
            }
        }
        payments.retain(|stored| {
            stored
                .payment
                .evidence
                .last()
                .is_some_and(|evidence| event_ids.contains(&evidence.event_id.as_str()))
        });
        self.store
            .commit_reconciliation(ReconcileBatch {
                scope,
                cursor: state.cursor,
                next: page.next,
                payments,
            })
            .await
            .map_err(Error::storage)?;
        Ok(page.events.len())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Payment>, Error> {
        self.store
            .load(id)
            .await
            .map(|stored| stored.map(|stored| stored.payment))
            .map_err(Error::storage)
    }

    #[must_use]
    pub fn supports_scopes(&self, scopes: &[IndexScope]) -> bool {
        let configured = self
            .wallets
            .values()
            .map(|entry| &entry.scope)
            .collect::<std::collections::BTreeSet<_>>();
        let reconciled = scopes.iter().collect::<std::collections::BTreeSet<_>>();
        !configured.is_empty() && configured == reconciled
    }
}

fn watch_key(payment_id: &str, transaction_id: &TransactionId) -> String {
    format!(
        "payment:{}:{payment_id}:transaction:{}:{transaction_id}",
        payment_id.len(),
        transaction_id.as_str().len(),
    )
}

fn apply_event(payment: &mut Payment, event: &ObservationEvent) {
    if Scope::from(&event.transaction.scope) != payment.scope
        || payment.stage.transaction_id() != Some(&event.transaction.transaction_id.value)
        || payment
            .evidence
            .iter()
            .any(|evidence| evidence.event_id == event.id.0)
    {
        return;
    }
    let status = match &event.transaction.status {
        TransactionStatus::Pending => EvidenceStatus::Pending,
        TransactionStatus::Included { confirmations, .. } => EvidenceStatus::Included {
            confirmations: *confirmations,
        },
        TransactionStatus::Confirmed { .. } => EvidenceStatus::Confirmed,
        TransactionStatus::Failed { .. } => EvidenceStatus::Failed,
        TransactionStatus::Replaced { .. } => EvidenceStatus::Replaced,
        TransactionStatus::Dropped => EvidenceStatus::Dropped,
        TransactionStatus::Reorged { .. } => EvidenceStatus::Reorged,
    };
    payment.evidence.push(Evidence {
        event_id: event.id.0.clone(),
        cursor: event.cursor.0,
        revision: event.transaction.revision.0,
        status,
    });

    let (Stage::Submitted {
        transaction_id,
        snapshot,
        watch,
    }
    | Stage::Confirmed {
        transaction_id,
        snapshot,
        watch,
        ..
    }) = &payment.stage
    else {
        return;
    };
    let transaction_id = transaction_id.clone();
    let snapshot = snapshot.clone();
    let watch = watch.clone();
    if confirmation_satisfies(&event.transaction.status, &payment.request) {
        payment.stage = Stage::Confirmed {
            transaction_id,
            confirmations: payment.request.confirmations,
            snapshot,
            watch,
        };
    } else {
        payment.stage = Stage::Submitted {
            transaction_id,
            snapshot,
            watch,
        };
    }
}

fn confirmation_satisfies(status: &TransactionStatus, request: &Request) -> bool {
    let TransactionStatus::Confirmed { proof, .. } = status else {
        return false;
    };
    match proof {
        ConfirmationProof::Depth { observed, .. } => {
            !request.require_finality && *observed >= request.confirmations
        }
        ConfirmationProof::ChainFinalized => request.require_finality && request.confirmations <= 1,
        ConfirmationProof::DepthAndChainFinalized { observed, .. } => {
            *observed >= request.confirmations
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidRequest,
    UnknownWallet,
    Conflict,
    Transaction,
    Indexer,
    Store,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn transaction(error: base::TransactionError) -> Self {
        Self::new(ErrorKind::Transaction, error.to_string())
    }

    fn address(error: wallets::Error) -> Self {
        Self::new(ErrorKind::InvalidRequest, error.to_string())
    }

    fn indexer(error: indexing::IndexError) -> Self {
        Self::new(ErrorKind::Indexer, error.to_string())
    }

    fn storage(error: storage::Error) -> Self {
        Self::new(
            if error.kind == StorageErrorKind::Conflict {
                ErrorKind::Conflict
            } else {
                ErrorKind::Store
            },
            error.to_string(),
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Error {}
