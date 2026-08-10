use std::collections::BTreeSet;

use chain_ethereum::{EthereumEip1559FeeInspection, Wei};
use chain_identity::{AtomicAmount, CanonicalAddress};
use deposits::{
    AcceptCollectionBroadcast, AttachCollectionWatch, BoxFuture, Collection, CollectionId,
    CollectionLeg, CollectionLegId, CollectionLegKind, CollectionLegState, CollectionMode,
    CollectionState, CollectionStore, CollectionTransitionGuard, CreateCollection,
    CreateCollectionJob, CreateCollectionLeg, Deposit, DepositError, DepositErrorKind,
    DepositIndexerClient, DepositLedger, DepositState, DepositStore, Job, JobPageRequest,
    JobPayload, JobStore, PersistentPaymentRepository, ReconciliationStore, RetryCollectionJob,
    RetryCollectionLeg, SignedEnvelopeBytes,
};
use indexing::{IndexScope, WatchRequest, WatchSelector};
use sha2::{Digest, Sha256};
use signer::OperationId;
use storage_rocksdb::RocksDbStorage;

use crate::{
    indexer_client::IndexerClient,
    policy::PaymentPolicy,
    wallet_client::{
        CollectionRequest, Erc20CollectionRequest, NativeCollectionRequest, NativeTransferRequest,
        PreparedCollection, PreparedTransaction, WalletClient, WalletCollectionRequirement,
        WalletReceipt, inspect_signed_envelope_fees,
    },
};

type Repository = PersistentPaymentRepository<RocksDbStorage>;

const COLLECTION_SCAN_PAGE_SIZE: usize = 1_000;

/// Executes or resumes one durable Ethereum collection command.
///
/// Every external write is separated by a durable PS transition. In
/// particular, signed bytes are stored before broadcast and a transaction ID
/// is stored before IX watch registration. A process crash can therefore only
/// repeat an idempotent operation with identical inputs.
pub(crate) async fn process_collection_job(
    repository: &Repository,
    indexer: &IndexerClient,
    wallet: &WalletClient,
    policy: &PaymentPolicy,
    scope: &IndexScope,
    job: &Job,
) -> Result<(), DepositError> {
    if &policy.scope != scope || job.policy != policy.identity() {
        return Err(invalid(
            "collection job policy identity differs from the active Payment Service policy",
        ));
    }

    let collection = match &job.payload {
        JobPayload::CreateCollection(payload) => {
            ensure_or_create(repository, wallet, policy, job, payload).await?
        }
        JobPayload::RetryCollection(payload) => {
            prepare_explicit_retry(repository, job, payload).await?
        }
        JobPayload::CreateDeposit(_)
        | JobPayload::CloseDeposit(_)
        | JobPayload::CreateUtxoBatchCollection(_)
        | JobPayload::RetryUtxoBatchCollection(_) => {
            return Err(invalid(
                "non-collection job reached the collection executor",
            ));
        }
    };
    drive_collection(repository, indexer, wallet, policy, scope, collection).await
}

async fn ensure_or_create(
    repository: &Repository,
    wallet: &WalletClient,
    policy: &PaymentPolicy,
    job: &Job,
    payload: &CreateCollectionJob,
) -> Result<Collection, DepositError> {
    if let Some(existing) = repository.collection(&payload.collection_id).await? {
        validate_job_collection(job, payload, &existing)?;
        return Ok(existing);
    }

    let deposit =
        required_eligible_deposit(repository, &payload.deposit_id, &payload.user_id).await?;
    if repository.automatic_actions_blocked(&deposit.id).await? {
        return Err(invalid_state(
            "collection is blocked by an unresolved reconciliation case",
        ));
    }
    let ledger = repository
        .current(&deposit.id)
        .await?
        .ok_or_else(|| invalid("collection deposit has no absolute ledger head"))?;
    let wallet_balance = wallet.balance(&deposit.asset, &deposit.address).await?;
    let reservation_amount = ledger.balances.balance.min(wallet_balance.spendable);
    let asset_policy = policy
        .asset(&deposit.asset)
        .map_err(|error| invalid(error.to_string()))?;
    if reservation_amount < asset_policy.minimum_collection_amount {
        return Err(invalid_state(
            "deposit spendable balance is below the active collection minimum",
        ));
    }

    let requirements_request = collection_request(
        &deposit,
        &asset_policy.master_destination,
        reservation_amount,
        stable_operation_id(&payload.collection_id, "requirements", 0)?,
    )?;
    let requirements = wallet
        .collection_requirements(&requirements_request)
        .await?;
    let (mode, legs) = collection_plan(policy, &payload.collection_id, &deposit, requirements)?;
    let outcome = repository
        .create_or_replay_collection(CreateCollection {
            id: payload.collection_id.clone(),
            job_id: job.id.clone(),
            user_id: payload.user_id.clone(),
            deposit_id: payload.deposit_id.clone(),
            mode,
            asset: deposit.asset,
            destination: asset_policy.master_destination.clone(),
            policy: job.policy.clone(),
            reservation_amount,
            legs,
            created_at: job.created_at,
        })
        .await?;
    Ok(outcome.collection().clone())
}

