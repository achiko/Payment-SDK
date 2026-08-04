use bincode::{Decode, Encode};
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use indexing::{
    BlockHash, BlockHeight, BlockRef, ConfirmationProof, EventCursor, IndexScope, MovementId,
    MovementKind, NetworkFee, ObservationEvent, ObservationEventId, ObservationRevision,
    ObservedTransaction, TransactionStatus, ValueMovement, WatchId,
};
use signer::{ChildIndex, DerivationPath, KeyLocator};
use storage::{
    Condition, Key, Namespace, Operation, ScanRequest, Storage, StorageError, StorageErrorKind,
    StoredValue, Value, WriteBatch,
};

use crate::{
    AccountingCommand, AppendObservation, AppendOutcome, ApplyResult, AwaitingWatchPage,
    AwaitingWatchPageRequest, BoxFuture, ConsumerCheckpoint, ConsumerCheckpointName, CreateDeposit,
    CreateDepositWithLedger, CreatedDeposit, Deposit, DepositBalances, DepositError,
    DepositErrorKind, DepositId, DepositLedger, DepositState, DepositStore, IdempotencyKey,
    LedgerEntry, LedgerEntryCause, LedgerEntryId, LedgerObservationKind, LedgerPage,
    LedgerPageRequest, MirrorObservation, MirrorOutcome, MirroredObservation,
    ObservationConsumerCheckpoints, ObservationEventLog, ObservationLogPage, ObservationLogRequest,
    OpenLedger, ProjectObservation, ProjectionId, ProjectionOutcome, ReconciliationCase,
    ReconciliationCaseId, ReconciliationPage, ReconciliationPageRequest, ReconciliationReason,
    ReconciliationState, ReconciliationStore, RecordObservationBalance, UserId,
};

const RECORD_VERSION: u16 = 1;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

fn ns(value: &str) -> Namespace {
    Namespace(value.to_owned())
}

fn deposit_ns() -> Namespace {
    ns("ps.v1.deposit")
}

fn deposit_address_ns() -> Namespace {
    ns("ps.v1.deposit_address")
}

fn deposit_idempotency_ns() -> Namespace {
    ns("ps.v1.deposit_idem")
}

fn awaiting_watch_ns() -> Namespace {
    ns("ps.v1.awaiting_watch")
}

fn ledger_head_ns() -> Namespace {
    ns("ps.v1.ledger_head")
}

fn ledger_entry_ns() -> Namespace {
    ns("ps.v1.ledger_entry")
}

fn projection_ns() -> Namespace {
    ns("ps.v1.projection")
}

fn accounting_idempotency_ns() -> Namespace {
    ns("ps.v1.accounting_idem")
}

fn observation_ns() -> Namespace {
    ns("ps.v1.observation")
}

fn observation_cursor_ns() -> Namespace {
    ns("ps.v1.observation_cursor")
}

fn consumer_checkpoint_ns() -> Namespace {
    ns("ps.v1.consumer_checkpoint")
}

fn reconciliation_ns() -> Namespace {
    ns("ps.v1.reconciliation")
}

fn reconciliation_deposit_ns() -> Namespace {
    ns("ps.v1.reconciliation_deposit")
}

fn key_text(value: &str) -> Key {
    Key(value.as_bytes().to_vec())
}

fn component_key(parts: &[&[u8]]) -> Result<Key, DepositError> {
    let mut output = Vec::new();
    for part in parts {
        let length = u32::try_from(part.len()).map_err(|_| invalid("key component is too long"))?;
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(part);
    }
    Ok(Key(output))
}

fn address_key(address: &CanonicalAddress) -> Result<Key, DepositError> {
    component_key(&[address.chain.0.as_bytes(), address.value.as_bytes()])
}

fn ledger_entry_key(deposit_id: &DepositId, entry_id: &LedgerEntryId) -> Result<Key, DepositError> {
    component_key(&[deposit_id.0.as_bytes(), entry_id.0.as_bytes()])
}

fn ledger_prefix(deposit_id: &DepositId) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[deposit_id.0.as_bytes()])?.0)
}

fn reconciliation_deposit_key(
    deposit_id: &DepositId,
    case_id: &ReconciliationCaseId,
) -> Result<Key, DepositError> {
    component_key(&[deposit_id.0.as_bytes(), case_id.0.as_bytes()])
}

fn reconciliation_deposit_prefix(deposit_id: &DepositId) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[deposit_id.0.as_bytes()])?.0)
}

fn cursor_key(cursor: EventCursor) -> Key {
    Key(cursor.0.to_be_bytes().to_vec())
}

fn checkpoint_key(name: ConsumerCheckpointName) -> Key {
    key_text(match name {
        ConsumerCheckpointName::IxIngestion => "ix_ingestion",
        ConsumerCheckpointName::IxProjection => "ix_projection",
    })
}

fn encode<T: Encode>(record: &T) -> Result<Value, DepositError> {
    bincode::encode_to_vec(
        record,
        bincode::config::standard()
            .with_fixed_int_encoding()
            .with_big_endian(),
    )
    .map(Value)
    .map_err(|error| storage_error(format!("failed to encode PS RecordV1: {error}")))
}

