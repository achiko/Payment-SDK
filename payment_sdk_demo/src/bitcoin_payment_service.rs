//! Offline native-Bitcoin Payment Service workflow.
//!
//! This demo persists two deposits and their confirmed fictional UTXOs, creates
//! one exact-outpoint collection reservation, builds and signs one no-change
//! drain transaction, and durably records its exact bytes and fee attribution.
//! It has no node or broadcast dependency and never prints custody locators or
//! signed transaction bytes.

use std::{error::Error, io};

use chain_bitcoin::{
    BitcoinAddress, BitcoinAddressGenerator, BitcoinAddressKind, BitcoinBuildRequest,
    BitcoinGenerateAddress, BitcoinNetwork, BitcoinOutPoint, BitcoinOutput,
    BitcoinTransactionBuilder, BitcoinTransactionCodec, BitcoinTransactionId,
    BitcoinTransactionSigning, BitcoinUtxo, Satoshi, SatoshisPerKvb, UnsignedBitcoinTransaction,
};
use chain_contract::DepositAddressGenerator;
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use deposits::{
    AppendObservation, ApplyResult, CollectionAllocation, CollectionId, CollectionLegId,
    CollectionLegKind, CollectionLegState, CollectionSpendResource,
    CollectionSpendResourceEvidence, CollectionSpendResourceId, CollectionState, CollectionStore,
    CollectionTransitionGuard, CommandIdentity, CommandOperation, CommandPrincipal, CreateDeposit,
    CreateDepositWithLedger, CreateJob, CreateUtxoBatchCollection, CreateUtxoBatchCollectionJob,
    CreateUtxoBatchParticipant, DepositBalances, DepositId, DepositLedger, DepositStore,
    EnsureUser, IdempotencyKey, InitializePaymentDatabase, JobId, JobPayload, JobStore,
    LedgerEffect, LedgerEntryId, MirroredObservation, ObservationEventLog,
    PaymentDatabaseMetadataStore, PersistentPaymentRepository, PolicyIdentity, RecordObservation,
    RecordSignedCollectionLeg, RequestHash, SignedEnvelopeBytes, UserId, UserStore,
};
use indexing::{
    BlockHash, BlockHeight, BlockRef, ConfirmationProof, EventCursor, IndexScope, MovementId,
    MovementKind, ObservationEvent, ObservationEventId, ObservationRevision, ObservedTransaction,
    TransactionStatus, ValueMovement, WatchId,
};
use signer::OperationId;
use signer_local::LocalSigner;
use storage_rocksdb::RocksDbStorage;
use tempfile::TempDir;

const NETWORK: BitcoinNetwork = BitcoinNetwork::Regtest;
const CHAIN_NAME: &str = "bitcoin";
const NETWORK_NAME: &str = "regtest";
const ASSET_NAME: &str = "native";
const DEPOSIT_KEY_PURPOSE: &str = "bitcoin-payment-service-demo-deposit-v1";
const POLICY_VERSION: &str = "bitcoin-payment-service-demo-policy-v1";
const FEE_RATE: SatoshisPerKvb = SatoshisPerKvb::new(1_000);
const CONFIRMED_HEIGHT: u64 = 200;
const CONFIRMATIONS: u64 = 2;
const CREATED_AT: u64 = 1_000;
const SIGNED_AT: u64 = 1_100;
const DEPOSIT_EXPIRES_AT: u64 = 86_400;
const FICTIONAL_BLOCK_HASH: [u8; 32] = [0x44; 32];
const FICTIONAL_PARENT_HASH: [u8; 32] = [0x33; 32];

type DemoResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Copy)]
struct ParticipantSpec {
    deposit_id: &'static str,
    user_id: &'static str,
    transaction_byte: u8,
    output_index: u32,
    gross: u64,
    cursor: u64,
}

const PARTICIPANT_SPECS: [ParticipantSpec; 2] = [
    ParticipantSpec {
        deposit_id: "btc-deposit-a",
        user_id: "exchange-user-a",
        transaction_byte: 0x11,
        output_index: 0,
        gross: 120_000,
        cursor: 1,
    },
    ParticipantSpec {
        deposit_id: "btc-deposit-b",
        user_id: "exchange-user-b",
        transaction_byte: 0x22,
        output_index: 1,
        gross: 80_000,
        cursor: 2,
    },
];

