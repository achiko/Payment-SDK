use std::{collections::BTreeMap, sync::Arc};

use base::{TransactionError, TransactionErrorKind, TransactionId as BaseTransactionId};
use indexing::SourceError;

use super::{BuildContext, SignedTransaction, TransactionBuilder, TransactionId, TransferRequest};
use crate::{Accounts, Address, AssetKind, ChainError, ChainErrorKind, Transactions};

#[path = "coordinator_requirements.rs"]
mod requirements;
#[path = "coordinator_state.rs"]
mod state;

use requirements::{RequiredAsset, Requirements, senders};
use state::{Admission, Claim, Core, Operation};

/// Process-local nonce, preparation, and ambiguous-submission coordination.
///
/// One instance must be shared by every native and ERC-20 provider using the
/// same Ethereum accounts. It deliberately does not claim cross-process
/// coordination or durable recovery after a process crash.
#[derive(Clone)]
pub struct TransactionCoordinator {
    core: Arc<Core>,
}

impl TransactionCoordinator {
    #[must_use]
    pub fn new(accounts: Arc<dyn Accounts>, transactions: Arc<dyn Transactions>) -> Self {
        Self {
            core: Arc::new(Core::new(accounts, transactions)),
        }
    }

    pub(crate) async fn prepare_one(
        &self,
        preparation: Preparation<'_>,
    ) -> Result<SignedTransaction, ChainError> {
        self.prepare_batch(vec![preparation])
            .await
            .map_err(|error| error.source)?
            .detach_one()
    }

    pub(crate) async fn prepare_batch(
        &self,
        preparations: Vec<Preparation<'_>>,
    ) -> Result<PreparedBatch, PreparationError> {
        if preparations.is_empty() {
            return Err(PreparationError::new(
                0,
                chain_error(
                    ChainErrorKind::InvalidTransaction,
                    "Ethereum transaction batch is empty",
                ),
            ));
        }

        let senders = senders(&preparations);
        let operation = self.admit(&senders).await?;
        let nonces = self.nonces(&preparations, &senders).await?;
        let mut drafts = Vec::with_capacity(preparations.len());
        for (index, (preparation, nonce)) in preparations.into_iter().zip(nonces).enumerate() {
            let context = self
                .core
                .transactions
                .build_context(&preparation.request, nonce)
                .await
                .map_err(|source| PreparationError::new(index, source))?;
            if context.chain_id != preparation.expected_chain_id {
                return Err(PreparationError::new(
                    index,
                    chain_error(
                        ChainErrorKind::Divergent,
                        "Ethereum RPC chain ID does not match the wallet network",
                    ),
                ));
            }
            drafts.push(Draft {
                preparation,
                context,
            });
        }
        self.ensure_aggregate_balances(&drafts).await?;

        let mut entries = Vec::with_capacity(drafts.len());
        for (index, draft) in drafts.into_iter().enumerate() {
            let source = draft.preparation.request.from().clone();
            let nonce = draft.context.nonce;
            let request = draft.preparation.request.clone();
            let signed = draft
                .preparation
                .sign(TransactionBuilder::new(request, draft.context))
                .await
                .map_err(|source| PreparationError::new(index, source))?;
            entries.push(PreparedEntry {
                source,
                nonce,
                signed,
            });
        }

        self.core
            .register(operation.id, &entries)
            .map_err(|source| PreparationError::new(0, source))?;
        Ok(PreparedBatch {
            coordinator: self.clone(),
            entries,
            cursor: 0,
            operation: Some(operation),
        })
    }

    pub(crate) async fn broadcast(
        &self,
        transaction: SignedTransaction,
    ) -> Result<TransactionId, TransactionError> {
        self.submit(Some(&transaction), transaction.id.clone())
            .await
    }

    async fn admit(&self, senders: &[(Address, usize)]) -> Result<Operation, PreparationError> {
        loop {
            let mut notified = Box::pin(self.core.changed.notified());
            notified.as_mut().enable();
            match self.core.admission(senders) {
                Admission::Acquired(operation) => return Ok(operation),
                Admission::Wait => notified.await,
                Admission::Recover { id, index } => {
                    self.submit(None, id).await.map_err(|source| {
                        PreparationError::new(
                            index,
                            chain_error(
                                ChainErrorKind::RpcUnavailable,
                                format!(
                                    "Ethereum sender is blocked by an ambiguous transaction: {source}"
                                ),
                            ),
                        )
                    })?;
                }
                Admission::Exhausted(index) => {
                    return Err(PreparationError::new(
                        index,
                        chain_error(
                            ChainErrorKind::Other,
                            "Ethereum transaction coordinator exhausted operation identifiers",
                        ),
                    ));
                }
            }
        }
    }