fn decode<T>(stored: &StoredValue) -> Result<T, DepositError>
where
    T: Decode<()>,
{
    let (record, consumed) = bincode::decode_from_slice::<T, _>(
        &stored.value.0,
        bincode::config::standard()
            .with_fixed_int_encoding()
            .with_big_endian()
            .with_limit::<MAX_RECORD_BYTES>(),
    )
    .map_err(|error| storage_error(format!("failed to decode PS RecordV1: {error}")))?;
    if consumed != stored.value.0.len() {
        return Err(storage_error("PS RecordV1 contains trailing bytes"));
    }
    Ok(record)
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct BlockRecordV1 {
    height: u64,
    hash: Vec<u8>,
    parent_hash: Option<Vec<u8>>,
    timestamp: Option<u64>,
}

impl From<&BlockRef> for BlockRecordV1 {
    fn from(value: &BlockRef) -> Self {
        Self {
            height: value.height.0,
            hash: value.hash.0.clone(),
            parent_hash: value.parent_hash.as_ref().map(|hash| hash.0.clone()),
            timestamp: value.timestamp,
        }
    }
}

impl From<BlockRecordV1> for BlockRef {
    fn from(value: BlockRecordV1) -> Self {
        Self {
            height: BlockHeight(value.height),
            hash: BlockHash(value.hash),
            parent_hash: value.parent_hash.map(BlockHash),
            timestamp: value.timestamp,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum StatusRecordV1 {
    Pending,
    Included {
        block: BlockRecordV1,
        confirmations: u64,
    },
    Confirmed {
        block: BlockRecordV1,
        proof: ConfirmationProofRecordV1,
    },
    Failed {
        block: Option<BlockRecordV1>,
        reason: Option<String>,
    },
    Replaced {
        chain: String,
        transaction_id: String,
    },
    Dropped,
    Reorged {
        previous_block: BlockRecordV1,
    },
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum ConfirmationProofRecordV1 {
    Depth { required: u64, observed: u64 },
    ChainFinalized,
    DepthAndChainFinalized { required: u64, observed: u64 },
}

impl From<&ConfirmationProof> for ConfirmationProofRecordV1 {
    fn from(value: &ConfirmationProof) -> Self {
        match value {
            ConfirmationProof::Depth { required, observed } => Self::Depth {
                required: *required,
                observed: *observed,
            },
            ConfirmationProof::ChainFinalized => Self::ChainFinalized,
            ConfirmationProof::DepthAndChainFinalized { required, observed } => {
                Self::DepthAndChainFinalized {
                    required: *required,
                    observed: *observed,
                }
            }
        }
    }
}

impl From<ConfirmationProofRecordV1> for ConfirmationProof {
    fn from(value: ConfirmationProofRecordV1) -> Self {
        match value {
            ConfirmationProofRecordV1::Depth { required, observed } => {
                Self::Depth { required, observed }
            }
            ConfirmationProofRecordV1::ChainFinalized => Self::ChainFinalized,
            ConfirmationProofRecordV1::DepthAndChainFinalized { required, observed } => {
                Self::DepthAndChainFinalized { required, observed }
            }
        }
    }
}

impl From<&TransactionStatus> for StatusRecordV1 {
    fn from(value: &TransactionStatus) -> Self {
        match value {
            TransactionStatus::Pending => Self::Pending,
            TransactionStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: block.into(),
                confirmations: *confirmations,
            },
            TransactionStatus::Confirmed { block, proof } => Self::Confirmed {
                block: block.into(),
                proof: proof.into(),
            },
            TransactionStatus::Failed { block, reason } => Self::Failed {
                block: block.as_ref().map(Into::into),
                reason: reason.clone(),
            },
            TransactionStatus::Replaced { by } => Self::Replaced {
                chain: by.chain.0.clone(),
                transaction_id: by.value.clone(),
            },
            TransactionStatus::Dropped => Self::Dropped,
            TransactionStatus::Reorged { previous_block } => Self::Reorged {
                previous_block: previous_block.into(),
            },
        }
    }
}

impl From<StatusRecordV1> for TransactionStatus {
    fn from(value: StatusRecordV1) -> Self {
        match value {
            StatusRecordV1::Pending => Self::Pending,
            StatusRecordV1::Included {
                block,
                confirmations,
            } => Self::Included {
                block: block.into(),
                confirmations,
            },
            StatusRecordV1::Confirmed { block, proof } => Self::Confirmed {
                block: block.into(),
                proof: proof.into(),
            },
            StatusRecordV1::Failed { block, reason } => Self::Failed {
                block: block.map(Into::into),
                reason,
            },
            StatusRecordV1::Replaced {
                chain,
                transaction_id,
            } => Self::Replaced {
                by: CanonicalTransactionId {
                    chain: ChainId(chain),
                    value: transaction_id,
                },
            },
            StatusRecordV1::Dropped => Self::Dropped,
            StatusRecordV1::Reorged { previous_block } => Self::Reorged {
                previous_block: previous_block.into(),
            },
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct MovementRecordV1 {
    id: String,
    asset_chain: String,
    asset: String,
    amount: [u8; 32],
    from: Option<AddressRecordV1>,
    to: Option<AddressRecordV1>,
    kind: u8,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct AddressRecordV1 {
    chain: String,
    value: String,
}

impl From<&CanonicalAddress> for AddressRecordV1 {
    fn from(value: &CanonicalAddress) -> Self {
        Self {
            chain: value.chain.0.clone(),
            value: value.value.clone(),
        }
    }
}

impl From<AddressRecordV1> for CanonicalAddress {
    fn from(value: AddressRecordV1) -> Self {
        Self {
            chain: ChainId(value.chain),
            value: value.value,
        }
    }
}

fn movement_kind_to_tag(kind: MovementKind) -> u8 {
    match kind {
        MovementKind::Transfer => 0,
        MovementKind::Input => 1,
        MovementKind::Output => 2,
        MovementKind::InternalTransfer => 3,
        MovementKind::Mint => 4,
        MovementKind::Burn => 5,
    }
}

fn movement_kind_from_tag(tag: u8) -> Result<MovementKind, DepositError> {
    match tag {
        0 => Ok(MovementKind::Transfer),
        1 => Ok(MovementKind::Input),
        2 => Ok(MovementKind::Output),
        3 => Ok(MovementKind::InternalTransfer),
        4 => Ok(MovementKind::Mint),
        5 => Ok(MovementKind::Burn),
        _ => Err(storage_error("PS movement record has an unknown kind")),
    }
}

impl From<&ValueMovement> for MovementRecordV1 {
    fn from(value: &ValueMovement) -> Self {
        Self {
            id: value.id.0.clone(),
            asset_chain: value.asset.chain.0.clone(),
            asset: value.asset.asset.clone(),
            amount: value.amount.0,
            from: value.from.as_ref().map(Into::into),
            to: value.to.as_ref().map(Into::into),
            kind: movement_kind_to_tag(value.kind),
        }
    }
}

impl TryFrom<MovementRecordV1> for ValueMovement {
    type Error = DepositError;

    fn try_from(value: MovementRecordV1) -> Result<Self, Self::Error> {
        Ok(Self {
            id: MovementId(value.id),
            asset: AssetId {
                chain: ChainId(value.asset_chain),
                asset: value.asset,
            },
            amount: AtomicAmount(value.amount),
            from: value.from.map(Into::into),
            to: value.to.map(Into::into),
            kind: movement_kind_from_tag(value.kind)?,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct FeeRecordV1 {
    asset_chain: String,
    asset: String,
    amount: [u8; 32],
    payer: Option<AddressRecordV1>,
}

impl From<&NetworkFee> for FeeRecordV1 {
    fn from(value: &NetworkFee) -> Self {
        Self {
            asset_chain: value.asset.chain.0.clone(),
            asset: value.asset.asset.clone(),
            amount: value.amount.0,
            payer: value.payer.as_ref().map(Into::into),
        }
    }
}

impl From<FeeRecordV1> for NetworkFee {
    fn from(value: FeeRecordV1) -> Self {
        Self {
            asset: AssetId {
                chain: ChainId(value.asset_chain),
                asset: value.asset,
            },
            amount: AtomicAmount(value.amount),
            payer: value.payer.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct ObservedTransactionRecordV1 {
    scope_chain: String,
    scope_network: String,
    transaction_chain: String,
    transaction_id: String,
    revision: u64,
    status: StatusRecordV1,
    movements: Vec<MovementRecordV1>,
    fee: Option<FeeRecordV1>,
    first_seen_at: u64,
    observed_at: u64,
}

impl From<&ObservedTransaction> for ObservedTransactionRecordV1 {
    fn from(value: &ObservedTransaction) -> Self {
        Self {
            scope_chain: value.scope.chain.0.clone(),
            scope_network: value.scope.network.clone(),
            transaction_chain: value.transaction_id.chain.0.clone(),
            transaction_id: value.transaction_id.value.clone(),
            revision: value.revision.0,
            status: (&value.status).into(),
            movements: value.movements.iter().map(Into::into).collect(),
            fee: value.fee.as_ref().map(Into::into),
            first_seen_at: value.first_seen_at,
            observed_at: value.observed_at,
        }
    }
}

impl TryFrom<ObservedTransactionRecordV1> for ObservedTransaction {
    type Error = DepositError;

    fn try_from(value: ObservedTransactionRecordV1) -> Result<Self, Self::Error> {
        Ok(Self {
            scope: IndexScope {
                chain: ChainId(value.scope_chain),
                network: value.scope_network,
            },
            transaction_id: CanonicalTransactionId {
                chain: ChainId(value.transaction_chain),
                value: value.transaction_id,
            },
            revision: ObservationRevision(value.revision),
            status: value.status.into(),
            movements: value
                .movements
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            fee: value.fee.map(Into::into),
            first_seen_at: value.first_seen_at,
            observed_at: value.observed_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct ObservationEventRecordV1 {
    version: u16,
    id: String,
    cursor: u64,
    watch_ids: Vec<String>,
    previous_status: Option<StatusRecordV1>,
    transaction: ObservedTransactionRecordV1,
    received_at: u64,
}

impl From<&MirroredObservation> for ObservationEventRecordV1 {
    fn from(value: &MirroredObservation) -> Self {
        Self {
            version: RECORD_VERSION,
            id: value.event.id.0.clone(),
            cursor: value.event.cursor.0,
            watch_ids: value
                .event
                .watch_ids
                .iter()
                .map(|id| id.0.clone())
                .collect(),
            previous_status: value.event.previous_status.as_ref().map(Into::into),
            transaction: (&value.event.transaction).into(),
            received_at: value.received_at,
        }
    }
}

impl TryFrom<ObservationEventRecordV1> for MirroredObservation {
    type Error = DepositError;

    fn try_from(value: ObservationEventRecordV1) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        Ok(Self {
            event: ObservationEvent {
                id: ObservationEventId(value.id),
                cursor: EventCursor(value.cursor),
                watch_ids: value.watch_ids.into_iter().map(WatchId).collect(),
                previous_status: value.previous_status.map(Into::into),
                transaction: value.transaction.try_into()?,
            },
            received_at: value.received_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum KeyLocatorRecordV1 {
    Identifier(String),
    DerivationPath(Vec<(u32, bool)>),
}

impl From<&KeyLocator> for KeyLocatorRecordV1 {
    fn from(value: &KeyLocator) -> Self {
        match value {
            KeyLocator::Identifier(value) => Self::Identifier(value.clone()),
            KeyLocator::DerivationPath(path) => Self::DerivationPath(
                path.0
                    .iter()
                    .map(|child| (child.index, child.hardened))
                    .collect(),
            ),
        }
    }
}

impl From<KeyLocatorRecordV1> for KeyLocator {
    fn from(value: KeyLocatorRecordV1) -> Self {
        match value {
            KeyLocatorRecordV1::Identifier(value) => Self::Identifier(value),
            KeyLocatorRecordV1::DerivationPath(path) => Self::DerivationPath(DerivationPath(
                path.into_iter()
                    .map(|(index, hardened)| ChildIndex { index, hardened })
                    .collect(),
            )),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum DepositStateRecordV1 {
    AwaitingWatch,
    Active(String),
    Expired,
    Closed,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct DepositRecordV1 {
    version: u16,
    id: String,
    idempotency_key: String,
    user_id: String,
    asset_chain: String,
    asset: String,
    address: AddressRecordV1,
    key: KeyLocatorRecordV1,
    expected: [u8; 32],
    birthday: u64,
    expires_at: u64,
    state: DepositStateRecordV1,
    created_at: u64,
}

impl From<&Deposit> for DepositRecordV1 {
    fn from(value: &Deposit) -> Self {
        Self {
            version: RECORD_VERSION,
            id: value.id.0.clone(),
            idempotency_key: value.idempotency_key.0.clone(),
            user_id: value.user_id.0.clone(),
            asset_chain: value.asset.chain.0.clone(),
            asset: value.asset.asset.clone(),
            address: (&value.address).into(),
            key: (&value.key).into(),
            expected: value.expected.0,
            birthday: value.birthday.0,
            expires_at: value.expires_at,
            state: match &value.state {
                DepositState::AwaitingWatch => DepositStateRecordV1::AwaitingWatch,
                DepositState::Active { watch_id } => {
                    DepositStateRecordV1::Active(watch_id.0.clone())
                }
                DepositState::Expired => DepositStateRecordV1::Expired,
                DepositState::Closed => DepositStateRecordV1::Closed,
            },
            created_at: value.created_at,
        }
    }
}

impl TryFrom<DepositRecordV1> for Deposit {
    type Error = DepositError;

    fn try_from(value: DepositRecordV1) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        Ok(Self {
            id: DepositId(value.id),
            idempotency_key: IdempotencyKey(value.idempotency_key),
            user_id: UserId(value.user_id),
            asset: AssetId {
                chain: ChainId(value.asset_chain),
                asset: value.asset,
            },
            address: value.address.into(),
            key: value.key.into(),
            expected: AtomicAmount(value.expected),
            birthday: BlockHeight(value.birthday),
            expires_at: value.expires_at,
            state: match value.state {
                DepositStateRecordV1::AwaitingWatch => DepositState::AwaitingWatch,
                DepositStateRecordV1::Active(watch_id) => DepositState::Active {
                    watch_id: WatchId(watch_id),
                },
                DepositStateRecordV1::Expired => DepositState::Expired,
                DepositStateRecordV1::Closed => DepositState::Closed,
            },
            created_at: value.created_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct BalancesRecordV1 {
    received: [u8; 32],
    confirmed: [u8; 32],
    balance: [u8; 32],
    collected: [u8; 32],
    accounted: [u8; 32],
}

impl From<DepositBalances> for BalancesRecordV1 {
    fn from(value: DepositBalances) -> Self {
        Self {
            received: value.received.0,
            confirmed: value.confirmed.0,
            balance: value.balance.0,
            collected: value.collected.0,
            accounted: value.accounted.0,
        }
    }
}

impl From<BalancesRecordV1> for DepositBalances {
    fn from(value: BalancesRecordV1) -> Self {
        Self {
            received: AtomicAmount(value.received),
            confirmed: AtomicAmount(value.confirmed),
            balance: AtomicAmount(value.balance),
            collected: AtomicAmount(value.collected),
            accounted: AtomicAmount(value.accounted),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum LedgerCauseRecordV1 {
    Opened {
        idempotency_key: String,
    },
    Observation {
        projection_id: String,
        event_id: String,
        revision: u64,
        status: StatusRecordV1,
        kind: u8,
        movement_ids: Vec<String>,
    },
    Accounting {
        idempotency_key: String,
    },
}

fn ledger_kind_to_tag(kind: LedgerObservationKind) -> u8 {
    match kind {
        LedgerObservationKind::Incoming => 0,
        LedgerObservationKind::Collection => 1,
        LedgerObservationKind::GasFunding => 2,
        LedgerObservationKind::OtherBalanceChange => 3,
        LedgerObservationKind::Reorg => 4,
    }
}

fn ledger_kind_from_tag(tag: u8) -> Result<LedgerObservationKind, DepositError> {
    match tag {
        0 => Ok(LedgerObservationKind::Incoming),
        1 => Ok(LedgerObservationKind::Collection),
        2 => Ok(LedgerObservationKind::GasFunding),
        3 => Ok(LedgerObservationKind::OtherBalanceChange),
        4 => Ok(LedgerObservationKind::Reorg),
        _ => Err(storage_error(
            "PS ledger record has an unknown observation kind",
        )),
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct LedgerEntryRecordV1 {
    version: u16,
    id: String,
    deposit_id: String,
    previous: Option<String>,
    cause: LedgerCauseRecordV1,
    balances: BalancesRecordV1,
    recorded_at: u64,
}

impl From<&LedgerEntry> for LedgerEntryRecordV1 {
    fn from(value: &LedgerEntry) -> Self {
        let cause = match &value.cause {
            LedgerEntryCause::Opened { idempotency_key } => LedgerCauseRecordV1::Opened {
                idempotency_key: idempotency_key.0.clone(),
            },
            LedgerEntryCause::Observation {
                projection_id,
                event_id,
                observation_revision,
                status,
                kind,
                movement_ids,
            } => LedgerCauseRecordV1::Observation {
                projection_id: projection_id.0.clone(),
                event_id: event_id.0.clone(),
                revision: observation_revision.0,
                status: status.into(),
                kind: ledger_kind_to_tag(*kind),
                movement_ids: movement_ids.iter().map(|id| id.0.clone()).collect(),
            },
            LedgerEntryCause::Accounting { idempotency_key } => LedgerCauseRecordV1::Accounting {
                idempotency_key: idempotency_key.0.clone(),
            },
        };
        Self {
            version: RECORD_VERSION,
            id: value.id.0.clone(),
            deposit_id: value.deposit_id.0.clone(),
            previous: value.previous.as_ref().map(|id| id.0.clone()),
            cause,
            balances: value.balances.into(),
            recorded_at: value.recorded_at,
        }
    }
}

impl TryFrom<LedgerEntryRecordV1> for LedgerEntry {
    type Error = DepositError;

    fn try_from(value: LedgerEntryRecordV1) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        let cause = match value.cause {
            LedgerCauseRecordV1::Opened { idempotency_key } => LedgerEntryCause::Opened {
                idempotency_key: IdempotencyKey(idempotency_key),
            },
            LedgerCauseRecordV1::Observation {
                projection_id,
                event_id,
                revision,
                status,
                kind,
                movement_ids,
            } => LedgerEntryCause::Observation {
                projection_id: ProjectionId(projection_id),
                event_id: ObservationEventId(event_id),
                observation_revision: ObservationRevision(revision),
                status: status.into(),
                kind: ledger_kind_from_tag(kind)?,
                movement_ids: movement_ids.into_iter().map(MovementId).collect(),
            },
            LedgerCauseRecordV1::Accounting { idempotency_key } => LedgerEntryCause::Accounting {
                idempotency_key: IdempotencyKey(idempotency_key),
            },
        };
        Ok(Self {
            id: LedgerEntryId(value.id),
            deposit_id: DepositId(value.deposit_id),
            previous: value.previous.map(LedgerEntryId),
            cause,
            balances: value.balances.into(),
            recorded_at: value.recorded_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum ReconciliationReasonRecordV1 {
    PostCreditReorg {
        accounted: [u8; 32],
        corrected_confirmed: [u8; 32],
    },
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum ReconciliationStateRecordV1 {
    Open,
    Resolved {
        resolution: String,
        resolved_at: u64,
    },
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct ReconciliationRecordV1 {
    version: u16,
    id: String,
    deposit_id: String,
    triggering_event_id: String,
    reason: ReconciliationReasonRecordV1,
    state: ReconciliationStateRecordV1,
    created_at: u64,
}

impl From<&ReconciliationCase> for ReconciliationRecordV1 {
    fn from(value: &ReconciliationCase) -> Self {
        let reason = match &value.reason {
            ReconciliationReason::PostCreditReorg {
                accounted,
                corrected_confirmed,
            } => ReconciliationReasonRecordV1::PostCreditReorg {
                accounted: accounted.0,
                corrected_confirmed: corrected_confirmed.0,
            },
        };
        let state = match &value.state {
            ReconciliationState::Open => ReconciliationStateRecordV1::Open,
            ReconciliationState::Resolved {
                resolution,
                resolved_at,
            } => ReconciliationStateRecordV1::Resolved {
                resolution: resolution.clone(),
                resolved_at: *resolved_at,
            },
        };
        Self {
            version: RECORD_VERSION,
            id: value.id.0.clone(),
            deposit_id: value.deposit_id.0.clone(),
            triggering_event_id: value.triggering_event_id.0.clone(),
            reason,
            state,
            created_at: value.created_at,
        }
    }
}

impl TryFrom<ReconciliationRecordV1> for ReconciliationCase {
    type Error = DepositError;

    fn try_from(value: ReconciliationRecordV1) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        Ok(Self {
            id: ReconciliationCaseId(value.id),
            deposit_id: DepositId(value.deposit_id),
            triggering_event_id: ObservationEventId(value.triggering_event_id),
            reason: match value.reason {
                ReconciliationReasonRecordV1::PostCreditReorg {
                    accounted,
                    corrected_confirmed,
                } => ReconciliationReason::PostCreditReorg {
                    accounted: AtomicAmount(accounted),
                    corrected_confirmed: AtomicAmount(corrected_confirmed),
                },
            },
            state: match value.state {
                ReconciliationStateRecordV1::Open => ReconciliationState::Open,
                ReconciliationStateRecordV1::Resolved {
                    resolution,
                    resolved_at,
                } => ReconciliationState::Resolved {
                    resolution,
                    resolved_at,
                },
            },
            created_at: value.created_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct IdRecordV1 {
    version: u16,
    id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct CursorRecordV1 {
    version: u16,
    cursor: Option<u64>,
}

/// Real PS semantic repository over the backend-independent atomic storage API.
#[derive(Clone, Debug)]
pub struct PersistentPaymentRepository<S> {
    storage: S,
}

impl<S> PersistentPaymentRepository<S> {
    #[must_use]
    pub const fn new(storage: S) -> Self {
        Self { storage }
    }

    #[must_use]
    pub const fn storage(&self) -> &S {
        &self.storage
    }
}

fn deposit_from_create(command: &CreateDeposit) -> Deposit {
    Deposit {
        id: command.id.clone(),
        idempotency_key: command.idempotency_key.clone(),
        user_id: command.user_id.clone(),
        asset: command.asset.clone(),
        address: command.address.clone(),
        key: command.key.clone(),
        expected: command.expected,
        birthday: command.birthday,
        expires_at: command.expires_at,
        state: DepositState::AwaitingWatch,
        created_at: command.created_at,
    }
}

fn open_entry(command: &CreateDepositWithLedger) -> LedgerEntry {
    LedgerEntry {
        id: LedgerEntryId(format!("open:{}", command.deposit.id.0)),
        deposit_id: command.deposit.id.clone(),
        previous: None,
        cause: LedgerEntryCause::Opened {
            idempotency_key: command.deposit.idempotency_key.clone(),
        },
        balances: DepositBalances::default(),
        recorded_at: command.ledger_recorded_at,
    }
}

fn ensure_version(version: u16) -> Result<(), DepositError> {
    if version == RECORD_VERSION {
        Ok(())
    } else {
        Err(storage_error(format!(
            "unsupported PS record version {version}"
        )))
    }
}

fn map_storage(error: StorageError) -> DepositError {
    let kind = match error.kind {
        StorageErrorKind::Conflict => DepositErrorKind::Conflict,
        StorageErrorKind::CorruptData => DepositErrorKind::InvariantViolation,
        StorageErrorKind::InvalidRequest => DepositErrorKind::InvariantViolation,
        StorageErrorKind::Unavailable | StorageErrorKind::Other => DepositErrorKind::Storage,
    };
    DepositError {
        kind,
        message: error.message,
    }
}

fn invalid(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}

fn conflict(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::Conflict,
        message: message.into(),
    }
}

fn not_found(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::NotFound,
        message: message.into(),
    }
}

fn invalid_state(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvalidState,
        message: message.into(),
    }
}

fn storage_error(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::Storage,
        message: message.into(),
    }
}

impl<S> PersistentPaymentRepository<S>
where
    S: Storage,
{
    async fn stored_deposit(
        &self,
        id: &DepositId,
    ) -> Result<Option<(Deposit, StoredValue)>, DepositError> {
        let stored = self
            .storage
            .get(&deposit_ns(), &key_text(&id.0))
            .await
            .map_err(map_storage)?;
        stored
            .map(|stored| {
                let record: DepositRecordV1 = decode(&stored)?;
                Ok((record.try_into()?, stored))
            })
            .transpose()
    }

    async fn stored_ledger_entry(
        &self,
        deposit_id: &DepositId,
        entry_id: &LedgerEntryId,
    ) -> Result<Option<LedgerEntry>, DepositError> {
        self.storage
            .get(&ledger_entry_ns(), &ledger_entry_key(deposit_id, entry_id)?)
            .await
            .map_err(map_storage)?
            .map(|stored| decode::<LedgerEntryRecordV1>(&stored)?.try_into())
            .transpose()
    }

    async fn stored_head(
        &self,
        deposit_id: &DepositId,
    ) -> Result<Option<(LedgerEntry, StoredValue)>, DepositError> {
        let Some(stored_head) = self
            .storage
            .get(&ledger_head_ns(), &key_text(&deposit_id.0))
            .await
            .map_err(map_storage)?
        else {
            return Ok(None);
        };
        let head: IdRecordV1 = decode(&stored_head)?;
        ensure_version(head.version)?;
        let entry_id = LedgerEntryId(head.id);
        let entry = self
            .stored_ledger_entry(deposit_id, &entry_id)
            .await?
            .ok_or_else(|| storage_error("PS ledger head points to a missing immutable entry"))?;
        Ok(Some((entry, stored_head)))
    }

    async fn idempotent_deposit(
        &self,
        command: &CreateDeposit,
    ) -> Result<Option<Deposit>, DepositError> {
        let Some(stored) = self
            .storage
            .get(
                &deposit_idempotency_ns(),
                &key_text(&command.idempotency_key.0),
            )
            .await
            .map_err(map_storage)?
        else {
            return Ok(None);
        };
        let index: IdRecordV1 = decode(&stored)?;
        ensure_version(index.version)?;
        let deposit = self
            .stored_deposit(&DepositId(index.id))
            .await?
            .map(|(deposit, _)| deposit)
            .ok_or_else(|| storage_error("deposit idempotency index points to a missing record"))?;
        let expected = deposit_from_create(command);
        if deposit == expected {
            Ok(Some(deposit))
        } else {
            Err(conflict(
                "deposit idempotency key was reused with a different request",
            ))
        }
    }

    async fn store_new_deposit(
        &self,
        deposit: &Deposit,
        ledger: Option<&LedgerEntry>,
    ) -> Result<(), DepositError> {
        let deposit_key = key_text(&deposit.id.0);
        let address_key = address_key(&deposit.address)?;
        let idempotency_key = key_text(&deposit.idempotency_key.0);
        let awaiting_key = key_text(&deposit.id.0);
        let mut conditions = vec![
            Condition::Missing {
                namespace: deposit_ns(),
                key: deposit_key.clone(),
            },
            Condition::Missing {
                namespace: deposit_address_ns(),
                key: address_key.clone(),
            },
            Condition::Missing {
                namespace: deposit_idempotency_ns(),
                key: idempotency_key.clone(),
            },
            Condition::Missing {
                namespace: awaiting_watch_ns(),
                key: awaiting_key.clone(),
            },
        ];
        let id_record = IdRecordV1 {
            version: RECORD_VERSION,
            id: deposit.id.0.clone(),
        };
        let mut operations = vec![
            Operation::Put {
                namespace: deposit_ns(),
                key: deposit_key,
                value: encode(&DepositRecordV1::from(deposit))?,
            },
            Operation::Put {
                namespace: deposit_address_ns(),
                key: address_key,
                value: encode(&id_record)?,
            },
            Operation::Put {
                namespace: deposit_idempotency_ns(),
                key: idempotency_key,
                value: encode(&id_record)?,
            },
            Operation::Put {
                namespace: awaiting_watch_ns(),
                key: awaiting_key,
                value: encode(&id_record)?,
            },
        ];
        if let Some(ledger) = ledger {
            let head_key = key_text(&deposit.id.0);
            let entry_key = ledger_entry_key(&deposit.id, &ledger.id)?;
            conditions.extend([
                Condition::Missing {
                    namespace: ledger_head_ns(),
                    key: head_key.clone(),
                },
                Condition::Missing {
                    namespace: ledger_entry_ns(),
                    key: entry_key.clone(),
                },
            ]);
            operations.extend([
                Operation::Put {
                    namespace: ledger_entry_ns(),
                    key: entry_key,
                    value: encode(&LedgerEntryRecordV1::from(ledger))?,
                },
                Operation::Put {
                    namespace: ledger_head_ns(),
                    key: head_key,
                    value: encode(&IdRecordV1 {
                        version: RECORD_VERSION,
                        id: ledger.id.0.clone(),
                    })?,
                },
            ]);
        }
        self.storage
            .commit(WriteBatch {
                conditions,
                operations,
            })
            .await
            .map_err(map_storage)?;
        Ok(())
    }

    async fn mirrored_observation(
        &self,
        event_id: &ObservationEventId,
    ) -> Result<Option<(MirroredObservation, StoredValue)>, DepositError> {
        self.storage
            .get(&observation_ns(), &key_text(&event_id.0))
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: ObservationEventRecordV1 = decode(&stored)?;
                Ok((record.try_into()?, stored))
            })
            .transpose()
    }

    async fn stored_checkpoint(
        &self,
        name: ConsumerCheckpointName,
    ) -> Result<(ConsumerCheckpoint, Option<StoredValue>), DepositError> {
        let stored = self
            .storage
            .get(&consumer_checkpoint_ns(), &checkpoint_key(name))
            .await
            .map_err(map_storage)?;
        let checkpoint = match &stored {
            Some(stored) => {
                let record: CursorRecordV1 = decode(stored)?;
                ensure_version(record.version)?;
                ConsumerCheckpoint {
                    name,
                    cursor: record.cursor.map(EventCursor),
                }
            }
            None => ConsumerCheckpoint { name, cursor: None },
        };
        Ok((checkpoint, stored))
    }
}

fn validate_new_deposit(command: &CreateDeposit) -> Result<(), DepositError> {
    if command.id.0.is_empty()
        || command.idempotency_key.0.is_empty()
        || command.address.value.is_empty()
    {
        return Err(invalid(
            "deposit ID, idempotency key, and canonical address must be non-empty",
        ));
    }
    if command.asset.chain != command.address.chain {
        return Err(invalid(
            "deposit asset and address must belong to the same chain",
        ));
    }
    if command.expires_at < command.created_at {
        return Err(invalid("deposit expiration precedes its creation time"));
    }
    Ok(())
}

fn transition_allowed(current: &DepositState, next: &DepositState) -> bool {
    current == next
        || matches!(
            (current, next),
            (DepositState::AwaitingWatch, DepositState::Active { .. })
                | (DepositState::AwaitingWatch, DepositState::Expired)
                | (DepositState::AwaitingWatch, DepositState::Closed)
                | (DepositState::Active { .. }, DepositState::Expired)
                | (DepositState::Active { .. }, DepositState::Closed)
                | (DepositState::Expired, DepositState::Closed)
        )
}

impl<S> DepositStore for PersistentPaymentRepository<S>
where
    S: Storage,
{
    fn create_with_ledger<'a>(
        &'a self,
        command: CreateDepositWithLedger,
    ) -> BoxFuture<'a, Result<CreatedDeposit, DepositError>> {
        Box::pin(async move {
            validate_new_deposit(&command.deposit)?;
            let ledger = open_entry(&command);
            if let Some(deposit) = self.idempotent_deposit(&command.deposit).await? {
                let existing = self
                    .stored_ledger_entry(&deposit.id, &ledger.id)
                    .await?
                    .ok_or_else(|| {
                        storage_error(
                            "idempotent deposit exists without its required opening ledger row",
                        )
                    })?;
                if existing != ledger {
                    return Err(conflict(
                        "deposit idempotency key resolved to a different opening ledger row",
                    ));
                }
                return Ok(CreatedDeposit {
                    deposit,
                    ledger: existing,
                });
            }

            let deposit = deposit_from_create(&command.deposit);
            match self.store_new_deposit(&deposit, Some(&ledger)).await {
                Ok(()) => Ok(CreatedDeposit { deposit, ledger }),
                Err(error) if error.kind == DepositErrorKind::Conflict => {
                    let existing = self
                        .idempotent_deposit(&command.deposit)
                        .await?
                        .ok_or(error)?;
                    let existing_ledger = self
                        .stored_ledger_entry(&existing.id, &ledger.id)
                        .await?
                        .ok_or_else(|| {
                            storage_error(
                                "idempotent deposit exists without its opening ledger row",
                            )
                        })?;
                    Ok(CreatedDeposit {
                        deposit: existing,
                        ledger: existing_ledger,
                    })
                }
                Err(error) => Err(error),
            }
        })
    }

    fn create<'a>(
        &'a self,
        command: CreateDeposit,
    ) -> BoxFuture<'a, Result<Deposit, DepositError>> {
        Box::pin(async move {
            validate_new_deposit(&command)?;
            if let Some(deposit) = self.idempotent_deposit(&command).await? {
                return Ok(deposit);
            }
            let deposit = deposit_from_create(&command);
            match self.store_new_deposit(&deposit, None).await {
                Ok(()) => Ok(deposit),
                Err(error) if error.kind == DepositErrorKind::Conflict => {
                    self.idempotent_deposit(&command).await?.ok_or(error)
                }
                Err(error) => Err(error),
            }
        })
    }

    fn deposit<'a>(
        &'a self,
        id: &'a DepositId,
    ) -> BoxFuture<'a, Result<Option<Deposit>, DepositError>> {
        Box::pin(async move { Ok(self.stored_deposit(id).await?.map(|(deposit, _)| deposit)) })
    }

    fn by_address<'a>(
        &'a self,
        address: &'a CanonicalAddress,
    ) -> BoxFuture<'a, Result<Option<Deposit>, DepositError>> {
        Box::pin(async move {
            let Some(stored) = self
                .storage
                .get(&deposit_address_ns(), &address_key(address)?)
                .await
                .map_err(map_storage)?
            else {
                return Ok(None);
            };
            let index: IdRecordV1 = decode(&stored)?;
            ensure_version(index.version)?;
            self.deposit(&DepositId(index.id)).await
        })
    }

    fn set_state<'a>(
        &'a self,
        id: &'a DepositId,
        state: DepositState,
    ) -> BoxFuture<'a, Result<(), DepositError>> {
        Box::pin(async move {
            let (mut deposit, stored) = self
                .stored_deposit(id)
                .await?
                .ok_or_else(|| not_found("deposit does not exist"))?;
            if !transition_allowed(&deposit.state, &state) {
                return Err(invalid_state("deposit lifecycle transition is not allowed"));
            }
            if deposit.state == state {
                return Ok(());
            }
            let was_awaiting = deposit.state == DepositState::AwaitingWatch;
            let is_awaiting = state == DepositState::AwaitingWatch;
            deposit.state = state;
            let mut operations = vec![Operation::Put {
                namespace: deposit_ns(),
                key: key_text(&id.0),
                value: encode(&DepositRecordV1::from(&deposit))?,
            }];
            if was_awaiting && !is_awaiting {
                operations.push(Operation::Delete {
                    namespace: awaiting_watch_ns(),
                    key: key_text(&id.0),
                });
            }
            self.storage
                .commit(WriteBatch {
                    conditions: vec![Condition::Version {
                        namespace: deposit_ns(),
                        key: key_text(&id.0),
                        expected: stored.version,
                    }],
                    operations,
                })
                .await
                .map_err(map_storage)?;
            Ok(())
        })
    }

    fn awaiting_watch<'a>(
        &'a self,
        request: AwaitingWatchPageRequest,
    ) -> BoxFuture<'a, Result<AwaitingWatchPage, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > 1_000 {
                return Err(invalid(
                    "AwaitingWatch page size must be between 1 and 1000",
                ));
            }
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: awaiting_watch_ns(),
                    prefix: Vec::new(),
                    after: request.after.as_ref().map(|id| key_text(&id.0)),
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let mut deposits = Vec::with_capacity(page.entries.len());
            for (_, stored) in page.entries {
                let index: IdRecordV1 = decode(&stored)?;
                ensure_version(index.version)?;
                let deposit = self
                    .deposit(&DepositId(index.id))
                    .await?
                    .ok_or_else(|| storage_error("AwaitingWatch index is dangling"))?;
                if deposit.state != DepositState::AwaitingWatch {
                    return Err(storage_error(
                        "AwaitingWatch index references a non-awaiting deposit",
                    ));
                }
                deposits.push(deposit);
            }
            Ok(AwaitingWatchPage {
                deposits,
                next: page
                    .next
                    .map(|key| DepositId(String::from_utf8_lossy(&key.0).into_owned())),
            })
        })
    }

    fn activate_watch<'a>(
        &'a self,
        id: &'a DepositId,
        idempotency_key: &'a IdempotencyKey,
        watch_id: WatchId,
    ) -> BoxFuture<'a, Result<Deposit, DepositError>> {
        Box::pin(async move {
            let (mut deposit, stored) = self
                .stored_deposit(id)
                .await?
                .ok_or_else(|| not_found("deposit does not exist"))?;
            if &deposit.idempotency_key != idempotency_key {
                return Err(conflict(
                    "deposit activation idempotency key does not match creation",
                ));
            }
            match &deposit.state {
                DepositState::Active { watch_id: existing } if existing == &watch_id => {
                    return Ok(deposit);
                }
                DepositState::Active { .. } => {
                    return Err(conflict("deposit is active under a different IX watch"));
                }
                DepositState::AwaitingWatch => {}
                _ => return Err(invalid_state("deposit cannot be activated from its state")),
            }
            deposit.state = DepositState::Active { watch_id };
            self.storage
                .commit(WriteBatch {
                    conditions: vec![Condition::Version {
                        namespace: deposit_ns(),
                        key: key_text(&id.0),
                        expected: stored.version,
                    }],
                    operations: vec![
                        Operation::Put {
                            namespace: deposit_ns(),
                            key: key_text(&id.0),
                            value: encode(&DepositRecordV1::from(&deposit))?,
                        },
                        Operation::Delete {
                            namespace: awaiting_watch_ns(),
                            key: key_text(&id.0),
                        },
                    ],
                })
                .await
                .map_err(map_storage)?;
            Ok(deposit)
        })
    }
}

fn projection_entry(command: &RecordObservationBalance) -> LedgerEntry {
    LedgerEntry {
        id: LedgerEntryId(format!("projection:{}", command.projection_id.0)),
        deposit_id: command.deposit_id.clone(),
        previous: command.expected_head.clone(),
        cause: LedgerEntryCause::Observation {
            projection_id: command.projection_id.clone(),
            event_id: command.event_id.clone(),
            observation_revision: command.observation_revision,
            status: command.status.clone(),
            kind: command.kind,
            movement_ids: command.movement_ids.clone(),
        },
        balances: command.next_balances,
        recorded_at: command.recorded_at,
    }
}

fn accounting_entry(command: &AccountingCommand, current: &LedgerEntry) -> LedgerEntry {
    let mut balances = current.balances;
    balances.accounted = command.next_accounted;
    LedgerEntry {
        id: LedgerEntryId(format!("accounting:{}", command.idempotency_key.0)),
        deposit_id: command.deposit_id.clone(),
        previous: Some(current.id.clone()),
        cause: LedgerEntryCause::Accounting {
            idempotency_key: command.idempotency_key.clone(),
        },
        balances,
        recorded_at: command.recorded_at,
    }
}

impl<S> DepositLedger for PersistentPaymentRepository<S>
where
    S: Storage,
{
    fn open<'a>(&'a self, command: OpenLedger) -> BoxFuture<'a, Result<ApplyResult, DepositError>> {
        Box::pin(async move {
            if let Some((current, _)) = self.stored_head(&command.deposit_id).await? {
                let expected_cause = LedgerEntryCause::Opened {
                    idempotency_key: command.idempotency_key.clone(),
                };
                if current.previous.is_none()
                    && current.cause == expected_cause
                    && current.balances == DepositBalances::default()
                {
                    return Ok(ApplyResult::AlreadyPresent { entry: current });
                }
                return Err(conflict(
                    "deposit ledger is already open under a different command",
                ));
            }
            let deposit = self
                .deposit(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("cannot open a ledger for a missing deposit"))?;
            if deposit.idempotency_key != command.idempotency_key {
                return Err(conflict(
                    "ledger-open idempotency key does not match deposit creation",
                ));
            }
            let entry = LedgerEntry {
                id: LedgerEntryId(format!("open:{}", command.deposit_id.0)),
                deposit_id: command.deposit_id.clone(),
                previous: None,
                cause: LedgerEntryCause::Opened {
                    idempotency_key: command.idempotency_key,
                },
                balances: DepositBalances::default(),
                recorded_at: command.recorded_at,
            };
            self.storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: ledger_head_ns(),
                            key: key_text(&entry.deposit_id.0),
                        },
                        Condition::Missing {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&entry.deposit_id, &entry.id)?,
                        },
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&entry.deposit_id, &entry.id)?,
                            value: encode(&LedgerEntryRecordV1::from(&entry))?,
                        },
                        Operation::Put {
                            namespace: ledger_head_ns(),
                            key: key_text(&entry.deposit_id.0),
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: entry.id.0.clone(),
                            })?,
                        },
                    ],
                })
                .await
                .map_err(map_storage)?;
            Ok(ApplyResult::Appended { entry })
        })
    }

    fn current<'a>(
        &'a self,
        deposit_id: &'a DepositId,
    ) -> BoxFuture<'a, Result<Option<LedgerEntry>, DepositError>> {
        Box::pin(async move { Ok(self.stored_head(deposit_id).await?.map(|(entry, _)| entry)) })
    }

    fn entries<'a>(
        &'a self,
        request: LedgerPageRequest,
    ) -> BoxFuture<'a, Result<LedgerPage, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > 1_000 {
                return Err(invalid("ledger page size must be between 1 and 1000"));
            }
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: ledger_entry_ns(),
                    prefix: ledger_prefix(&request.deposit_id)?,
                    after: request
                        .after
                        .as_ref()
                        .map(|entry| ledger_entry_key(&request.deposit_id, entry))
                        .transpose()?,
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let entries = page
                .entries
                .into_iter()
                .map(|(_, stored)| decode::<LedgerEntryRecordV1>(&stored)?.try_into())
                .collect::<Result<Vec<LedgerEntry>, DepositError>>()?;
            let next = if page.next.is_some() {
                entries.last().map(|entry| entry.id.clone())
            } else {
                None
            };
            Ok(LedgerPage { entries, next })
        })
    }

    fn record_observation<'a>(
        &'a self,
        command: RecordObservationBalance,
    ) -> BoxFuture<'a, Result<ApplyResult, DepositError>> {
        Box::pin(async move {
            let projection_key = key_text(&command.projection_id.0);
            if let Some(stored) = self
                .storage
                .get(&projection_ns(), &projection_key)
                .await
                .map_err(map_storage)?
            {
                let index: IdRecordV1 = decode(&stored)?;
                ensure_version(index.version)?;
                let existing = self
                    .stored_ledger_entry(&command.deposit_id, &LedgerEntryId(index.id))
                    .await?
                    .ok_or_else(|| storage_error("projection index is dangling"))?;
                if existing == projection_entry(&command) {
                    return Ok(ApplyResult::AlreadyPresent { entry: existing });
                }
                return Err(conflict(
                    "projection ID was reused with a different ledger update",
                ));
            }
            let (current, head_stored) = self
                .stored_head(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("deposit ledger is not open"))?;
            if command.expected_head.as_ref() != Some(&current.id) {
                return Err(conflict("ledger expected head does not match current head"));
            }
            if command.next_balances.accounted != current.balances.accounted {
                return Err(invalid(
                    "observation projection attempted to change accounted value",
                ));
            }
            let entry = projection_entry(&command);
            self.storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: projection_ns(),
                            key: projection_key.clone(),
                        },
                        Condition::Version {
                            namespace: ledger_head_ns(),
                            key: key_text(&command.deposit_id.0),
                            expected: head_stored.version,
                        },
                        Condition::Missing {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&command.deposit_id, &entry.id)?,
                        },
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&command.deposit_id, &entry.id)?,
                            value: encode(&LedgerEntryRecordV1::from(&entry))?,
                        },
                        Operation::Put {
                            namespace: ledger_head_ns(),
                            key: key_text(&command.deposit_id.0),
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: entry.id.0.clone(),
                            })?,
                        },
                        Operation::Put {
                            namespace: projection_ns(),
                            key: projection_key,
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: entry.id.0.clone(),
                            })?,
                        },
                    ],
                })
                .await
                .map_err(map_storage)?;
            Ok(ApplyResult::Appended { entry })
        })
    }

    fn record_accounting<'a>(
        &'a self,
        command: AccountingCommand,
    ) -> BoxFuture<'a, Result<ApplyResult, DepositError>> {
        Box::pin(async move {
            let idempotency_key = key_text(&command.idempotency_key.0);
            if let Some(stored) = self
                .storage
                .get(&accounting_idempotency_ns(), &idempotency_key)
                .await
                .map_err(map_storage)?
            {
                let index: IdRecordV1 = decode(&stored)?;
                ensure_version(index.version)?;
                let existing = self
                    .stored_ledger_entry(&command.deposit_id, &LedgerEntryId(index.id))
                    .await?
                    .ok_or_else(|| storage_error("accounting idempotency index is dangling"))?;
                return Ok(ApplyResult::AlreadyPresent { entry: existing });
            }
            if self.automatic_actions_blocked(&command.deposit_id).await? {
                return Err(invalid_state(
                    "automatic accounting is blocked by an open reconciliation case",
                ));
            }
            let (current, head_stored) = self
                .stored_head(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("deposit ledger is not open"))?;
            if command.expected_head.as_ref() != Some(&current.id) {
                return Err(conflict("ledger expected head does not match current head"));
            }
            if command.next_accounted > current.balances.confirmed {
                return Err(invalid(
                    "accounted value cannot exceed confirmation-qualified value",
                ));
            }
            let entry = accounting_entry(&command, &current);
            self.storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: accounting_idempotency_ns(),
                            key: idempotency_key.clone(),
                        },
                        Condition::Version {
                            namespace: ledger_head_ns(),
                            key: key_text(&command.deposit_id.0),
                            expected: head_stored.version,
                        },
                        Condition::Missing {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&command.deposit_id, &entry.id)?,
                        },
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&command.deposit_id, &entry.id)?,
                            value: encode(&LedgerEntryRecordV1::from(&entry))?,
                        },
                        Operation::Put {
                            namespace: ledger_head_ns(),
                            key: key_text(&command.deposit_id.0),
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: entry.id.0.clone(),
                            })?,
                        },
                        Operation::Put {
                            namespace: accounting_idempotency_ns(),
                            key: idempotency_key,
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: entry.id.0.clone(),
                            })?,
                        },
                    ],
                })
                .await
                .map_err(map_storage)?;
            Ok(ApplyResult::Appended { entry })
        })
    }
}

