//! Durable native-Bitcoin collection workflow owned by Payment Service.
//!
//! IX supplies canonical UTXO facts, WS performs stateless signing/broadcast,
//! and this module owns exact-outpoint reservation, policy, attribution, and
//! crash recovery. It never treats a missing receipt as proof that a signed
//! input is safe to release or sign again.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::SystemTime,
};

use chain_bitcoin::{
    BitcoinAddress, BitcoinNetwork, BitcoinOutPoint, BitcoinTransactionId, Satoshi,
};
use chain_identity::{AtomicAmount, CanonicalTransactionId, ChainId};
use deposits::{
    AcceptCollectionBroadcast, AttachCollectionWatch, BoxFuture, Collection, CollectionAllocation,
    CollectionId, CollectionLeg, CollectionLegId, CollectionLegKind, CollectionLegState,
    CollectionSpendResource, CollectionSpendResourceEvidence, CollectionSpendResourceId,
    CollectionState, CollectionStore, CollectionTransitionGuard, CreateCollectionLeg,
    CreateUtxoBatchCollection, CreateUtxoBatchParticipant, Deposit, DepositError, DepositErrorKind,
    DepositId, DepositIndexerClient, DepositLedger, DepositState, DepositStore, Job, JobPayload,
    LedgerEntryId, MAX_COLLECTION_SPEND_RESOURCES, PersistentPaymentRepository,
    ReconciliationStore, RecordSignedCollectionLeg, RetryCollectionLeg, SignedEnvelopeBytes,
};
use indexing::{IndexScope, WatchRequest, WatchSelector};
use sha2::{Digest, Sha256};
use signer::OperationId;
use storage_rocksdb::RocksDbStorage;

use crate::{
    bitcoin_fee_allocation::{BitcoinFeeAllocationInput, allocate_bitcoin_fee},
    bitcoin_policy::BitcoinPaymentPolicy,
    bitcoin_wallet_client::{
        BitcoinCollectionInput, BitcoinPreparedCollection, BitcoinSignCollectionRequest,
        BitcoinWalletClient, BitcoinWalletCollectionSource, BitcoinWalletReceipt,
    },
    indexer_client::{BitcoinUtxo, BitcoinUtxoSnapshot, IndexerClient},
};

type Repository = PersistentPaymentRepository<RocksDbStorage>;

trait BitcoinBroadcastGateway: Send + Sync {
    fn broadcast<'a>(
        &'a self,
        transaction_id: BitcoinTransactionId,
        bytes: &'a SignedEnvelopeBytes,
    ) -> BoxFuture<'a, Result<BitcoinTransactionId, DepositError>>;

    fn receipt<'a>(
        &'a self,
        transaction_id: BitcoinTransactionId,
    ) -> BoxFuture<'a, Result<Option<BitcoinWalletReceipt>, DepositError>>;
}

impl BitcoinBroadcastGateway for BitcoinWalletClient {
    fn broadcast<'a>(
        &'a self,
        transaction_id: BitcoinTransactionId,
        bytes: &'a SignedEnvelopeBytes,
    ) -> BoxFuture<'a, Result<BitcoinTransactionId, DepositError>> {
        Box::pin(async move { BitcoinWalletClient::broadcast(self, transaction_id, bytes).await })
    }

    fn receipt<'a>(
        &'a self,
        transaction_id: BitcoinTransactionId,
    ) -> BoxFuture<'a, Result<Option<BitcoinWalletReceipt>, DepositError>> {
        Box::pin(async move { BitcoinWalletClient::receipt(self, transaction_id).await })
    }
}

const BITCOIN_CHAIN: &str = "bitcoin";
const COINBASE_MATURITY: u64 = 100;
const IX_UTXO_PAGE_SIZE: usize = 1_000;
const MAX_UTXO_PAGES_PER_ADDRESS: usize =
    MAX_COLLECTION_SPEND_RESOURCES.div_ceil(IX_UTXO_PAGE_SIZE) + 1;
const MAX_RESERVATION_SELECTION_ATTEMPTS: usize = 3;
const RBF_SEQUENCE_NO_LOCKTIME: u32 = 0xffff_fffd;
const EVIDENCE_VERSION: u8 = 1;
const EVIDENCE_MAGIC: &[u8; 8] = b"btcpsutx";
const MAX_EVIDENCE_ADDRESS_BYTES: usize = 128;
const MAX_EVIDENCE_SCRIPT_BYTES: usize = 128;

#[derive(Clone)]
struct SelectedParticipant {
    deposit: Deposit,
    expected_ledger_head: LedgerEntryId,
    inputs: Vec<BitcoinUtxo>,
    gross: Satoshi,
}

struct BitcoinSpendEvidenceV1 {
    transaction_id: BitcoinTransactionId,
    output_index: u32,
    value: Satoshi,
    script_pubkey: Vec<u8>,
    address: String,
    created_height: u64,
    coinbase: bool,
    observed_confirmations: u64,
    snapshot_height: u64,
}

pub(crate) async fn process_bitcoin_collection_job(
    repository: &Repository,
    indexer: &IndexerClient,
    wallet: &BitcoinWalletClient,
    policy: &BitcoinPaymentPolicy,
    scope: &IndexScope,
    job: &Job,
) -> Result<(), DepositError> {
    let collection = match &job.payload {
        JobPayload::CreateUtxoBatchCollection(payload) => {
            ensure_or_create_bitcoin_batch(
                repository,
                indexer,
                policy,
                job,
                &payload.collection_id,
                &payload.deposit_ids,
            )
            .await?
        }
        JobPayload::RetryUtxoBatchCollection(payload) => {
            prepare_bitcoin_retry(
                repository,
                job,
                &payload.collection_id,
                &payload.deposit_ids,
            )
            .await?
        }
        JobPayload::CreateDeposit(_)
        | JobPayload::CloseDeposit(_)
        | JobPayload::CreateCollection(_)
        | JobPayload::RetryCollection(_) => {
            return Err(invalid(
                "Bitcoin collection worker received a non-Bitcoin collection job",
            ));
        }
    };
    drive_bitcoin_batch(repository, indexer, wallet, policy, scope, collection).await
}

async fn prepare_bitcoin_retry(
    repository: &Repository,
    job: &Job,
    collection_id: &CollectionId,
    deposit_ids: &[DepositId],
) -> Result<Collection, DepositError> {
    let mut collection = repository
        .collection(collection_id)
        .await?
        .ok_or_else(|| not_found("Bitcoin retry collection does not exist"))?;
    validate_existing_batch(job, collection_id, deposit_ids, &collection)?;
    if collection.state == CollectionState::Completed {
        return Ok(collection);
    }
    if matches!(
        collection.state,
        CollectionState::Failed | CollectionState::Reorged
    ) {
        let leg = collection
            .legs
            .first()
            .cloned()
            .ok_or_else(|| invalid("Bitcoin retry collection has no sweep leg"))?;
        if collection.legs.len() != 1
            || !matches!(
                leg.state,
                CollectionLegState::Failed { .. } | CollectionLegState::Reorged { .. }
            )
        {
            return Err(invalid(
                "Bitcoin retry collection has no retained terminal transaction",
            ));
        }
        collection = repository
            .retry_leg(RetryCollectionLeg {
                collection_id: collection.id.clone(),
                leg_id: leg.id.clone(),
                expected: guard(&collection, &leg),
                updated_at: unix_timestamp()?,
            })
            .await?;
    }
    Ok(collection)
}