async fn prepare_explicit_retry(
    repository: &Repository,
    job: &Job,
    payload: &RetryCollectionJob,
) -> Result<Collection, DepositError> {
    let mut collection = repository
        .collection(&payload.collection_id)
        .await?
        .ok_or_else(|| not_found("retry collection does not exist"))?;
    if collection.deposit_id != payload.deposit_id
        || collection.user_id != payload.user_id
        || collection.policy != job.policy
    {
        return Err(invalid(
            "retry job identity differs from its durable collection",
        ));
    }
    if collection.state == CollectionState::Completed {
        return Ok(collection);
    }
    if matches!(
        collection.state,
        CollectionState::Failed | CollectionState::Reorged
    ) {
        let position = collection
            .legs
            .iter()
            .position(|leg| {
                matches!(
                    leg.state,
                    CollectionLegState::Failed { .. } | CollectionLegState::Reorged { .. }
                )
            })
            .ok_or_else(|| invalid("terminal collection has no terminal leg"))?;
        let leg = collection.legs[position].clone();
        let expected = guard(&collection, &leg);
        collection = repository
            .retry_leg(RetryCollectionLeg {
                collection_id: collection.id.clone(),
                leg_id: leg.id,
                expected,
                updated_at: unix_timestamp()?,
            })
            .await?;
    }
    Ok(collection)
}

async fn drive_collection(
    repository: &Repository,
    indexer: &IndexerClient,
    wallet: &WalletClient,
    policy: &PaymentPolicy,
    scope: &IndexScope,
    mut collection: Collection,
) -> Result<(), DepositError> {
    let deposit =
        required_eligible_deposit(repository, &collection.deposit_id, &collection.user_id).await?;

    loop {
        match collection.state {
            CollectionState::Completed => return Ok(()),
            CollectionState::Failed | CollectionState::Reorged => {
                return Err(invalid(
                    "collection reached a terminal state and requires an explicit retry command",
                ));
            }
            CollectionState::Required | CollectionState::InProgress => {}
        }

        let Some(leg) = collection
            .legs
            .iter()
            .find(|leg| !matches!(leg.state, CollectionLegState::Confirmed { .. }))
            .cloned()
        else {
            return Err(invalid(
                "collection has confirmed legs but is not durably completed",
            ));
        };

        match &leg.state {
            CollectionLegState::Required => {
                if leg.kind == CollectionLegKind::GasFunding
                    && another_gas_funding_transaction_is_in_flight(repository, &collection.id)
                        .await?
                {
                    return Err(invalid_state(
                        "another gas-funder transaction is awaiting an IX terminal fact",
                    ));
                }
                collection =
                    sign_and_persist(repository, wallet, policy, &deposit, collection, leg).await?;
            }
            CollectionLegState::Signed { transaction_id } => {
                let envelope = repository
                    .signed_envelope(&collection.id, &leg.id)
                    .await?
                    .ok_or_else(|| invalid("signed collection leg has no durable envelope"))?;
                if &envelope.expected_transaction_id != transaction_id {
                    return Err(invalid(
                        "signed collection leg transaction ID differs from its envelope",
                    ));
                }
                validate_eip1559_fee_policy(
                    policy,
                    &inspect_signed_envelope_fees(transaction_id, &envelope.bytes)?,
                )?;
                let accepted =
                    broadcast_or_recover(wallet, transaction_id, &envelope.bytes).await?;
                let expected = guard(&collection, &leg);
                collection = repository
                    .accept_broadcast(AcceptCollectionBroadcast {
                        collection_id: collection.id.clone(),
                        leg_id: leg.id,
                        expected,
                        transaction_id: accepted,
                        accepted_at: unix_timestamp()?,
                    })
                    .await?;
            }
            CollectionLegState::Broadcast { transaction_id } => {
                if leg.watch_id.is_none() {
                    let receipt = indexer
                        .watch(WatchRequest {
                            scope: scope.clone(),
                            selector: WatchSelector::Transaction(transaction_id.clone()),
                            start_height: deposit.birthday,
                            idempotency_key: watch_idempotency_key(&collection.id, &leg.id),
                        })
                        .await
                        .map_err(index_error)?;
                    let expected = guard(&collection, &leg);
                    repository
                        .attach_watch(AttachCollectionWatch {
                            collection_id: collection.id.clone(),
                            leg_id: leg.id,
                            expected,
                            watch_id: receipt.id,
                            updated_at: unix_timestamp()?,
                        })
                        .await?;
                }
                return Err(invalid_state(
                    "collection transaction is awaiting an IX terminal fact",
                ));
            }
            CollectionLegState::Confirmed { .. } => {
                collection = repository
                    .collection(&collection.id)
                    .await?
                    .ok_or_else(|| invalid("durable collection disappeared"))?;
            }
            CollectionLegState::Failed { .. } | CollectionLegState::Reorged { .. } => {
                return Err(invalid("collection leg requires an explicit retry command"));
            }
        }
    }
}