impl<S> PersistentPaymentRepository<S>
where
    S: Storage,
{
    async fn append_mirror_only(
        &self,
        observation: &MirroredObservation,
    ) -> Result<AppendOutcome, DepositError> {
        if let Some((existing, _)) = self.mirrored_observation(&observation.event.id).await? {
            return if existing == *observation {
                Ok(AppendOutcome::AlreadyPresent)
            } else {
                Err(conflict(
                    "IX event ID was reused with a different mirrored payload",
                ))
            };
        }
        let event_key = key_text(&observation.event.id.0);
        let cursor = cursor_key(observation.event.cursor);
        if let Some(stored) = self
            .storage
            .get(&observation_cursor_ns(), &cursor)
            .await
            .map_err(map_storage)?
        {
            let existing: IdRecordV1 = decode(&stored)?;
            ensure_version(existing.version)?;
            return Err(conflict(format!(
                "IX cursor {} is already assigned to event {}",
                observation.event.cursor.0, existing.id
            )));
        }
        self.storage
            .commit(WriteBatch {
                conditions: vec![
                    Condition::Missing {
                        namespace: observation_ns(),
                        key: event_key.clone(),
                    },
                    Condition::Missing {
                        namespace: observation_cursor_ns(),
                        key: cursor.clone(),
                    },
                ],
                operations: vec![
                    Operation::Put {
                        namespace: observation_ns(),
                        key: event_key,
                        value: encode(&ObservationEventRecordV1::from(observation))?,
                    },
                    Operation::Put {
                        namespace: observation_cursor_ns(),
                        key: cursor,
                        value: encode(&IdRecordV1 {
                            version: RECORD_VERSION,
                            id: observation.event.id.0.clone(),
                        })?,
                    },
                ],
            })
            .await
            .map_err(map_storage)?;
        Ok(AppendOutcome::Appended)
    }
}

