use base::Decimal;
use indexing::{
    AssetId, CanonicalAddress, Checkpoint, IndexError, IndexScope, WatchRequest, WatchSelector,
    Watcher,
};

use crate::{
    AwaitingQuery, BoxFuture, Deposit, DepositCreator, DepositError, DepositErrorKind, DepositId,
    DepositPlan, DepositReader, DepositState, IdempotencyKey, KeyId, OpenDeposit, UserId,
    WatchQueue,
};

// These exact prefixes are the durable v1 domain encoding. Keep their bytes
// unchanged for replay compatibility; a future version must use new prefixes.
const DEPOSIT_WATCH_IDEMPOTENCY_DOMAIN: &str = "ps-deposit";
const DEPOSIT_ADDRESS_OPERATION_DOMAIN: &str = "ps:deposit-address";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressRequest {
    pub scope: IndexScope,
    pub asset: AssetId,
    /// PS-owned durable custody operation identity. WS forwards it unchanged
    /// and must return the same address after a lost response.
    pub operation_id: String,
    /// Zero-based candidate selected by PS while resolving its finite address
    /// source. The same operation and candidate must always resolve alike.
    pub candidate: u32,
    /// Business-command audit context. Custody idempotency uses the
    /// server-owned `operation_id`, not this client-provided key.
    pub idempotency_key: IdempotencyKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionedAddress {
    pub address: CanonicalAddress,
    pub key: KeyId,
    /// Opaque provisioning purpose selected by the address source.
    pub key_purpose: String,
}

/// Stateless Wallet Service boundary used by PS deposit orchestration.
pub trait DepositAddressSource: Send + Sync {
    fn address<'a>(
        &'a self,
        request: AddressRequest,
    ) -> BoxFuture<'a, Result<ProvisionedAddress, DepositError>>;
}

/// Indexing capabilities required by deposit address activation.
///
/// This marker gives the deposit workflow a descriptive bound without
/// duplicating the small consumer contracts owned by `indexing`.
pub trait DepositIndexerClient: Checkpoint + Watcher {}

impl<T: Checkpoint + Watcher + ?Sized> DepositIndexerClient for T {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositRegistration {
    pub scope: IndexScope,
    pub id: DepositId,
    pub idempotency_key: IdempotencyKey,
    pub user_id: UserId,
    pub asset: AssetId,
    pub expected: Decimal,
    pub expires_at: u64,
    pub created_at: u64,
}

/// Crash-safe PS coordinator for the `AwaitingWatch` handshake.
///
/// A retry first consults the durable deposit. Consequently a crash after the
/// local transaction cannot replace the captured checkpoint birthday or
/// address with newer values. A crash before that transaction is covered by
/// the Wallet Service idempotency key. IX watch idempotency is derived from the
/// server-owned deposit ID, never from the client-provided command key.
pub struct WatchCoordinator<'a, S: ?Sized, I: ?Sized, A: ?Sized> {
    store: &'a S,
    indexer: &'a I,
    addresses: &'a A,
    scope: IndexScope,
}