pub(crate) async fn ensure_or_create_bitcoin_batch(
    repository: &Repository,
    indexer: &IndexerClient,
    policy: &BitcoinPaymentPolicy,
    job: &Job,
    collection_id: &CollectionId,
    deposit_ids: &[DepositId],
) -> Result<Collection, DepositError> {
    if let Some(existing) = repository.collection(collection_id).await? {
        validate_existing_batch(job, collection_id, deposit_ids, &existing)?;
        return Ok(existing);
    }
    if deposit_ids.is_empty() || deposit_ids.len() > policy.maximum_deposits {
        return Err(invalid_state(
            "Bitcoin collection deposit count violates the active policy",
        ));
    }
    let canonical_ids = canonical_deposit_ids(deposit_ids)?;
    if canonical_ids != deposit_ids {
        return Err(invalid(
            "Bitcoin collection job deposit IDs are not in canonical order",
        ));
    }

    for selection_attempt in 1..=MAX_RESERVATION_SELECTION_ATTEMPTS {
        if let Some(owner) = retained_batch_owner(repository, &canonical_ids, &policy.asset).await?
        {
            if owner.id == *collection_id {
                validate_existing_batch(job, collection_id, deposit_ids, &owner)?;
                return Ok(owner);
            }
            return Err(foreign_retained_reservation());
        }
        let (participants, snapshot) =
            select_participants(repository, indexer, policy, &canonical_ids).await?;
        let total_inputs = participants
            .iter()
            .try_fold(0_usize, |total, participant| {
                total.checked_add(participant.inputs.len())
            })
            .ok_or_else(|| invalid("Bitcoin collection input count overflowed"))?;
        if total_inputs == 0 || total_inputs > policy.maximum_inputs {
            return Err(invalid_state(
                "Bitcoin collection input count violates the active policy",
            ));
        }

        let mut create_participants = Vec::with_capacity(participants.len());
        for participant in participants {
            let reservation_amount = atomic_from_satoshis(participant.gross);
            let mut spend_resources = Vec::with_capacity(participant.inputs.len());
            for output in participant.inputs {
                spend_resources.push(CollectionSpendResource {
                    id: CollectionSpendResourceId {
                        transaction_id: canonical_transaction_id(output.outpoint.transaction_id),
                        output_index: output.outpoint.output_index,
                    },
                    amount: atomic_from_satoshis(output.value),
                    evidence: encode_evidence(&output, &snapshot)?,
                });
            }
            create_participants.push(CreateUtxoBatchParticipant {
                user_id: participant.deposit.user_id,
                deposit_id: participant.deposit.id,
                expected_ledger_head: participant.expected_ledger_head,
                reservation_amount,
                spend_resources,
            });
        }

        let outcome = repository
            .create_or_replay_utxo_batch(CreateUtxoBatchCollection {
                id: collection_id.clone(),
                job_id: job.id.clone(),
                asset: policy.asset.clone(),
                destination: policy.master_destination.clone(),
                policy: job.policy.clone(),
                participants: create_participants,
                leg: CreateCollectionLeg {
                    id: stable_leg_id(collection_id),
                    kind: CollectionLegKind::Sweep,
                    planned_amount: None,
                },
                created_at: job.created_at,
            })
            .await;
        match outcome {
            Ok(outcome) => return Ok(outcome.collection().clone()),
            Err(error) if error.kind == DepositErrorKind::Conflict => {
                // A concurrent exact-outpoint winner may have been this same
                // idempotent collection. Reload it before selecting again.
                if let Some(existing) = repository.collection(collection_id).await? {
                    validate_existing_batch(job, collection_id, deposit_ids, &existing)?;
                    return Ok(existing);
                }
                if let Some(owner) =
                    retained_batch_owner(repository, &canonical_ids, &policy.asset).await?
                {
                    if owner.id == *collection_id {
                        validate_existing_batch(job, collection_id, deposit_ids, &owner)?;
                        return Ok(owner);
                    }
                    return Err(foreign_retained_reservation());
                }
                if !should_reselect_after_reservation_conflict(&error, selection_attempt) {
                    return Err(invalid_state(
                        "Bitcoin reservation state changed throughout bounded reselection; retry from a fresh job attempt",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(invalid(
        "Bitcoin reservation selection loop exited without an outcome",
    ))
}

async fn retained_batch_owner(
    repository: &Repository,
    deposit_ids: &[DepositId],
    asset: &chain_identity::AssetId,
) -> Result<Option<Collection>, DepositError> {
    for deposit_id in deposit_ids {
        if let Some(owner) = repository
            .retained_collection_for(deposit_id, asset)
            .await?
        {
            if owner.mode != deposits::CollectionMode::UtxoBatch
                || owner.participant(deposit_id).is_none()
                || &owner.asset != asset
            {
                return Err(invalid(
                    "Bitcoin retained reservation index points to an incompatible collection",
                ));
            }
            return Ok(Some(owner));
        }
    }
    Ok(None)
}

fn foreign_retained_reservation() -> DepositError {
    invalid(
        "Bitcoin Payment Service v1 permits only one collection aggregate per deposit; this deposit is already owned by another retained UTXO collection",
    )
}

fn should_reselect_after_reservation_conflict(
    error: &DepositError,
    selection_attempt: usize,
) -> bool {
    error.kind == DepositErrorKind::Conflict
        && selection_attempt < MAX_RESERVATION_SELECTION_ATTEMPTS
}

async fn select_participants(
    repository: &Repository,
    indexer: &IndexerClient,
    policy: &BitcoinPaymentPolicy,
    deposit_ids: &[DepositId],
) -> Result<(Vec<SelectedParticipant>, BitcoinUtxoSnapshot), DepositError> {
    let mut participants = Vec::with_capacity(deposit_ids.len());
    let mut expected_snapshot = None;
    let mut all_outpoints = BTreeSet::new();
    let mut total_inputs = 0_usize;

    for deposit_id in deposit_ids {
        let deposit = required_bitcoin_deposit(repository, policy, deposit_id).await?;
        if repository.automatic_actions_blocked(&deposit.id).await? {
            return Err(invalid_state(
                "Bitcoin collection is blocked by an unresolved reconciliation case",
            ));
        }
        let address = BitcoinAddress::parse_for_network(&deposit.address.value, policy.network)
            .map_err(|_| invalid("durable Bitcoin deposit address is invalid for its policy"))?;
        let (mut inputs, snapshot) = fetch_all_utxos(
            indexer,
            &policy.scope,
            &address,
            policy.minimum_spend_confirmations,
            policy.maximum_inputs,
        )
        .await?;
        match &expected_snapshot {
            Some(expected) if expected != &snapshot => {
                return Err(retryable_utxo_view_change(
                    "Bitcoin UTXO pages moved between batch source addresses; retry selection",
                ));
            }
            None => expected_snapshot = Some(snapshot),
            Some(_) => {}
        }
        inputs.sort_by(canonical_outpoint_order);
        for pair in inputs.windows(2) {
            if pair[0].outpoint == pair[1].outpoint {
                return Err(invalid("Bitcoin IX returned a duplicate UTXO"));
            }
        }
        for output in &inputs {
            validate_owned_output(&deposit, policy.network, output)?;
            if !all_outpoints.insert(output.outpoint) {
                return Err(invalid(
                    "Bitcoin IX returned one outpoint for more than one deposit",
                ));
            }
        }
        total_inputs = total_inputs
            .checked_add(inputs.len())
            .ok_or_else(|| invalid("Bitcoin collection input count overflowed"))?;
        if total_inputs > policy.maximum_inputs {
            return Err(invalid_state(
                "Bitcoin collection exceeds the active maximum input count",
            ));
        }
        let gross = inputs.iter().try_fold(0_u64, |total, output| {
            total
                .checked_add(output.value.0)
                .ok_or_else(|| invalid("Bitcoin collection gross input overflowed u64"))
        })?;
        if inputs.is_empty() || gross < policy.minimum_collection.0 {
            return Err(invalid_state(
                "a Bitcoin deposit has no full eligible UTXO set at the collection minimum",
            ));
        }
        let ledger = repository
            .current(&deposit.id)
            .await?
            .ok_or_else(|| invalid("Bitcoin collection deposit has no ledger head"))?;
        if ledger.balances.balance < atomic_from_satoshis(Satoshi(gross)) {
            return Err(conflict(
                "Bitcoin IX UTXO selection is ahead of the PS deposit projection",
            ));
        }
        participants.push(SelectedParticipant {
            deposit,
            expected_ledger_head: ledger.id,
            inputs,
            gross: Satoshi(gross),
        });
    }
    let snapshot = expected_snapshot
        .ok_or_else(|| invalid("Bitcoin collection selection has no IX snapshot"))?;
    Ok((participants, snapshot))
}

async fn fetch_all_utxos(
    indexer: &IndexerClient,
    scope: &IndexScope,
    address: &BitcoinAddress,
    minimum_confirmations: u64,
    maximum_inputs: usize,
) -> Result<(Vec<BitcoinUtxo>, BitcoinUtxoSnapshot), DepositError> {
    let mut outputs = Vec::new();
    let mut after = None;
    let mut snapshot = None;
    let mut seen_cursors = BTreeSet::new();
    let mut facts_seen = 0_usize;
    let mut pages_seen = 0_usize;
    loop {
        pages_seen = pages_seen
            .checked_add(1)
            .ok_or_else(|| invalid("Bitcoin IX UTXO page count overflowed"))?;
        if pages_seen > MAX_UTXO_PAGES_PER_ADDRESS {
            return Err(invalid_state(
                "Bitcoin address UTXO pagination exceeds the PS work bound",
            ));
        }
        let page = indexer
            .bitcoin_utxos(scope, address, after.as_deref(), IX_UTXO_PAGE_SIZE)
            .await
            .map_err(index_error)?;
        match &snapshot {
            Some(expected) if expected != &page.snapshot => {
                return Err(retryable_utxo_view_change(
                    "Bitcoin IX UTXO pagination crossed a canonical snapshot",
                ));
            }
            None => snapshot = Some(page.snapshot.clone()),
            Some(_) => {}
        }
        facts_seen = facts_seen
            .checked_add(page.outputs.len())
            .ok_or_else(|| invalid("Bitcoin IX UTXO fact count overflowed"))?;
        if facts_seen > MAX_COLLECTION_SPEND_RESOURCES {
            return Err(invalid_state(
                "Bitcoin address UTXO fact count exceeds the PS work bound",
            ));
        }
        outputs.extend(
            page.outputs
                .into_iter()
                .filter(|output| eligible(output, minimum_confirmations)),
        );
        if outputs.len() > maximum_inputs {
            return Err(invalid_state(
                "Bitcoin address UTXO count exceeds the active input limit",
            ));
        }
        match page.next {
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    return Err(retryable_utxo_view_change(
                        "Bitcoin IX repeated a UTXO continuation cursor",
                    ));
                }
                after = Some(next);
            }
            None => break,
        }
    }
    Ok((
        outputs,
        snapshot.ok_or_else(|| invalid("Bitcoin IX returned no UTXO snapshot"))?,
    ))
}

const fn eligible(output: &BitcoinUtxo, minimum_confirmations: u64) -> bool {
    output.confirmations >= minimum_confirmations
        && (!output.coinbase || output.confirmations >= COINBASE_MATURITY)
}

fn validate_owned_output(
    deposit: &Deposit,
    network: BitcoinNetwork,
    output: &BitcoinUtxo,
) -> Result<(), DepositError> {
    if output.address.0 != deposit.address.value || output.value.0 == 0 {
        return Err(invalid(
            "Bitcoin IX UTXO ownership or value differs from its durable deposit",
        ));
    }
    let expected = BitcoinAddress::parse_for_network(&deposit.address.value, network)
        .and_then(|address| address.script_pubkey_for_network(network))
        .map_err(|_| invalid("durable Bitcoin deposit script cannot be derived"))?;
    if expected.as_bytes() != output.script_pubkey {
        return Err(invalid(
            "Bitcoin IX UTXO script does not match its durable deposit address",
        ));
    }
    Ok(())
}

async fn required_bitcoin_deposit(
    repository: &Repository,
    policy: &BitcoinPaymentPolicy,
    deposit_id: &DepositId,
) -> Result<Deposit, DepositError> {
    let deposit = repository
        .deposit(deposit_id)
        .await?
        .ok_or_else(|| not_found("Bitcoin collection deposit does not exist"))?;
    if !collection_eligible_deposit_state(&deposit.state)
        || deposit.asset != policy.asset
        || deposit.address.chain.0 != BITCOIN_CHAIN
    {
        return Err(invalid_state(
            "Bitcoin collection requires observed active, expired, or closed native-BTC deposits from this policy scope",
        ));
    }
    repository
        .current(&deposit.id)
        .await?
        .ok_or_else(|| invalid("Bitcoin collection deposit has no absolute ledger head"))?;
    Ok(deposit)
}

fn encode_evidence(
    output: &BitcoinUtxo,
    snapshot: &BitcoinUtxoSnapshot,
) -> Result<CollectionSpendResourceEvidence, DepositError> {
    let address = output.address.0.as_bytes();
    if address.is_empty() || address.len() > MAX_EVIDENCE_ADDRESS_BYTES {
        return Err(invalid(
            "Bitcoin spend-resource address exceeds the evidence bound",
        ));
    }
    if output.script_pubkey.is_empty() || output.script_pubkey.len() > MAX_EVIDENCE_SCRIPT_BYTES {
        return Err(invalid(
            "Bitcoin spend-resource script exceeds the evidence bound",
        ));
    }
    let address_len = u16::try_from(address.len())
        .map_err(|_| invalid("Bitcoin spend-resource address length overflowed"))?;
    let script_len = u16::try_from(output.script_pubkey.len())
        .map_err(|_| invalid("Bitcoin spend-resource script length overflowed"))?;
    let mut bytes = Vec::with_capacity(256);
    bytes.extend_from_slice(EVIDENCE_MAGIC);
    bytes.push(EVIDENCE_VERSION);
    bytes.extend_from_slice(&output.outpoint.transaction_id.0);
    bytes.extend_from_slice(&output.outpoint.output_index.to_be_bytes());
    bytes.extend_from_slice(&output.value.0.to_be_bytes());
    bytes.extend_from_slice(&output.created_height.0.to_be_bytes());
    bytes.push(u8::from(output.coinbase));
    bytes.extend_from_slice(&output.confirmations.to_be_bytes());
    bytes.extend_from_slice(&snapshot.generation.to_be_bytes());
    bytes.extend_from_slice(&snapshot.revision.to_be_bytes());
    bytes.extend_from_slice(&snapshot.checkpoint.height.0.to_be_bytes());
    bytes.extend_from_slice(&snapshot.checkpoint.hash.0);
    bytes.extend_from_slice(&address_len.to_be_bytes());
    bytes.extend_from_slice(address);
    bytes.extend_from_slice(&script_len.to_be_bytes());
    bytes.extend_from_slice(&output.script_pubkey);
    CollectionSpendResourceEvidence::new(bytes)
}

fn decode_evidence(
    resource: &CollectionSpendResource,
    deposit: &Deposit,
    policy: &BitcoinPaymentPolicy,
) -> Result<BitcoinCollectionInput, DepositError> {
    let evidence = decode_evidence_v1(resource.evidence.as_bytes())?;
    if resource.id.transaction_id.chain.0 != BITCOIN_CHAIN
        || evidence.transaction_id.to_string() != resource.id.transaction_id.value
        || evidence.output_index != resource.id.output_index
        || atomic_from_satoshis(evidence.value) != resource.amount
        || evidence.address != deposit.address.value
    {
        return Err(invalid(
            "Bitcoin spend-resource evidence differs from its durable reservation",
        ));
    }
    let expected_confirmations = evidence
        .snapshot_height
        .checked_sub(evidence.created_height)
        .and_then(|distance| distance.checked_add(1))
        .ok_or_else(|| invalid("Bitcoin spend-resource snapshot height is invalid"))?;
    if evidence.observed_confirmations != expected_confirmations
        || evidence.observed_confirmations < policy.minimum_spend_confirmations
        || (evidence.coinbase && evidence.observed_confirmations < COINBASE_MATURITY)
    {
        return Err(invalid(
            "Bitcoin spend-resource confirmation evidence violates policy",
        ));
    }
    let address = BitcoinAddress::parse_for_network(&evidence.address, policy.network)
        .map_err(|_| invalid("Bitcoin reserved input address is invalid"))?;
    if address.0 != evidence.address {
        return Err(invalid("Bitcoin reserved input address is not canonical"));
    }
    let expected_script = address
        .script_pubkey_for_network(policy.network)
        .map_err(|_| invalid("Bitcoin reserved input script cannot be derived"))?;
    if expected_script.as_bytes() != evidence.script_pubkey {
        return Err(invalid(
            "Bitcoin reserved input script does not match its address",
        ));
    }
    Ok(BitcoinCollectionInput {
        outpoint: BitcoinOutPoint {
            transaction_id: evidence.transaction_id,
            output_index: evidence.output_index,
        },
        value: evidence.value,
        script_pubkey: evidence.script_pubkey,
    })
}

fn decode_evidence_v1(bytes: &[u8]) -> Result<BitcoinSpendEvidenceV1, DepositError> {
    let mut reader = EvidenceReader::new(bytes);
    if reader.take_array::<8>()? != *EVIDENCE_MAGIC || reader.take_u8()? != EVIDENCE_VERSION {
        return Err(invalid(
            "Bitcoin spend-resource evidence version is invalid",
        ));
    }
    let transaction_id = BitcoinTransactionId(reader.take_array::<32>()?);
    let output_index = reader.take_u32()?;
    let value = Satoshi(reader.take_u64()?);
    let created_height = reader.take_u64()?;
    let coinbase = match reader.take_u8()? {
        0 => false,
        1 => true,
        _ => return Err(invalid("Bitcoin spend-resource coinbase flag is invalid")),
    };
    let observed_confirmations = reader.take_u64()?;
    let _snapshot_generation = reader.take_u64()?;
    let _snapshot_revision = reader.take_u64()?;
    let snapshot_height = reader.take_u64()?;
    let _snapshot_hash = reader.take_array::<32>()?;
    let address_len = usize::from(reader.take_u16()?);
    if address_len == 0 || address_len > MAX_EVIDENCE_ADDRESS_BYTES {
        return Err(invalid("Bitcoin spend-resource address length is invalid"));
    }
    let address = std::str::from_utf8(reader.take(address_len)?)
        .map_err(|_| invalid("Bitcoin spend-resource address is not UTF-8"))?
        .to_owned();
    let script_len = usize::from(reader.take_u16()?);
    if script_len == 0 || script_len > MAX_EVIDENCE_SCRIPT_BYTES {
        return Err(invalid("Bitcoin spend-resource script length is invalid"));
    }
    let script_pubkey = reader.take(script_len)?.to_vec();
    if !reader.is_empty() || value.0 == 0 {
        return Err(invalid(
            "Bitcoin spend-resource evidence has invalid trailing data",
        ));
    }
    Ok(BitcoinSpendEvidenceV1 {
        transaction_id,
        output_index,
        value,
        script_pubkey,
        address,
        created_height,
        coinbase,
        observed_confirmations,
        snapshot_height,
    })
}

struct EvidenceReader<'a> {
    remaining: &'a [u8],
}

impl<'a> EvidenceReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], DepositError> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(length)
            .ok_or_else(|| invalid("Bitcoin spend-resource evidence is truncated"))?;
        self.remaining = remaining;
        Ok(value)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], DepositError> {
        self.take(N)?
            .try_into()
            .map_err(|_| invalid("Bitcoin spend-resource evidence field is malformed"))
    }

    fn take_u8(&mut self) -> Result<u8, DepositError> {
        self.take_array::<1>().map(|bytes| bytes[0])
    }

    fn take_u16(&mut self) -> Result<u16, DepositError> {
        self.take_array::<2>().map(u16::from_be_bytes)
    }

    fn take_u32(&mut self) -> Result<u32, DepositError> {
        self.take_array::<4>().map(u32::from_be_bytes)
    }

    fn take_u64(&mut self) -> Result<u64, DepositError> {
        self.take_array::<8>().map(u64::from_be_bytes)
    }

    const fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }
}