    async fn nonces(
        &self,
        preparations: &[Preparation<'_>],
        senders: &[(Address, usize)],
    ) -> Result<Vec<u64>, PreparationError> {
        let mut next = BTreeMap::new();
        for (source, first_index) in senders {
            let pending = self
                .core
                .accounts
                .nonce(source.clone())
                .await
                .map_err(|error| PreparationError::new(*first_index, rpc_error(error)))?;
            let start = self
                .core
                .floor(source)
                .map_or(pending, |floor| floor.max(pending));
            let count = preparations
                .iter()
                .filter(|preparation| preparation.request.from() == source)
                .count();
            let count = u64::try_from(count).map_err(|_| {
                PreparationError::new(
                    *first_index,
                    chain_error(
                        ChainErrorKind::InvalidTransaction,
                        "Ethereum transaction count exceeds u64",
                    ),
                )
            })?;
            start.checked_add(count).ok_or_else(|| {
                PreparationError::new(
                    *first_index,
                    chain_error(
                        ChainErrorKind::InvalidTransaction,
                        "Ethereum batch nonce range overflows u64",
                    ),
                )
            })?;
            next.insert(source.clone(), start);
        }

        preparations
            .iter()
            .enumerate()
            .map(|(index, preparation)| {
                let nonce = next.get_mut(preparation.request.from()).ok_or_else(|| {
                    PreparationError::new(
                        index,
                        chain_error(
                            ChainErrorKind::Other,
                            "Ethereum sender was not admitted for nonce assignment",
                        ),
                    )
                })?;
                let assigned = *nonce;
                *nonce = nonce.checked_add(1).ok_or_else(|| {
                    PreparationError::new(
                        index,
                        chain_error(
                            ChainErrorKind::InvalidTransaction,
                            "Ethereum transaction nonce overflows u64",
                        ),
                    )
                })?;
                Ok(assigned)
            })
            .collect()
    }

    async fn ensure_aggregate_balances(
        &self,
        drafts: &[Draft<'_>],
    ) -> Result<(), PreparationError> {
        let requirements = Requirements::from_drafts(drafts)?;
        let mut insufficient = None;
        for requirement in requirements.values {
            let first_index = requirement.first_index();
            if insufficient.is_some_and(|index| first_index >= index) {
                break;
            }
            let asset = match &requirement.asset {
                RequiredAsset::Native => AssetKind::Native,
                RequiredAsset::Erc20(token) => AssetKind::Erc20(token.clone()),
            };
            let balance = self
                .core
                .accounts
                .balance(requirement.source.clone(), &asset, None)
                .await
                .map_err(|error| PreparationError::new(first_index, rpc_error(error)))?;
            if balance < requirement.amount {
                let index = requirement.failure_index(&balance);
                insufficient =
                    Some(insufficient.map_or(index, |current: usize| current.min(index)));
            }
        }
        if let Some(index) = insufficient {
            return Err(PreparationError::new(
                index,
                chain_error(
                    ChainErrorKind::InsufficientFunds,
                    "Ethereum aggregate batch balance is insufficient",
                ),
            ));
        }
        Ok(())
    }

    async fn submit(
        &self,
        expected: Option<&SignedTransaction>,
        id: TransactionId,
    ) -> Result<TransactionId, TransactionError> {
        loop {
            let mut notified = Box::pin(self.core.changed.notified());
            notified.as_mut().enable();
            let claim = match self
                .core
                .claim(&id, expected)
                .map_err(definite_submission_error)?
            {
                Claim::Ready(claim) => claim,
                Claim::Wait => {
                    notified.await;
                    continue;
                }
            };
            if claim.recovery {
                match self.core.transactions.known(&id).await {
                    Ok(true) => return claim.guard.accept().map_err(definite_submission_error),
                    Ok(false) => {}
                    Err(error) => return Err(ambiguous_submission_error(&id, error)),
                }
            }

            match self
                .core
                .transactions
                .broadcast(claim.transaction.clone())
                .await
            {
                Ok(returned) if returned == id => {
                    return claim.guard.accept().map_err(definite_submission_error);
                }
                Ok(_) => {
                    return Err(ambiguous_submission_error(
                        &id,
                        "Ethereum node returned a different hash for the exact signed envelope",
                    ));
                }
                Err(error) if !claim.recovery && error.ambiguous_transaction_id.is_none() => {
                    claim.guard.reject();
                    return Err(error);
                }
                Err(error) => return Err(ambiguous_submission_error(&id, error)),
            }
        }
    }
}

pub(crate) struct Preparation<'a> {
    pub(super) request: TransferRequest,
    expected_chain_id: u64,
    signer: PreparationSigner<'a>,
}

impl<'a> Preparation<'a> {
    pub(crate) fn signer(
        request: TransferRequest,
        expected_chain_id: u64,
        signer: &'a dyn base::Signer,
    ) -> Self {
        Self {
            request,
            expected_chain_id,
            signer: PreparationSigner::Signer(signer),
        }
    }