impl<S> ObservationEventLog for PersistentPaymentRepository<S>
where
    S: Storage,
{
    fn append<'a>(
        &'a self,
        command: AppendObservation,
    ) -> BoxFuture<'a, Result<AppendOutcome, DepositError>> {
        Box::pin(async move { self.append_mirror_only(&command.observation).await })
    }

    fn observation<'a>(
        &'a self,
        event_id: &'a ObservationEventId,
    ) -> BoxFuture<'a, Result<Option<MirroredObservation>, DepositError>> {
        Box::pin(async move {
            Ok(self
                .mirrored_observation(event_id)
                .await?
                .map(|(observation, _)| observation))
        })
    }

    fn observations<'a>(
        &'a self,
        request: ObservationLogRequest,
    ) -> BoxFuture<'a, Result<ObservationLogPage, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > 1_000 {
                return Err(invalid("observation page size must be between 1 and 1000"));
            }
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: observation_cursor_ns(),
                    prefix: Vec::new(),
                    after: request.after.map(cursor_key),
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let has_next = page.next.is_some();
            let mut observations = Vec::with_capacity(page.entries.len());
            for (_, stored) in page.entries {
                let index: IdRecordV1 = decode(&stored)?;
                ensure_version(index.version)?;
                observations.push(
                    self.observation(&ObservationEventId(index.id))
                        .await?
                        .ok_or_else(|| storage_error("observation cursor index is dangling"))?,
                );
            }
            let next = if has_next {
                observations
                    .last()
                    .map(|observation| observation.event.cursor)
            } else {
                None
            };
            Ok(ObservationLogPage { observations, next })
        })
    }
}