pub(crate) async fn drive_bitcoin_batch(
    repository: &Repository,
    indexer: &IndexerClient,
    wallet: &BitcoinWalletClient,
    policy: &BitcoinPaymentPolicy,
    scope: &IndexScope,
    mut collection: Collection,
) -> Result<(), DepositError> {
    if collection.mode != deposits::CollectionMode::UtxoBatch
        || collection.asset != policy.asset
        || collection.destination != policy.master_destination
    {
        return Err(invalid(
            "durable Bitcoin collection differs from the active policy",
        ));
    }
    loop {
        match collection.state {
            CollectionState::Completed => return Ok(()),
            CollectionState::Failed | CollectionState::Reorged => {
                return Err(invalid_state(
                    "Bitcoin collection requires explicit same-transaction recovery",
                ));
            }
            CollectionState::Required | CollectionState::InProgress => {}
        }
        let leg = collection
            .legs
            .first()
            .cloned()
            .ok_or_else(|| invalid("Bitcoin collection has no sweep leg"))?;
        if collection.legs.len() != 1 || leg.kind != CollectionLegKind::Sweep {
            return Err(invalid(
                "Bitcoin collection must contain exactly one sweep leg",
            ));
        }
        match &leg.state {
            CollectionLegState::Required => {
                collection = sign_and_persist(repository, wallet, policy, collection, leg).await?;
            }
            CollectionLegState::Signed { transaction_id } => {
                let envelope = required_envelope(repository, &collection, &leg).await?;
                if &envelope.expected_transaction_id != transaction_id {
                    return Err(invalid(
                        "Bitcoin signed leg transaction ID differs from its envelope",
                    ));
                }
                let bitcoin_id = parse_canonical_transaction_id(transaction_id)?;
                let accepted = broadcast_or_recover(wallet, bitcoin_id, &envelope.bytes).await?;
                collection = repository
                    .accept_broadcast(AcceptCollectionBroadcast {
                        collection_id: collection.id.clone(),
                        leg_id: leg.id.clone(),
                        expected: guard(&collection, &leg),
                        transaction_id: canonical_transaction_id(accepted),
                        accepted_at: unix_timestamp()?,
                    })
                    .await?;
            }
            CollectionLegState::Broadcast { transaction_id } => {
                if leg.watch_id.is_none() {
                    let birthday = minimum_birthday(repository, &collection).await?;
                    let receipt = indexer
                        .watch(WatchRequest {
                            scope: scope.clone(),
                            selector: WatchSelector::Transaction(transaction_id.clone()),
                            start_height: birthday,
                            idempotency_key: watch_idempotency_key(&collection.id, &leg.id),
                        })
                        .await
                        .map_err(index_error)?;
                    repository
                        .attach_watch(AttachCollectionWatch {
                            collection_id: collection.id.clone(),
                            leg_id: leg.id.clone(),
                            expected: guard(&collection, &leg),
                            watch_id: receipt.id,
                            updated_at: unix_timestamp()?,
                        })
                        .await?;
                }
                return Err(invalid_state(
                    "Bitcoin collection is awaiting an IX confirmation or reorg fact",
                ));
            }
            CollectionLegState::Confirmed { .. } => {
                collection = repository
                    .collection(&collection.id)
                    .await?
                    .ok_or_else(|| invalid("durable Bitcoin collection disappeared"))?;
            }
            CollectionLegState::Failed { .. } | CollectionLegState::Reorged { .. } => {
                return Err(invalid_state(
                    "Bitcoin collection requires explicit same-transaction recovery",
                ));
            }
        }
    }
}