    pub(crate) fn wallet(
        request: TransferRequest,
        expected_chain_id: u64,
        wallet: &'a dyn wallets::Wallet,
    ) -> Self {
        Self {
            request,
            expected_chain_id,
            signer: PreparationSigner::Wallet(wallet),
        }
    }

    async fn sign(self, builder: TransactionBuilder) -> Result<SignedTransaction, ChainError> {
        match self.signer {
            PreparationSigner::Signer(signer) => builder.sign(signer).await,
            PreparationSigner::Wallet(wallet) => builder.sign(&WalletSigner(wallet)).await,
        }
    }
}

enum PreparationSigner<'a> {
    Signer(&'a dyn base::Signer),
    Wallet(&'a dyn wallets::Wallet),
}

struct WalletSigner<'a>(&'a dyn wallets::Wallet);

impl base::Signer for WalletSigner<'_> {
    fn sign<'a>(&'a self, request: base::SignRequest) -> base::SignFuture<'a> {
        self.0.sign(request)
    }
}

#[derive(Debug)]
pub(crate) struct PreparationError {
    pub(crate) index: usize,
    pub(crate) source: ChainError,
}

impl PreparationError {
    pub(super) fn new(index: usize, source: ChainError) -> Self {
        Self { index, source }
    }
}

pub(crate) struct PreparedBatch {
    coordinator: TransactionCoordinator,
    entries: Vec<PreparedEntry>,
    cursor: usize,
    operation: Option<Operation>,
}

impl PreparedBatch {
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) async fn next(&mut self) -> Result<Option<TransactionId>, TransactionError> {
        let Some(entry) = self.entries.get(self.cursor) else {
            self.operation.take();
            return Ok(None);
        };
        let id = self
            .coordinator
            .submit(Some(&entry.signed), entry.signed.id.clone())
            .await?;
        self.cursor += 1;
        if self.cursor == self.entries.len() {
            self.operation.take();
        }
        Ok(Some(id))
    }

    fn detach_one(mut self) -> Result<SignedTransaction, ChainError> {
        let entry = self.entries.pop().ok_or_else(|| {
            chain_error(
                ChainErrorKind::Other,
                "Ethereum single preparation produced no transaction",
            )
        })?;
        let operation = self.operation.as_ref().ok_or_else(|| {
            chain_error(
                ChainErrorKind::Other,
                "Ethereum single preparation lost its coordinator admission",
            )
        })?;
        self.coordinator
            .core
            .detach(operation.id, &entry.signed.id)?;
        self.cursor = 1;
        self.operation.take();
        Ok(entry.signed)
    }
}

struct Draft<'a> {
    preparation: Preparation<'a>,
    context: BuildContext,
}

struct PreparedEntry {
    source: Address,
    nonce: u64,
    signed: SignedTransaction,
}

fn chain_error(kind: ChainErrorKind, message: impl Into<String>) -> ChainError {
    ChainError {
        kind,
        message: message.into(),
    }
}

fn rpc_error(error: SourceError) -> ChainError {
    chain_error(ChainErrorKind::RpcUnavailable, error.message)
}

fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

fn definite_submission_error(error: SourceError) -> TransactionError {
    let kind = if error.retryable {
        TransactionErrorKind::Unavailable
    } else {
        TransactionErrorKind::Rejected
    };
    TransactionError::new(kind, error.message)
}

// design-lint: allow unclassified-free-function -- shared coordinator uncertainty mapping preserves the original error message and exact local envelope ID without changing claim state
fn ambiguous_submission_error(
    id: &TransactionId,
    error: impl std::fmt::Display,
) -> TransactionError {
    TransactionError::new(TransactionErrorKind::Unavailable, error.to_string())
        .with_ambiguous_transaction_id(BaseTransactionId::new(id.to_string()))
}

#[cfg(test)]
#[path = "coordinator_test.rs"]
mod tests;