/// Resolves the exact signed transaction across the broadcast-response-loss
/// window. A receipt proves that the expected hash reached the chain even when
/// the original WS/RPC response never reached PS. If it is not yet included,
/// replaying the identical envelope remains the safe recovery operation.
trait BroadcastRecoveryWallet: Send + Sync {
    fn broadcast<'a>(
        &'a self,
        transaction_id: &'a chain_identity::CanonicalTransactionId,
        envelope: &'a SignedEnvelopeBytes,
    ) -> BoxFuture<'a, Result<chain_identity::CanonicalTransactionId, DepositError>>;

    fn receipt<'a>(
        &'a self,
        transaction_id: &'a chain_identity::CanonicalTransactionId,
    ) -> BoxFuture<'a, Result<Option<WalletReceipt>, DepositError>>;
}

impl BroadcastRecoveryWallet for WalletClient {
    fn broadcast<'a>(
        &'a self,
        transaction_id: &'a chain_identity::CanonicalTransactionId,
        envelope: &'a SignedEnvelopeBytes,
    ) -> BoxFuture<'a, Result<chain_identity::CanonicalTransactionId, DepositError>> {
        Box::pin(WalletClient::broadcast(self, transaction_id, envelope))
    }

    fn receipt<'a>(
        &'a self,
        transaction_id: &'a chain_identity::CanonicalTransactionId,
    ) -> BoxFuture<'a, Result<Option<WalletReceipt>, DepositError>> {
        Box::pin(WalletClient::receipt(self, transaction_id))
    }
}

async fn broadcast_or_recover(
    wallet: &dyn BroadcastRecoveryWallet,
    transaction_id: &chain_identity::CanonicalTransactionId,
    envelope: &SignedEnvelopeBytes,
) -> Result<chain_identity::CanonicalTransactionId, DepositError> {
    if receipt_proves_submission(wallet, transaction_id).await? {
        return Ok(transaction_id.clone());
    }

    match wallet.broadcast(transaction_id, envelope).await {
        Ok(accepted) => Ok(accepted),
        Err(broadcast_error) => {
            if receipt_proves_submission(wallet, transaction_id).await? {
                Ok(transaction_id.clone())
            } else {
                Err(broadcast_error)
            }
        }
    }
}