async fn sign_and_persist(
    repository: &Repository,
    wallet: &BitcoinWalletClient,
    policy: &BitcoinPaymentPolicy,
    collection: Collection,
    leg: CollectionLeg,
) -> Result<Collection, DepositError> {
    if !leg.allocations.is_empty() {
        return Err(invalid(
            "unsigned Bitcoin collection unexpectedly has fee allocations",
        ));
    }
    let attempt = leg
        .attempt_count
        .checked_add(1)
        .ok_or_else(|| invalid("Bitcoin signing attempt counter is exhausted"))?;
    let operation_id = stable_operation_id(&collection.id, attempt)?;
    let mut sources = Vec::with_capacity(collection.participants.len());
    let mut allocation_inputs = Vec::with_capacity(collection.participants.len());
    let mut gross_by_address = BTreeMap::new();
    let mut all_outpoints = BTreeSet::new();

    for participant in &collection.participants {
        let deposit = repository
            .deposit(&participant.reservation.deposit_id)
            .await?
            .ok_or_else(|| invalid("Bitcoin collection participant deposit is missing"))?;
        if deposit.user_id != participant.user_id
            || deposit.asset != collection.asset
            || !collection_eligible_deposit_state(&deposit.state)
        {
            return Err(invalid(
                "Bitcoin collection participant differs from its durable deposit",
            ));
        }
        let address = BitcoinAddress::parse_for_network(&deposit.address.value, policy.network)
            .map_err(|_| invalid("Bitcoin collection deposit address is invalid"))?;
        let mut inputs = participant
            .spend_resources
            .iter()
            .map(|resource| decode_evidence(resource, &deposit, policy))
            .collect::<Result<Vec<_>, _>>()?;
        inputs.sort_by(|left, right| {
            left.outpoint
                .transaction_id
                .to_string()
                .cmp(&right.outpoint.transaction_id.to_string())
                .then_with(|| left.outpoint.output_index.cmp(&right.outpoint.output_index))
        });
        let gross = inputs.iter().try_fold(0_u64, |total, input| {
            if !all_outpoints.insert(input.outpoint) {
                return Err(invalid("Bitcoin collection contains a duplicate outpoint"));
            }
            total
                .checked_add(input.value.0)
                .ok_or_else(|| invalid("Bitcoin collection source gross overflowed"))
        })?;
        if atomic_from_satoshis(Satoshi(gross)) != participant.reservation.amount {
            return Err(invalid(
                "Bitcoin collection resources do not sum to their reservation",
            ));
        }
        gross_by_address.insert(address.clone(), Satoshi(gross));
        allocation_inputs.push(BitcoinFeeAllocationInput {
            deposit_id: deposit.id,
            gross: participant.reservation.amount,
        });
        sources.push(BitcoinWalletCollectionSource {
            address,
            key_locator: deposit.key,
            inputs,
        });
    }
    if all_outpoints.len() > policy.maximum_inputs {
        return Err(invalid_state(
            "durable Bitcoin collection exceeds its policy input limit",
        ));
    }
    let destination =
        BitcoinAddress::parse_for_network(&policy.master_destination.value, policy.network)
            .map_err(|_| invalid("Bitcoin master destination is invalid"))?;
    let prepared = wallet
        .sign_collection(&BitcoinSignCollectionRequest {
            operation_id,
            sources,
            destination,
            fee_rate: policy.requested_fee_rate,
        })
        .await?;
    validate_prepared(
        policy,
        &collection,
        &prepared,
        &gross_by_address,
        &all_outpoints,
    )?;

    let allocated = allocate_bitcoin_fee(&allocation_inputs, prepared.fee)
        .map_err(|error| invalid(format!("Bitcoin fee allocation failed: {error}")))?;
    let allocations = allocated
        .into_iter()
        .map(|allocation| CollectionAllocation {
            deposit_id: allocation.deposit_id,
            asset: policy.asset.clone(),
            gross_debit: atomic_from_satoshis(allocation.gross),
            master_credit: atomic_from_satoshis(allocation.master_credit),
            allocated_fee_asset: policy.asset.clone(),
            allocated_fee: atomic_from_satoshis(allocation.allocated_fee),
        })
        .collect::<Vec<_>>();
    let output_total = prepared
        .inspection
        .outputs
        .iter()
        .try_fold(0_u64, |total, output| total.checked_add(output.value.0))
        .ok_or_else(|| invalid("Bitcoin signed output total overflowed"))?;
    let master_credit_total = allocations.iter().try_fold(0_u64, |total, allocation| {
        let amount = atomic_to_u64(&allocation.master_credit)
            .ok_or_else(|| invalid("Bitcoin master credit exceeds u64"))?;
        total
            .checked_add(amount)
            .ok_or_else(|| invalid("Bitcoin master credit total overflowed"))
    })?;
    if output_total != master_credit_total {
        return Err(invalid(
            "Bitcoin fee allocations do not sum to the signed master output",
        ));
    }

    repository
        .record_signed(RecordSignedCollectionLeg {
            collection_id: collection.id.clone(),
            leg_id: leg.id.clone(),
            expected: guard(&collection, &leg),
            expected_transaction_id: canonical_transaction_id(prepared.transaction_id),
            envelope: prepared.raw_transaction,
            allocations,
            signed_at: unix_timestamp()?,
            expires_at: u64::MAX,
        })
        .await
}

