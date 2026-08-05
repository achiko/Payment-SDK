use bincode::{Decode, Encode};
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use indexing::{
    BlockHash, BlockHeight, BlockRef, ConfirmationProof, EventCursor, IndexScope, MovementId,
    MovementKind, NetworkFee, ObservationEvent, ObservationEventId, ObservationRevision,
    ObservedTransaction, TransactionStatus, ValueMovement, WatchId,
};
use sha2::{Digest, Sha256};
use signer::{ChildIndex, DerivationPath, KeyLocator};
use storage::{
    Condition, Key, Namespace, Operation, ScanRequest, Storage, StorageError, StorageErrorKind,
    StoredValue, Value, WriteBatch,
};

use crate::{
    AccountingCommand, AppendObservation, AppendOutcome, ApplyResult, AwaitingWatchPage,
    AwaitingWatchPageRequest, BoxFuture, CloseDeposit, CommandIdentity, CommandOperation,
    CommandPrincipal, ConsumerCheckpoint, ConsumerCheckpointName, CreateDeposit,
    CreateDepositWithLedger, CreatedDeposit, Deposit, DepositBalances, DepositError,
    DepositErrorKind, DepositId, DepositIndexRebuild, DepositIndexRebuildRequest, DepositLedger,
    DepositObservationLogPage, DepositObservationLogRequest, DepositPage, DepositPageRequest,
    DepositState, DepositStateKind, DepositStore, IdempotencyKey, LEGACY_DEPOSIT_KEY_PURPOSE,
    LedgerEffect, LedgerEntry, LedgerEntryCause, LedgerEntryId, LedgerObservationKind,
    LedgerObservationTransition, LedgerPage, LedgerPageRequest, MirrorObservation, MirrorOutcome,
    MirroredObservation, ObservationConsumerCheckpoints, ObservationEventLog,
    ObservationLedgerEffect, ObservationLogPage, ObservationLogRequest, OpenLedger,
    ProjectObservation, ProjectionId, ProjectionOutcome, ReconciliationCase, ReconciliationCaseId,
    ReconciliationDecision, ReconciliationPage, ReconciliationPageRequest, ReconciliationReason,
    ReconciliationResolution, ReconciliationState, ReconciliationStore, RecordObservation,
    RequestHash, ResolveReconciliation, UserId, apply_observation_transition,
};

const RECORD_VERSION: u16 = 1;
const DEPOSIT_RECORD_VERSION: u16 = 2;
const RECONCILIATION_RECORD_VERSION: u16 = 2;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 1_000;
const MAX_KEY_PURPOSE_BYTES: usize = 1_024;
const MAX_ACCOUNTING_REASON_BYTES: usize = 1_024;
const MAX_RECONCILIATION_REASON_BYTES: usize = 1_024;
const MAX_EXTERNAL_DEBT_REFERENCE_BYTES: usize = 4_096;
const LEGACY_ACCOUNTING_REASON: &str = "legacy accounting reason unavailable";

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

fn closed_deposit_watch_ns() -> Namespace {
    ns("ps.v1.closed_deposit_watch")
}

fn user_deposit_ns() -> Namespace {
    ns("ps.v1.user_deposit")
}

fn deposit_state_ns() -> Namespace {
    ns("ps.v1.deposit_state")
}

fn user_deposit_state_ns() -> Namespace {
    ns("ps.v1.user_deposit_state")
}

fn deposit_index_metadata_ns() -> Namespace {
    ns("ps.v1.deposit_index_metadata")
}

fn deposit_index_complete_key() -> Key {
    key_text("v1_complete")
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

fn deposit_observation_ns() -> Namespace {
    ns("ps.v1.deposit_observation")
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

fn reconciliation_resolution_idempotency_ns() -> Namespace {
    ns("ps.v1.reconciliation_resolution_idem")
}

fn reconciliation_generation_ns() -> Namespace {
    ns("ps.v1.reconciliation_generation")
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

fn accounting_command_key(identity: &CommandIdentity) -> Result<Key, DepositError> {
    component_key(&[
        identity.principal.0.as_bytes(),
        b"accounting",
        identity.client_key.0.as_bytes(),
    ])
}

fn reconciliation_command_key(identity: &CommandIdentity) -> Result<Key, DepositError> {
    component_key(&[
        identity.principal.0.as_bytes(),
        b"resolve_reconciliation",
        identity.client_key.0.as_bytes(),
    ])
}

fn address_key(address: &CanonicalAddress) -> Result<Key, DepositError> {
    component_key(&[address.chain.0.as_bytes(), address.value.as_bytes()])
}

const fn deposit_state_tag(state: DepositStateKind) -> &'static [u8] {
    match state {
        DepositStateKind::AwaitingWatch => b"awaiting_watch",
        DepositStateKind::Active => b"active",
        DepositStateKind::Expired => b"expired",
        DepositStateKind::Closed => b"closed",
    }
}

fn user_deposit_key(user_id: &UserId, deposit_id: &DepositId) -> Result<Key, DepositError> {
    component_key(&[user_id.0.as_bytes(), deposit_id.0.as_bytes()])
}

fn user_deposit_prefix(user_id: &UserId) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[user_id.0.as_bytes()])?.0)
}

fn state_deposit_key(state: DepositStateKind, deposit_id: &DepositId) -> Result<Key, DepositError> {
    component_key(&[deposit_state_tag(state), deposit_id.0.as_bytes()])
}

fn state_deposit_prefix(state: DepositStateKind) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[deposit_state_tag(state)])?.0)
}

fn user_state_deposit_key(
    user_id: &UserId,
    state: DepositStateKind,
    deposit_id: &DepositId,
) -> Result<Key, DepositError> {
    component_key(&[
        user_id.0.as_bytes(),
        deposit_state_tag(state),
        deposit_id.0.as_bytes(),
    ])
}

fn user_state_deposit_prefix(
    user_id: &UserId,
    state: DepositStateKind,
) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[user_id.0.as_bytes(), deposit_state_tag(state)])?.0)
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

fn deposit_observation_key(
    deposit_id: &DepositId,
    cursor: EventCursor,
) -> Result<Key, DepositError> {
    component_key(&[deposit_id.0.as_bytes(), &cursor.0.to_be_bytes()])
}

fn deposit_observation_prefix(deposit_id: &DepositId) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[deposit_id.0.as_bytes()])?.0)
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

fn decode_deposit(stored: &StoredValue) -> Result<Deposit, DepositError> {
    let config = bincode::config::standard()
        .with_fixed_int_encoding()
        .with_big_endian()
        .with_limit::<MAX_RECORD_BYTES>();
    let (version, _) = bincode::decode_from_slice::<u16, _>(&stored.value.0, config)
        .map_err(|error| storage_error(format!("failed to decode PS deposit version: {error}")))?;
    match version {
        RECORD_VERSION => decode::<DepositRecordV1>(stored)?.try_into(),
        DEPOSIT_RECORD_VERSION => decode::<DepositRecordV2>(stored)?.try_into(),
        _ => Err(storage_error(format!(
            "unsupported PS deposit record version {version}"
        ))),
    }
}