async fn receipt_proves_submission(
    wallet: &dyn BroadcastRecoveryWallet,
    transaction_id: &chain_identity::CanonicalTransactionId,
) -> Result<bool, DepositError> {
    match wallet.receipt(transaction_id).await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(error)
            if matches!(
                error.kind,
                DepositErrorKind::Other | DepositErrorKind::NotFound
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

async fn sign_and_persist(
    repository: &Repository,
    wallet: &WalletClient,
    policy: &PaymentPolicy,
    deposit: &Deposit,
    collection: Collection,
    leg: CollectionLeg,
) -> Result<Collection, DepositError> {
    let operation_id = stable_operation_id(
        &collection.id,
        match leg.kind {
            CollectionLegKind::GasFunding => "gas-funding",
            CollectionLegKind::Sweep => "sweep",
        },
        leg.attempt_count
            .checked_add(1)
            .ok_or_else(|| invalid("collection leg attempt counter is exhausted"))?,
    )?;

    let (transaction_id, envelope) = match leg.kind {
        CollectionLegKind::GasFunding => {
            let amount = leg
                .planned_amount
                .ok_or_else(|| invalid("gas-funding leg has no durable planned amount"))?;
            if amount > policy.gas_funder.maximum_funding_amount {
                return Err(invalid(
                    "durable gas-funding amount exceeds the active policy ceiling",
                ));
            }
            let prepared = wallet
                .sign_native_transfer(&NativeTransferRequest {
                    operation_id,
                    key_locator: policy.gas_funder.key.clone(),
                    from: policy.gas_funder.address.clone(),
                    to: deposit.address.clone(),
                    value: amount,
                })
                .await?;
            validate_prepared_transaction_fees(policy, &prepared)?;
            prepared_transaction_parts(prepared)
        }
        CollectionLegKind::Sweep => {
            let request = collection_request(
                deposit,
                &collection.destination,
                collection.reservation.amount,
                operation_id,
            )?;
            let prepared = match request {
                CollectionRequest::Native(request) => {
                    wallet.sign_native_collection(&request).await?
                }
                CollectionRequest::Erc20(request) => wallet.sign_erc20_collection(&request).await?,
            };
            validate_prepared_attribution(deposit, &collection, &prepared)?;
            validate_prepared_collection_fees(policy, &prepared)?;
            (prepared.transaction_id, prepared.signed_envelope)
        }
    };
    let signed_at = unix_timestamp()?;
    let expected = guard(&collection, &leg);
    repository
        .record_signed(deposits::RecordSignedCollectionLeg {
            collection_id: collection.id.clone(),
            leg_id: leg.id,
            expected,
            expected_transaction_id: transaction_id,
            envelope,
            allocations: Vec::new(),
            signed_at,
            // This is a cleanup hint, not an authorization boundary. Exact
            // signed bytes remain recoverable until broadcast is accepted.
            expires_at: u64::MAX,
        })
        .await
}

fn prepared_transaction_parts(
    prepared: PreparedTransaction,
) -> (chain_identity::CanonicalTransactionId, SignedEnvelopeBytes) {
    (prepared.transaction_id, prepared.signed_envelope)
}

fn validate_prepared_transaction_fees(
    policy: &PaymentPolicy,
    prepared: &PreparedTransaction,
) -> Result<(), DepositError> {
    validate_eip1559_fee_policy(policy, &prepared.inspect_eip1559_fees()?)
}

fn validate_prepared_collection_fees(
    policy: &PaymentPolicy,
    prepared: &PreparedCollection,
) -> Result<(), DepositError> {
    validate_eip1559_fee_policy(policy, &prepared.inspect_eip1559_fees()?)
}

fn validate_eip1559_fee_policy(
    policy: &PaymentPolicy,
    inspection: &EthereumEip1559FeeInspection,
) -> Result<(), DepositError> {
    if inspection.chain_id != policy.ethereum_chain_id {
        return Err(invalid(
            "signed Ethereum transaction chain ID differs from the active policy",
        ));
    }
    if inspection.gas_limit > policy.fees.max_gas_limit {
        return Err(invalid(
            "signed Ethereum transaction gas limit exceeds the active policy ceiling",
        ));
    }
    if ethereum_amount(&inspection.max_fee_per_gas) > policy.fees.max_fee_per_gas {
        return Err(invalid(
            "signed Ethereum transaction maximum fee per gas exceeds the active policy ceiling",
        ));
    }
    if ethereum_amount(&inspection.max_priority_fee_per_gas) > policy.fees.max_priority_fee_per_gas
    {
        return Err(invalid(
            "signed Ethereum transaction priority fee per gas exceeds the active policy ceiling",
        ));
    }

    let checked_maximum_total_fee = inspection
        .max_fee_per_gas
        .checked_mul_u64(inspection.gas_limit)
        .ok_or_else(|| invalid("signed Ethereum transaction maximum total fee overflowed"))?;
    if checked_maximum_total_fee != inspection.maximum_total_fee {
        return Err(invalid(
            "signed Ethereum transaction has inconsistent maximum total fee fields",
        ));
    }
    if ethereum_amount(&checked_maximum_total_fee) > policy.fees.max_total_fee {
        return Err(invalid(
            "signed Ethereum transaction maximum total fee exceeds the active policy ceiling",
        ));
    }
    Ok(())
}

fn ethereum_amount(value: &Wei) -> AtomicAmount {
    AtomicAmount(value.0)
}

fn validate_prepared_attribution(
    deposit: &Deposit,
    collection: &Collection,
    prepared: &PreparedCollection,
) -> Result<(), DepositError> {
    if prepared.attribution.len() != 1 {
        return Err(invalid(
            "Wallet Service collection must return exactly one deposit attribution",
        ));
    }
    let attribution = &prepared.attribution[0];
    if attribution.address != deposit.address || attribution.asset != collection.asset {
        return Err(invalid(
            "Wallet Service collection attribution differs from the durable deposit",
        ));
    }
    if attribution.gross_debit.is_zero() || attribution.gross_debit > collection.reservation.amount
    {
        return Err(invalid(
            "Wallet Service collection attribution exceeds the durable reservation",
        ));
    }
    if collection.asset.asset != "native"
        && attribution.gross_debit != collection.reservation.amount
    {
        return Err(invalid(
            "ERC-20 collection attribution must equal the reserved token amount",
        ));
    }
    Ok(())
}

async fn required_eligible_deposit(
    repository: &Repository,
    deposit_id: &deposits::DepositId,
    user_id: &deposits::UserId,
) -> Result<Deposit, DepositError> {
    let deposit = repository
        .deposit(deposit_id)
        .await?
        .ok_or_else(|| not_found("collection deposit does not exist"))?;
    if &deposit.user_id != user_id {
        return Err(invalid(
            "collection user association differs from the durable deposit",
        ));
    }
    if !matches!(
        deposit.state,
        DepositState::Active { .. } | DepositState::Expired { .. } | DepositState::Closed
    ) {
        return Err(invalid_state(
            "only an observed active, expired, or closed deposit can be collected",
        ));
    }
    Ok(deposit)
}

fn validate_job_collection(
    job: &Job,
    payload: &CreateCollectionJob,
    collection: &Collection,
) -> Result<(), DepositError> {
    if collection.job_id != job.id
        || collection.id != payload.collection_id
        || collection.deposit_id != payload.deposit_id
        || collection.user_id != payload.user_id
        || collection.policy != job.policy
    {
        Err(invalid(
            "collection create job differs from its durable aggregate",
        ))
    } else {
        Ok(())
    }
}

fn collection_plan(
    policy: &PaymentPolicy,
    collection_id: &CollectionId,
    deposit: &Deposit,
    requirements: Vec<WalletCollectionRequirement>,
) -> Result<(CollectionMode, Vec<CreateCollectionLeg>), DepositError> {
    if deposit.asset.asset == "native" {
        if !requirements.is_empty() {
            return Err(invalid(
                "native Ethereum collection unexpectedly requires gas prefunding",
            ));
        }
        return Ok((
            CollectionMode::AccountTransfer,
            vec![CreateCollectionLeg {
                id: stable_leg_id(collection_id, "sweep"),
                kind: CollectionLegKind::Sweep,
                planned_amount: None,
            }],
        ));
    }

    let deficit = match requirements.as_slice() {
        [] => AtomicAmount::ZERO,
        [
            WalletCollectionRequirement::NativeGasBalance {
                address, deficit, ..
            },
        ] if address == &deposit.address => *deficit,
        [_] => {
            return Err(invalid(
                "token gas requirement belongs to a different deposit address",
            ));
        }
        _ => return Err(invalid("token collection has duplicate gas requirements")),
    };
    if deficit > policy.gas_funder.maximum_funding_amount {
        return Err(invalid_state(
            "token gas deficit exceeds the active funding ceiling",
        ));
    }
    let mut legs = Vec::with_capacity(if deficit.is_zero() { 1 } else { 2 });
    if !deficit.is_zero() {
        legs.push(CreateCollectionLeg {
            id: stable_leg_id(collection_id, "gas"),
            kind: CollectionLegKind::GasFunding,
            planned_amount: Some(deficit),
        });
    }
    legs.push(CreateCollectionLeg {
        id: stable_leg_id(collection_id, "sweep"),
        kind: CollectionLegKind::Sweep,
        planned_amount: None,
    });
    Ok((CollectionMode::TokenWithGas, legs))
}

fn collection_request(
    deposit: &Deposit,
    destination: &CanonicalAddress,
    amount: AtomicAmount,
    operation_id: OperationId,
) -> Result<CollectionRequest, DepositError> {
    if deposit.asset.asset == "native" {
        Ok(CollectionRequest::Native(NativeCollectionRequest {
            operation_id,
            key_locator: deposit.key.clone(),
            from: deposit.address.clone(),
            destination: destination.clone(),
        }))
    } else {
        let token = CanonicalAddress {
            chain: deposit.asset.chain.clone(),
            value: deposit.asset.asset.clone(),
        };
        Ok(CollectionRequest::Erc20(Erc20CollectionRequest {
            operation_id,
            key_locator: deposit.key.clone(),
            token,
            from: deposit.address.clone(),
            destination: destination.clone(),
            amount: Some(amount),
        }))
    }
}

async fn another_gas_funding_transaction_is_in_flight(
    repository: &Repository,
    current_collection_id: &CollectionId,
) -> Result<bool, DepositError> {
    let mut after = None;
    let mut inspected = BTreeSet::new();
    loop {
        let page = repository
            .jobs(JobPageRequest {
                after: after.clone(),
                limit: COLLECTION_SCAN_PAGE_SIZE,
            })
            .await?;
        for job in page.jobs {
            let collection_id = match job.payload {
                JobPayload::CreateCollection(payload) => payload.collection_id,
                JobPayload::RetryCollection(payload) => payload.collection_id,
                JobPayload::CreateDeposit(_)
                | JobPayload::CloseDeposit(_)
                | JobPayload::CreateUtxoBatchCollection(_)
                | JobPayload::RetryUtxoBatchCollection(_) => continue,
            };
            if &collection_id == current_collection_id || !inspected.insert(collection_id.clone()) {
                continue;
            }
            if let Some(collection) = repository.collection(&collection_id).await?
                && collection.legs.iter().any(|leg| {
                    leg.kind == CollectionLegKind::GasFunding
                        && matches!(
                            leg.state,
                            CollectionLegState::Signed { .. }
                                | CollectionLegState::Broadcast { .. }
                        )
                })
            {
                return Ok(true);
            }
        }
        let Some(next) = page.next else {
            return Ok(false);
        };
        after = Some(next);
    }
}

fn guard(collection: &Collection, leg: &CollectionLeg) -> CollectionTransitionGuard {
    CollectionTransitionGuard {
        collection_state: collection.state,
        leg_state: leg.state.clone(),
    }
}

fn stable_operation_id(
    collection_id: &CollectionId,
    purpose: &str,
    attempt: u32,
) -> Result<OperationId, DepositError> {
    OperationId::new(format!(
        "ps-collection-{}",
        digest_hex(&[
            collection_id.0.as_bytes(),
            purpose.as_bytes(),
            &attempt.to_be_bytes(),
        ])
    ))
    .map_err(|_| invalid("failed to derive a valid custody operation ID"))
}

fn stable_leg_id(collection_id: &CollectionId, purpose: &str) -> CollectionLegId {
    CollectionLegId(format!(
        "leg-{}",
        digest_hex(&[collection_id.0.as_bytes(), purpose.as_bytes()])
    ))
}

fn watch_idempotency_key(collection_id: &CollectionId, leg_id: &CollectionLegId) -> String {
    format!(
        "ps-collection-watch-{}",
        digest_hex(&[collection_id.0.as_bytes(), leg_id.0.as_bytes()])
    )
}

fn digest_hex(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    hex::encode(digest.finalize())
}

fn index_error(error: indexing::IndexError) -> DepositError {
    let kind = if error.retryable {
        DepositErrorKind::Other
    } else {
        DepositErrorKind::InvariantViolation
    };
    DepositError {
        kind,
        message: error.message,
    }
}

fn unix_timestamp() -> Result<u64, DepositError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| invalid("system clock predates the Unix epoch"))
}