impl<'a, S: ?Sized, I: ?Sized, A: ?Sized> WatchCoordinator<'a, S, I, A> {
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

impl<S, I, A> WatchCoordinator<'_, S, I, A>
where
    S: DepositCreator + DepositReader + WatchQueue + ?Sized,
    I: DepositIndexerClient + ?Sized,
    A: DepositAddressSource + ?Sized,
{
    pub async fn register(&self, command: DepositRegistration) -> Result<Deposit, DepositError> {
        command.validate()?;
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
        let operation_id = address_operation_id(&command.id);
        let mut candidate = 0_u32;
        loop {
            let generated = self
                .addresses
                .address(AddressRequest {
                    scope: command.scope.clone(),
                    asset: command.asset.clone(),
                    operation_id: operation_id.clone(),
                    candidate,
                    idempotency_key: command.idempotency_key.clone(),
                })
                .await?;
            validate_generated(&generated, &command)?;

            if let Some(owner) = self.store.by_address(&generated.address).await? {
                if owner.id == command.id {
                    validate_existing(&owner, &command)?;
                    return self.activate(owner).await;
                }
                candidate = candidate
                    .checked_add(1)
                    .ok_or_else(|| invariant("deposit address candidate counter overflowed"))?;
                continue;
            }

            let opened = OpenDeposit {
                deposit: DepositPlan {
                    id: command.id.clone(),
                    idempotency_key: command.idempotency_key.clone(),
                    user_id: command.user_id.clone(),
                    asset: command.asset.clone(),
                    address: generated.address.clone(),
                    key: generated.key,
                    key_purpose: generated.key_purpose,
                    expected: command.expected.clone(),
                    birthday: checkpoint.height,
                    expires_at: command.expires_at,
                    created_at: command.created_at,
                },
                ledger_recorded_at: command.created_at,
            };
            match self.store.create_with_ledger(opened).await {
                Ok(created) => return self.activate(created.deposit).await,
                Err(error) if error.kind == DepositErrorKind::Conflict => {
                    if let Some(existing) = self.store.deposit(&command.id).await? {
                        validate_existing(&existing, &command)?;
                        return self.activate(existing).await;
                    }
                    if self.store.by_address(&generated.address).await?.is_some() {
                        candidate = candidate.checked_add(1).ok_or_else(|| {
                            invariant("deposit address candidate counter overflowed")
                        })?;
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Retries every durable local record that has not yet completed the IX
    /// acknowledgement. The scan is bounded per call so supervision can apply
    /// backoff and avoid monopolizing the PS runtime.
    pub async fn resume_awaiting(&self, limit: usize) -> Result<usize, DepositError> {
        let page = self
            .store
            .awaiting_watch(AwaitingQuery { after: None, limit })
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
            DepositState::Expired { .. } | DepositState::Closed => {
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
                idempotency_key: watch_idempotency_key(&deposit.id),
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
    I: DepositIndexerClient + ?Sized,
{
    indexer
        .checkpoint(scope)
        .await
        .map_err(map_index_error)?
        .ok_or_else(|| DepositError {
            kind: DepositErrorKind::InvalidState,
            message: "Indexer has no canonical checkpoint birthday".to_owned(),
        })
}

fn watch_idempotency_key(id: &DepositId) -> String {
    format!("{DEPOSIT_WATCH_IDEMPOTENCY_DOMAIN}:{}", id.0)
}

fn address_operation_id(id: &DepositId) -> String {
    format!("{DEPOSIT_ADDRESS_OPERATION_DOMAIN}:{}", id.0)
}

impl DepositRegistration {
    fn validate(&self) -> Result<(), DepositError> {
        let command = self;
        if command.scope.chain.0.trim().is_empty()
            || command.scope.network.trim().is_empty()
            || command.id.0.trim().is_empty()
            || command.idempotency_key.0.trim().is_empty()
        {
            return Err(invariant(
                "deposit scope, ID, and idempotency key must be valid",
            ));
        }
        if command.asset.chain != command.scope.chain {
            return Err(invariant(
                "deposit asset must belong to the configured Indexer chain",
            ));
        }
        crate::amount::to_bytes(&command.expected)?;
        if command.expires_at < command.created_at {
            return Err(invariant("deposit expiration precedes creation"));
        }
        Ok(())
    }
}

fn validate_existing(
    existing: &Deposit,
    command: &DepositRegistration,
) -> Result<(), DepositError> {
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

fn validate_generated(
    generated: &ProvisionedAddress,
    command: &DepositRegistration,
) -> Result<(), DepositError> {
    if generated.address.scope != command.scope || command.asset.chain != command.scope.chain {
        return Err(invariant(
            "deposit asset, generated address, and Indexer scope must share one chain/network scope",
        ));
    }
    if generated.key_purpose.trim().is_empty()
        || generated.key_purpose.len() > 1_024
        || generated
            .key_purpose
            .bytes()
            .any(|byte| byte.is_ascii_control())
    {
        return Err(invariant("generated deposit key purpose must be valid"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use indexing::ChainId;

    #[test]
    fn deposit_child_identity_bytes_are_stable() {
        let deposit_id = DepositId("deposit-1".to_owned());

        assert_eq!(
            address_operation_id(&deposit_id),
            "ps:deposit-address:deposit-1"
        );
        assert_eq!(watch_idempotency_key(&deposit_id), "ps-deposit:deposit-1");
    }

    #[test]
    fn registration_rejects_amounts_that_cannot_be_persisted() {
        let chain = ChainId("fixture".to_owned());
        let registration = DepositRegistration {
            scope: IndexScope {
                chain: chain.clone(),
                network: "test".to_owned(),
            },
            id: DepositId("deposit-1".to_owned()),
            idempotency_key: IdempotencyKey("command-1".to_owned()),
            user_id: UserId("user-1".to_owned()),
            asset: AssetId {
                chain,
                asset: "native".to_owned(),
            },
            expected: "1.25".parse().expect("valid decimal"),
            expires_at: 2,
            created_at: 1,
        };

        let error = registration
            .validate()
            .expect_err("fractional atomic amounts must be rejected before persistence");
        assert_eq!(error.kind, DepositErrorKind::InvariantViolation);
    }
}