fn validate_prepared(
    policy: &BitcoinPaymentPolicy,
    collection: &Collection,
    prepared: &BitcoinPreparedCollection,
    expected_gross: &BTreeMap<BitcoinAddress, Satoshi>,
    expected_outpoints: &BTreeSet<BitcoinOutPoint>,
) -> Result<(), DepositError> {
    if prepared.transaction_id != prepared.inspection.transaction_id
        || prepared.inspection.version != 2
        || prepared.inspection.lock_time != 0
        || prepared.inspection.virtual_size == 0
        || prepared.inspection.inputs.len() != expected_outpoints.len()
        || prepared.inspection.outputs.len() != 1
        || prepared.inspection.outputs[0].output_index != 0
    {
        return Err(invalid(
            "Bitcoin signed collection has unexpected transaction structure",
        ));
    }
    let actual_outpoints = prepared
        .inspection
        .inputs
        .iter()
        .map(|input| {
            if input.sequence != RBF_SEQUENCE_NO_LOCKTIME {
                return Err(invalid(
                    "Bitcoin signed collection input sequence violates v1 policy",
                ));
            }
            Ok(input.outpoint)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let actual_outpoint_set = actual_outpoints.iter().copied().collect::<BTreeSet<_>>();
    if &actual_outpoint_set != expected_outpoints {
        return Err(invalid(
            "Bitcoin signed collection inputs differ from exact reservations",
        ));
    }
    let mut canonical_outpoints = expected_outpoints.iter().copied().collect::<Vec<_>>();
    canonical_outpoints.sort_by(canonical_bitcoin_outpoint_order);
    if actual_outpoints != canonical_outpoints {
        return Err(invalid(
            "Bitcoin signed collection inputs are not in canonical (txid, vout) order",
        ));
    }
    if prepared.fee.0 > policy.maximum_absolute_fee.0 {
        return Err(invalid_state(
            "Bitcoin signed collection exceeds the absolute fee ceiling",
        ));
    }
    let maximum_fee = u128::from(policy.maximum_fee_rate.satoshis_per_kvb())
        .checked_mul(u128::from(prepared.inspection.virtual_size))
        .and_then(|value| value.checked_add(999))
        .map(|value| value / 1_000)
        .ok_or_else(|| invalid("Bitcoin maximum fee-rate calculation overflowed"))?;
    if u128::from(prepared.fee.0) > maximum_fee {
        return Err(invalid_state(
            "Bitcoin signed collection exceeds the fee-rate ceiling",
        ));
    }
    let fee_weighted = u128::from(prepared.fee.0)
        .checked_mul(1_000)
        .ok_or_else(|| invalid("Bitcoin actual fee-rate calculation overflowed"))?;
    let requested_weighted = u128::from(policy.requested_fee_rate.satoshis_per_kvb())
        .checked_mul(u128::from(prepared.inspection.virtual_size))
        .ok_or_else(|| invalid("Bitcoin requested fee-rate calculation overflowed"))?;
    if fee_weighted < requested_weighted {
        return Err(invalid_state(
            "Bitcoin signed collection is below the requested fee rate",
        ));
    }
    let returned = prepared
        .attribution
        .iter()
        .map(|item| (item.address.clone(), item.gross_input))
        .collect::<BTreeMap<_, _>>();
    if prepared.attribution.len() != expected_gross.len() || &returned != expected_gross {
        return Err(invalid(
            "Bitcoin signed collection attribution differs from its reservations",
        ));
    }
    let gross_total = expected_gross.values().try_fold(0_u64, |total, value| {
        total
            .checked_add(value.0)
            .ok_or_else(|| invalid("Bitcoin collection gross input total overflowed"))
    })?;
    let output_plus_fee = prepared.inspection.outputs[0]
        .value
        .0
        .checked_add(prepared.fee.0)
        .ok_or_else(|| invalid("Bitcoin collection output and fee total overflowed"))?;
    if output_plus_fee != gross_total {
        return Err(invalid(
            "Bitcoin signed collection output and fee do not conserve reserved inputs",
        ));
    }
    let destination =
        BitcoinAddress::parse_for_network(&collection.destination.value, policy.network)
            .and_then(|address| address.script_pubkey_for_network(policy.network))
            .map_err(|_| invalid("Bitcoin collection destination script is invalid"))?;
    let minimum_output = destination.minimal_non_dust().to_sat();
    if prepared.inspection.outputs[0].value.0 < minimum_output {
        return Err(invalid_state(
            "Bitcoin signed collection master output is dust",
        ));
    }
    if prepared.inspection.outputs[0].script_pubkey != destination.as_bytes() {
        return Err(invalid(
            "Bitcoin signed collection output is not the policy master destination",
        ));
    }
    Ok(())
}

async fn required_envelope(
    repository: &Repository,
    collection: &Collection,
    leg: &CollectionLeg,
) -> Result<deposits::SignedCollectionEnvelope, DepositError> {
    repository
        .signed_envelope(&collection.id, &leg.id)
        .await?
        .ok_or_else(|| invalid("Bitcoin signed collection has no retained exact envelope"))
}

async fn broadcast_or_recover(
    wallet: &(impl BitcoinBroadcastGateway + ?Sized),
    transaction_id: BitcoinTransactionId,
    bytes: &SignedEnvelopeBytes,
) -> Result<BitcoinTransactionId, DepositError> {
    if receipt_proves_submission(wallet, transaction_id).await? {
        return Ok(transaction_id);
    }
    match wallet.broadcast(transaction_id, bytes).await {
        Ok(accepted) => Ok(accepted),
        Err(error) => {
            if receipt_proves_submission(wallet, transaction_id).await? {
                Ok(transaction_id)
            } else {
                Err(error)
            }
        }
    }
}

async fn receipt_proves_submission(
    wallet: &(impl BitcoinBroadcastGateway + ?Sized),
    transaction_id: BitcoinTransactionId,
) -> Result<bool, DepositError> {
    let Some(receipt) = wallet.receipt(transaction_id).await? else {
        return Ok(false);
    };
    if receipt.transaction_id != transaction_id {
        return Err(invalid(
            "Bitcoin receipt transaction ID differs from the signed collection",
        ));
    }
    if receipt.replaced_by.is_some() {
        return Err(invalid_state(
            "Bitcoin collection has a conflicting replacement and requires reconciliation",
        ));
    }
    // Bitcoin Core returns a receipt for both mempool and included
    // transactions. Either state proves the exact txid was submitted.
    Ok(true)
}

async fn minimum_birthday(
    repository: &Repository,
    collection: &Collection,
) -> Result<indexing::BlockHeight, DepositError> {
    let mut minimum = None;
    for participant in &collection.participants {
        let deposit = repository
            .deposit(&participant.reservation.deposit_id)
            .await?
            .ok_or_else(|| invalid("Bitcoin collection participant deposit is missing"))?;
        minimum = Some(
            minimum.map_or(deposit.birthday, |height: indexing::BlockHeight| {
                height.min(deposit.birthday)
            }),
        );
    }
    minimum.ok_or_else(|| invalid("Bitcoin collection has no participant birthday"))
}

fn validate_existing_batch(
    job: &Job,
    expected_collection_id: &CollectionId,
    expected_deposit_ids: &[DepositId],
    collection: &Collection,
) -> Result<(), DepositError> {
    let actual = collection
        .participants
        .iter()
        .map(|participant| participant.reservation.deposit_id.clone())
        .collect::<Vec<_>>();
    let create_job_id_mismatch = matches!(&job.payload, JobPayload::CreateUtxoBatchCollection(_))
        && collection.job_id != job.id;
    if &collection.id != expected_collection_id
        || create_job_id_mismatch
        || collection.policy != job.policy
        || collection.mode != deposits::CollectionMode::UtxoBatch
        || actual != expected_deposit_ids
    {
        return Err(invalid(
            "Bitcoin collection replay differs from its durable job identity",
        ));
    }
    Ok(())
}

fn canonical_deposit_ids(deposit_ids: &[DepositId]) -> Result<Vec<DepositId>, DepositError> {
    let mut canonical = deposit_ids.to_vec();
    canonical.sort();
    if canonical.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid("Bitcoin collection contains duplicate deposit IDs"));
    }
    Ok(canonical)
}