#[derive(Clone)]
struct DemoParticipant {
    deposit_id: DepositId,
    user_id: UserId,
    expected_ledger_head: LedgerEntryId,
    address: BitcoinAddress,
    utxo: BitcoinUtxo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FeeShare {
    deposit_id: DepositId,
    gross: u64,
    allocated_fee: u64,
    master_credit: u64,
    remainder: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DemoSummary {
    deposit_addresses: Vec<String>,
    master_address: String,
    collection_id: String,
    participant_count: usize,
    input_count: usize,
    output_count: usize,
    gross: u64,
    fee: u64,
    net: u64,
    transaction_id: String,
    virtual_size: u64,
}

#[tokio::main]
async fn main() -> DemoResult<()> {
    let summary = execute_demo().await?;
    print_summary(&summary);
    Ok(())
}

async fn execute_demo() -> DemoResult<DemoSummary> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    let chain = ChainId(CHAIN_NAME.to_owned());
    let scope = IndexScope {
        chain: chain.clone(),
        network: NETWORK_NAME.to_owned(),
    };
    let asset = AssetId {
        chain: chain.clone(),
        asset: ASSET_NAME.to_owned(),
    };
    let policy = PolicyIdentity {
        version: POLICY_VERSION.to_owned(),
        digest: [0x51; 32],
    };
    repository
        .initialize_or_validate(InitializePaymentDatabase {
            scope: scope.clone(),
            active_policy: policy.clone(),
            initialized_at: CREATED_AT,
        })
        .await?;

    let exchange_principal = CommandPrincipal("offline-demo-exchange".to_owned());
    for (offset, spec) in PARTICIPANT_SPECS.iter().enumerate() {
        repository
            .ensure_user(EnsureUser {
                id: UserId(spec.user_id.to_owned()),
                owner: exchange_principal.clone(),
                first_seen_at: CREATED_AT + u64::try_from(offset)?,
            })
            .await?;
    }

    let signer = LocalSigner::ephemeral_for_testing();
    let address_generator = BitcoinAddressGenerator;
    let mut participants = Vec::with_capacity(PARTICIPANT_SPECS.len());
    for spec in PARTICIPANT_SPECS {
        participants.push(
            create_confirmed_participant(
                &repository,
                &address_generator,
                &signer,
                &scope,
                &asset,
                spec,
            )
            .await?,
        );
    }
    participants.sort_by(|left, right| left.deposit_id.cmp(&right.deposit_id));

    let master = address_generator
        .generate_address(
            BitcoinGenerateAddress::new(
                NETWORK,
                BitcoinAddressKind::SegwitV0,
                OperationId::new("bitcoin-payment-demo-master-address")?,
                "bitcoin-payment-demo-master",
            ),
            &signer,
        )
        .await?;
    let collection_id = CollectionId("btc-collection-batch-1".to_owned());
    let job_id = JobId("btc-collection-job-1".to_owned());
    let deposit_ids = participants
        .iter()
        .map(|participant| participant.deposit_id.clone())
        .collect::<Vec<_>>();
    let job = repository
        .create_or_replay(CreateJob {
            id: job_id,
            command: CommandIdentity {
                principal: exchange_principal.clone(),
                operation: CommandOperation::CreateCollection,
                client_key: IdempotencyKey("bitcoin-demo-create-batch-1".to_owned()),
                request_hash: RequestHash([0x61; 32]),
            },
            payload: JobPayload::CreateUtxoBatchCollection(CreateUtxoBatchCollectionJob {
                collection_id: collection_id.clone(),
                deposit_ids: deposit_ids.clone(),
            }),
            user_owner: exchange_principal,
            policy: policy.clone(),
            created_at: CREATED_AT + 20,
        })
        .await?
        .job()
        .clone();

    let create_participants = participants
        .iter()
        .map(|participant| {
            Ok(CreateUtxoBatchParticipant {
                user_id: participant.user_id.clone(),
                deposit_id: participant.deposit_id.clone(),
                expected_ledger_head: participant.expected_ledger_head.clone(),
                reservation_amount: atomic(participant.utxo.value.0),
                spend_resources: vec![spend_resource(participant)?],
            })
        })
        .collect::<DemoResult<Vec<_>>>()?;
    let destination = canonical_address(&chain, &master.address);
    let collection = repository
        .create_or_replay_utxo_batch(CreateUtxoBatchCollection {
            id: collection_id.clone(),
            job_id: job.id,
            asset: asset.clone(),
            destination,
            policy,
            participants: create_participants,
            leg: deposits::CreateCollectionLeg {
                id: CollectionLegId("sweep".to_owned()),
                kind: CollectionLegKind::Sweep,
                planned_amount: None,
            },
            created_at: CREATED_AT + 30,
        })
        .await?
        .collection()
        .clone();
    require(
        collection.participants.len() == participants.len(),
        "durable collection participant count changed",
    )?;

    // The exact outpoints are durably owned by the collection before custody
    // receives any signing request.
    let available = participants
        .iter()
        .map(|participant| participant.utxo.clone())
        .collect::<Vec<_>>();
    let codec = BitcoinTransactionCodec::new(NETWORK);
    let unsigned = codec.build(BitcoinBuildRequest {
        signing_operation_id: OperationId::new("bitcoin-payment-demo-sign-batch-1")?,
        available,
        recipients: vec![BitcoinOutput {
            address: master.address.clone(),
            value: Satoshi(0),
        }],
        change_address: master.address.clone(),
        fee_rate: FEE_RATE,
        drain_wallet: true,
    })?;
    validate_unsigned_selection(&unsigned, &participants)?;
    let gross = transaction_input_total(&unsigned)?;
    let output_total = transaction_output_total(&unsigned)?;
    let fee = gross
        .checked_sub(output_total)
        .ok_or_else(|| demo_error("Bitcoin demo outputs exceed exact reserved inputs"))?;
    let fee_shares = allocate_fee(
        &participants
            .iter()
            .map(|participant| (participant.deposit_id.clone(), participant.utxo.value.0))
            .collect::<Vec<_>>(),
        fee,
    )?;
    let allocations = fee_shares
        .iter()
        .map(|share| CollectionAllocation {
            deposit_id: share.deposit_id.clone(),
            asset: asset.clone(),
            gross_debit: atomic(share.gross),
            master_credit: atomic(share.master_credit),
            allocated_fee_asset: asset.clone(),
            allocated_fee: atomic(share.allocated_fee),
        })
        .collect::<Vec<_>>();
    require(
        checked_sum(
            fee_shares.iter().map(|share| share.allocated_fee),
            "Bitcoin allocated fee total overflowed",
        )? == fee,
        "Bitcoin fee allocations do not conserve the transaction fee",
    )?;
    require(
        checked_sum(
            fee_shares.iter().map(|share| share.master_credit),
            "Bitcoin master-credit total overflowed",
        )? == output_total,
        "Bitcoin fee allocations do not conserve the master output",
    )?;

    let signed = codec.sign(unsigned, &signer).await?;
    let inspection = signed.inspect()?;
    validate_signed_transaction(&inspection, &participants, &master.address, gross, fee)?;
    let virtual_size = inspection.virtual_size;
    let transaction_id = signed.id();
    let canonical_transaction_id = canonical_transaction_id(&chain, transaction_id);
    let signed_bytes = signed.consensus_bytes().to_vec();
    let leg = collection
        .legs
        .first()
        .ok_or_else(|| demo_error("durable collection has no sweep leg"))?;
    let signed_collection = repository
        .record_signed(RecordSignedCollectionLeg {
            collection_id: collection.id.clone(),
            leg_id: leg.id.clone(),
            expected: CollectionTransitionGuard {
                collection_state: collection.state,
                leg_state: leg.state.clone(),
            },
            expected_transaction_id: canonical_transaction_id.clone(),
            envelope: SignedEnvelopeBytes::new(signed_bytes.clone())?,
            allocations: allocations.clone(),
            signed_at: SIGNED_AT,
            // Bitcoin exact-input ownership and the identical signed bytes do
            // not expire after signing; reorg recovery must reuse them.
            expires_at: u64::MAX,
        })
        .await?;
    require(
        signed_collection.state == CollectionState::InProgress,
        "signed collection did not enter in-progress state",
    )?;
    require(
        signed_collection.legs[0].state
            == CollectionLegState::Signed {
                transaction_id: canonical_transaction_id.clone(),
            },
        "signed collection did not retain its transaction ID",
    )?;
    require(
        signed_collection.legs[0].allocations == allocations,
        "signed collection did not retain canonical fee allocations",
    )?;
    let envelope = repository
        .signed_envelope(&collection.id, &leg.id)
        .await?
        .ok_or_else(|| demo_error("signed Bitcoin envelope was not durably retained"))?;
    require(
        envelope.expected_transaction_id == canonical_transaction_id,
        "durable envelope transaction ID changed",
    )?;
    require(
        envelope.bytes.as_bytes() == signed_bytes.as_slice(),
        "durable envelope bytes changed",
    )?;

    Ok(DemoSummary {
        deposit_addresses: participants
            .iter()
            .map(|participant| participant.address.0.clone())
            .collect(),
        master_address: master.address.0,
        collection_id: collection_id.0,
        participant_count: participants.len(),
        input_count: inspection.inputs.len(),
        output_count: inspection.outputs.len(),
        gross,
        fee,
        net: output_total,
        transaction_id: transaction_id.to_string(),
        virtual_size,
    })
}

async fn create_confirmed_participant(
    repository: &PersistentPaymentRepository<RocksDbStorage>,
    address_generator: &BitcoinAddressGenerator,
    signer: &LocalSigner,
    scope: &IndexScope,
    asset: &AssetId,
    spec: ParticipantSpec,
) -> DemoResult<DemoParticipant> {
    let generated = address_generator
        .generate_address(
            BitcoinGenerateAddress::new(
                NETWORK,
                BitcoinAddressKind::SegwitV0,
                OperationId::new(format!("generate-{}", spec.deposit_id))?,
                DEPOSIT_KEY_PURPOSE,
            ),
            signer,
        )
        .await?;
    let deposit_id = DepositId(spec.deposit_id.to_owned());
    let user_id = UserId(spec.user_id.to_owned());
    let idempotency_key = IdempotencyKey(format!("create-{}", spec.deposit_id));
    let created = repository
        .create_with_ledger(CreateDepositWithLedger {
            deposit: CreateDeposit {
                id: deposit_id.clone(),
                idempotency_key: idempotency_key.clone(),
                user_id: user_id.clone(),
                asset: asset.clone(),
                address: canonical_address(&scope.chain, &generated.address),
                key: generated.key.clone(),
                key_purpose: DEPOSIT_KEY_PURPOSE.to_owned(),
                expected: atomic(spec.gross),
                birthday: BlockHeight(CONFIRMED_HEIGHT),
                expires_at: DEPOSIT_EXPIRES_AT,
                created_at: CREATED_AT + spec.cursor,
            },
            ledger_recorded_at: CREATED_AT + spec.cursor,
        })
        .await?;
    require(
        created.ledger.balances == DepositBalances::default(),
        "opening deposit ledger was not zero",
    )?;
    let watch_id = WatchId(format!("watch-{}", spec.deposit_id));
    repository
        .activate_watch(&deposit_id, &idempotency_key, watch_id.clone())
        .await?;

    let transaction_id = BitcoinTransactionId([spec.transaction_byte; 32]);
    let canonical_transaction_id = canonical_transaction_id(&scope.chain, transaction_id);
    let movement_id = MovementId(format!("funding-output-{}", spec.deposit_id));
    let event_id = ObservationEventId(format!("confirmed-utxo-{}", spec.deposit_id));
    repository
        .append(AppendObservation {
            observation: MirroredObservation {
                event: ObservationEvent {
                    id: event_id.clone(),
                    cursor: EventCursor(spec.cursor),
                    watch_ids: vec![watch_id],
                    previous_status: None,
                    transaction: ObservedTransaction {
                        scope: scope.clone(),
                        transaction_id: canonical_transaction_id,
                        revision: ObservationRevision(1),
                        status: TransactionStatus::Confirmed {
                            block: fictional_block(),
                            proof: ConfirmationProof::Depth {
                                required: CONFIRMATIONS,
                                observed: CONFIRMATIONS,
                            },
                        },
                        movements: vec![ValueMovement {
                            id: movement_id.clone(),
                            asset: asset.clone(),
                            amount: atomic(spec.gross),
                            from: None,
                            to: Some(created.deposit.address.clone()),
                            kind: MovementKind::Output,
                        }],
                        fee: None,
                        first_seen_at: CREATED_AT + 10 + spec.cursor,
                        observed_at: CREATED_AT + 11 + spec.cursor,
                    },
                },
                received_at: CREATED_AT + 12 + spec.cursor,
            },
        })
        .await?;
    let confirmed_ledger = repository
        .record_observation(RecordObservation {
            event_id,
            effect: LedgerEffect::Incoming {
                movements: vec![movement_id],
            },
            deposit_id: deposit_id.clone(),
            expected_head: Some(created.ledger.id),
            recorded_at: CREATED_AT + 13 + spec.cursor,
        })
        .await?;
    let confirmed_ledger = match confirmed_ledger {
        ApplyResult::Appended { entry } | ApplyResult::AlreadyPresent { entry } => entry,
    };
    let expected = atomic(spec.gross);
    require(
        confirmed_ledger.balances.received == expected
            && confirmed_ledger.balances.confirmed == expected
            && confirmed_ledger.balances.balance == expected
            && confirmed_ledger.balances.collected == AtomicAmount::ZERO
            && confirmed_ledger.balances.accounted == AtomicAmount::ZERO,
        "confirmed fictional UTXO did not produce the expected ledger snapshot",
    )?;

    let script_pubkey = generated
        .address
        .script_pubkey_for_network(NETWORK)?
        .into_bytes();
    let utxo = BitcoinUtxo::from_exact_selection(
        NETWORK,
        &generated.address,
        generated.key,
        transaction_id,
        spec.output_index,
        Satoshi(spec.gross),
        script_pubkey,
    )?;
    Ok(DemoParticipant {
        deposit_id,
        user_id,
        expected_ledger_head: confirmed_ledger.id,
        address: generated.address,
        utxo,
    })
}

fn spend_resource(participant: &DemoParticipant) -> DemoResult<CollectionSpendResource> {
    Ok(CollectionSpendResource {
        id: CollectionSpendResourceId {
            transaction_id: CanonicalTransactionId {
                chain: ChainId(CHAIN_NAME.to_owned()),
                value: BitcoinTransactionId(participant.utxo.transaction_id).to_string(),
            },
            output_index: participant.utxo.output_index,
        },
        amount: atomic(participant.utxo.value.0),
        evidence: CollectionSpendResourceEvidence::new(utxo_evidence(&participant.utxo)?)?,
    })
}

fn utxo_evidence(utxo: &BitcoinUtxo) -> Result<Vec<u8>, io::Error> {
    let script_length = u32::try_from(utxo.script_pubkey.len())
        .map_err(|_| demo_error("Bitcoin script length exceeds u32"))?;
    let mut evidence = Vec::with_capacity(128 + utxo.script_pubkey.len());
    evidence.extend_from_slice(b"bitcoin-ps-utxo-evidence-v1\0");
    evidence.extend_from_slice(NETWORK_NAME.as_bytes());
    evidence.push(0);
    evidence.extend_from_slice(&CONFIRMED_HEIGHT.to_be_bytes());
    evidence.extend_from_slice(&FICTIONAL_BLOCK_HASH);
    evidence.extend_from_slice(&1_u64.to_be_bytes());
    evidence.extend_from_slice(&utxo.transaction_id);
    evidence.extend_from_slice(&utxo.output_index.to_be_bytes());
    evidence.extend_from_slice(&utxo.value.0.to_be_bytes());
    evidence.extend_from_slice(&script_length.to_be_bytes());
    evidence.extend_from_slice(&utxo.script_pubkey);
    Ok(evidence)
}

fn allocate_fee(inputs: &[(DepositId, u64)], total_fee: u64) -> Result<Vec<FeeShare>, io::Error> {
    if inputs.is_empty() {
        return Err(demo_error("Bitcoin fee allocation is empty"));
    }
    let mut shares = inputs
        .iter()
        .map(|(deposit_id, gross)| FeeShare {
            deposit_id: deposit_id.clone(),
            gross: *gross,
            allocated_fee: 0,
            master_credit: 0,
            remainder: 0,
        })
        .collect::<Vec<_>>();
    shares.sort_by(|left, right| left.deposit_id.cmp(&right.deposit_id));
    for pair in shares.windows(2) {
        if pair[0].deposit_id == pair[1].deposit_id {
            return Err(demo_error("Bitcoin fee allocation has duplicate deposits"));
        }
    }
    let total_gross = shares.iter().try_fold(0_u64, |total, share| {
        if share.gross == 0 {
            return Err(demo_error("Bitcoin fee allocation has a zero gross input"));
        }
        total
            .checked_add(share.gross)
            .ok_or_else(|| demo_error("Bitcoin fee allocation gross total overflowed"))
    })?;
    if total_fee > total_gross {
        return Err(demo_error("Bitcoin fee exceeds collection gross input"));
    }
    let denominator = u128::from(total_gross);
    let mut allocated = 0_u64;
    for share in &mut shares {
        let numerator = u128::from(total_fee)
            .checked_mul(u128::from(share.gross))
            .ok_or_else(|| demo_error("Bitcoin fee allocation numerator overflowed"))?;
        share.allocated_fee = u64::try_from(numerator / denominator)
            .map_err(|_| demo_error("Bitcoin fee share exceeds u64"))?;
        share.remainder = numerator % denominator;
        allocated = allocated
            .checked_add(share.allocated_fee)
            .ok_or_else(|| demo_error("Bitcoin allocated fee total overflowed"))?;
    }
    let mut remaining = total_fee
        .checked_sub(allocated)
        .ok_or_else(|| demo_error("Bitcoin allocated fee exceeds total fee"))?;
    let mut remainder_order = (0..shares.len()).collect::<Vec<_>>();
    remainder_order.sort_by(|left, right| {
        shares[*right]
            .remainder
            .cmp(&shares[*left].remainder)
            .then_with(|| shares[*left].deposit_id.cmp(&shares[*right].deposit_id))
    });
    for index in remainder_order {
        if remaining == 0 {
            break;
        }
        shares[index].allocated_fee = shares[index]
            .allocated_fee
            .checked_add(1)
            .ok_or_else(|| demo_error("Bitcoin fee share overflowed"))?;
        remaining -= 1;
    }
    if remaining != 0 {
        return Err(demo_error("Bitcoin fee remainder was not fully allocated"));
    }
    for share in &mut shares {
        share.master_credit = share
            .gross
            .checked_sub(share.allocated_fee)
            .filter(|credit| *credit > 0)
            .ok_or_else(|| demo_error("Bitcoin fee allocation leaves no master credit"))?;
    }
    Ok(shares)
}

fn validate_unsigned_selection(
    transaction: &UnsignedBitcoinTransaction,
    participants: &[DemoParticipant],
) -> Result<(), io::Error> {
    let actual = transaction
        .inputs
        .iter()
        .map(|input| {
            (
                BitcoinTransactionId(input.utxo.transaction_id),
                input.utxo.output_index,
                input.utxo.value.0,
            )
        })
        .collect::<Vec<_>>();
    let mut expected = participants
        .iter()
        .map(|participant| {
            (
                BitcoinTransactionId(participant.utxo.transaction_id),
                participant.utxo.output_index,
                participant.utxo.value.0,
            )
        })
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    require(
        actual == expected,
        "unsigned transaction does not spend the exact canonical reservation",
    )?;
    require(
        transaction.outputs.len() == 1,
        "Bitcoin collection must have exactly one no-change output",
    )
}

fn validate_signed_transaction(
    inspection: &chain_bitcoin::BitcoinSignedTransactionInspection,
    participants: &[DemoParticipant],
    destination: &BitcoinAddress,
    gross: u64,
    fee: u64,
) -> Result<(), io::Error> {
    require(
        inspection.version == 2 && inspection.lock_time == 0,
        "signed Bitcoin collection changed version or locktime",
    )?;
    let expected_inputs = participants
        .iter()
        .map(|participant| BitcoinOutPoint {
            transaction_id: BitcoinTransactionId(participant.utxo.transaction_id),
            output_index: participant.utxo.output_index,
        })
        .collect::<Vec<_>>();
    let actual_inputs = inspection
        .inputs
        .iter()
        .map(|input| input.outpoint)
        .collect::<Vec<_>>();
    require(
        actual_inputs == expected_inputs,
        "signed Bitcoin inputs differ from the exact durable reservation",
    )?;
    require(
        inspection
            .inputs
            .iter()
            .all(|input| input.sequence == 0xffff_fffd),
        "signed Bitcoin collection changed its expected input sequences",
    )?;
    let expected_fee = u64::try_from(
        u128::from(FEE_RATE.satoshis_per_kvb())
            .checked_mul(u128::from(inspection.virtual_size))
            .and_then(|value| value.checked_add(999))
            .ok_or_else(|| demo_error("Bitcoin fee-rate validation overflowed"))?
            / 1_000,
    )
    .map_err(|_| demo_error("Bitcoin fee-rate validation exceeds u64"))?;
    require(
        fee >= expected_fee,
        "signed Bitcoin collection fee is below its requested sat/kvB rate",
    )?;
    require(
        inspection.outputs.len() == 1,
        "signed Bitcoin collection contains hidden change or extra outputs",
    )?;
    let output = &inspection.outputs[0];
    require(
        output.script_pubkey == destination_script(destination)?,
        "signed Bitcoin collection destination script changed",
    )?;
    require(
        output.value.0
            == gross
                .checked_sub(fee)
                .ok_or_else(|| demo_error("Bitcoin fee exceeds gross input"))?,
        "signed Bitcoin collection output does not equal gross minus fee",
    )
}

fn transaction_input_total(transaction: &UnsignedBitcoinTransaction) -> Result<u64, io::Error> {
    checked_sum(
        transaction.inputs.iter().map(|input| input.utxo.value.0),
        "Bitcoin input total overflowed",
    )
}

fn transaction_output_total(transaction: &UnsignedBitcoinTransaction) -> Result<u64, io::Error> {
    checked_sum(
        transaction.outputs.iter().map(|output| output.value.0),
        "Bitcoin output total overflowed",
    )
}

fn checked_sum(
    mut values: impl Iterator<Item = u64>,
    message: &'static str,
) -> Result<u64, io::Error> {
    values.try_fold(0_u64, |total, value| {
        total.checked_add(value).ok_or_else(|| demo_error(message))
    })
}

fn destination_script(address: &BitcoinAddress) -> Result<Vec<u8>, io::Error> {
    address
        .script_pubkey_for_network(NETWORK)
        .map(|script| script.into_bytes())
        .map_err(|_| demo_error("Bitcoin destination is not a regtest address"))
}

fn canonical_address(chain: &ChainId, address: &BitcoinAddress) -> CanonicalAddress {
    CanonicalAddress {
        chain: chain.clone(),
        value: address.0.clone(),
    }
}

fn canonical_transaction_id(
    chain: &ChainId,
    transaction_id: BitcoinTransactionId,
) -> CanonicalTransactionId {
    CanonicalTransactionId {
        chain: chain.clone(),
        value: transaction_id.to_string(),
    }
}

fn atomic(value: u64) -> AtomicAmount {
    let mut bytes = [0_u8; 32];
    bytes[24..].copy_from_slice(&value.to_be_bytes());
    AtomicAmount(bytes)
}

fn fictional_block() -> BlockRef {
    BlockRef {
        height: BlockHeight(CONFIRMED_HEIGHT),
        hash: BlockHash(FICTIONAL_BLOCK_HASH.to_vec()),
        parent_hash: Some(BlockHash(FICTIONAL_PARENT_HASH.to_vec())),
        timestamp: Some(CREATED_AT),
    }
}

fn require(condition: bool, message: &'static str) -> Result<(), io::Error> {
    if condition {
        Ok(())
    } else {
        Err(demo_error(message))
    }
}

fn demo_error(message: &'static str) -> io::Error {
    io::Error::other(message)
}

fn print_summary(summary: &DemoSummary) {
    println!("Bitcoin Payment Service demo network: {NETWORK_NAME}");
    for (index, address) in summary.deposit_addresses.iter().enumerate() {
        println!("Deposit {} P2WPKH address: {address}", index + 1);
    }
    println!("Master P2WPKH address: {}", summary.master_address);
    println!("Collection ID: {}", summary.collection_id);
    println!(
        "Reserved batch: {} participant(s), {} exact input(s), {} output(s)",
        summary.participant_count, summary.input_count, summary.output_count
    );
    println!(
        "Collection value: {} gross - {} fee = {} net satoshis at {} sat/kvB",
        summary.gross,
        summary.fee,
        summary.net,
        FEE_RATE.satoshis_per_kvb()
    );
    println!("Signed transaction ID: {}", summary.transaction_id);
    println!("Signed virtual size: {} vbytes", summary.virtual_size);
    println!("Signed bytes and per-deposit allocations were persisted before exit.");
    println!("NO BROADCAST: this offline demo uses fictional confirmed UTXOs and no node.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_remainder_is_order_independent_and_uses_deposit_id_for_ties() {
        let forward = allocate_fee(
            &[
                (DepositId("deposit-b".to_owned()), 10),
                (DepositId("deposit-a".to_owned()), 10),
                (DepositId("deposit-c".to_owned()), 10),
            ],
            2,
        )
        .expect("valid fee must allocate");
        let reverse = allocate_fee(
            &[
                (DepositId("deposit-c".to_owned()), 10),
                (DepositId("deposit-b".to_owned()), 10),
                (DepositId("deposit-a".to_owned()), 10),
            ],
            2,
        )
        .expect("reordered fee must allocate");

        assert_eq!(forward, reverse);
        assert_eq!(
            forward
                .iter()
                .map(|share| {
                    (
                        share.deposit_id.0.as_str(),
                        share.allocated_fee,
                        share.master_credit,
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                ("deposit-a", 1, 9),
                ("deposit-b", 1, 9),
                ("deposit-c", 0, 10)
            ]
        );
    }

    #[test]
    fn largest_remainder_conserves_every_small_valid_fee() {
        for gross_a in 1..=8_u64 {
            for gross_b in 1..=8_u64 {
                let gross = gross_a + gross_b;
                for fee in 0..gross {
                    let result = allocate_fee(
                        &[
                            (DepositId("a".to_owned()), gross_a),
                            (DepositId("b".to_owned()), gross_b),
                        ],
                        fee,
                    );
                    match result {
                        Ok(shares) => {
                            assert_eq!(
                                shares.iter().map(|share| share.allocated_fee).sum::<u64>(),
                                fee
                            );
                            assert_eq!(
                                shares.iter().map(|share| share.master_credit).sum::<u64>(),
                                gross - fee
                            );
                        }
                        Err(error) => assert_eq!(
                            error.to_string(),
                            "Bitcoin fee allocation leaves no master credit"
                        ),
                    }
                }
            }
        }
    }

    #[test]
    fn evidence_is_versioned_deterministic_and_binds_the_exact_outpoint() {
        let utxo = BitcoinUtxo {
            transaction_id: [0x11; 32],
            output_index: 7,
            value: Satoshi(42_000),
            script_pubkey: vec![0, 20, 1, 2, 3],
            satisfaction_weight: 109,
            key: signer::KeyLocator::Identifier("test-only-opaque-key".to_owned()),
        };
        let first = utxo_evidence(&utxo).expect("evidence must encode");
        let second = utxo_evidence(&utxo).expect("same evidence must encode");
        assert_eq!(first, second);
        assert!(first.starts_with(b"bitcoin-ps-utxo-evidence-v1\0"));

        let mut changed = utxo;
        changed.output_index += 1;
        assert_ne!(
            first,
            utxo_evidence(&changed).expect("changed evidence must encode")
        );
    }

    #[tokio::test]
    async fn offline_workflow_persists_signed_state_without_broadcast() {
        let summary = execute_demo().await.expect("offline workflow must succeed");

        assert_eq!(summary.participant_count, 2);
        assert_eq!(summary.input_count, 2);
        assert_eq!(summary.output_count, 1);
        assert_eq!(summary.gross, 200_000);
        assert_eq!(summary.gross, summary.fee + summary.net);
        assert!(summary.fee > 0);
        assert!(summary.virtual_size > 0);
    }
}