fn decode_reconciliation(stored: &StoredValue) -> Result<ReconciliationCase, DepositError> {
    let config = bincode::config::standard()
        .with_fixed_int_encoding()
        .with_big_endian()
        .with_limit::<MAX_RECORD_BYTES>();
    let (version, _) =
        bincode::decode_from_slice::<u16, _>(&stored.value.0, config).map_err(|error| {
            storage_error(format!(
                "failed to decode PS reconciliation version: {error}"
            ))
        })?;
    match version {
        RECORD_VERSION => decode::<ReconciliationRecordV1>(stored)?.try_into(),
        RECONCILIATION_RECORD_VERSION => decode::<ReconciliationRecordV2>(stored)?.try_into(),
        _ => Err(storage_error(format!(
            "unsupported PS reconciliation record version {version}"
        ))),
    }
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
            key_purpose: LEGACY_DEPOSIT_KEY_PURPOSE.to_owned(),
            expected: AtomicAmount(value.expected),
            birthday: BlockHeight(value.birthday),
            expires_at: value.expires_at,
            state: match value.state {
                DepositStateRecordV1::AwaitingWatch => DepositState::AwaitingWatch,
                DepositStateRecordV1::Active(watch_id) => DepositState::Active {
                    watch_id: WatchId(watch_id),
                },
                DepositStateRecordV1::Expired => {
                    return Err(storage_error(
                        "legacy PS deposit V1 expired state lacks its IX watch ID; explicit migration is required",
                    ));
                }
                DepositStateRecordV1::Closed => DepositState::Closed,
            },
            created_at: value.created_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum DepositStateRecordV2 {
    AwaitingWatch,
    Active(String),
    Expired(String),
    Closed,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct DepositRecordV2 {
    version: u16,
    id: String,
    idempotency_key: String,
    user_id: String,
    asset_chain: String,
    asset: String,
    address: AddressRecordV1,
    key: KeyLocatorRecordV1,
    key_purpose: String,
    expected: [u8; 32],
    birthday: u64,
    expires_at: u64,
    state: DepositStateRecordV2,
    created_at: u64,
}

impl From<&Deposit> for DepositRecordV2 {
    fn from(value: &Deposit) -> Self {
        Self {
            version: DEPOSIT_RECORD_VERSION,
            id: value.id.0.clone(),
            idempotency_key: value.idempotency_key.0.clone(),
            user_id: value.user_id.0.clone(),
            asset_chain: value.asset.chain.0.clone(),
            asset: value.asset.asset.clone(),
            address: (&value.address).into(),
            key: (&value.key).into(),
            key_purpose: value.key_purpose.clone(),
            expected: value.expected.0,
            birthday: value.birthday.0,
            expires_at: value.expires_at,
            state: match &value.state {
                DepositState::AwaitingWatch => DepositStateRecordV2::AwaitingWatch,
                DepositState::Active { watch_id } => {
                    DepositStateRecordV2::Active(watch_id.0.clone())
                }
                DepositState::Expired { watch_id } => {
                    DepositStateRecordV2::Expired(watch_id.0.clone())
                }
                DepositState::Closed => DepositStateRecordV2::Closed,
            },
            created_at: value.created_at,
        }
    }
}

impl TryFrom<DepositRecordV2> for Deposit {
    type Error = DepositError;

    fn try_from(value: DepositRecordV2) -> Result<Self, Self::Error> {
        if value.version != DEPOSIT_RECORD_VERSION {
            return Err(storage_error(format!(
                "unsupported PS deposit record version {}",
                value.version
            )));
        }
        if value.key_purpose.trim().is_empty()
            || value.key_purpose.len() > MAX_KEY_PURPOSE_BYTES
            || value
                .key_purpose
                .bytes()
                .any(|byte| byte.is_ascii_control())
        {
            return Err(storage_error("persisted deposit key purpose is invalid"));
        }
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
            key_purpose: value.key_purpose,
            expected: AtomicAmount(value.expected),
            birthday: BlockHeight(value.birthday),
            expires_at: value.expires_at,
            state: match value.state {
                DepositStateRecordV2::AwaitingWatch => DepositState::AwaitingWatch,
                DepositStateRecordV2::Active(watch_id) => DepositState::Active {
                    watch_id: WatchId(watch_id),
                },
                DepositStateRecordV2::Expired(watch_id) => DepositState::Expired {
                    watch_id: WatchId(watch_id),
                },
                DepositStateRecordV2::Closed => DepositState::Closed,
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
    AccountingV2 {
        idempotency_key: String,
        reason: String,
    },
    ReconciliationResolution {
        case_id: String,
        idempotency_key: String,
        reason: String,
    },
    /// New observation rows use an appended enum variant so the bincode shape
    /// of historical `Observation` rows remains decodable.
    ObservationV2 {
        projection_id: String,
        event_id: String,
        revision: u64,
        status: StatusRecordV1,
        kind: u8,
        movement_ids: Vec<String>,
        network_fee: Option<[u8; 32]>,
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
                network_fee,
            } => LedgerCauseRecordV1::ObservationV2 {
                projection_id: projection_id.0.clone(),
                event_id: event_id.0.clone(),
                revision: observation_revision.0,
                status: status.into(),
                kind: ledger_kind_to_tag(*kind),
                movement_ids: movement_ids.iter().map(|id| id.0.clone()).collect(),
                network_fee: network_fee.map(|amount| amount.0),
            },
            LedgerEntryCause::Accounting {
                idempotency_key,
                reason,
            } => LedgerCauseRecordV1::AccountingV2 {
                idempotency_key: idempotency_key.0.clone(),
                reason: reason.clone(),
            },
            LedgerEntryCause::ReconciliationResolution {
                case_id,
                idempotency_key,
                reason,
            } => LedgerCauseRecordV1::ReconciliationResolution {
                case_id: case_id.0.clone(),
                idempotency_key: idempotency_key.0.clone(),
                reason: reason.clone(),
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
                network_fee: None,
            },
            LedgerCauseRecordV1::Accounting { idempotency_key } => LedgerEntryCause::Accounting {
                idempotency_key: IdempotencyKey(idempotency_key),
                reason: LEGACY_ACCOUNTING_REASON.to_owned(),
            },
            LedgerCauseRecordV1::AccountingV2 {
                idempotency_key,
                reason,
            } => LedgerEntryCause::Accounting {
                idempotency_key: IdempotencyKey(idempotency_key),
                reason,
            },
            LedgerCauseRecordV1::ReconciliationResolution {
                case_id,
                idempotency_key,
                reason,
            } => LedgerEntryCause::ReconciliationResolution {
                case_id: ReconciliationCaseId(case_id),
                idempotency_key: IdempotencyKey(idempotency_key),
                reason,
            },
            LedgerCauseRecordV1::ObservationV2 {
                projection_id,
                event_id,
                revision,
                status,
                kind,
                movement_ids,
                network_fee,
            } => LedgerEntryCause::Observation {
                projection_id: ProjectionId(projection_id),
                event_id: ObservationEventId(event_id),
                observation_revision: ObservationRevision(revision),
                status: status.into(),
                kind: ledger_kind_from_tag(kind)?,
                movement_ids: movement_ids.into_iter().map(MovementId).collect(),
                network_fee: network_fee.map(AtomicAmount),
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
                } => ReconciliationState::LegacyResolved {
                    description: resolution,
                    resolved_at,
                },
            },
            created_at: value.created_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct ReconciliationIdentityRecordV2 {
    principal: String,
    operation: u8,
    client_key: String,
    request_hash: [u8; 32],
}

impl From<&CommandIdentity> for ReconciliationIdentityRecordV2 {
    fn from(value: &CommandIdentity) -> Self {
        Self {
            principal: value.principal.0.clone(),
            operation: 0,
            client_key: value.client_key.0.clone(),
            request_hash: value.request_hash.0,
        }
    }
}

impl TryFrom<ReconciliationIdentityRecordV2> for CommandIdentity {
    type Error = DepositError;

    fn try_from(value: ReconciliationIdentityRecordV2) -> Result<Self, Self::Error> {
        if value.operation != 0 {
            return Err(storage_error(
                "reconciliation resolution record has an unknown command operation",
            ));
        }
        Ok(Self {
            principal: CommandPrincipal(value.principal),
            operation: CommandOperation::ResolveReconciliation,
            client_key: IdempotencyKey(value.client_key),
            request_hash: RequestHash(value.request_hash),
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum ReconciliationDecisionRecordV2 {
    ReverseCredit {
        expected_head: String,
        reason: String,
    },
    AcceptLiability {
        reason: String,
    },
    ExternalDebtRecorded {
        external_reference: String,
        reason: String,
    },
}

impl From<&ReconciliationDecision> for ReconciliationDecisionRecordV2 {
    fn from(value: &ReconciliationDecision) -> Self {
        match value {
            ReconciliationDecision::ReverseCredit {
                expected_head,
                reason,
            } => Self::ReverseCredit {
                expected_head: expected_head.0.clone(),
                reason: reason.clone(),
            },
            ReconciliationDecision::AcceptLiability { reason } => Self::AcceptLiability {
                reason: reason.clone(),
            },
            ReconciliationDecision::ExternalDebtRecorded {
                external_reference,
                reason,
            } => Self::ExternalDebtRecorded {
                external_reference: external_reference.clone(),
                reason: reason.clone(),
            },
        }
    }
}

impl From<ReconciliationDecisionRecordV2> for ReconciliationDecision {
    fn from(value: ReconciliationDecisionRecordV2) -> Self {
        match value {
            ReconciliationDecisionRecordV2::ReverseCredit {
                expected_head,
                reason,
            } => Self::ReverseCredit {
                expected_head: LedgerEntryId(expected_head),
                reason,
            },
            ReconciliationDecisionRecordV2::AcceptLiability { reason } => {
                Self::AcceptLiability { reason }
            }
            ReconciliationDecisionRecordV2::ExternalDebtRecorded {
                external_reference,
                reason,
            } => Self::ExternalDebtRecorded {
                external_reference,
                reason,
            },
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct ReconciliationResolutionRecordV2 {
    command: ReconciliationIdentityRecordV2,
    decision: ReconciliationDecisionRecordV2,
    ledger_entry_id: Option<String>,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum ReconciliationStateRecordV2 {
    Open,
    Resolved {
        resolution: ReconciliationResolutionRecordV2,
        resolved_at: u64,
    },
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct ReconciliationRecordV2 {
    version: u16,
    id: String,
    deposit_id: String,
    triggering_event_id: String,
    reason: ReconciliationReasonRecordV1,
    state: ReconciliationStateRecordV2,
    created_at: u64,
}

impl TryFrom<&ReconciliationCase> for ReconciliationRecordV2 {
    type Error = DepositError;

    fn try_from(value: &ReconciliationCase) -> Result<Self, Self::Error> {
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
            ReconciliationState::Open => ReconciliationStateRecordV2::Open,
            ReconciliationState::Resolved {
                resolution,
                resolved_at,
            } => ReconciliationStateRecordV2::Resolved {
                resolution: ReconciliationResolutionRecordV2 {
                    command: (&resolution.command).into(),
                    decision: (&resolution.decision).into(),
                    ledger_entry_id: resolution
                        .ledger_entry_id
                        .as_ref()
                        .map(|entry| entry.0.clone()),
                },
                resolved_at: *resolved_at,
            },
            ReconciliationState::LegacyResolved { .. } => {
                return Err(storage_error(
                    "legacy reconciliation resolution cannot be rewritten as a typed V2 record",
                ));
            }
        };
        Ok(Self {
            version: RECONCILIATION_RECORD_VERSION,
            id: value.id.0.clone(),
            deposit_id: value.deposit_id.0.clone(),
            triggering_event_id: value.triggering_event_id.0.clone(),
            reason,
            state,
            created_at: value.created_at,
        })
    }
}

impl TryFrom<ReconciliationRecordV2> for ReconciliationCase {
    type Error = DepositError;

    fn try_from(value: ReconciliationRecordV2) -> Result<Self, Self::Error> {
        if value.version != RECONCILIATION_RECORD_VERSION {
            return Err(storage_error(format!(
                "unsupported PS reconciliation record version {}",
                value.version
            )));
        }
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
                ReconciliationStateRecordV2::Open => ReconciliationState::Open,
                ReconciliationStateRecordV2::Resolved {
                    resolution,
                    resolved_at,
                } => {
                    let decision = ReconciliationDecision::from(resolution.decision);
                    let ledger_entry_id = resolution.ledger_entry_id.map(LedgerEntryId);
                    if matches!(decision, ReconciliationDecision::ReverseCredit { .. })
                        != ledger_entry_id.is_some()
                    {
                        return Err(storage_error(
                            "typed reconciliation ledger entry does not match its decision",
                        ));
                    }
                    ReconciliationState::Resolved {
                        resolution: ReconciliationResolution {
                            command: resolution.command.try_into()?,
                            decision,
                            ledger_entry_id,
                        },
                        resolved_at,
                    }
                }
            },
            created_at: value.created_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct ReconciliationResolutionIdempotencyRecordV1 {
    version: u16,
    command: ReconciliationIdentityRecordV2,
    case_id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct IdRecordV1 {
    version: u16,
    id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct AccountingIdentityRecordV1 {
    principal: String,
    operation: u8,
    client_key: String,
    request_hash: [u8; 32],
}

impl From<&CommandIdentity> for AccountingIdentityRecordV1 {
    fn from(value: &CommandIdentity) -> Self {
        Self {
            principal: value.principal.0.clone(),
            operation: 0,
            client_key: value.client_key.0.clone(),
            request_hash: value.request_hash.0,
        }
    }
}

impl TryFrom<AccountingIdentityRecordV1> for CommandIdentity {
    type Error = DepositError;

    fn try_from(value: AccountingIdentityRecordV1) -> Result<Self, Self::Error> {
        if value.operation != 0 {
            return Err(storage_error(
                "accounting idempotency record has an unknown operation",
            ));
        }
        Ok(Self {
            principal: CommandPrincipal(value.principal),
            operation: CommandOperation::Accounting,
            client_key: IdempotencyKey(value.client_key),
            request_hash: RequestHash(value.request_hash),
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct AccountingIdempotencyRecordV1 {
    version: u16,
    command: AccountingIdentityRecordV1,
    ledger_entry_id: String,
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
        key_purpose: command.key_purpose.clone(),
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
            .map(|stored| Ok((decode_deposit(&stored)?, stored)))
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

    async fn reconciliation_generation_change(
        &self,
        deposit_id: &DepositId,
    ) -> Result<(Condition, Operation), DepositError> {
        let key = key_text(&deposit_id.0);
        let stored = self
            .storage
            .get(&reconciliation_generation_ns(), &key)
            .await
            .map_err(map_storage)?;
        if let Some(stored) = &stored {
            let record: IdRecordV1 = decode(stored)?;
            ensure_version(record.version)?;
            if record.id != deposit_id.0 {
                return Err(storage_error(
                    "reconciliation generation belongs to another deposit",
                ));
            }
        }
        let condition = stored.map_or_else(
            || Condition::Missing {
                namespace: reconciliation_generation_ns(),
                key: key.clone(),
            },
            |stored| Condition::Version {
                namespace: reconciliation_generation_ns(),
                key: key.clone(),
                expected: stored.version,
            },
        );
        let operation = Operation::Put {
            namespace: reconciliation_generation_ns(),
            key,
            value: encode(&IdRecordV1 {
                version: RECORD_VERSION,
                id: deposit_id.0.clone(),
            })?,
        };
        Ok((condition, operation))
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

    pub(crate) async fn validate_migration_deposit_idempotency_indexes(
        &self,
        deposits: &[Deposit],
    ) -> Result<(), DepositError> {
        for deposit in deposits {
            let stored = self
                .storage
                .get(
                    &deposit_idempotency_ns(),
                    &key_text(&deposit.idempotency_key.0),
                )
                .await
                .map_err(map_storage)?
                .ok_or_else(|| storage_error("deposit idempotency index is missing"))?;
            let index: IdRecordV1 = decode(&stored)?;
            ensure_version(index.version)?;
            if index.id != deposit.id.0 {
                return Err(storage_error(
                    "deposit idempotency index points to another deposit",
                ));
            }
        }
        Ok(())
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
        let user_key = user_deposit_key(&deposit.user_id, &deposit.id)?;
        let state_key = state_deposit_key(deposit.state.kind(), &deposit.id)?;
        let user_state_key =
            user_state_deposit_key(&deposit.user_id, deposit.state.kind(), &deposit.id)?;
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
            Condition::Missing {
                namespace: user_deposit_ns(),
                key: user_key.clone(),
            },
            Condition::Missing {
                namespace: deposit_state_ns(),
                key: state_key.clone(),
            },
            Condition::Missing {
                namespace: user_deposit_state_ns(),
                key: user_state_key.clone(),
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
                value: encode(&DepositRecordV2::from(deposit))?,
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
            Operation::Put {
                namespace: user_deposit_ns(),
                key: user_key,
                value: encode(&id_record)?,
            },
            Operation::Put {
                namespace: deposit_state_ns(),
                key: state_key,
                value: encode(&id_record)?,
            },
            Operation::Put {
                namespace: user_deposit_state_ns(),
                key: user_state_key,
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

    async fn deposit_indexes_complete(&self) -> Result<bool, DepositError> {
        let Some(stored) = self
            .storage
            .get(&deposit_index_metadata_ns(), &deposit_index_complete_key())
            .await
            .map_err(map_storage)?
        else {
            return Ok(false);
        };
        let record: IdRecordV1 = decode(&stored)?;
        ensure_version(record.version)?;
        if record.id != "complete" {
            return Err(storage_error(
                "deposit index completion marker has an invalid value",
            ));
        }
        Ok(true)
    }

    async fn ensure_deposit_indexes(&self, id: &DepositId) -> Result<(), DepositError> {
        for _ in 0..3 {
            let (deposit, stored_deposit) = self
                .stored_deposit(id)
                .await?
                .ok_or_else(|| storage_error("deposit disappeared during index rebuild"))?;
            let index = IdRecordV1 {
                version: RECORD_VERSION,
                id: deposit.id.0.clone(),
            };
            let specifications = [
                (
                    user_deposit_ns(),
                    user_deposit_key(&deposit.user_id, &deposit.id)?,
                ),
                (
                    deposit_state_ns(),
                    state_deposit_key(deposit.state.kind(), &deposit.id)?,
                ),
                (
                    user_deposit_state_ns(),
                    user_state_deposit_key(&deposit.user_id, deposit.state.kind(), &deposit.id)?,
                ),
            ];
            let mut conditions = vec![Condition::Version {
                namespace: deposit_ns(),
                key: key_text(&deposit.id.0),
                expected: stored_deposit.version,
            }];
            let mut operations = Vec::new();
            for (namespace, key) in &specifications {
                match self
                    .storage
                    .get(namespace, key)
                    .await
                    .map_err(map_storage)?
                {
                    Some(stored) => {
                        let persisted: IdRecordV1 = decode(&stored)?;
                        ensure_version(persisted.version)?;
                        if persisted != index {
                            return Err(storage_error(
                                "deposit association index points to a different deposit",
                            ));
                        }
                    }
                    None => {
                        conditions.push(Condition::Missing {
                            namespace: namespace.clone(),
                            key: key.clone(),
                        });
                        operations.push(Operation::Put {
                            namespace: namespace.clone(),
                            key: key.clone(),
                            value: encode(&index)?,
                        });
                    }
                }
            }
            if operations.is_empty() {
                return Ok(());
            }
            match self
                .storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await
                .map_err(map_storage)
            {
                Ok(_) => return Ok(()),
                Err(error) if error.kind == DepositErrorKind::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        Err(conflict(
            "deposit association indexes changed concurrently during rebuild",
        ))
    }

    async fn scan_authoritative_deposits(
        &self,
        request: &DepositPageRequest,
    ) -> Result<DepositPage, DepositError> {
        let page = self
            .storage
            .scan(ScanRequest {
                namespace: deposit_ns(),
                prefix: Vec::new(),
                after: request.after.as_ref().map(|id| key_text(&id.0)),
                limit: request.limit,
            })
            .await
            .map_err(map_storage)?;
        let has_next = page.next.is_some();
        let mut last_scanned = None;
        let mut deposits = Vec::with_capacity(page.entries.len());
        for (key, stored) in page.entries {
            let deposit = decode_deposit(&stored)?;
            if key != key_text(&deposit.id.0) {
                return Err(storage_error(
                    "deposit row key does not match its record ID",
                ));
            }
            last_scanned = Some(deposit.id.clone());
            if request
                .user_id
                .as_ref()
                .is_some_and(|user_id| user_id != &deposit.user_id)
                || request
                    .state
                    .is_some_and(|state| state != deposit.state.kind())
            {
                continue;
            }
            deposits.push(deposit);
        }
        Ok(DepositPage {
            deposits,
            next: has_next.then_some(last_scanned).flatten(),
        })
    }

    async fn scan_indexed_deposits(
        &self,
        namespace: Namespace,
        prefix: Vec<u8>,
        after: Option<Key>,
        request: &DepositPageRequest,
    ) -> Result<DepositPage, DepositError> {
        let page = self
            .storage
            .scan(ScanRequest {
                namespace,
                prefix,
                after,
                limit: request.limit,
            })
            .await
            .map_err(map_storage)?;
        let has_next = page.next.is_some();
        let mut deposits = Vec::with_capacity(page.entries.len());
        for (_, stored) in page.entries {
            let index: IdRecordV1 = decode(&stored)?;
            ensure_version(index.version)?;
            let deposit = self
                .stored_deposit(&DepositId(index.id))
                .await?
                .map(|(deposit, _)| deposit)
                .ok_or_else(|| storage_error("deposit association index is dangling"))?;
            if request
                .user_id
                .as_ref()
                .is_some_and(|user_id| user_id != &deposit.user_id)
                || request
                    .state
                    .is_some_and(|state| state != deposit.state.kind())
            {
                return Err(storage_error(
                    "deposit association index does not match its filter",
                ));
            }
            deposits.push(deposit);
        }
        let next = has_next
            .then(|| deposits.last().map(|deposit| deposit.id.clone()))
            .flatten();
        Ok(DepositPage { deposits, next })
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
        || command.key_purpose.trim().is_empty()
    {
        return Err(invalid(
            "deposit ID, idempotency key, canonical address, and key purpose must be non-empty",
        ));
    }
    if command.key_purpose.len() > MAX_KEY_PURPOSE_BYTES
        || command
            .key_purpose
            .bytes()
            .any(|byte| byte.is_ascii_control())
    {
        return Err(invalid(
            "deposit key purpose must contain between 1 and 1024 safe bytes",
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

fn validate_deposit_page(limit: usize) -> Result<(), DepositError> {
    if limit == 0 || limit > MAX_PAGE_SIZE {
        Err(invalid("deposit page size must be between 1 and 1000"))
    } else {
        Ok(())
    }
}

fn transition_allowed(current: &DepositState, next: &DepositState) -> bool {
    current == next
        || match (current, next) {
            (DepositState::AwaitingWatch, DepositState::Active { .. }) => true,
            (
                DepositState::Active {
                    watch_id: current_watch,
                },
                DepositState::Expired {
                    watch_id: next_watch,
                },
            ) => current_watch == next_watch,
            _ => false,
        }
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

    fn deposits<'a>(
        &'a self,
        request: DepositPageRequest,
    ) -> BoxFuture<'a, Result<DepositPage, DepositError>> {
        Box::pin(async move {
            validate_deposit_page(request.limit)?;
            if (request.user_id.is_none() && request.state.is_none())
                || !self.deposit_indexes_complete().await?
            {
                return self.scan_authoritative_deposits(&request).await;
            }
            match (&request.user_id, request.state) {
                (None, None) => self.scan_authoritative_deposits(&request).await,
                (Some(user_id), None) => {
                    self.scan_indexed_deposits(
                        user_deposit_ns(),
                        user_deposit_prefix(user_id)?,
                        request
                            .after
                            .as_ref()
                            .map(|id| user_deposit_key(user_id, id))
                            .transpose()?,
                        &request,
                    )
                    .await
                }
                (None, Some(state)) => {
                    self.scan_indexed_deposits(
                        deposit_state_ns(),
                        state_deposit_prefix(state)?,
                        request
                            .after
                            .as_ref()
                            .map(|id| state_deposit_key(state, id))
                            .transpose()?,
                        &request,
                    )
                    .await
                }
                (Some(user_id), Some(state)) => {
                    self.scan_indexed_deposits(
                        user_deposit_state_ns(),
                        user_state_deposit_prefix(user_id, state)?,
                        request
                            .after
                            .as_ref()
                            .map(|id| user_state_deposit_key(user_id, state, id))
                            .transpose()?,
                        &request,
                    )
                    .await
                }
            }
        })
    }

    fn rebuild_deposit_indexes<'a>(
        &'a self,
        request: DepositIndexRebuildRequest,
    ) -> BoxFuture<'a, Result<DepositIndexRebuild, DepositError>> {
        Box::pin(async move {
            validate_deposit_page(request.limit)?;
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: deposit_ns(),
                    prefix: Vec::new(),
                    after: request.after.as_ref().map(|id| key_text(&id.0)),
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let has_next = page.next.is_some();
            let scanned = page.entries.len();
            let mut last_scanned = None;
            for (key, stored) in page.entries {
                let deposit = decode_deposit(&stored)?;
                if key != key_text(&deposit.id.0) {
                    return Err(storage_error(
                        "deposit row key does not match its record ID",
                    ));
                }
                last_scanned = Some(deposit.id.clone());
                self.ensure_deposit_indexes(&deposit.id).await?;
            }
            let next = has_next.then_some(last_scanned).flatten();
            let complete = next.is_none();
            if complete {
                self.storage
                    .commit(WriteBatch {
                        conditions: Vec::new(),
                        operations: vec![Operation::Put {
                            namespace: deposit_index_metadata_ns(),
                            key: deposit_index_complete_key(),
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: "complete".to_owned(),
                            })?,
                        }],
                    })
                    .await
                    .map_err(map_storage)?;
            }
            Ok(DepositIndexRebuild {
                scanned,
                next,
                complete,
            })
        })
    }

    fn set_state<'a>(
        &'a self,
        id: &'a DepositId,
        state: DepositState,
    ) -> BoxFuture<'a, Result<(), DepositError>> {
        Box::pin(async move {
            if state == DepositState::Closed {
                return Err(invalid_state(
                    "deposit closure must use the guarded close command",
                ));
            }
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
            let previous_kind = deposit.state.kind();
            let next_kind = state.kind();
            let was_awaiting = deposit.state == DepositState::AwaitingWatch;
            let is_awaiting = state == DepositState::AwaitingWatch;
            deposit.state = state;
            let mut operations = vec![Operation::Put {
                namespace: deposit_ns(),
                key: key_text(&id.0),
                value: encode(&DepositRecordV2::from(&deposit))?,
            }];
            if was_awaiting && !is_awaiting {
                operations.push(Operation::Delete {
                    namespace: awaiting_watch_ns(),
                    key: key_text(&id.0),
                });
            }
            let mut conditions = vec![Condition::Version {
                namespace: deposit_ns(),
                key: key_text(&id.0),
                expected: stored.version,
            }];
            if previous_kind != next_kind {
                let next_state_key = state_deposit_key(next_kind, id)?;
                let next_user_state_key = user_state_deposit_key(&deposit.user_id, next_kind, id)?;
                conditions.extend([
                    Condition::Missing {
                        namespace: deposit_state_ns(),
                        key: next_state_key.clone(),
                    },
                    Condition::Missing {
                        namespace: user_deposit_state_ns(),
                        key: next_user_state_key.clone(),
                    },
                ]);
                let index = IdRecordV1 {
                    version: RECORD_VERSION,
                    id: id.0.clone(),
                };
                operations.extend([
                    Operation::Delete {
                        namespace: deposit_state_ns(),
                        key: state_deposit_key(previous_kind, id)?,
                    },
                    Operation::Delete {
                        namespace: user_deposit_state_ns(),
                        key: user_state_deposit_key(&deposit.user_id, previous_kind, id)?,
                    },
                    Operation::Put {
                        namespace: deposit_state_ns(),
                        key: next_state_key,
                        value: encode(&index)?,
                    },
                    Operation::Put {
                        namespace: user_deposit_state_ns(),
                        key: next_user_state_key,
                        value: encode(&index)?,
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
        })
    }

    fn close<'a>(&'a self, command: CloseDeposit) -> BoxFuture<'a, Result<(), DepositError>> {
        Box::pin(async move {
            let (mut deposit, deposit_stored) = self
                .stored_deposit(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("deposit does not exist"))?;
            if deposit.state == DepositState::Closed {
                return Ok(());
            }
            if deposit.state != command.expected_state {
                return Err(conflict("deposit state changed before close"));
            }
            let retained_watch = match &deposit.state {
                DepositState::Active { watch_id } | DepositState::Expired { watch_id } => {
                    watch_id.clone()
                }
                DepositState::AwaitingWatch | DepositState::Closed => {
                    return Err(invalid_state(
                        "only an observed active or expired deposit can close",
                    ));
                }
            };
            let (head, head_stored) = self
                .stored_head(&command.deposit_id)
                .await?
                .ok_or_else(|| storage_error("closing deposit has no ledger head"))?;
            if head.id != command.expected_ledger_head {
                return Err(conflict("deposit ledger head changed before close"));
            }
            if !head.balances.balance.is_zero() {
                return Err(invalid_state(
                    "deposit cannot close while its current balance is non-zero",
                ));
            }
            if self.automatic_actions_blocked(&command.deposit_id).await? {
                return Err(invalid_state(
                    "deposit cannot close while reconciliation is unresolved",
                ));
            }
            let (reconciliation_condition, reconciliation_operation) = self
                .reconciliation_generation_change(&command.deposit_id)
                .await?;

            let reservation_key = crate::persistent_collections::reservation_key(
                &command.deposit_id,
                &deposit.asset,
            )?;
            if self
                .storage
                .get(
                    &crate::persistent_collections::active_reservation_ns(),
                    &reservation_key,
                )
                .await
                .map_err(map_storage)?
                .is_some()
            {
                return Err(invalid_state(
                    "deposit cannot close while a collection reservation is active",
                ));
            }
            let (collection_condition, collection_operation) = self
                .collection_eligibility_generation_change(&command.deposit_id, &deposit.asset)
                .await?;

            let previous_kind = deposit.state.kind();
            let next_kind = DepositStateKind::Closed;
            let closed_state_key = state_deposit_key(next_kind, &command.deposit_id)?;
            let closed_user_state_key =
                user_state_deposit_key(&deposit.user_id, next_kind, &command.deposit_id)?;
            let closed_watch_key = key_text(&command.deposit_id.0);
            let index = IdRecordV1 {
                version: RECORD_VERSION,
                id: command.deposit_id.0.clone(),
            };
            deposit.state = DepositState::Closed;

            self.storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Version {
                            namespace: deposit_ns(),
                            key: key_text(&command.deposit_id.0),
                            expected: deposit_stored.version,
                        },
                        Condition::Version {
                            namespace: ledger_head_ns(),
                            key: key_text(&command.deposit_id.0),
                            expected: head_stored.version,
                        },
                        Condition::Missing {
                            namespace: crate::persistent_collections::active_reservation_ns(),
                            key: reservation_key,
                        },
                        Condition::Missing {
                            namespace: closed_deposit_watch_ns(),
                            key: closed_watch_key.clone(),
                        },
                        Condition::Missing {
                            namespace: deposit_state_ns(),
                            key: closed_state_key.clone(),
                        },
                        Condition::Missing {
                            namespace: user_deposit_state_ns(),
                            key: closed_user_state_key.clone(),
                        },
                        reconciliation_condition,
                        collection_condition,
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: deposit_ns(),
                            key: key_text(&command.deposit_id.0),
                            value: encode(&DepositRecordV2::from(&deposit))?,
                        },
                        // Rewriting the same head ID increments its storage
                        // version, invalidating a projection that read the zero
                        // head before this close committed.
                        Operation::Put {
                            namespace: ledger_head_ns(),
                            key: key_text(&command.deposit_id.0),
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: head.id.0,
                            })?,
                        },
                        Operation::Delete {
                            namespace: deposit_state_ns(),
                            key: state_deposit_key(previous_kind, &command.deposit_id)?,
                        },
                        Operation::Delete {
                            namespace: user_deposit_state_ns(),
                            key: user_state_deposit_key(
                                &deposit.user_id,
                                previous_kind,
                                &command.deposit_id,
                            )?,
                        },
                        Operation::Put {
                            namespace: deposit_state_ns(),
                            key: closed_state_key,
                            value: encode(&index)?,
                        },
                        Operation::Put {
                            namespace: user_deposit_state_ns(),
                            key: closed_user_state_key,
                            value: encode(&index)?,
                        },
                        // Keep the durable watch relationship after closure.
                        // Late transfers remain visible; a future explicit IX
                        // cutoff protocol can use this retained identifier.
                        Operation::Put {
                            namespace: closed_deposit_watch_ns(),
                            key: closed_watch_key,
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: retained_watch.0,
                            })?,
                        },
                        reconciliation_operation,
                        collection_operation,
                    ],
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
            let expected_watch_id = watch_id.clone();
            deposit.state = DepositState::Active { watch_id };
            let active_state_key = state_deposit_key(DepositStateKind::Active, id)?;
            let active_user_state_key =
                user_state_deposit_key(&deposit.user_id, DepositStateKind::Active, id)?;
            let index = IdRecordV1 {
                version: RECORD_VERSION,
                id: id.0.clone(),
            };
            let commit = self
                .storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Version {
                            namespace: deposit_ns(),
                            key: key_text(&id.0),
                            expected: stored.version,
                        },
                        Condition::Missing {
                            namespace: deposit_state_ns(),
                            key: active_state_key.clone(),
                        },
                        Condition::Missing {
                            namespace: user_deposit_state_ns(),
                            key: active_user_state_key.clone(),
                        },
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: deposit_ns(),
                            key: key_text(&id.0),
                            value: encode(&DepositRecordV2::from(&deposit))?,
                        },
                        Operation::Delete {
                            namespace: awaiting_watch_ns(),
                            key: key_text(&id.0),
                        },
                        Operation::Delete {
                            namespace: deposit_state_ns(),
                            key: state_deposit_key(DepositStateKind::AwaitingWatch, id)?,
                        },
                        Operation::Delete {
                            namespace: user_deposit_state_ns(),
                            key: user_state_deposit_key(
                                &deposit.user_id,
                                DepositStateKind::AwaitingWatch,
                                id,
                            )?,
                        },
                        Operation::Put {
                            namespace: deposit_state_ns(),
                            key: active_state_key,
                            value: encode(&index)?,
                        },
                        Operation::Put {
                            namespace: user_deposit_state_ns(),
                            key: active_user_state_key,
                            value: encode(&index)?,
                        },
                    ],
                })
                .await;
            match commit {
                Ok(_) => Ok(deposit),
                Err(error) if error.kind == StorageErrorKind::Conflict => {
                    let Some((concurrent, _)) = self.stored_deposit(id).await? else {
                        return Err(map_storage(error));
                    };
                    if &concurrent.idempotency_key == idempotency_key
                        && concurrent.state
                            == (DepositState::Active {
                                watch_id: expected_watch_id,
                            })
                    {
                        Ok(concurrent)
                    } else {
                        Err(map_storage(error))
                    }
                }
                Err(error) => Err(map_storage(error)),
            }
        })
    }
}

fn resolved_movement_amounts(
    event: &ObservationEvent,
    deposit: &Deposit,
    movement_ids: &[MovementId],
) -> Result<Vec<AtomicAmount>, DepositError> {
    let mut seen = std::collections::BTreeSet::new();
    movement_ids
        .iter()
        .map(|movement_id| {
            if !seen.insert(movement_id.clone()) {
                return Err(invalid(
                    "observation ledger effect contains a duplicate movement ID",
                ));
            }
            let mut matches = event
                .transaction
                .movements
                .iter()
                .filter(|movement| movement.id == *movement_id);
            let movement = matches.next().ok_or_else(|| {
                invalid("observation ledger effect references a missing IX movement")
            })?;
            if matches.next().is_some() {
                return Err(invalid(
                    "mirrored IX event contains a duplicate movement ID",
                ));
            }
            if movement.asset != deposit.asset {
                return Err(invalid(
                    "observation ledger movement asset does not match the deposit asset",
                ));
            }
            Ok(movement.amount)
        })
        .collect()
}

fn resolved_network_fee(event: &ObservationEvent, deposit: &Deposit) -> Option<AtomicAmount> {
    event
        .transaction
        .fee
        .as_ref()
        .filter(|fee| fee.asset == deposit.asset && fee.payer.as_ref() == Some(&deposit.address))
        .map(|fee| fee.amount)
}

fn resolved_effect(
    event: &ObservationEvent,
    deposit: &Deposit,
    effect: &ObservationLedgerEffect,
) -> Result<LedgerEffect<AtomicAmount>, DepositError> {
    Ok(match effect {
        LedgerEffect::Incoming { movements } => LedgerEffect::Incoming {
            movements: resolved_movement_amounts(event, deposit, movements)?,
        },
        LedgerEffect::Collection { movements } => LedgerEffect::Collection {
            movements: resolved_movement_amounts(event, deposit, movements)?,
        },
        LedgerEffect::GasFunding { movements } => LedgerEffect::GasFunding {
            movements: resolved_movement_amounts(event, deposit, movements)?,
        },
        LedgerEffect::OtherBalanceChange {
            direction,
            movements,
        } => LedgerEffect::OtherBalanceChange {
            direction: *direction,
            movements: resolved_movement_amounts(event, deposit, movements)?,
        },
    })
}

fn projection_entry(
    command: &RecordObservation,
    event: &ObservationEvent,
    current: &LedgerEntry,
    effect: LedgerEffect<AtomicAmount>,
    network_fee: Option<AtomicAmount>,
) -> Result<LedgerEntry, DepositError> {
    let projection_id =
        ProjectionId::for_observation(&event.id, event.transaction.revision, &command.deposit_id);
    let balances = apply_observation_transition(
        current.balances,
        &LedgerObservationTransition {
            status: event.transaction.status.clone(),
            previous_status: event.previous_status.clone(),
            effect,
            network_fee,
        },
    )
    .map_err(|error| invalid(format!("invalid observation ledger transition: {error}")))?;
    Ok(LedgerEntry {
        id: LedgerEntryId(format!("projection:{}", projection_id.0)),
        deposit_id: command.deposit_id.clone(),
        previous: Some(current.id.clone()),
        cause: LedgerEntryCause::Observation {
            projection_id,
            event_id: event.id.clone(),
            observation_revision: event.transaction.revision,
            status: event.transaction.status.clone(),
            kind: command.effect.kind(),
            movement_ids: command.effect.movements().to_vec(),
            network_fee,
        },
        balances,
        recorded_at: command.recorded_at,
    })
}

fn accounting_entry(command: &AccountingCommand, current: &LedgerEntry) -> LedgerEntry {
    let mut balances = current.balances;
    balances.accounted = command.next_accounted;
    LedgerEntry {
        id: opaque_command_ledger_entry_id("accounting", &command.command),
        deposit_id: command.deposit_id.clone(),
        previous: Some(current.id.clone()),
        cause: LedgerEntryCause::Accounting {
            idempotency_key: command.command.client_key.clone(),
            reason: command.reason.clone(),
        },
        balances,
        recorded_at: command.recorded_at,
    }
}

fn validate_accounting_command(command: &AccountingCommand) -> Result<(), DepositError> {
    if command.command.operation != CommandOperation::Accounting {
        return Err(invalid(
            "accounting command identity must use the accounting operation",
        ));
    }
    if command.command.principal.0.is_empty()
        || command.command.client_key.0.is_empty()
        || command.deposit_id.0.is_empty()
    {
        return Err(invalid(
            "accounting principal, client key, and deposit ID must be non-empty",
        ));
    }
    if command.reason.trim().is_empty() {
        return Err(invalid("accounting reason must not be blank"));
    }
    if command.reason.len() > MAX_ACCOUNTING_REASON_BYTES {
        return Err(invalid(format!(
            "accounting reason must not exceed {MAX_ACCOUNTING_REASON_BYTES} bytes"
        )));
    }
    Ok(())
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
        command: RecordObservation,
    ) -> BoxFuture<'a, Result<ApplyResult, DepositError>> {
        Box::pin(async move {
            let mirrored = self
                .mirrored_observation(&command.event_id)
                .await?
                .map(|(observation, _)| observation)
                .ok_or_else(|| not_found("observation projection requires a mirrored IX event"))?;
            let deposit = self
                .deposit(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("observation projection deposit does not exist"))?;
            let expected_head_id = command
                .expected_head
                .as_ref()
                .ok_or_else(|| conflict("observation projection requires an expected head"))?;
            let expected_head = self
                .stored_ledger_entry(&command.deposit_id, expected_head_id)
                .await?
                .ok_or_else(|| conflict("ledger expected head does not exist"))?;
            let entry = projection_entry(
                &command,
                &mirrored.event,
                &expected_head,
                resolved_effect(&mirrored.event, &deposit, &command.effect)?,
                resolved_network_fee(&mirrored.event, &deposit),
            )?;
            let projection_id = ProjectionId::for_observation(
                &mirrored.event.id,
                mirrored.event.transaction.revision,
                &command.deposit_id,
            );
            let projection_key = key_text(&projection_id.0);
            let deposit_observation_key =
                deposit_observation_key(&command.deposit_id, mirrored.event.cursor)?;
            let stored_deposit_observation = self
                .storage
                .get(&deposit_observation_ns(), &deposit_observation_key)
                .await
                .map_err(map_storage)?;
            if let Some(stored) = &stored_deposit_observation {
                let index: IdRecordV1 = decode(stored)?;
                ensure_version(index.version)?;
                if index.id != mirrored.event.id.0 {
                    return Err(conflict(
                        "deposit observation cursor is assigned to a different IX event",
                    ));
                }
            }
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
                if existing == entry {
                    if stored_deposit_observation.is_none() {
                        self.storage
                            .commit(WriteBatch {
                                conditions: vec![Condition::Missing {
                                    namespace: deposit_observation_ns(),
                                    key: deposit_observation_key.clone(),
                                }],
                                operations: vec![Operation::Put {
                                    namespace: deposit_observation_ns(),
                                    key: deposit_observation_key,
                                    value: encode(&IdRecordV1 {
                                        version: RECORD_VERSION,
                                        id: mirrored.event.id.0.clone(),
                                    })?,
                                }],
                            })
                            .await
                            .map_err(map_storage)?;
                    }
                    return Ok(ApplyResult::AlreadyPresent { entry: existing });
                }
                return Err(conflict(
                    "deterministic projection identity was reused with a different ledger effect",
                ));
            }
            let (current, head_stored) = self
                .stored_head(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("deposit ledger is not open"))?;
            if command.expected_head.as_ref() != Some(&current.id) {
                return Err(conflict("ledger expected head does not match current head"));
            }
            let mut conditions = vec![
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
            ];
            let mut operations = vec![
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
            ];
            if stored_deposit_observation.is_none() {
                conditions.push(Condition::Missing {
                    namespace: deposit_observation_ns(),
                    key: deposit_observation_key.clone(),
                });
                operations.push(Operation::Put {
                    namespace: deposit_observation_ns(),
                    key: deposit_observation_key,
                    value: encode(&IdRecordV1 {
                        version: RECORD_VERSION,
                        id: mirrored.event.id.0.clone(),
                    })?,
                });
            }
            self.storage
                .commit(WriteBatch {
                    conditions,
                    operations,
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
            validate_accounting_command(&command)?;
            let idempotency_key = accounting_command_key(&command.command)?;
            if let Some(stored) = self
                .storage
                .get(&accounting_idempotency_ns(), &idempotency_key)
                .await
                .map_err(map_storage)?
            {
                let index: AccountingIdempotencyRecordV1 = decode(&stored)?;
                ensure_version(index.version)?;
                let stored_command = CommandIdentity::try_from(index.command)?;
                if stored_command != command.command {
                    return Err(conflict(
                        "accounting idempotency key was reused with a different request hash",
                    ));
                }
                let existing = self
                    .stored_ledger_entry(&command.deposit_id, &LedgerEntryId(index.ledger_entry_id))
                    .await?
                    .ok_or_else(|| storage_error("accounting idempotency index is dangling"))?;
                return Ok(ApplyResult::AlreadyPresent { entry: existing });
            }
            if self
                .storage
                .get(
                    &accounting_idempotency_ns(),
                    &key_text(&command.command.client_key.0),
                )
                .await
                .map_err(map_storage)?
                .is_some()
            {
                return Err(conflict(
                    "legacy unscoped accounting idempotency record requires migration",
                ));
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
                            value: encode(&AccountingIdempotencyRecordV1 {
                                version: RECORD_VERSION,
                                command: AccountingIdentityRecordV1::from(&command.command),
                                ledger_entry_id: entry.id.0.clone(),
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

    fn observations_for_deposit<'a>(
        &'a self,
        request: DepositObservationLogRequest,
    ) -> BoxFuture<'a, Result<DepositObservationLogPage, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > MAX_PAGE_SIZE {
                return Err(invalid(
                    "deposit observation page size must be between 1 and 1000",
                ));
            }
            if request.deposit_id.0.is_empty() {
                return Err(invalid("deposit observation lookup requires a deposit ID"));
            }
            if self.deposit(&request.deposit_id).await?.is_none() {
                return Err(not_found(
                    "deposit observation lookup deposit does not exist",
                ));
            }
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: deposit_observation_ns(),
                    prefix: deposit_observation_prefix(&request.deposit_id)?,
                    after: request
                        .after
                        .map(|cursor| deposit_observation_key(&request.deposit_id, cursor))
                        .transpose()?,
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let has_next = page.next.is_some();
            let mut observations = Vec::with_capacity(page.entries.len());
            for (key, stored) in page.entries {
                let index: IdRecordV1 = decode(&stored)?;
                ensure_version(index.version)?;
                let observation = self
                    .observation(&ObservationEventId(index.id))
                    .await?
                    .ok_or_else(|| storage_error("deposit observation index is dangling"))?;
                if key != deposit_observation_key(&request.deposit_id, observation.event.cursor)? {
                    return Err(storage_error(
                        "deposit observation index key does not match its mirrored IX cursor",
                    ));
                }
                observations.push(observation);
            }
            let next = if has_next {
                observations
                    .last()
                    .map(|observation| observation.event.cursor)
            } else {
                None
            };
            Ok(DepositObservationLogPage { observations, next })
        })
    }
}

impl<S> PersistentPaymentRepository<S>
where
    S: Storage,
{
    /// Rebuilds the derived deposit-to-observation index while an offline
    /// semantic migration exclusively owns the database.
    pub(crate) async fn migration_rebuild_deposit_observation_index(
        &self,
        attributions: &[(DepositId, EventCursor, ObservationEventId)],
        page_size: usize,
    ) -> Result<usize, DepositError> {
        if page_size == 0 || page_size > MAX_PAGE_SIZE {
            return Err(invalid("migration page size must be between 1 and 1000"));
        }

        let mut desired = std::collections::BTreeMap::<Key, IdRecordV1>::new();
        for (deposit_id, cursor, event_id) in attributions {
            if self.deposit(deposit_id).await?.is_none() {
                return Err(not_found(
                    "deposit observation migration references a missing deposit",
                ));
            }
            let observation = self.observation(event_id).await?.ok_or_else(|| {
                storage_error("deposit observation migration references a missing mirror")
            })?;
            if observation.event.cursor != *cursor {
                return Err(conflict(
                    "deposit observation migration cursor does not match its mirror",
                ));
            }
            let record = IdRecordV1 {
                version: RECORD_VERSION,
                id: event_id.0.clone(),
            };
            if let Some(existing) = desired.insert(
                deposit_observation_key(deposit_id, *cursor)?,
                record.clone(),
            ) && existing != record
            {
                return Err(conflict(
                    "one deposit observation cursor maps to multiple IX events",
                ));
            }
        }

        let mut existing = std::collections::BTreeMap::<Key, StoredValue>::new();
        let mut after = None;
        loop {
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: deposit_observation_ns(),
                    prefix: Vec::new(),
                    after,
                    limit: page_size,
                })
                .await
                .map_err(map_storage)?;
            for (key, stored) in page.entries {
                existing.insert(key, stored);
            }
            let Some(next) = page.next else {
                break;
            };
            after = Some(next);
        }

        let mut conditions = Vec::new();
        let mut operations = Vec::new();
        for (key, stored) in &existing {
            if desired.contains_key(key) {
                continue;
            }
            conditions.push(Condition::Version {
                namespace: deposit_observation_ns(),
                key: key.clone(),
                expected: stored.version,
            });
            operations.push(Operation::Delete {
                namespace: deposit_observation_ns(),
                key: key.clone(),
            });
        }
        for (key, record) in &desired {
            let encoded = encode(record)?;
            match existing.get(key) {
                Some(stored) if stored.value == encoded => {}
                Some(stored) => {
                    conditions.push(Condition::Version {
                        namespace: deposit_observation_ns(),
                        key: key.clone(),
                        expected: stored.version,
                    });
                    operations.push(Operation::Put {
                        namespace: deposit_observation_ns(),
                        key: key.clone(),
                        value: encoded,
                    });
                }
                None => {
                    conditions.push(Condition::Missing {
                        namespace: deposit_observation_ns(),
                        key: key.clone(),
                    });
                    operations.push(Operation::Put {
                        namespace: deposit_observation_ns(),
                        key: key.clone(),
                        value: encoded,
                    });
                }
            }
        }
        if !operations.is_empty() {
            self.storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await
                .map_err(map_storage)?;
        }
        Ok(desired.len())
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
            let affected_deposits = command
                .affected_deposits
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            if affected_deposits.len() != command.affected_deposits.len()
                || affected_deposits
                    .iter()
                    .any(|deposit_id| deposit_id.0.is_empty())
            {
                return Err(invalid(
                    "projection affected deposits must be unique and non-empty",
                ));
            }
            if command
                .ledger_updates
                .iter()
                .any(|update| !affected_deposits.contains(&update.deposit_id))
                || command
                    .reconciliation_cases
                    .iter()
                    .any(|case| !affected_deposits.contains(&case.deposit_id))
            {
                return Err(invalid(
                    "every projected ledger update and reconciliation case must identify an affected deposit",
                ));
            }
            let ledger_update_deposits = command
                .ledger_updates
                .iter()
                .map(|update| update.deposit_id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            if command
                .reconciliation_cases
                .iter()
                .any(|case| !ledger_update_deposits.contains(&case.deposit_id))
            {
                return Err(invalid(
                    "every projected reconciliation case must share its deposit's atomic ledger update",
                ));
            }
            let (checkpoint, checkpoint_stored) = self
                .stored_checkpoint(ConsumerCheckpointName::IxProjection)
                .await?;
            if checkpoint.cursor == Some(command.through) {
                let mut ledger_results = Vec::with_capacity(command.ledger_updates.len());
                for update in &command.ledger_updates {
                    let mirrored = self.observation(&update.event_id).await?.ok_or_else(|| {
                        storage_error("projection cursor advanced without its mirrored event")
                    })?;
                    if mirrored.event.cursor != command.through {
                        return Err(conflict(
                            "projection retry references a different IX cursor",
                        ));
                    }
                    let deposit = self
                        .deposit(&update.deposit_id)
                        .await?
                        .ok_or_else(|| storage_error("projected deposit is missing"))?;
                    let expected_head_id = update.expected_head.as_ref().ok_or_else(|| {
                        conflict("observation projection requires an expected head")
                    })?;
                    let expected_head = self
                        .stored_ledger_entry(&update.deposit_id, expected_head_id)
                        .await?
                        .ok_or_else(|| conflict("ledger expected head does not exist"))?;
                    let expected_entry = projection_entry(
                        update,
                        &mirrored.event,
                        &expected_head,
                        resolved_effect(&mirrored.event, &deposit, &update.effect)?,
                        resolved_network_fee(&mirrored.event, &deposit),
                    )?;
                    let projection_id = ProjectionId::for_observation(
                        &mirrored.event.id,
                        mirrored.event.transaction.revision,
                        &update.deposit_id,
                    );
                    let stored = self
                        .storage
                        .get(&projection_ns(), &key_text(&projection_id.0))
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
                    if entry != expected_entry {
                        return Err(conflict(
                            "projection retry changed the deterministic ledger effect",
                        ));
                    }
                    ledger_results.push(ApplyResult::AlreadyPresent { entry });
                }
                let mut cases = Vec::with_capacity(command.reconciliation_cases.len());
                for case in &command.reconciliation_cases {
                    cases.push(self.case(&case.id).await?.ok_or_else(|| {
                        storage_error("projection cursor advanced without a reconciliation case")
                    })?);
                }
                for deposit_id in &affected_deposits {
                    let stored = self
                        .storage
                        .get(
                            &deposit_observation_ns(),
                            &deposit_observation_key(deposit_id, command.through)?,
                        )
                        .await
                        .map_err(map_storage)?
                        .ok_or_else(|| {
                            storage_error(
                                "projection cursor advanced without a deposit observation index",
                            )
                        })?;
                    let index: IdRecordV1 = decode(&stored)?;
                    ensure_version(index.version)?;
                    let event = self.observation(&ObservationEventId(index.id)).await?;
                    if event.as_ref().map(|observation| observation.event.cursor)
                        != Some(command.through)
                    {
                        return Err(conflict(
                            "projection retry changed a deposit observation attribution",
                        ));
                    }
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
                let projection_id = ProjectionId::for_observation(
                    &event.event.id,
                    event.event.transaction.revision,
                    &update.deposit_id,
                );
                let projection_key = key_text(&projection_id.0);
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
                let deposit = self
                    .deposit(&update.deposit_id)
                    .await?
                    .ok_or_else(|| not_found("observation projection deposit does not exist"))?;
                let entry = projection_entry(
                    update,
                    &event.event,
                    &head,
                    resolved_effect(&event.event, &deposit, &update.effect)?,
                    resolved_network_fee(&event.event, &deposit),
                )?;
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

            for deposit_id in &affected_deposits {
                if self.deposit(deposit_id).await?.is_none() {
                    return Err(not_found(
                        "deposit observation attribution references a missing deposit",
                    ));
                }
                let key = deposit_observation_key(deposit_id, command.through)?;
                if self
                    .storage
                    .get(&deposit_observation_ns(), &key)
                    .await
                    .map_err(map_storage)?
                    .is_some()
                {
                    return Err(conflict(
                        "deposit observation index exists while projection cursor is behind",
                    ));
                }
                conditions.push(Condition::Missing {
                    namespace: deposit_observation_ns(),
                    key: key.clone(),
                });
                operations.push(Operation::Put {
                    namespace: deposit_observation_ns(),
                    key,
                    value: encode(&IdRecordV1 {
                        version: RECORD_VERSION,
                        id: event.event.id.0.clone(),
                    })?,
                });
            }

            let mut reconciliation_generation_deposits = std::collections::BTreeSet::new();
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
                if reconciliation_generation_deposits.insert(case.deposit_id.clone()) {
                    let (generation_condition, generation_operation) = self
                        .reconciliation_generation_change(&case.deposit_id)
                        .await?;
                    conditions.push(generation_condition);
                    operations.push(generation_operation);
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
                        value: encode(&ReconciliationRecordV2::try_from(case)?)?,
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

fn reconciliation_decision_reason(decision: &ReconciliationDecision) -> &str {
    match decision {
        ReconciliationDecision::ReverseCredit { reason, .. }
        | ReconciliationDecision::AcceptLiability { reason }
        | ReconciliationDecision::ExternalDebtRecorded { reason, .. } => reason,
    }
}

fn validate_reconciliation_resolution(command: &ResolveReconciliation) -> Result<(), DepositError> {
    if command.command.operation != CommandOperation::ResolveReconciliation {
        return Err(invalid(
            "reconciliation command identity must use the resolve-reconciliation operation",
        ));
    }
    if command.command.principal.0.is_empty()
        || command.command.client_key.0.is_empty()
        || command.case_id.0.is_empty()
    {
        return Err(invalid(
            "reconciliation principal, client key, and case ID must be non-empty",
        ));
    }
    let reason = reconciliation_decision_reason(&command.decision);
    if reason.trim().is_empty() {
        return Err(invalid(
            "reconciliation resolution reason must not be blank",
        ));
    }
    if reason.len() > MAX_RECONCILIATION_REASON_BYTES {
        return Err(invalid(format!(
            "reconciliation resolution reason must not exceed {MAX_RECONCILIATION_REASON_BYTES} bytes"
        )));
    }
    match &command.decision {
        ReconciliationDecision::ReverseCredit { expected_head, .. }
            if expected_head.0.is_empty() =>
        {
            Err(invalid(
                "reverse-credit resolution requires a non-empty expected ledger head",
            ))
        }
        ReconciliationDecision::ExternalDebtRecorded {
            external_reference, ..
        } if external_reference.trim().is_empty()
            || external_reference.len() > MAX_EXTERNAL_DEBT_REFERENCE_BYTES
            || external_reference
                .bytes()
                .any(|byte| byte.is_ascii_control()) =>
        {
            Err(invalid(format!(
                "external debt reference must contain between 1 and {MAX_EXTERNAL_DEBT_REFERENCE_BYTES} safe bytes"
            )))
        }
        _ => Ok(()),
    }
}

fn reconciliation_resolution_entry(
    command: &ResolveReconciliation,
    current: &LedgerEntry,
) -> LedgerEntry {
    let mut balances = current.balances;
    balances.accounted = balances.accounted.min(balances.confirmed);
    LedgerEntry {
        id: opaque_command_ledger_entry_id("reconciliation", &command.command),
        deposit_id: current.deposit_id.clone(),
        previous: Some(current.id.clone()),
        cause: LedgerEntryCause::ReconciliationResolution {
            case_id: command.case_id.clone(),
            idempotency_key: command.command.client_key.clone(),
            reason: reconciliation_decision_reason(&command.decision).to_owned(),
        },
        balances,
        recorded_at: command.resolved_at,
    }
}

fn opaque_command_ledger_entry_id(kind: &str, command: &CommandIdentity) -> LedgerEntryId {
    let mut digest = Sha256::new();
    digest.update(b"payment-service-ledger-entry-v1");
    update_hash_component(&mut digest, kind.as_bytes());
    update_hash_component(&mut digest, command.principal.0.as_bytes());
    update_hash_component(&mut digest, command_operation_tag(command.operation));
    update_hash_component(&mut digest, command.client_key.0.as_bytes());
    update_hash_component(&mut digest, &command.request_hash.0);
    let digest = digest.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    LedgerEntryId(format!("{kind}:{encoded}"))
}

fn update_hash_component(digest: &mut Sha256, component: &[u8]) {
    digest.update(
        u64::try_from(component.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(component);
}

const fn command_operation_tag(operation: CommandOperation) -> &'static [u8] {
    match operation {
        CommandOperation::CreateDeposit => b"create_deposit",
        CommandOperation::CloseDeposit => b"close_deposit",
        CommandOperation::CreateCollection => b"create_collection",
        CommandOperation::RetryCollection => b"retry_collection",
        CommandOperation::Accounting => b"accounting",
        CommandOperation::ResolveReconciliation => b"resolve_reconciliation",
    }
}

impl<S> PersistentPaymentRepository<S>
where
    S: Storage,
{
    async fn replay_reconciliation_resolution(
        &self,
        command: &ResolveReconciliation,
        idempotency_key: &Key,
    ) -> Result<Option<ReconciliationCase>, DepositError> {
        let Some(stored) = self
            .storage
            .get(&reconciliation_resolution_idempotency_ns(), idempotency_key)
            .await
            .map_err(map_storage)?
        else {
            return Ok(None);
        };
        let record: ReconciliationResolutionIdempotencyRecordV1 = decode(&stored)?;
        ensure_version(record.version)?;
        let stored_command = CommandIdentity::try_from(record.command)?;
        if stored_command != command.command || record.case_id != command.case_id.0 {
            return Err(conflict(
                "reconciliation idempotency key was reused with different request content",
            ));
        }
        let case = self
            .case(&command.case_id)
            .await?
            .ok_or_else(|| storage_error("reconciliation idempotency index is dangling"))?;
        match &case.state {
            ReconciliationState::Resolved { resolution, .. }
                if resolution.command == command.command
                    && resolution.decision == command.decision =>
            {
                Ok(Some(case))
            }
            ReconciliationState::Resolved { .. } => Err(conflict(
                "reconciliation idempotency key was reused with different request content",
            )),
            ReconciliationState::Open | ReconciliationState::LegacyResolved { .. } => Err(
                storage_error("reconciliation idempotency index does not reference a typed result"),
            ),
        }
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
            let (generation_condition, generation_operation) = self
                .reconciliation_generation_change(&case.deposit_id)
                .await?;
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
                        generation_condition,
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: reconciliation_ns(),
                            key: case_key,
                            value: encode(&ReconciliationRecordV2::try_from(&case)?)?,
                        },
                        Operation::Put {
                            namespace: reconciliation_deposit_ns(),
                            key: deposit_key,
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: case.id.0.clone(),
                            })?,
                        },
                        generation_operation,
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
                .map(|stored| decode_reconciliation(&stored))
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
                        decode_reconciliation(&stored)?
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
        command: ResolveReconciliation,
    ) -> BoxFuture<'a, Result<ReconciliationCase, DepositError>> {
        Box::pin(async move {
            validate_reconciliation_resolution(&command)?;
            let idempotency_key = reconciliation_command_key(&command.command)?;
            if let Some(case) = self
                .replay_reconciliation_resolution(&command, &idempotency_key)
                .await?
            {
                return Ok(case);
            }
            let key = key_text(&command.case_id.0);
            let stored = self
                .storage
                .get(&reconciliation_ns(), &key)
                .await
                .map_err(map_storage)?
                .ok_or_else(|| not_found("reconciliation case does not exist"))?;
            let mut case = decode_reconciliation(&stored)?;
            match &case.state {
                ReconciliationState::Open => {}
                ReconciliationState::Resolved { .. }
                | ReconciliationState::LegacyResolved { .. } => {
                    return Err(conflict("reconciliation case has already been resolved"));
                }
            }

            let (generation_condition, generation_operation) = self
                .reconciliation_generation_change(&case.deposit_id)
                .await?;

            let mut conditions = vec![
                Condition::Missing {
                    namespace: reconciliation_resolution_idempotency_ns(),
                    key: idempotency_key.clone(),
                },
                Condition::Version {
                    namespace: reconciliation_ns(),
                    key: key.clone(),
                    expected: stored.version,
                },
                generation_condition,
            ];
            let mut operations = vec![generation_operation];
            let ledger_entry = match &command.decision {
                ReconciliationDecision::ReverseCredit { expected_head, .. } => {
                    let (current, head_stored) = self
                        .stored_head(&case.deposit_id)
                        .await?
                        .ok_or_else(|| not_found("reconciliation deposit ledger is not open"))?;
                    if expected_head != &current.id {
                        return Err(conflict(
                            "reconciliation expected ledger head does not match current head",
                        ));
                    }
                    let entry = reconciliation_resolution_entry(&command, &current);
                    if entry.balances.accounted > entry.balances.confirmed {
                        return Err(invalid(
                            "reverse-credit resolution left accounted above confirmed",
                        ));
                    }
                    conditions.extend([
                        Condition::Version {
                            namespace: ledger_head_ns(),
                            key: key_text(&case.deposit_id.0),
                            expected: head_stored.version,
                        },
                        Condition::Missing {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&case.deposit_id, &entry.id)?,
                        },
                    ]);
                    operations.extend([
                        Operation::Put {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&case.deposit_id, &entry.id)?,
                            value: encode(&LedgerEntryRecordV1::from(&entry))?,
                        },
                        Operation::Put {
                            namespace: ledger_head_ns(),
                            key: key_text(&case.deposit_id.0),
                            value: encode(&IdRecordV1 {
                                version: RECORD_VERSION,
                                id: entry.id.0.clone(),
                            })?,
                        },
                    ]);
                    Some(entry)
                }
                ReconciliationDecision::AcceptLiability { .. }
                | ReconciliationDecision::ExternalDebtRecorded { .. } => None,
            };
            let resolution = ReconciliationResolution {
                command: command.command.clone(),
                decision: command.decision.clone(),
                ledger_entry_id: ledger_entry.as_ref().map(|entry| entry.id.clone()),
            };
            case.state = ReconciliationState::Resolved {
                resolution,
                resolved_at: command.resolved_at,
            };
            operations.extend([
                Operation::Put {
                    namespace: reconciliation_ns(),
                    key,
                    value: encode(&ReconciliationRecordV2::try_from(&case)?)?,
                },
                Operation::Put {
                    namespace: reconciliation_resolution_idempotency_ns(),
                    key: idempotency_key.clone(),
                    value: encode(&ReconciliationResolutionIdempotencyRecordV1 {
                        version: RECORD_VERSION,
                        command: ReconciliationIdentityRecordV2::from(&command.command),
                        case_id: command.case_id.0.clone(),
                    })?,
                },
            ]);
            let result = self
                .storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await;
            match result {
                Ok(_) => Ok(case),
                Err(error) if error.kind == StorageErrorKind::Conflict => {
                    if let Some(case) = self
                        .replay_reconciliation_resolution(&command, &idempotency_key)
                        .await?
                    {
                        Ok(case)
                    } else {
                        Err(conflict(
                            "reconciliation case or ledger head changed concurrently",
                        ))
                    }
                }
                Err(error) => Err(map_storage(error)),
            }
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