fn canonical_outpoint_order(left: &BitcoinUtxo, right: &BitcoinUtxo) -> std::cmp::Ordering {
    canonical_bitcoin_outpoint_order(&left.outpoint, &right.outpoint)
}

fn canonical_bitcoin_outpoint_order(
    left: &BitcoinOutPoint,
    right: &BitcoinOutPoint,
) -> std::cmp::Ordering {
    left.transaction_id
        .to_string()
        .cmp(&right.transaction_id.to_string())
        .then_with(|| left.output_index.cmp(&right.output_index))
}

const fn collection_eligible_deposit_state(state: &DepositState) -> bool {
    matches!(
        state,
        DepositState::Active { .. } | DepositState::Expired { .. } | DepositState::Closed
    )
}

fn stable_leg_id(collection_id: &CollectionId) -> CollectionLegId {
    CollectionLegId(format!(
        "leg-{}",
        digest_hex(&[collection_id.0.as_bytes(), b"bitcoin-sweep"])
    ))
}

fn stable_operation_id(
    collection_id: &CollectionId,
    attempt: u32,
) -> Result<OperationId, DepositError> {
    OperationId::new(format!(
        "ps-bitcoin-collection-{}",
        digest_hex(&[
            collection_id.0.as_bytes(),
            b"bitcoin-sweep",
            &attempt.to_be_bytes(),
        ])
    ))
    .map_err(|_| invalid("failed to derive a valid Bitcoin custody operation ID"))
}

fn watch_idempotency_key(collection_id: &CollectionId, leg_id: &CollectionLegId) -> String {
    format!(
        "ps-bitcoin-watch-{}",
        digest_hex(&[collection_id.0.as_bytes(), leg_id.0.as_bytes()])
    )
}

fn digest_hex(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(part);
    }
    hex::encode(digest.finalize())
}

fn guard(collection: &Collection, leg: &CollectionLeg) -> CollectionTransitionGuard {
    CollectionTransitionGuard {
        collection_state: collection.state,
        leg_state: leg.state.clone(),
    }
}

fn canonical_transaction_id(transaction_id: BitcoinTransactionId) -> CanonicalTransactionId {
    CanonicalTransactionId {
        chain: ChainId(BITCOIN_CHAIN.to_owned()),
        value: transaction_id.to_string(),
    }
}

fn parse_canonical_transaction_id(
    transaction_id: &CanonicalTransactionId,
) -> Result<BitcoinTransactionId, DepositError> {
    if transaction_id.chain.0 != BITCOIN_CHAIN {
        return Err(invalid(
            "collection transaction ID does not belong to Bitcoin",
        ));
    }
    let parsed = transaction_id
        .value
        .parse::<BitcoinTransactionId>()
        .map_err(|_| invalid("collection transaction ID is not canonical Bitcoin hex"))?;
    if parsed.to_string() != transaction_id.value {
        return Err(invalid(
            "collection transaction ID does not use canonical Bitcoin display encoding",
        ));
    }
    Ok(parsed)
}

fn atomic_from_satoshis(value: Satoshi) -> AtomicAmount {
    let mut magnitude = [0_u8; 32];
    magnitude[24..].copy_from_slice(&value.0.to_be_bytes());
    AtomicAmount(magnitude)
}

fn atomic_to_u64(value: &AtomicAmount) -> Option<u64> {
    if value.0[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&value.0[24..]);
    Some(u64::from_be_bytes(bytes))
}

fn unix_timestamp() -> Result<u64, DepositError> {
    SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|duration| duration.as_secs())
        .map_err(|_| invalid("system clock precedes the Unix epoch"))
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

fn retryable_utxo_view_change(message: impl Into<String>) -> DepositError {
    invalid_state(message)
}

fn not_found(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::NotFound,
        message: message.into(),
    }
}

fn conflict(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::Conflict,
        message: message.into(),
    }
}

