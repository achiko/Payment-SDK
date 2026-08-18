use base::{ChildIndex, Decimal, DerivationPath};
use bincode::{Decode, Encode};
use indexing::{AssetId, CanonicalAddress, ChainId, TransactionRef};
use indexing::{
    BlockHash, BlockHeight, BlockRef, ConfirmationProof, EventCursor, EventId, IndexScope,
    MovementId, MovementKind, NetworkFee, ObservationEvent, ObservationRevision,
    ObservedTransaction, TransactionStatus, ValueMovement, WatchId,
};
use sha2::{Digest, Sha256};
use storage::{
    Condition, Error, ErrorKind, Key, Namespace, Operation, ScanRequest, Store, StoredValue, Value,
    WriteBatch,
};

use crate::amount;
use crate::{
    AccountingCommand, ActionGuard, AppendObservation, AppendOutcome, ApplyResult, AwaitingPage,
    AwaitingQuery, BatchMutation, BatchOutcome, BoxFuture, CaseId, CaseOpener, CaseQuery,
    CaseReader, CaseResolver, CloseDeposit, CommandIdentity, CommandOperation, CommandPrincipal,
    ConsumerCheckpoint, ConsumerCheckpointName, CreatedDeposit, Deposit, DepositBalances,
    DepositCreator, DepositError, DepositErrorKind, DepositEvents, DepositFilter, DepositId,
    DepositLifecycle, DepositPage, DepositPlan, DepositQuery, DepositReader, DepositState,
    DepositStateKind, EntryId, EventProjector, EventReader, EventWriter, IdempotencyKey,
    IndexRebuild, IndexRebuilder, KeyId, LedgerEffect, LedgerEntry, LedgerEntryCause,
    LedgerObservationKind, LedgerPage, LedgerQuery, LedgerReader, LedgerTransition, LedgerWriter,
    LogPage, LogQuery, MirrorObservation, MirrorOutcome, MirroredObservation,
    ObservationLedgerEffect, OpenDeposit, OpenLedger, ProgressReader, ProjectBatch,
    ProjectObservation, ProjectionFeeTreatment, ProjectionId, ProjectionOutcome, RebuildRequest,
    ReconciliationCase, ReconciliationDecision, ReconciliationPage, ReconciliationReason,
    ReconciliationResolution, ReconciliationState, RecordObservation, RequestHash,
    ResolveReconciliation, UserId, UtxoBatchProjectionTransition, WatchQueue,
    apply_observation_transition,
};

mod accounting;
mod batch;
mod collection;
mod command_record;
mod deposit;
mod deposit_index;
mod deposit_record;
mod event;
mod event_read;
mod ingestion;
mod ledger;
mod ledger_record;
mod lifecycle;
mod observation_record;
mod projection;
mod projection_replay;
mod reconciliation;
mod reconciliation_record;
mod reconciliation_support;
mod repository;
mod watch;

use command_record::*;
use deposit_record::*;
use ledger_record::*;
use observation_record::*;
use reconciliation_record::*;

const RECORD_VERSION: u16 = 1;
const DEPOSIT_RECORD_VERSION: u16 = 3;
const RECONCILIATION_RECORD_VERSION: u16 = 3;
const OBSERVATION_RECORD_VERSION: u16 = 2;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 1_000;
const MAX_KEY_PURPOSE_BYTES: usize = 1_024;
const MAX_ACCOUNTING_REASON_BYTES: usize = 1_024;
const MAX_RECONCILIATION_REASON_BYTES: usize = 1_024;
const MAX_EXTERNAL_DEBT_REFERENCE_BYTES: usize = 4_096;

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
    component_key(&[
        address.scope.chain.0.as_bytes(),
        address.scope.network.as_bytes(),
        address.value.as_bytes(),
    ])
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

fn ledger_entry_key(deposit_id: &DepositId, entry_id: &EntryId) -> Result<Key, DepositError> {
    component_key(&[deposit_id.0.as_bytes(), entry_id.0.as_bytes()])
}

fn ledger_prefix(deposit_id: &DepositId) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[deposit_id.0.as_bytes()])?.0)
}

fn reconciliation_deposit_key(
    deposit_id: &DepositId,
    case_id: &CaseId,
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
    .map_err(|error| storage_error(format!("failed to encode PS record: {error}")))
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
    .map_err(|error| storage_error(format!("failed to decode PS record: {error}")))?;
    if consumed != stored.value.0.len() {
        return Err(storage_error("PS record contains trailing bytes"));
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
        DEPOSIT_RECORD_VERSION => decode::<DepositRecord>(stored)?.try_into(),
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
        RECONCILIATION_RECORD_VERSION => decode::<ReconciliationRecord>(stored)?.try_into(),
        _ => Err(storage_error(format!(
            "unsupported PS reconciliation record version {version}"
        ))),
    }
}

/// Real PS semantic repository over the backend-independent atomic storage API.
#[derive(Clone, Debug)]
pub struct PaymentStore<S> {
    storage: S,
}

impl<S> PaymentStore<S> {
    #[must_use]
    pub const fn new(storage: S) -> Self {
        Self { storage }
    }

    #[must_use]
    pub const fn storage(&self) -> &S {
        &self.storage
    }
}

fn deposit_from_create(command: &DepositPlan) -> Deposit {
    Deposit {
        id: command.id.clone(),
        idempotency_key: command.idempotency_key.clone(),
        user_id: command.user_id.clone(),
        asset: command.asset.clone(),
        address: command.address.clone(),
        key: command.key.clone(),
        key_purpose: command.key_purpose.clone(),
        expected: command.expected.clone(),
        birthday: command.birthday,
        expires_at: command.expires_at,
        state: DepositState::AwaitingWatch,
        created_at: command.created_at,
    }
}

fn open_entry(command: &OpenDeposit) -> LedgerEntry {
    LedgerEntry {
        id: EntryId(format!("open:{}", command.deposit.id.0)),
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

fn map_storage(error: Error) -> DepositError {
    let kind = match error.kind {
        ErrorKind::Conflict => DepositErrorKind::Conflict,
        ErrorKind::CorruptData => DepositErrorKind::InvariantViolation,
        ErrorKind::InvalidRequest => DepositErrorKind::InvariantViolation,
        ErrorKind::Unavailable | ErrorKind::Other => DepositErrorKind::Store,
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
        kind: DepositErrorKind::Store,
        message: message.into(),
    }
}