fn expected_next_cursor(current: Option<EventCursor>) -> Result<EventCursor, DepositError> {
    match current {
        Some(cursor) => cursor
            .0
            .checked_add(1)
            .map(EventCursor)
            .ok_or_else(|| invalid("PS consumer cursor is exhausted")),
        None => Ok(EventCursor(1)),
    }
}

fn checkpoint_condition(name: ConsumerCheckpointName, stored: Option<&StoredValue>) -> Condition {
    match stored {
        Some(stored) => Condition::Version {
            namespace: consumer_checkpoint_ns(),
            key: checkpoint_key(name),
            expected: stored.version,
        },
        None => Condition::Missing {
            namespace: consumer_checkpoint_ns(),
            key: checkpoint_key(name),
        },
    }
}

impl<S> ObservationConsumerCheckpoints for PersistentPaymentRepository<S>
where
    S: Storage,
{
    fn consumer_checkpoint<'a>(
        &'a self,
        name: ConsumerCheckpointName,
    ) -> BoxFuture<'a, Result<ConsumerCheckpoint, DepositError>> {
        Box::pin(async move { Ok(self.stored_checkpoint(name).await?.0) })
    }

    fn mirror_and_advance<'a>(
        &'a self,
        command: MirrorObservation,
    ) -> BoxFuture<'a, Result<MirrorOutcome, DepositError>> {
        Box::pin(async move {
            let (checkpoint, checkpoint_stored) = self
                .stored_checkpoint(ConsumerCheckpointName::IxIngestion)
                .await?;
            let cursor = command.observation.event.cursor;
            if checkpoint.cursor == Some(cursor) {
                let existing = self
                    .observation(&command.observation.event.id)
                    .await?
                    .ok_or_else(|| {
                        storage_error("ingestion cursor advanced without its mirrored event")
                    })?;
                if existing == command.observation {
                    return Ok(MirrorOutcome::AlreadyPresent { cursor });
                }
                return Err(conflict(
                    "ingestion retry contains a different mirrored event payload",
                ));
            }
            if checkpoint.cursor != command.expected_cursor {
                return Err(conflict(
                    "ingestion expected cursor does not match durable cursor",
                ));
            }
            if expected_next_cursor(checkpoint.cursor)? != cursor {
                return Err(conflict(
                    "IX events must be mirrored in contiguous cursor order",
                ));
            }

            let existing_event = self
                .mirrored_observation(&command.observation.event.id)
                .await?;
            if let Some((existing, _)) = &existing_event {
                if existing != &command.observation {
                    return Err(conflict(
                        "IX event ID was reused with a different mirrored payload",
                    ));
                }
            }
            let cursor_index = self
                .storage
                .get(&observation_cursor_ns(), &cursor_key(cursor))
                .await
                .map_err(map_storage)?;
            if let Some(stored) = &cursor_index {
                let index: IdRecordV1 = decode(stored)?;
                ensure_version(index.version)?;
                if index.id != command.observation.event.id.0 {
                    return Err(conflict("IX cursor is assigned to a different event"));
                }
            }

            let mut conditions = vec![checkpoint_condition(
                ConsumerCheckpointName::IxIngestion,
                checkpoint_stored.as_ref(),
            )];
            let mut operations = Vec::new();
            if existing_event.is_none() {
                conditions.push(Condition::Missing {
                    namespace: observation_ns(),
                    key: key_text(&command.observation.event.id.0),
                });
                operations.push(Operation::Put {
                    namespace: observation_ns(),
                    key: key_text(&command.observation.event.id.0),
                    value: encode(&ObservationEventRecordV1::from(&command.observation))?,
                });
            }
            if cursor_index.is_none() {
                conditions.push(Condition::Missing {
                    namespace: observation_cursor_ns(),
                    key: cursor_key(cursor),
                });
                operations.push(Operation::Put {
                    namespace: observation_cursor_ns(),
                    key: cursor_key(cursor),
                    value: encode(&IdRecordV1 {
                        version: RECORD_VERSION,
                        id: command.observation.event.id.0.clone(),
                    })?,
                });
            }
            operations.push(Operation::Put {
                namespace: consumer_checkpoint_ns(),
                key: checkpoint_key(ConsumerCheckpointName::IxIngestion),
                value: encode(&CursorRecordV1 {
                    version: RECORD_VERSION,
                    cursor: Some(cursor.0),
                })?,
            });
            self.storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await
                .map_err(map_storage)?;
            Ok(MirrorOutcome::Appended { cursor })
        })
    }

    fn project_and_advance<'a>(
        &'a self,
        command: ProjectObservation,
    ) -> BoxFuture<'a, Result<ProjectionOutcome, DepositError>> {
        Box::pin(async move {
            let (checkpoint, checkpoint_stored) = self
                .stored_checkpoint(ConsumerCheckpointName::IxProjection)
                .await?;
            if checkpoint.cursor == Some(command.through) {
                let mut ledger_results = Vec::with_capacity(command.ledger_updates.len());
                for update in &command.ledger_updates {
                    let stored = self
                        .storage
                        .get(&projection_ns(), &key_text(&update.projection_id.0))
                        .await
                        .map_err(map_storage)?
                        .ok_or_else(|| {
                            storage_error("projection cursor advanced without a projection record")
                        })?;
                    let index: IdRecordV1 = decode(&stored)?;
                    ensure_version(index.version)?;
                    let entry = self
                        .stored_ledger_entry(&update.deposit_id, &LedgerEntryId(index.id))
                        .await?
                        .ok_or_else(|| storage_error("projection record is dangling"))?;
                    ledger_results.push(ApplyResult::AlreadyPresent { entry });
                }
                let mut cases = Vec::with_capacity(command.reconciliation_cases.len());
                for case in &command.reconciliation_cases {
                    cases.push(self.case(&case.id).await?.ok_or_else(|| {
                        storage_error("projection cursor advanced without a reconciliation case")
                    })?);
                }
                return Ok(ProjectionOutcome {
                    checkpoint,
                    ledger_results,
                    reconciliation_cases: cases,
                });
            }
            if checkpoint.cursor != command.expected_cursor {
                return Err(conflict(
                    "projection expected cursor does not match durable cursor",
                ));
            }
            if expected_next_cursor(checkpoint.cursor)? != command.through {
                return Err(conflict(
                    "mirrored observations must be projected in contiguous cursor order",
                ));
            }
            let mirrored = self
                .observations(ObservationLogRequest {
                    after: checkpoint.cursor,
                    limit: 1,
                })
                .await?;
            let event = mirrored
                .observations
                .first()
                .ok_or_else(|| not_found("projection cursor has no mirrored IX event"))?;
            if event.event.cursor != command.through {
                return Err(conflict(
                    "next mirrored event does not match projection target cursor",
                ));
            }

            let mut conditions = vec![checkpoint_condition(
                ConsumerCheckpointName::IxProjection,
                checkpoint_stored.as_ref(),
            )];
            let mut operations = Vec::new();
            let mut ledger_results = Vec::with_capacity(command.ledger_updates.len());
            let mut seen_deposits = std::collections::BTreeSet::new();
            for update in &command.ledger_updates {
                if update.event_id != event.event.id {
                    return Err(invalid(
                        "ledger projection references a different mirrored IX event",
                    ));
                }
                if !seen_deposits.insert(update.deposit_id.clone()) {
                    return Err(invalid(
                        "one projection command contains multiple updates for one deposit",
                    ));
                }
                let projection_key = key_text(&update.projection_id.0);
                if self
                    .storage
                    .get(&projection_ns(), &projection_key)
                    .await
                    .map_err(map_storage)?
                    .is_some()
                {
                    return Err(conflict(
                        "projection ID exists while the projection cursor is behind",
                    ));
                }
                let (head, head_stored) = self
                    .stored_head(&update.deposit_id)
                    .await?
                    .ok_or_else(|| not_found("deposit ledger is not open"))?;
                if update.expected_head.as_ref() != Some(&head.id) {
                    return Err(conflict("ledger expected head does not match current head"));
                }
                if update.next_balances.accounted != head.balances.accounted {
                    return Err(invalid(
                        "observation projection attempted to change accounted value",
                    ));
                }
                let entry = projection_entry(update);
                conditions.extend([
                    Condition::Missing {
                        namespace: projection_ns(),
                        key: projection_key.clone(),
                    },
                    Condition::Version {
                        namespace: ledger_head_ns(),
                        key: key_text(&update.deposit_id.0),
                        expected: head_stored.version,
                    },
                    Condition::Missing {
                        namespace: ledger_entry_ns(),
                        key: ledger_entry_key(&update.deposit_id, &entry.id)?,
                    },
                ]);
                operations.extend([
                    Operation::Put {
                        namespace: ledger_entry_ns(),
                        key: ledger_entry_key(&update.deposit_id, &entry.id)?,
                        value: encode(&LedgerEntryRecordV1::from(&entry))?,
                    },
                    Operation::Put {
                        namespace: ledger_head_ns(),
                        key: key_text(&update.deposit_id.0),
                        value: encode(&IdRecordV1 {
                            version: RECORD_VERSION,
                            id: entry.id.0.clone(),
                        })?,
                    },
                    Operation::Put {
                        namespace: projection_ns(),
                        key: projection_key,
                        value: encode(&IdRecordV1 {
                            version: RECORD_VERSION,
                            id: entry.id.0.clone(),
                        })?,
                    },
                ]);
                ledger_results.push(ApplyResult::Appended { entry });
            }

            for case in &command.reconciliation_cases {
                if case.triggering_event_id != event.event.id
                    || case.state != ReconciliationState::Open
                {
                    return Err(invalid(
                        "projection reconciliation case must be open and reference its IX event",
                    ));
                }
                let ReconciliationReason::PostCreditReorg {
                    accounted,
                    corrected_confirmed,
                } = &case.reason;
                if accounted <= corrected_confirmed {
                    return Err(invalid(
                        "post-credit reorg case requires accounted to exceed corrected confirmed",
                    ));
                }
                if self.case(&case.id).await?.is_some() {
                    return Err(conflict(
                        "reconciliation case exists while projection cursor is behind",
                    ));
                }
                conditions.extend([
                    Condition::Missing {
                        namespace: reconciliation_ns(),
                        key: key_text(&case.id.0),
                    },
                    Condition::Missing {
                        namespace: reconciliation_deposit_ns(),
                        key: reconciliation_deposit_key(&case.deposit_id, &case.id)?,
                    },
                ]);
                operations.extend([
                    Operation::Put {
                        namespace: reconciliation_ns(),
                        key: key_text(&case.id.0),
                        value: encode(&ReconciliationRecordV1::from(case))?,
                    },
                    Operation::Put {
                        namespace: reconciliation_deposit_ns(),
                        key: reconciliation_deposit_key(&case.deposit_id, &case.id)?,
                        value: encode(&IdRecordV1 {
                            version: RECORD_VERSION,
                            id: case.id.0.clone(),
                        })?,
                    },
                ]);
            }
            operations.push(Operation::Put {
                namespace: consumer_checkpoint_ns(),
                key: checkpoint_key(ConsumerCheckpointName::IxProjection),
                value: encode(&CursorRecordV1 {
                    version: RECORD_VERSION,
                    cursor: Some(command.through.0),
                })?,
            });
            self.storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await
                .map_err(map_storage)?;
            Ok(ProjectionOutcome {
                checkpoint: ConsumerCheckpoint {
                    name: ConsumerCheckpointName::IxProjection,
                    cursor: Some(command.through),
                },
                ledger_results,
                reconciliation_cases: command.reconciliation_cases,
            })
        })
    }
}