fn invalid(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}

fn invalid_state(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvalidState,
        message: message.into(),
    }
}

fn not_found(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::NotFound,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chain_identity::{AssetId, CanonicalTransactionId, ChainId};
    use deposits::{DepositId, IdempotencyKey, UserId};
    use indexing::{BlockHeight, WatchId};
    use signer::KeyLocator;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn amount(value: u64) -> AtomicAmount {
        let mut bytes = [0; 32];
        bytes[24..].copy_from_slice(&value.to_be_bytes());
        AtomicAmount(bytes)
    }

    fn deposit(asset: &str) -> Deposit {
        Deposit {
            id: DepositId("deposit-1".to_owned()),
            idempotency_key: IdempotencyKey("request-1".to_owned()),
            user_id: UserId("user-1".to_owned()),
            asset: AssetId {
                chain: ChainId("ethereum".to_owned()),
                asset: asset.to_owned(),
            },
            address: CanonicalAddress {
                chain: ChainId("ethereum".to_owned()),
                value: "0x1111111111111111111111111111111111111111".to_owned(),
            },
            key: KeyLocator::Identifier("key-1".to_owned()),
            key_purpose: "deposit".to_owned(),
            expected: amount(100),
            birthday: BlockHeight(10),
            expires_at: 1_000,
            state: DepositState::Active {
                watch_id: WatchId("watch-1".to_owned()),
            },
            created_at: 1,
        }
    }

    fn policy() -> PaymentPolicy {
        use crate::policy::{AssetPolicy, EthereumFeePolicy, GasFunderPolicy};
        use std::{collections::BTreeMap, time::Duration};
        let token = AssetId {
            chain: ChainId("ethereum".to_owned()),
            asset: "0x2222222222222222222222222222222222222222".to_owned(),
        };
        PaymentPolicy {
            version: 1,
            scope: IndexScope {
                chain: ChainId("ethereum".to_owned()),
                network: "test".to_owned(),
            },
            ethereum_chain_id: 1,
            deposit_ttl: Duration::from_secs(60),
            assets: BTreeMap::from([(
                token.clone(),
                AssetPolicy {
                    asset: token,
                    master_destination: CanonicalAddress {
                        chain: ChainId("ethereum".to_owned()),
                        value: "0x3333333333333333333333333333333333333333".to_owned(),
                    },
                    minimum_collection_amount: amount(1),
                },
            )]),
            fees: EthereumFeePolicy {
                max_fee_per_gas: amount(10),
                max_priority_fee_per_gas: amount(1),
                max_gas_limit: 100_000,
                max_total_fee: amount(1_000),
            },
            gas_funder: GasFunderPolicy {
                address: CanonicalAddress {
                    chain: ChainId("ethereum".to_owned()),
                    value: "0x4444444444444444444444444444444444444444".to_owned(),
                },
                key: KeyLocator::Identifier("gas-key".to_owned()),
                maximum_funding_amount: amount(50),
            },
            digest: [7; 32],
        }
    }

    fn fee_inspection(
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> EthereumEip1559FeeInspection {
        let max_fee_per_gas = Wei::from_u128(max_fee_per_gas);
        let maximum_total_fee = max_fee_per_gas
            .checked_mul_u64(gas_limit)
            .expect("test fee multiplication must fit");
        EthereumEip1559FeeInspection {
            chain_id: 1,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas: Wei::from_u128(max_priority_fee_per_gas),
            maximum_total_fee,
        }
    }

    fn hello_transaction_id() -> CanonicalTransactionId {
        CanonicalTransactionId {
            chain: ChainId("ethereum".to_owned()),
            value: "0x1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8".to_owned(),
        }
    }

    fn hello_envelope() -> SignedEnvelopeBytes {
        SignedEnvelopeBytes::new(b"hello".to_vec()).expect("test envelope must be non-empty")
    }

    struct RecoveryWallet {
        receipt_calls: AtomicUsize,
        broadcast_calls: AtomicUsize,
        receipt_visible_after: usize,
    }

    impl RecoveryWallet {
        fn new(receipt_visible_after: usize) -> Self {
            Self {
                receipt_calls: AtomicUsize::new(0),
                broadcast_calls: AtomicUsize::new(0),
                receipt_visible_after,
            }
        }
    }

    impl BroadcastRecoveryWallet for RecoveryWallet {
        fn broadcast<'a>(
            &'a self,
            transaction_id: &'a CanonicalTransactionId,
            envelope: &'a SignedEnvelopeBytes,
        ) -> BoxFuture<'a, Result<CanonicalTransactionId, DepositError>> {
            Box::pin(async move {
                assert_eq!(transaction_id, &hello_transaction_id());
                assert_eq!(envelope.as_bytes(), b"hello");
                self.broadcast_calls.fetch_add(1, Ordering::SeqCst);
                Err(DepositError {
                    kind: DepositErrorKind::Other,
                    message: "broadcast response was lost".to_owned(),
                })
            })
        }

        fn receipt<'a>(
            &'a self,
            transaction_id: &'a CanonicalTransactionId,
        ) -> BoxFuture<'a, Result<Option<WalletReceipt>, DepositError>> {
            Box::pin(async move {
                assert_eq!(transaction_id, &hello_transaction_id());
                let call = self.receipt_calls.fetch_add(1, Ordering::SeqCst);
                Ok((call >= self.receipt_visible_after).then(|| WalletReceipt {
                    transaction_id: transaction_id.clone(),
                    included_in: None,
                    succeeded: None,
                    confirmations: 0,
                }))
            })
        }
    }

    #[tokio::test]
    async fn broadcast_recovery_uses_receipt_before_and_after_a_lost_response() {
        let known_wallet = RecoveryWallet::new(0);
        assert_eq!(
            broadcast_or_recover(&known_wallet, &hello_transaction_id(), &hello_envelope())
                .await
                .expect("existing receipt must recover without broadcast"),
            hello_transaction_id()
        );
        assert_eq!(known_wallet.receipt_calls.load(Ordering::SeqCst), 1);
        assert_eq!(known_wallet.broadcast_calls.load(Ordering::SeqCst), 0);

        let ambiguous_wallet = RecoveryWallet::new(1);
        assert_eq!(
            broadcast_or_recover(
                &ambiguous_wallet,
                &hello_transaction_id(),
                &hello_envelope(),
            )
            .await
            .expect("receipt after response loss must prove exact submission"),
            hello_transaction_id()
        );
        assert_eq!(ambiguous_wallet.broadcast_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ambiguous_wallet.receipt_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn eip1559_fee_policy_accepts_exact_ceilings_and_rejects_each_excess() {
        let policy = policy();
        validate_eip1559_fee_policy(&policy, &fee_inspection(100, 10, 1))
            .expect("exact fee ceilings must be accepted");

        let mut wrong_chain = fee_inspection(100, 10, 1);
        wrong_chain.chain_id = 2;
        let cases = [
            (wrong_chain, "chain ID"),
            (fee_inspection(100_001, 1, 1), "gas limit"),
            (fee_inspection(1, 11, 1), "maximum fee per gas"),
            (fee_inspection(1, 10, 2), "priority fee per gas"),
            (fee_inspection(101, 10, 1), "maximum total fee"),
        ];
        for (inspection, expected_message) in cases {
            let error = validate_eip1559_fee_policy(&policy, &inspection)
                .expect_err("fee field above its policy ceiling must fail");
            assert_eq!(error.kind, DepositErrorKind::InvariantViolation);
            assert!(error.message.contains(expected_message));
        }
    }

    #[test]
    fn fee_policy_rechecks_the_chain_owned_total() {
        let policy = policy();
        let mut inspection = fee_inspection(100, 10, 1);
        inspection.maximum_total_fee = Wei::from_u128(999);
        let error = validate_eip1559_fee_policy(&policy, &inspection)
            .expect_err("inconsistent maximum total fee must fail");
        assert!(error.message.contains("inconsistent maximum total fee"));
    }

    #[test]
    fn both_prepared_result_types_reject_malformed_envelopes_and_hash_mismatches() {
        let policy = policy();
        let transaction = PreparedTransaction {
            transaction_id: hello_transaction_id(),
            signed_envelope: hello_envelope(),
        };
        let transaction_error = validate_prepared_transaction_fees(&policy, &transaction)
            .expect_err("malformed prepared transfer envelope must fail");
        assert!(transaction_error.message.contains("malformed"));

        let collection = PreparedCollection {
            transaction_id: hello_transaction_id(),
            signed_envelope: hello_envelope(),
            attribution: Vec::new(),
        };
        let collection_error = validate_prepared_collection_fees(&policy, &collection)
            .expect_err("malformed prepared collection envelope must fail");
        assert!(collection_error.message.contains("malformed"));

        let mismatched = PreparedTransaction {
            transaction_id: CanonicalTransactionId {
                chain: ChainId("ethereum".to_owned()),
                value: format!("0x{}", "00".repeat(32)),
            },
            signed_envelope: hello_envelope(),
        };
        let mismatch_error = validate_prepared_transaction_fees(&policy, &mismatched)
            .expect_err("prepared envelope hash mismatch must fail");
        assert!(mismatch_error.message.contains("expected transaction ID"));
    }

    #[test]
    fn token_plan_durably_captures_gas_deficit_before_sweep() {
        let deposit = deposit("0x2222222222222222222222222222222222222222");
        let collection_id = CollectionId("collection-1".to_owned());
        let (mode, legs) = collection_plan(
            &policy(),
            &collection_id,
            &deposit,
            vec![WalletCollectionRequirement::NativeGasBalance {
                address: deposit.address.clone(),
                current: amount(2),
                required: amount(12),
                deficit: amount(10),
            }],
        )
        .expect("valid token plan must be built");
        assert_eq!(mode, CollectionMode::TokenWithGas);
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].kind, CollectionLegKind::GasFunding);
        assert_eq!(legs[0].planned_amount, Some(amount(10)));
        assert_eq!(legs[1].kind, CollectionLegKind::Sweep);
        assert_eq!(legs[1].planned_amount, None);
    }

    #[test]
    fn token_plan_rejects_funding_above_policy_ceiling() {
        let deposit = deposit("0x2222222222222222222222222222222222222222");
        let error = collection_plan(
            &policy(),
            &CollectionId("collection-1".to_owned()),
            &deposit,
            vec![WalletCollectionRequirement::NativeGasBalance {
                address: deposit.address.clone(),
                current: AtomicAmount::ZERO,
                required: amount(51),
                deficit: amount(51),
            }],
        )
        .expect_err("funding above policy must fail");
        assert_eq!(error.kind, DepositErrorKind::InvalidState);
    }

    #[test]
    fn operation_and_watch_identities_are_stable_and_bounded() {
        let collection = CollectionId("collection-1".to_owned());
        let leg = stable_leg_id(&collection, "sweep");
        assert_eq!(
            stable_operation_id(&collection, "sweep", 1)
                .expect("operation ID must be valid")
                .as_str(),
            stable_operation_id(&collection, "sweep", 1)
                .expect("operation ID must be stable")
                .as_str()
        );
        assert!(watch_idempotency_key(&collection, &leg).len() < 256);
    }
}
