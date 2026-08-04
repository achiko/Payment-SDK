use chain_identity::{AssetId, AtomicAmount, CanonicalAddress};
use indexing::{
    IndexError, IndexScope, SyncPhase, SyncStatus, WatchReceipt, WatchRequest, WatchSelector,
};
use signer::KeyLocator;

use crate::{
    AwaitingWatchPageRequest, BoxFuture, CreateDeposit, CreateDepositWithLedger, Deposit,
    DepositError, DepositErrorKind, DepositId, DepositState, DepositStore, IdempotencyKey, UserId,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositAddressRequest {
    pub scope: IndexScope,
    /// The Wallet Service adapter must use this to return the same address when
    /// PS retries after losing the first response.
    pub idempotency_key: IdempotencyKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedDepositAddress {
    pub address: CanonicalAddress,
    pub key: KeyLocator,
}

/// Stateless Wallet Service boundary used by PS deposit orchestration.
pub trait DepositAddressSource: Send + Sync {
    fn address<'a>(
        &'a self,
        request: DepositAddressRequest,
    ) -> BoxFuture<'a, Result<GeneratedDepositAddress, DepositError>>;
}

/// Narrow Indexer Service client boundary owned by the PS composition root.
/// IX remains independent of deposits and business accounting.
pub trait DepositIndexerClient: Send + Sync {
    fn status<'a>(&'a self, scope: &'a IndexScope)
    -> BoxFuture<'a, Result<SyncStatus, IndexError>>;

    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterDeposit {
    pub scope: IndexScope,
    pub id: DepositId,
    pub idempotency_key: IdempotencyKey,
    pub user_id: UserId,
    pub asset: AssetId,
    pub expected: AtomicAmount,
    pub expires_at: u64,
    pub created_at: u64,
}

/// Crash-safe PS coordinator for the `AwaitingWatch` handshake.
///
/// A retry first consults the durable deposit. Consequently a crash after the
/// local transaction cannot replace the captured checkpoint birthday or
/// address with newer values. A crash before that transaction is covered by
/// the Wallet Service idempotency key.
pub struct DepositWatchCoordinator<'a, S, I, A> {
    store: &'a S,
    indexer: &'a I,
    addresses: &'a A,
    scope: IndexScope,
}

impl<'a, S, I, A> DepositWatchCoordinator<'a, S, I, A> {
    #[must_use]
    pub fn new(store: &'a S, indexer: &'a I, addresses: &'a A, scope: IndexScope) -> Self {
        Self {
            store,
            indexer,
            addresses,
            scope,
        }
    }
}

impl<S, I, A> DepositWatchCoordinator<'_, S, I, A>
where
    S: DepositStore,
    I: DepositIndexerClient,
    A: DepositAddressSource,
{
    pub async fn register(&self, command: RegisterDeposit) -> Result<Deposit, DepositError> {
        validate_command(&command)?;
        if command.scope != self.scope {
            return Err(invariant(
                "deposit registration scope differs from the configured Indexer scope",
            ));
        }
        if let Some(existing) = self.store.deposit(&command.id).await? {
            validate_existing(&existing, &command)?;
            return self.activate(existing).await;
        }

        let checkpoint = ready_checkpoint(self.indexer, &command.scope).await?;
        let generated = self
            .addresses
            .address(DepositAddressRequest {
                scope: command.scope.clone(),
                idempotency_key: command.idempotency_key.clone(),
            })
            .await?;
        if generated.address.chain != command.scope.chain
            || command.asset.chain != command.scope.chain
        {
            return Err(invariant(
                "deposit asset, generated address, and Indexer scope must share a chain",
            ));
        }

        let created = self
            .store
            .create_with_ledger(CreateDepositWithLedger {
                deposit: CreateDeposit {
                    id: command.id,
                    idempotency_key: command.idempotency_key,
                    user_id: command.user_id,
                    asset: command.asset,
                    address: generated.address,
                    key: generated.key,
                    expected: command.expected,
                    birthday: checkpoint.height,
                    expires_at: command.expires_at,
                    created_at: command.created_at,
                },
                ledger_recorded_at: command.created_at,
            })
            .await?;
        self.activate(created.deposit).await
    }

    /// Retries every durable local record that has not yet completed the IX
    /// acknowledgement. The scan is bounded per call so supervision can apply
    /// backoff and avoid monopolizing the PS runtime.
    pub async fn resume_awaiting(&self, limit: usize) -> Result<usize, DepositError> {
        let page = self
            .store
            .awaiting_watch(AwaitingWatchPageRequest { after: None, limit })
            .await?;
        let mut activated = 0_usize;
        for deposit in page.deposits {
            self.activate(deposit).await?;
            activated = activated
                .checked_add(1)
                .ok_or_else(|| invariant("AwaitingWatch activation counter overflowed"))?;
        }
        Ok(activated)
    }

    async fn activate(&self, deposit: Deposit) -> Result<Deposit, DepositError> {
        match &deposit.state {
            DepositState::Active { .. } => return Ok(deposit),
            DepositState::AwaitingWatch => {}
            DepositState::Expired | DepositState::Closed => {
                return Err(DepositError {
                    kind: DepositErrorKind::InvalidState,
                    message: "only an AwaitingWatch deposit can complete watch registration"
                        .to_owned(),
                });
            }
        }

        ready_checkpoint(self.indexer, &self.scope).await?;
        let receipt = self
            .indexer
            .watch(WatchRequest {
                scope: self.scope.clone(),
                selector: WatchSelector::Address(deposit.address.clone()),
                start_height: deposit.birthday,
                idempotency_key: watch_idempotency_key(&deposit.idempotency_key),
            })
            .await
            .map_err(map_index_error)?;
        if receipt.scope != self.scope
            || receipt.start_height != deposit.birthday
            || receipt.selector != WatchSelector::Address(deposit.address.clone())
        {
            return Err(invariant(
                "Indexer watch acknowledgement does not match the deposit request",
            ));
        }
        self.store
            .activate_watch(&deposit.id, &deposit.idempotency_key, receipt.id)
            .await
    }
}

async fn ready_checkpoint<I>(
    indexer: &I,
    scope: &IndexScope,
) -> Result<indexing::BlockRef, DepositError>
where
    I: DepositIndexerClient,
{
    let status = indexer.status(scope).await.map_err(map_index_error)?;
    if status.scope != *scope {
        return Err(invariant("Indexer status returned a different scope"));
    }
    if status.phase != SyncPhase::Ready {
        return Err(DepositError {
            kind: DepositErrorKind::InvalidState,
            message: "Indexer is not ready to acknowledge a deposit watch".to_owned(),
        });
    }
    status.checkpoint.ok_or_else(|| DepositError {
        kind: DepositErrorKind::InvalidState,
        message: "Indexer is Ready without a canonical checkpoint birthday".to_owned(),
    })
}

fn watch_idempotency_key(key: &IdempotencyKey) -> String {
    format!("ps-deposit:{}", key.0)
}

fn validate_command(command: &RegisterDeposit) -> Result<(), DepositError> {
    if command.scope.chain.0.trim().is_empty()
        || command.scope.network.trim().is_empty()
        || command.id.0.trim().is_empty()
        || command.idempotency_key.0.trim().is_empty()
    {
        return Err(invariant(
            "deposit scope, ID, and idempotency key must be non-empty",
        ));
    }
    if command.asset.chain != command.scope.chain {
        return Err(invariant(
            "deposit asset must belong to the configured Indexer chain",
        ));
    }
    if command.expires_at < command.created_at {
        return Err(invariant("deposit expiration precedes creation"));
    }
    Ok(())
}

fn validate_existing(existing: &Deposit, command: &RegisterDeposit) -> Result<(), DepositError> {
    if existing.idempotency_key != command.idempotency_key
        || existing.user_id != command.user_id
        || existing.asset != command.asset
        || existing.expected != command.expected
        || existing.expires_at != command.expires_at
        || existing.created_at != command.created_at
    {
        return Err(DepositError {
            kind: DepositErrorKind::Conflict,
            message: "deposit ID was reused with a different registration request".to_owned(),
        });
    }
    Ok(())
}

fn map_index_error(error: IndexError) -> DepositError {
    let kind = match error.kind {
        indexing::IndexErrorKind::Conflict => DepositErrorKind::Conflict,
        indexing::IndexErrorKind::InvalidRequest
        | indexing::IndexErrorKind::InvalidWatch
        | indexing::IndexErrorKind::ScopeMismatch
        | indexing::IndexErrorKind::PolicyMismatch => DepositErrorKind::InvariantViolation,
        indexing::IndexErrorKind::Halted
        | indexing::IndexErrorKind::RebuildRequired
        | indexing::IndexErrorKind::ReorgBeyondRetention => DepositErrorKind::InvalidState,
        _ => DepositErrorKind::Other,
    };
    DepositError {
        kind,
        message: error.message,
    }
}

fn invariant(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}