fn validate_open_reconciliation(case: &ReconciliationCase) -> Result<(), DepositError> {
    if case.id.0.is_empty() || case.deposit_id.0.is_empty() || case.triggering_event_id.0.is_empty()
    {
        return Err(invalid(
            "reconciliation case, deposit, and triggering event IDs must be non-empty",
        ));
    }
    if case.state != ReconciliationState::Open {
        return Err(invalid("a new reconciliation case must be open"));
    }
    match &case.reason {
        ReconciliationReason::PostCreditReorg {
            accounted,
            corrected_confirmed,
        } if accounted > corrected_confirmed => Ok(()),
        ReconciliationReason::PostCreditReorg { .. } => Err(invalid(
            "post-credit reorg requires accounted to exceed corrected confirmed",
        )),
    }
}

impl<S> ReconciliationStore for PersistentPaymentRepository<S>
where
    S: Storage,
{
    fn open_case<'a>(
        &'a self,
        case: ReconciliationCase,
    ) -> BoxFuture<'a, Result<ReconciliationCase, DepositError>> {
        Box::pin(async move {
            validate_open_reconciliation(&case)?;
            if let Some(existing) = self.case(&case.id).await? {
                return if existing == case {
                    Ok(existing)
                } else {
                    Err(conflict(
                        "reconciliation case ID was reused with a different payload",
                    ))
                };
            }
            if self.deposit(&case.deposit_id).await?.is_none() {
                return Err(not_found(
                    "cannot open a reconciliation case for a missing deposit",
                ));
            }

            let case_key = key_text(&case.id.0);
            let deposit_key = reconciliation_deposit_key(&case.deposit_id, &case.id)?;
            let result = self
                .storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: reconciliation_ns(),
                            key: case_key.clone(),
                        },
                        Condition::Missing {
                            namespace: reconciliation_deposit_ns(),
                            key: deposit_key.clone(),
                        },
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: reconciliation_ns(),
                            key: case_key,
                            value: encode(&ReconciliationRecordV1::from(&case))?,
                        },
                        Operation::Put {
                            namespace: reconciliation_deposit_ns(),
                            key: deposit_key,
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: case.id.0.clone(),
                            })?,
                        },
                    ],
                })
                .await;
            match result {
                Ok(_) => Ok(case),
                Err(error) if error.kind == StorageErrorKind::Conflict => {
                    let existing = self
                        .case(&case.id)
                        .await?
                        .ok_or_else(|| map_storage(error))?;
                    if existing == case {
                        Ok(existing)
                    } else {
                        Err(conflict(
                            "reconciliation case ID was concurrently reused with a different payload",
                        ))
                    }
                }
                Err(error) => Err(map_storage(error)),
            }
        })
    }

    fn case<'a>(
        &'a self,
        id: &'a ReconciliationCaseId,
    ) -> BoxFuture<'a, Result<Option<ReconciliationCase>, DepositError>> {
        Box::pin(async move {
            self.storage
                .get(&reconciliation_ns(), &key_text(&id.0))
                .await
                .map_err(map_storage)?
                .map(|stored| decode::<ReconciliationRecordV1>(&stored)?.try_into())
                .transpose()
        })
    }

    fn cases<'a>(
        &'a self,
        request: ReconciliationPageRequest,
    ) -> BoxFuture<'a, Result<ReconciliationPage, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > 1_000 {
                return Err(invalid(
                    "reconciliation page size must be between 1 and 1000",
                ));
            }

            let (namespace, prefix, mut after) = match &request.deposit_id {
                Some(deposit_id) => (
                    reconciliation_deposit_ns(),
                    reconciliation_deposit_prefix(deposit_id)?,
                    request
                        .after
                        .as_ref()
                        .map(|case_id| reconciliation_deposit_key(deposit_id, case_id))
                        .transpose()?,
                ),
                None => (
                    reconciliation_ns(),
                    Vec::new(),
                    request.after.as_ref().map(|case_id| key_text(&case_id.0)),
                ),
            };

            let mut cases = Vec::with_capacity(request.limit);
            let mut exhausted = false;
            while cases.len() < request.limit && !exhausted {
                let page = self
                    .storage
                    .scan(ScanRequest {
                        namespace: namespace.clone(),
                        prefix: prefix.clone(),
                        after,
                        limit: request.limit,
                    })
                    .await
                    .map_err(map_storage)?;
                exhausted = page.next.is_none();
                after = page.next;

                for (_, stored) in page.entries {
                    let case = if request.deposit_id.is_some() {
                        let index: IdRecordV1 = decode(&stored)?;
                        ensure_version(index.version)?;
                        self.case(&ReconciliationCaseId(index.id))
                            .await?
                            .ok_or_else(|| {
                                storage_error("reconciliation deposit index is dangling")
                            })?
                    } else {
                        decode::<ReconciliationRecordV1>(&stored)?.try_into()?
                    };
                    if !request.open_only || case.state == ReconciliationState::Open {
                        cases.push(case);
                        if cases.len() == request.limit {
                            break;
                        }
                    }
                }
            }

            let next = if exhausted {
                None
            } else {
                cases.last().map(|case| case.id.clone())
            };
            Ok(ReconciliationPage { cases, next })
        })
    }

    fn resolve_case<'a>(
        &'a self,
        id: &'a ReconciliationCaseId,
        resolution: String,
        resolved_at: u64,
    ) -> BoxFuture<'a, Result<ReconciliationCase, DepositError>> {
        Box::pin(async move {
            if resolution.trim().is_empty() {
                return Err(invalid("reconciliation resolution must not be empty"));
            }
            let key = key_text(&id.0);
            let stored = self
                .storage
                .get(&reconciliation_ns(), &key)
                .await
                .map_err(map_storage)?
                .ok_or_else(|| not_found("reconciliation case does not exist"))?;
            let mut case: ReconciliationCase =
                decode::<ReconciliationRecordV1>(&stored)?.try_into()?;
            match &case.state {
                ReconciliationState::Open => {}
                ReconciliationState::Resolved {
                    resolution: existing,
                    resolved_at: existing_at,
                } if existing == &resolution && *existing_at == resolved_at => return Ok(case),
                ReconciliationState::Resolved { .. } => {
                    return Err(conflict(
                        "reconciliation case was already resolved differently",
                    ));
                }
            }
            case.state = ReconciliationState::Resolved {
                resolution,
                resolved_at,
            };
            self.storage
                .commit(WriteBatch {
                    conditions: vec![Condition::Version {
                        namespace: reconciliation_ns(),
                        key: key.clone(),
                        expected: stored.version,
                    }],
                    operations: vec![Operation::Put {
                        namespace: reconciliation_ns(),
                        key,
                        value: encode(&ReconciliationRecordV1::from(&case))?,
                    }],
                })
                .await
                .map_err(map_storage)?;
            Ok(case)
        })
    }

    fn automatic_actions_blocked<'a>(
        &'a self,
        deposit_id: &'a DepositId,
    ) -> BoxFuture<'a, Result<bool, DepositError>> {
        Box::pin(async move {
            let mut after = None;
            let prefix = reconciliation_deposit_prefix(deposit_id)?;
            loop {
                let page = self
                    .storage
                    .scan(ScanRequest {
                        namespace: reconciliation_deposit_ns(),
                        prefix: prefix.clone(),
                        after,
                        limit: 256,
                    })
                    .await
                    .map_err(map_storage)?;
                for (_, stored) in page.entries {
                    let index: IdRecordV1 = decode(&stored)?;
                    ensure_version(index.version)?;
                    let case = self
                        .case(&ReconciliationCaseId(index.id))
                        .await?
                        .ok_or_else(|| storage_error("reconciliation deposit index is dangling"))?;
                    if case.state == ReconciliationState::Open {
                        return Ok(true);
                    }
                }
                match page.next {
                    Some(next) => after = Some(next),
                    None => return Ok(false),
                }
            }
        })
    }
}