fn index_error(error: indexing::IndexError) -> DepositError {
    DepositError {
        kind: if error.retryable {
            DepositErrorKind::Other
        } else {
            DepositErrorKind::InvalidState
        },
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use bitcoin::{Address, Network, XOnlyPublicKey, secp256k1::Secp256k1};
    use chain_bitcoin::{
        BitcoinSignedInputInspection, BitcoinSignedOutputInspection,
        BitcoinSignedTransactionInspection,
    };
    use chain_identity::{AssetId, CanonicalAddress};
    use deposits::{
        CollectionReservation, CollectionReservationState, DepositState, IdempotencyKey, JobId,
        PolicyIdentity, UserId,
    };
    use indexing::{BlockHash, BlockHeight, BlockRef, WatchId};
    use signer::KeyLocator;

    use super::*;

    const ADDRESS: &str = "bcrt1qtwxw3vnj3f29szvhvr84k0aekcrhh9cla5nxa0";

    fn policy() -> BitcoinPaymentPolicy {
        policy_with(ADDRESS, "p2wpkh", 1_000, 5_000)
    }

    fn policy_with(
        master_destination: &str,
        address_kind: &str,
        requested_fee_rate: u64,
        maximum_fee_rate: u64,
    ) -> BitcoinPaymentPolicy {
        BitcoinPaymentPolicy::from_json(
            format!(
                r#"{{
                    "version": 1,
                    "scope": {{"chain": "bitcoin", "network": "regtest"}},
                    "deposit_address_kind": "{address_kind}",
                    "deposit_ttl_seconds": 3600,
                    "master_destination": "{master_destination}",
                    "minimum_collection_satoshis": "10000",
                    "minimum_spend_confirmations": 2,
                    "requested_satoshis_per_kvb": "{requested_fee_rate}",
                    "maximum_satoshis_per_kvb": "{maximum_fee_rate}",
                    "maximum_absolute_fee_satoshis": "50000",
                    "maximum_deposits": 2,
                    "maximum_inputs": 10
                }}"#,
            )
            .as_bytes(),
        )
        .expect("complete Bitcoin test policy must parse")
    }

    fn deposit() -> Deposit {
        let address = BitcoinAddress::parse_for_network(ADDRESS, BitcoinNetwork::Regtest)
            .expect("test address must parse");
        deposit_for(&address)
    }

    fn deposit_for(address: &BitcoinAddress) -> Deposit {
        Deposit {
            id: DepositId("deposit-a".to_owned()),
            idempotency_key: IdempotencyKey("create-deposit-a".to_owned()),
            user_id: UserId("user-a".to_owned()),
            asset: AssetId {
                chain: ChainId(BITCOIN_CHAIN.to_owned()),
                asset: "native".to_owned(),
            },
            address: CanonicalAddress {
                chain: ChainId(BITCOIN_CHAIN.to_owned()),
                value: address.0.clone(),
            },
            key: KeyLocator::Identifier("test-key-a".to_owned()),
            key_purpose: "test".to_owned(),
            expected: atomic_from_satoshis(Satoshi(50_000)),
            birthday: BlockHeight(100),
            expires_at: 1_000,
            state: DepositState::Active {
                watch_id: WatchId("watch-a".to_owned()),
            },
            created_at: 1,
        }
    }

    fn output() -> BitcoinUtxo {
        let address = BitcoinAddress::parse_for_network(ADDRESS, BitcoinNetwork::Regtest)
            .expect("test address must parse");
        output_for(&address)
    }

    fn output_for(address: &BitcoinAddress) -> BitcoinUtxo {
        let script_pubkey = address
            .script_pubkey_for_network(BitcoinNetwork::Regtest)
            .expect("test script must derive")
            .into_bytes();
        BitcoinUtxo {
            outpoint: BitcoinOutPoint {
                transaction_id: BitcoinTransactionId([0x11; 32]),
                output_index: 7,
            },
            value: Satoshi(50_000),
            script_pubkey,
            address: address.clone(),
            created_height: BlockHeight(100),
            coinbase: false,
            confirmations: 2,
        }
    }

    fn taproot_address() -> BitcoinAddress {
        let key = XOnlyPublicKey::from_slice(&[
            0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
            0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b,
            0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test x-only key must parse");
        BitcoinAddress(
            Address::p2tr(&Secp256k1::verification_only(), key, None, Network::Regtest).to_string(),
        )
    }

    fn snapshot() -> BitcoinUtxoSnapshot {
        BitcoinUtxoSnapshot {
            generation: 3,
            revision: 9,
            checkpoint: BlockRef {
                height: BlockHeight(101),
                hash: BlockHash(vec![0x22; 32]),
                parent_hash: Some(BlockHash(vec![0x21; 32])),
                timestamp: Some(123),
            },
        }
    }

    fn collection(destination: &BitcoinAddress) -> Collection {
        let asset = AssetId {
            chain: ChainId(BITCOIN_CHAIN.to_owned()),
            asset: "native".to_owned(),
        };
        Collection {
            id: CollectionId("collection-a".to_owned()),
            job_id: JobId("job-a".to_owned()),
            user_id: UserId("user-a".to_owned()),
            deposit_id: DepositId("deposit-a".to_owned()),
            mode: deposits::CollectionMode::UtxoBatch,
            asset: asset.clone(),
            destination: CanonicalAddress {
                chain: ChainId(BITCOIN_CHAIN.to_owned()),
                value: destination.0.clone(),
            },
            policy: PolicyIdentity {
                version: "1".to_owned(),
                digest: [0x44; 32],
            },
            state: CollectionState::Required,
            reservation: CollectionReservation {
                deposit_id: DepositId("deposit-a".to_owned()),
                asset,
                amount: atomic_from_satoshis(Satoshi(50_000)),
                state: CollectionReservationState::Active,
            },
            participants: Vec::new(),
            legs: Vec::new(),
            attempt_count: 0,
            last_error: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn prepared(
        destination: &BitcoinAddress,
        outpoint: BitcoinOutPoint,
        gross: Satoshi,
        fee: Satoshi,
        output_value: Satoshi,
    ) -> BitcoinPreparedCollection {
        let transaction_id = BitcoinTransactionId([0x55; 32]);
        let script_pubkey = destination
            .script_pubkey_for_network(BitcoinNetwork::Regtest)
            .expect("test destination script must derive")
            .into_bytes();
        BitcoinPreparedCollection {
            transaction_id,
            raw_transaction: SignedEnvelopeBytes::new(vec![1])
                .expect("test envelope must be non-empty"),
            inspection: BitcoinSignedTransactionInspection {
                transaction_id,
                version: 2,
                lock_time: 0,
                virtual_size: 100,
                inputs: vec![BitcoinSignedInputInspection {
                    outpoint,
                    sequence: RBF_SEQUENCE_NO_LOCKTIME,
                }],
                outputs: vec![BitcoinSignedOutputInspection {
                    output_index: 0,
                    value: output_value,
                    script_pubkey,
                }],
            },
            fee,
            attribution: vec![
                crate::bitcoin_wallet_client::BitcoinWalletCollectionAttribution {
                    address: destination.clone(),
                    gross_input: gross,
                },
            ],
        }
    }

    struct BroadcastRecoveryDouble {
        receipts: Mutex<VecDeque<Option<BitcoinWalletReceipt>>>,
        broadcast_result: Result<BitcoinTransactionId, DepositError>,
        broadcast_attempts: AtomicUsize,
    }

    impl BitcoinBroadcastGateway for BroadcastRecoveryDouble {
        fn broadcast<'a>(
            &'a self,
            _transaction_id: BitcoinTransactionId,
            _bytes: &'a SignedEnvelopeBytes,
        ) -> BoxFuture<'a, Result<BitcoinTransactionId, DepositError>> {
            Box::pin(async move {
                self.broadcast_attempts.fetch_add(1, Ordering::SeqCst);
                self.broadcast_result.clone()
            })
        }

        fn receipt<'a>(
            &'a self,
            _transaction_id: BitcoinTransactionId,
        ) -> BoxFuture<'a, Result<Option<BitcoinWalletReceipt>, DepositError>> {
            Box::pin(async move {
                Ok(self
                    .receipts
                    .lock()
                    .expect("test receipt queue mutex must not be poisoned")
                    .pop_front()
                    .expect("test receipt queue must contain the expected lookup"))
            })
        }
    }

    fn mempool_receipt(transaction_id: BitcoinTransactionId) -> BitcoinWalletReceipt {
        BitcoinWalletReceipt {
            transaction_id,
            included_in: None,
            confirmations: 0,
            replaced_by: None,
        }
    }

    #[test]
    fn compact_evidence_round_trips_and_is_fail_closed() {
        let output = output();
        let evidence =
            encode_evidence(&output, &snapshot()).expect("canonical UTXO evidence must encode");
        assert!(evidence.as_bytes().len() < 512);
        let resource = CollectionSpendResource {
            id: CollectionSpendResourceId {
                transaction_id: canonical_transaction_id(output.outpoint.transaction_id),
                output_index: output.outpoint.output_index,
            },
            amount: atomic_from_satoshis(output.value),
            evidence: evidence.clone(),
        };

        let decoded = decode_evidence(&resource, &deposit(), &policy())
            .expect("canonical compact evidence must decode");
        assert_eq!(decoded.outpoint, output.outpoint);
        assert_eq!(decoded.value, output.value);
        assert_eq!(decoded.script_pubkey, output.script_pubkey);

        let mut truncated = evidence.as_bytes().to_vec();
        truncated.pop();
        let mut corrupted = resource.clone();
        corrupted.evidence = CollectionSpendResourceEvidence::new(truncated)
            .expect("non-empty truncated evidence remains bounded");
        assert!(decode_evidence(&corrupted, &deposit(), &policy()).is_err());

        let mut trailing = evidence.as_bytes().to_vec();
        trailing.push(0);
        corrupted.evidence = CollectionSpendResourceEvidence::new(trailing)
            .expect("bounded trailing evidence must construct");
        assert!(decode_evidence(&corrupted, &deposit(), &policy()).is_err());
    }

    #[test]
    fn canonical_batch_membership_rejects_duplicates() {
        let first = DepositId("deposit-a".to_owned());
        let second = DepositId("deposit-b".to_owned());
        assert_eq!(
            canonical_deposit_ids(&[second.clone(), first.clone()])
                .expect("unique IDs must canonicalize"),
            vec![first.clone(), second]
        );
        assert!(canonical_deposit_ids(&[first.clone(), first]).is_err());
    }

    #[test]
    fn collection_remains_eligible_after_observation_lifecycle_races() {
        let watch_id = WatchId("watch-lifecycle".to_owned());
        assert!(collection_eligible_deposit_state(&DepositState::Active {
            watch_id: watch_id.clone(),
        }));
        assert!(collection_eligible_deposit_state(&DepositState::Expired {
            watch_id,
        }));
        assert!(collection_eligible_deposit_state(&DepositState::Closed));
        assert!(!collection_eligible_deposit_state(
            &DepositState::AwaitingWatch
        ));
    }

    #[test]
    fn canonical_utxo_view_changes_are_retryable_without_hiding_other_conflicts() {
        let view_change = retryable_utxo_view_change("test IX snapshot moved");
        assert_eq!(view_change.kind, DepositErrorKind::InvalidState);

        let projection_conflict = conflict("test PS projection conflict");
        assert_eq!(projection_conflict.kind, DepositErrorKind::Conflict);
    }

    #[test]
    fn unowned_reservation_conflict_reselection_is_bounded_and_retained_owner_is_terminal() {
        let reservation_conflict = conflict("test exact-outpoint reservation conflict");
        assert!(should_reselect_after_reservation_conflict(
            &reservation_conflict,
            1
        ));
        assert!(should_reselect_after_reservation_conflict(
            &reservation_conflict,
            MAX_RESERVATION_SELECTION_ATTEMPTS - 1
        ));
        assert!(!should_reselect_after_reservation_conflict(
            &reservation_conflict,
            MAX_RESERVATION_SELECTION_ATTEMPTS
        ));
        assert!(!should_reselect_after_reservation_conflict(
            &invalid_state("test non-conflict"),
            1
        ));

        let blocked = foreign_retained_reservation();
        assert_eq!(blocked.kind, DepositErrorKind::InvariantViolation);
        assert!(
            blocked
                .message
                .contains("only one collection aggregate per deposit")
        );
        assert!(blocked.message.contains("retained UTXO collection"));
    }

    #[tokio::test]
    async fn mempool_receipt_before_broadcast_recovers_without_submission_retry() {
        let transaction_id = BitcoinTransactionId([0x61; 32]);
        let gateway = BroadcastRecoveryDouble {
            receipts: Mutex::new(VecDeque::from([Some(mempool_receipt(transaction_id))])),
            broadcast_result: Ok(transaction_id),
            broadcast_attempts: AtomicUsize::new(0),
        };
        let bytes = SignedEnvelopeBytes::new(vec![1]).expect("test envelope must be non-empty");

        let recovered = broadcast_or_recover(&gateway, transaction_id, &bytes)
            .await
            .expect("mempool receipt must prove prior exact-byte submission");

        assert_eq!(recovered, transaction_id);
        assert_eq!(gateway.broadcast_attempts.load(Ordering::SeqCst), 0);
        assert!(
            gateway
                .receipts
                .lock()
                .expect("test receipt queue mutex must not be poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn mempool_receipt_after_lost_response_recovers_one_broadcast_attempt() {
        let transaction_id = BitcoinTransactionId([0x62; 32]);
        let gateway = BroadcastRecoveryDouble {
            receipts: Mutex::new(VecDeque::from([
                None,
                Some(mempool_receipt(transaction_id)),
            ])),
            broadcast_result: Err(DepositError {
                kind: DepositErrorKind::Other,
                message: "test lost broadcast response".to_owned(),
            }),
            broadcast_attempts: AtomicUsize::new(0),
        };
        let bytes = SignedEnvelopeBytes::new(vec![2]).expect("test envelope must be non-empty");

        let recovered = broadcast_or_recover(&gateway, transaction_id, &bytes)
            .await
            .expect("post-error mempool receipt must recover the exact submission");

        assert_eq!(recovered, transaction_id);
        assert_eq!(gateway.broadcast_attempts.load(Ordering::SeqCst), 1);
        assert!(
            gateway
                .receipts
                .lock()
                .expect("test receipt queue mutex must not be poisoned")
                .is_empty()
        );
    }

    #[test]
    fn maximum_fee_rate_accepts_the_chain_owned_ceiling_satoshi() {
        let policy = policy_with(ADDRESS, "p2wpkh", 1_001, 1_001);
        let destination = BitcoinAddress::parse_for_network(ADDRESS, BitcoinNetwork::Regtest)
            .expect("test address must parse");
        let outpoint = BitcoinOutPoint {
            transaction_id: BitcoinTransactionId([0x11; 32]),
            output_index: 7,
        };
        let collection = collection(&destination);
        let expected_gross = BTreeMap::from([(destination.clone(), Satoshi(50_000))]);
        let expected_outpoints = BTreeSet::from([outpoint]);

        let exact_ceiling = prepared(
            &destination,
            outpoint,
            Satoshi(50_000),
            Satoshi(101),
            Satoshi(49_899),
        );
        validate_prepared(
            &policy,
            &collection,
            &exact_ceiling,
            &expected_gross,
            &expected_outpoints,
        )
        .expect("ceil(1001 sat/kvB * 100 vB) must allow a 101-satoshi fee");

        let above_ceiling = prepared(
            &destination,
            outpoint,
            Satoshi(50_000),
            Satoshi(102),
            Satoshi(49_898),
        );
        assert!(
            validate_prepared(
                &policy,
                &collection,
                &above_ceiling,
                &expected_gross,
                &expected_outpoints,
            )
            .is_err()
        );
    }

    #[test]
    fn prepared_collection_requires_canonical_input_order() {
        let policy = policy();
        let destination = BitcoinAddress::parse_for_network(ADDRESS, BitcoinNetwork::Regtest)
            .expect("test address must parse");
        let first = BitcoinOutPoint {
            transaction_id: BitcoinTransactionId([0x11; 32]),
            output_index: 7,
        };
        let second = BitcoinOutPoint {
            transaction_id: BitcoinTransactionId([0x22; 32]),
            output_index: 3,
        };
        let expected_outpoints = BTreeSet::from([first, second]);
        let mut canonical = expected_outpoints.iter().copied().collect::<Vec<_>>();
        canonical.sort_by(canonical_bitcoin_outpoint_order);
        let mut noncanonical = canonical.clone();
        noncanonical.reverse();
        let collection = collection(&destination);
        let expected_gross = BTreeMap::from([(destination.clone(), Satoshi(50_000))]);
        let mut prepared = prepared(
            &destination,
            first,
            Satoshi(50_000),
            Satoshi(100),
            Satoshi(49_900),
        );
        prepared.inspection.inputs = noncanonical
            .into_iter()
            .map(|outpoint| BitcoinSignedInputInspection {
                outpoint,
                sequence: RBF_SEQUENCE_NO_LOCKTIME,
            })
            .collect();

        let error = validate_prepared(
            &policy,
            &collection,
            &prepared,
            &expected_gross,
            &expected_outpoints,
        )
        .expect_err("the exact input set in a noncanonical order must fail closed");
        assert!(error.message.contains("canonical (txid, vout) order"));

        prepared.inspection.inputs = canonical
            .into_iter()
            .map(|outpoint| BitcoinSignedInputInspection {
                outpoint,
                sequence: RBF_SEQUENCE_NO_LOCKTIME,
            })
            .collect();
        validate_prepared(
            &policy,
            &collection,
            &prepared,
            &expected_gross,
            &expected_outpoints,
        )
        .expect("the exact input set in canonical order must validate");
    }

    #[test]
    fn taproot_selection_evidence_and_prepared_batch_validate_offline() {
        let address = taproot_address();
        let policy = policy_with(&address.0, "p2tr", 1_000, 5_000);
        let output = output_for(&address);
        let evidence =
            encode_evidence(&output, &snapshot()).expect("Taproot UTXO evidence must encode");
        let resource = CollectionSpendResource {
            id: CollectionSpendResourceId {
                transaction_id: canonical_transaction_id(output.outpoint.transaction_id),
                output_index: output.outpoint.output_index,
            },
            amount: atomic_from_satoshis(output.value),
            evidence,
        };
        let decoded = decode_evidence(&resource, &deposit_for(&address), &policy)
            .expect("Taproot exact selection evidence must validate");
        assert_eq!(decoded.script_pubkey, output.script_pubkey);

        let collection = collection(&address);
        let prepared = prepared(
            &address,
            output.outpoint,
            output.value,
            Satoshi(100),
            Satoshi(49_900),
        );
        validate_prepared(
            &policy,
            &collection,
            &prepared,
            &BTreeMap::from([(address, output.value)]),
            &BTreeSet::from([output.outpoint]),
        )
        .expect("one-output Taproot batch must satisfy executor validation");
    }

    #[test]
    fn prepared_collection_enforces_conservation_fee_rate_and_dust() {
        let policy = policy();
        let destination = BitcoinAddress::parse_for_network(ADDRESS, BitcoinNetwork::Regtest)
            .expect("test address must parse");
        let outpoint = BitcoinOutPoint {
            transaction_id: BitcoinTransactionId([0x11; 32]),
            output_index: 7,
        };
        let expected_outpoints = BTreeSet::from([outpoint]);
        let mut expected_gross = BTreeMap::from([(destination.clone(), Satoshi(50_000))]);
        let collection = collection(&destination);
        let valid = prepared(
            &destination,
            outpoint,
            Satoshi(50_000),
            Satoshi(100),
            Satoshi(49_900),
        );
        validate_prepared(
            &policy,
            &collection,
            &valid,
            &expected_gross,
            &expected_outpoints,
        )
        .expect("conserving transaction at the requested fee rate must pass");

        let below_requested = prepared(
            &destination,
            outpoint,
            Satoshi(50_000),
            Satoshi(99),
            Satoshi(49_901),
        );
        assert!(
            validate_prepared(
                &policy,
                &collection,
                &below_requested,
                &expected_gross,
                &expected_outpoints,
            )
            .is_err()
        );

        let above_maximum = prepared(
            &destination,
            outpoint,
            Satoshi(50_000),
            Satoshi(501),
            Satoshi(49_499),
        );
        assert!(
            validate_prepared(
                &policy,
                &collection,
                &above_maximum,
                &expected_gross,
                &expected_outpoints,
            )
            .is_err()
        );

        let nonconserving = prepared(
            &destination,
            outpoint,
            Satoshi(50_000),
            Satoshi(100),
            Satoshi(49_899),
        );
        assert!(
            validate_prepared(
                &policy,
                &collection,
                &nonconserving,
                &expected_gross,
                &expected_outpoints,
            )
            .is_err()
        );

        expected_gross.insert(destination.clone(), Satoshi(501));
        let dust = prepared(
            &destination,
            outpoint,
            Satoshi(501),
            Satoshi(500),
            Satoshi(1),
        );
        assert!(
            validate_prepared(
                &policy,
                &collection,
                &dust,
                &expected_gross,
                &expected_outpoints,
            )
            .is_err()
        );
    }
}
