use std::collections::BTreeSet;

use bincode::{Decode, Encode};
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use indexing::WatchId;
use storage::{
    Condition, Key, Namespace, Operation, ScanRequest, Storage, StorageError, StorageErrorKind,
    StoredValue, Value, WriteBatch,
};

use crate::{
    AcceptCollectionBroadcast, AttachCollectionWatch, BoxFuture, Collection, CollectionAllocation,
    CollectionId, CollectionLeg, CollectionLegId, CollectionLegKind, CollectionLegReference,
    CollectionLegState, CollectionMode, CollectionPage, CollectionPageRequest,
    CollectionReservation, CollectionReservationState, CollectionState, CollectionStore,
    CollectionTransitionGuard, ConfirmCollectionLeg, CreateCollection, CreateCollectionOutcome,
    DepositError, DepositErrorKind, DepositId, FailCollectionLeg, JobId, PolicyIdentity,
    RecordSignedCollectionLeg, ReleaseCollectionReservation, ReorgCollectionLeg,
    ReservationReleaseReason, RetryCollectionLeg, SafeCollectionError, SignedCollectionEnvelope,
    SignedEnvelopeBytes, UserId,
};

use crate::PersistentPaymentRepository;

const RECORD_VERSION: u16 = 1;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 1_000;
const MAX_SAFE_ERROR_CODE_BYTES: usize = 128;
const MAX_SAFE_ERROR_MESSAGE_BYTES: usize = 4_096;

fn ns(value: &str) -> Namespace {
    Namespace(value.to_owned())
}

fn collection_ns() -> Namespace {
    ns("ps.v1.collection")
}

fn collection_job_ns() -> Namespace {
    ns("ps.v1.collection_job")
}

fn deposit_collection_ns() -> Namespace {
    ns("ps.v1.deposit_collection")
}

pub(crate) fn active_reservation_ns() -> Namespace {
    ns("ps.v1.active_collection_reservation")
}

fn collection_eligibility_generation_ns() -> Namespace {
    ns("ps.v1.collection_eligibility_generation")
}

fn transaction_leg_ns() -> Namespace {
    ns("ps.v1.collection_transaction")
}

fn signed_envelope_ns() -> Namespace {
    ns("ps.v1.signed_collection_envelope")
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

fn deposit_collection_key(
    deposit_id: &DepositId,
    collection_id: &CollectionId,
) -> Result<Key, DepositError> {
    component_key(&[deposit_id.0.as_bytes(), collection_id.0.as_bytes()])
}

fn deposit_collection_prefix(deposit_id: &DepositId) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[deposit_id.0.as_bytes()])?.0)
}

pub(crate) fn reservation_key(
    deposit_id: &DepositId,
    asset: &AssetId,
) -> Result<Key, DepositError> {
    component_key(&[
        deposit_id.0.as_bytes(),
        asset.chain.0.as_bytes(),
        asset.asset.as_bytes(),
    ])
}

fn transaction_key(transaction_id: &CanonicalTransactionId) -> Result<Key, DepositError> {
    component_key(&[
        transaction_id.chain.0.as_bytes(),
        transaction_id.value.as_bytes(),
    ])
}

fn envelope_key(
    collection_id: &CollectionId,
    leg_id: &CollectionLegId,
) -> Result<Key, DepositError> {
    component_key(&[collection_id.0.as_bytes(), leg_id.0.as_bytes()])
}

fn encode<T: Encode>(record: &T) -> Result<Value, DepositError> {
    bincode::encode_to_vec(
        record,
        bincode::config::standard()
            .with_fixed_int_encoding()
            .with_big_endian(),
    )
    .map(Value)
    .map_err(|error| storage_error(format!("failed to encode PS collection RecordV1: {error}")))
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
    .map_err(|error| storage_error(format!("failed to decode PS collection RecordV1: {error}")))?;
    if consumed != stored.value.0.len() {
        return Err(storage_error(
            "PS collection RecordV1 contains trailing bytes",
        ));
    }
    Ok(record)
}

fn ensure_version(version: u16) -> Result<(), DepositError> {
    if version == RECORD_VERSION {
        Ok(())
    } else {
        Err(storage_error(format!(
            "unsupported PS collection record version {version}"
        )))
    }
}

fn map_storage(error: StorageError) -> DepositError {
    let kind = match error.kind {
        StorageErrorKind::Conflict => DepositErrorKind::Conflict,
        StorageErrorKind::CorruptData | StorageErrorKind::InvalidRequest => {
            DepositErrorKind::InvariantViolation
        }
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

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct AssetRecordV1 {
    chain: String,
    asset: String,
}

impl From<&AssetId> for AssetRecordV1 {
    fn from(value: &AssetId) -> Self {
        Self {
            chain: value.chain.0.clone(),
            asset: value.asset.clone(),
        }
    }
}

impl From<AssetRecordV1> for AssetId {
    fn from(value: AssetRecordV1) -> Self {
        Self {
            chain: ChainId(value.chain),
            asset: value.asset,
        }
    }
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

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct TransactionRecordV1 {
    chain: String,
    value: String,
}

impl From<&CanonicalTransactionId> for TransactionRecordV1 {
    fn from(value: &CanonicalTransactionId) -> Self {
        Self {
            chain: value.chain.0.clone(),
            value: value.value.clone(),
        }
    }
}

impl From<TransactionRecordV1> for CanonicalTransactionId {
    fn from(value: TransactionRecordV1) -> Self {
        Self {
            chain: ChainId(value.chain),
            value: value.value,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct PolicyRecordV1 {
    version: String,
    digest: [u8; 32],
}

impl From<&PolicyIdentity> for PolicyRecordV1 {
    fn from(value: &PolicyIdentity) -> Self {
        Self {
            version: value.version.clone(),
            digest: value.digest,
        }
    }
}

impl From<PolicyRecordV1> for PolicyIdentity {
    fn from(value: PolicyRecordV1) -> Self {
        Self {
            version: value.version,
            digest: value.digest,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct SafeErrorRecordV1 {
    code: String,
    message: String,
    retryable: bool,
}

impl From<&SafeCollectionError> for SafeErrorRecordV1 {
    fn from(value: &SafeCollectionError) -> Self {
        Self {
            code: value.code.clone(),
            message: value.message.clone(),
            retryable: value.retryable,
        }
    }
}

impl From<SafeErrorRecordV1> for SafeCollectionError {
    fn from(value: SafeErrorRecordV1) -> Self {
        Self {
            code: value.code,
            message: value.message,
            retryable: value.retryable,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum LegStateRecordV1 {
    Required,
    Signed { transaction_id: TransactionRecordV1 },
    Broadcast { transaction_id: TransactionRecordV1 },
    Confirmed { transaction_id: TransactionRecordV1 },
    Failed { transaction_id: TransactionRecordV1 },
    Reorged { transaction_id: TransactionRecordV1 },
}

impl From<&CollectionLegState> for LegStateRecordV1 {
    fn from(value: &CollectionLegState) -> Self {
        match value {
            CollectionLegState::Required => Self::Required,
            CollectionLegState::Signed { transaction_id } => Self::Signed {
                transaction_id: transaction_id.into(),
            },
            CollectionLegState::Broadcast { transaction_id } => Self::Broadcast {
                transaction_id: transaction_id.into(),
            },
            CollectionLegState::Confirmed { transaction_id } => Self::Confirmed {
                transaction_id: transaction_id.into(),
            },
            CollectionLegState::Failed { transaction_id } => Self::Failed {
                transaction_id: transaction_id.into(),
            },
            CollectionLegState::Reorged { transaction_id } => Self::Reorged {
                transaction_id: transaction_id.into(),
            },
        }
    }
}

impl From<LegStateRecordV1> for CollectionLegState {
    fn from(value: LegStateRecordV1) -> Self {
        match value {
            LegStateRecordV1::Required => Self::Required,
            LegStateRecordV1::Signed { transaction_id } => Self::Signed {
                transaction_id: transaction_id.into(),
            },
            LegStateRecordV1::Broadcast { transaction_id } => Self::Broadcast {
                transaction_id: transaction_id.into(),
            },
            LegStateRecordV1::Confirmed { transaction_id } => Self::Confirmed {
                transaction_id: transaction_id.into(),
            },
            LegStateRecordV1::Failed { transaction_id } => Self::Failed {
                transaction_id: transaction_id.into(),
            },
            LegStateRecordV1::Reorged { transaction_id } => Self::Reorged {
                transaction_id: transaction_id.into(),
            },
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum ReservationStateRecordV1 {
    Active,
    Consumed {
        transaction_id: TransactionRecordV1,
        consumed_at: u64,
    },
    Released {
        reason: u8,
        released_at: u64,
    },
}

impl From<&CollectionReservationState> for ReservationStateRecordV1 {
    fn from(value: &CollectionReservationState) -> Self {
        match value {
            CollectionReservationState::Active => Self::Active,
            CollectionReservationState::Consumed {
                transaction_id,
                consumed_at,
            } => Self::Consumed {
                transaction_id: transaction_id.into(),
                consumed_at: *consumed_at,
            },
            CollectionReservationState::Released {
                reason,
                released_at,
            } => Self::Released {
                reason: release_reason_tag(*reason),
                released_at: *released_at,
            },
        }
    }
}

impl TryFrom<ReservationStateRecordV1> for CollectionReservationState {
    type Error = DepositError;

    fn try_from(value: ReservationStateRecordV1) -> Result<Self, Self::Error> {
        Ok(match value {
            ReservationStateRecordV1::Active => Self::Active,
            ReservationStateRecordV1::Consumed {
                transaction_id,
                consumed_at,
            } => Self::Consumed {
                transaction_id: transaction_id.into(),
                consumed_at,
            },
            ReservationStateRecordV1::Released {
                reason,
                released_at,
            } => Self::Released {
                reason: release_reason_from_tag(reason)?,
                released_at,
            },
        })
    }
}

fn release_reason_tag(reason: ReservationReleaseReason) -> u8 {
    match reason {
        ReservationReleaseReason::TerminalFailure => 0,
        ReservationReleaseReason::Reorg => 1,
    }
}

fn release_reason_from_tag(tag: u8) -> Result<ReservationReleaseReason, DepositError> {
    match tag {
        0 => Ok(ReservationReleaseReason::TerminalFailure),
        1 => Ok(ReservationReleaseReason::Reorg),
        _ => Err(storage_error(
            "PS collection record has an unknown reservation release reason",
        )),
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct AllocationRecordV1 {
    deposit_id: String,
    asset: AssetRecordV1,
    gross_debit: [u8; 32],
    master_credit: [u8; 32],
    allocated_fee_asset: AssetRecordV1,
    allocated_fee: [u8; 32],
}

impl From<&CollectionAllocation> for AllocationRecordV1 {
    fn from(value: &CollectionAllocation) -> Self {
        Self {
            deposit_id: value.deposit_id.0.clone(),
            asset: (&value.asset).into(),
            gross_debit: value.gross_debit.0,
            master_credit: value.master_credit.0,
            allocated_fee_asset: (&value.allocated_fee_asset).into(),
            allocated_fee: value.allocated_fee.0,
        }
    }
}

impl From<AllocationRecordV1> for CollectionAllocation {
    fn from(value: AllocationRecordV1) -> Self {
        Self {
            deposit_id: DepositId(value.deposit_id),
            asset: value.asset.into(),
            gross_debit: AtomicAmount(value.gross_debit),
            master_credit: AtomicAmount(value.master_credit),
            allocated_fee_asset: value.allocated_fee_asset.into(),
            allocated_fee: AtomicAmount(value.allocated_fee),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct LegRecordV1 {
    id: String,
    position: u16,
    kind: u8,
    planned_amount: Option<[u8; 32]>,
    state: LegStateRecordV1,
    watch_id: Option<String>,
    attempt_count: u32,
    allocation: Option<AllocationRecordV1>,
    last_error: Option<SafeErrorRecordV1>,
    updated_at: u64,
}

impl From<&CollectionLeg> for LegRecordV1 {
    fn from(value: &CollectionLeg) -> Self {
        Self {
            id: value.id.0.clone(),
            position: value.position,
            kind: leg_kind_tag(value.kind),
            planned_amount: value.planned_amount.map(|amount| amount.0),
            state: (&value.state).into(),
            watch_id: value.watch_id.as_ref().map(|watch| watch.0.clone()),
            attempt_count: value.attempt_count,
            allocation: value.allocation.as_ref().map(Into::into),
            last_error: value.last_error.as_ref().map(Into::into),
            updated_at: value.updated_at,
        }
    }
}

impl TryFrom<LegRecordV1> for CollectionLeg {
    type Error = DepositError;

    fn try_from(value: LegRecordV1) -> Result<Self, Self::Error> {
        Ok(Self {
            id: CollectionLegId(value.id),
            position: value.position,
            kind: leg_kind_from_tag(value.kind)?,
            planned_amount: value.planned_amount.map(AtomicAmount),
            state: value.state.into(),
            watch_id: value.watch_id.map(WatchId),
            attempt_count: value.attempt_count,
            allocation: value.allocation.map(Into::into),
            last_error: value.last_error.map(Into::into),
            updated_at: value.updated_at,
        })
    }
}

fn leg_kind_tag(kind: CollectionLegKind) -> u8 {
    match kind {
        CollectionLegKind::GasFunding => 0,
        CollectionLegKind::Sweep => 1,
    }
}

fn leg_kind_from_tag(tag: u8) -> Result<CollectionLegKind, DepositError> {
    match tag {
        0 => Ok(CollectionLegKind::GasFunding),
        1 => Ok(CollectionLegKind::Sweep),
        _ => Err(storage_error(
            "PS collection record has an unknown leg kind",
        )),
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct CollectionRecordV1 {
    version: u16,
    id: String,
    job_id: String,
    user_id: String,
    deposit_id: String,
    mode: u8,
    asset: AssetRecordV1,
    destination: AddressRecordV1,
    policy: PolicyRecordV1,
    state: u8,
    reservation_amount: [u8; 32],
    reservation_state: ReservationStateRecordV1,
    legs: Vec<LegRecordV1>,
    attempt_count: u32,
    last_error: Option<SafeErrorRecordV1>,
    created_at: u64,
    updated_at: u64,
}

impl From<&Collection> for CollectionRecordV1 {
    fn from(value: &Collection) -> Self {
        Self {
            version: RECORD_VERSION,
            id: value.id.0.clone(),
            job_id: value.job_id.0.clone(),
            user_id: value.user_id.0.clone(),
            deposit_id: value.deposit_id.0.clone(),
            mode: collection_mode_tag(value.mode),
            asset: (&value.asset).into(),
            destination: (&value.destination).into(),
            policy: (&value.policy).into(),
            state: collection_state_tag(value.state),
            reservation_amount: value.reservation.amount.0,
            reservation_state: (&value.reservation.state).into(),
            legs: value.legs.iter().map(Into::into).collect(),
            attempt_count: value.attempt_count,
            last_error: value.last_error.as_ref().map(Into::into),
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

impl TryFrom<CollectionRecordV1> for Collection {
    type Error = DepositError;

    fn try_from(value: CollectionRecordV1) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        let asset: AssetId = value.asset.into();
        let deposit_id = DepositId(value.deposit_id);
        let collection = Self {
            id: CollectionId(value.id),
            job_id: JobId(value.job_id),
            user_id: UserId(value.user_id),
            deposit_id: deposit_id.clone(),
            mode: collection_mode_from_tag(value.mode)?,
            destination: value.destination.into(),
            policy: value.policy.into(),
            state: collection_state_from_tag(value.state)?,
            reservation: CollectionReservation {
                deposit_id,
                asset: asset.clone(),
                amount: AtomicAmount(value.reservation_amount),
                state: value.reservation_state.try_into()?,
            },
            asset,
            legs: value
                .legs
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            attempt_count: value.attempt_count,
            last_error: value.last_error.map(Into::into),
            created_at: value.created_at,
            updated_at: value.updated_at,
        };
        validate_persisted_collection(&collection)?;
        Ok(collection)
    }
}

fn collection_mode_tag(mode: CollectionMode) -> u8 {
    match mode {
        CollectionMode::AccountTransfer => 0,
        CollectionMode::UtxoBatch => 1,
        CollectionMode::TokenWithGas => 2,
    }
}

fn collection_mode_from_tag(tag: u8) -> Result<CollectionMode, DepositError> {
    match tag {
        0 => Ok(CollectionMode::AccountTransfer),
        1 => Ok(CollectionMode::UtxoBatch),
        2 => Ok(CollectionMode::TokenWithGas),
        _ => Err(storage_error(
            "PS collection record has an unknown collection mode",
        )),
    }
}

fn collection_state_tag(state: CollectionState) -> u8 {
    match state {
        CollectionState::Required => 0,
        CollectionState::InProgress => 1,
        CollectionState::Completed => 2,
        CollectionState::Failed => 3,
        CollectionState::Reorged => 4,
    }
}

fn collection_state_from_tag(tag: u8) -> Result<CollectionState, DepositError> {
    match tag {
        0 => Ok(CollectionState::Required),
        1 => Ok(CollectionState::InProgress),
        2 => Ok(CollectionState::Completed),
        3 => Ok(CollectionState::Failed),
        4 => Ok(CollectionState::Reorged),
        _ => Err(storage_error(
            "PS collection record has an unknown aggregate state",
        )),
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct CollectionIndexRecordV1 {
    version: u16,
    collection_id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct LegIndexRecordV1 {
    version: u16,
    collection_id: String,
    leg_id: String,
}

/// Intentionally does not derive `Debug`: its opaque bytes must not enter
/// diagnostic output even inside this persistence implementation.
#[derive(Clone, Decode, Encode, PartialEq, Eq)]
struct SignedEnvelopeRecordV1 {
    version: u16,
    collection_id: String,
    leg_id: String,
    expected_transaction_id: TransactionRecordV1,
    bytes: Vec<u8>,
    signed_at: u64,
    expires_at: u64,
}

impl From<&SignedCollectionEnvelope> for SignedEnvelopeRecordV1 {
    fn from(value: &SignedCollectionEnvelope) -> Self {
        Self {
            version: RECORD_VERSION,
            collection_id: value.collection_id.0.clone(),
            leg_id: value.leg_id.0.clone(),
            expected_transaction_id: (&value.expected_transaction_id).into(),
            bytes: value.bytes.as_bytes().to_vec(),
            signed_at: value.signed_at,
            expires_at: value.expires_at,
        }
    }
}

impl TryFrom<SignedEnvelopeRecordV1> for SignedCollectionEnvelope {
    type Error = DepositError;

    fn try_from(value: SignedEnvelopeRecordV1) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        Ok(Self {
            collection_id: CollectionId(value.collection_id),
            leg_id: CollectionLegId(value.leg_id),
            expected_transaction_id: value.expected_transaction_id.into(),
            bytes: SignedEnvelopeBytes::new(value.bytes)
                .map_err(|error| storage_error(error.message))?,
            signed_at: value.signed_at,
            expires_at: value.expires_at,
        })
    }
}

fn validate_non_empty(value: &str, field: &str) -> Result<(), DepositError> {
    if value.is_empty() {
        Err(invalid(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

fn validate_error(error: &SafeCollectionError) -> Result<(), DepositError> {
    validate_non_empty(&error.code, "safe collection error code")?;
    validate_non_empty(&error.message, "safe collection error message")?;
    if error.code.len() > MAX_SAFE_ERROR_CODE_BYTES {
        return Err(invalid("safe collection error code is too long"));
    }
    if error.message.len() > MAX_SAFE_ERROR_MESSAGE_BYTES {
        return Err(invalid("safe collection error message is too long"));
    }
    if !error
        .code
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(invalid(
            "safe collection error code must use lowercase ASCII, digits, or underscore",
        ));
    }
    Ok(())
}

fn validate_asset(asset: &AssetId, field: &str) -> Result<(), DepositError> {
    validate_non_empty(&asset.chain.0, &format!("{field} chain"))?;
    validate_non_empty(&asset.asset, field)
}

fn validate_transaction_for_collection(
    collection: &Collection,
    transaction_id: &CanonicalTransactionId,
) -> Result<(), DepositError> {
    validate_non_empty(&transaction_id.value, "transaction ID")?;
    if transaction_id.chain != collection.asset.chain {
        return Err(invalid(
            "collection transaction chain does not match asset chain",
        ));
    }
    Ok(())
}

fn validate_leg_shape(
    mode: CollectionMode,
    legs: &[(CollectionLegId, CollectionLegKind)],
) -> Result<(), DepositError> {
    if legs.is_empty() {
        return Err(invalid("collection must contain at least one leg"));
    }
    if legs.len() > usize::from(u16::MAX) + 1 {
        return Err(invalid("collection contains too many ordered legs"));
    }
    let mut ids = BTreeSet::new();
    for (id, _) in legs {
        validate_non_empty(&id.0, "collection leg ID")?;
        if !ids.insert(&id.0) {
            return Err(invalid("collection leg IDs must be unique"));
        }
    }
    let kinds = legs.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
    let valid = match mode {
        CollectionMode::AccountTransfer | CollectionMode::UtxoBatch => {
            kinds == [CollectionLegKind::Sweep]
        }
        CollectionMode::TokenWithGas => {
            kinds == [CollectionLegKind::Sweep]
                || kinds == [CollectionLegKind::GasFunding, CollectionLegKind::Sweep]
        }
    };
    if !valid {
        return Err(invalid(
            "collection legs must be one sweep or ordered gas-funding then sweep",
        ));
    }
    Ok(())
}

fn validate_create(command: &CreateCollection) -> Result<(), DepositError> {
    validate_non_empty(&command.id.0, "collection ID")?;
    validate_non_empty(&command.job_id.0, "collection job ID")?;
    validate_non_empty(&command.user_id.0, "collection user ID")?;
    validate_non_empty(&command.deposit_id.0, "collection deposit ID")?;
    validate_asset(&command.asset, "collection asset")?;
    validate_non_empty(&command.destination.value, "collection destination")?;
    if command.destination.chain != command.asset.chain {
        return Err(invalid(
            "collection destination chain does not match asset chain",
        ));
    }
    validate_non_empty(&command.policy.version, "collection policy version")?;
    if command.reservation_amount.is_zero() {
        return Err(invalid("collection reservation amount must be positive"));
    }
    validate_leg_shape(
        command.mode,
        &command
            .legs
            .iter()
            .map(|leg| (leg.id.clone(), leg.kind))
            .collect::<Vec<_>>(),
    )?;
    for leg in &command.legs {
        match (leg.kind, leg.planned_amount) {
            (CollectionLegKind::GasFunding, Some(amount)) if !amount.is_zero() => {}
            (CollectionLegKind::GasFunding, _) => {
                return Err(invalid(
                    "gas-funding collection leg requires a positive planned amount",
                ));
            }
            (CollectionLegKind::Sweep, None) => {}
            (CollectionLegKind::Sweep, Some(_)) => {
                return Err(invalid(
                    "sweep collection leg must use the aggregate reservation amount",
                ));
            }
        }
    }
    Ok(())
}

fn validate_allocation(
    collection: &Collection,
    allocation: &CollectionAllocation,
) -> Result<(), DepositError> {
    if allocation.deposit_id != collection.deposit_id {
        return Err(invalid(
            "collection allocation belongs to a different deposit",
        ));
    }
    if allocation.asset != collection.asset {
        return Err(invalid(
            "collection allocation asset does not match the reservation",
        ));
    }
    validate_asset(&allocation.allocated_fee_asset, "allocated fee asset")?;
    if allocation.allocated_fee_asset.chain != collection.asset.chain {
        return Err(invalid(
            "allocated fee chain does not match the collection chain",
        ));
    }
    if allocation.gross_debit.is_zero() {
        return Err(invalid("confirmed collection gross debit must be positive"));
    }
    collection
        .reservation
        .amount
        .checked_sub(&allocation.gross_debit)
        .map_err(|_| invalid("confirmed gross debit exceeds the durable reservation"))?;

    if allocation.allocated_fee_asset == collection.asset {
        let attributed = allocation
            .master_credit
            .checked_add(&allocation.allocated_fee)
            .map_err(|_| invalid("confirmed collection attribution overflows"))?;
        if attributed != allocation.gross_debit {
            return Err(invalid(
                "same-asset master credit plus allocated fee must equal gross debit",
            ));
        }
    } else if allocation.master_credit != allocation.gross_debit {
        return Err(invalid(
            "cross-asset fee attribution must preserve gross collection asset credit",
        ));
    }
    Ok(())
}

fn validate_persisted_collection(collection: &Collection) -> Result<(), DepositError> {
    validate_non_empty(&collection.id.0, "persisted collection ID")?;
    validate_non_empty(&collection.job_id.0, "persisted collection job ID")?;
    validate_non_empty(&collection.user_id.0, "persisted collection user ID")?;
    validate_non_empty(&collection.deposit_id.0, "persisted collection deposit ID")?;
    validate_asset(&collection.asset, "persisted collection asset")?;
    if collection.destination.chain != collection.asset.chain {
        return Err(storage_error(
            "persisted collection destination chain does not match asset chain",
        ));
    }
    if collection.reservation.deposit_id != collection.deposit_id
        || collection.reservation.asset != collection.asset
    {
        return Err(storage_error(
            "persisted collection reservation identity does not match aggregate",
        ));
    }
    if collection.reservation.amount.is_zero() {
        return Err(storage_error(
            "persisted collection reservation must be positive",
        ));
    }
    if collection.updated_at < collection.created_at {
        return Err(storage_error(
            "persisted collection update predates its creation",
        ));
    }
    validate_leg_shape(
        collection.mode,
        &collection
            .legs
            .iter()
            .map(|leg| (leg.id.clone(), leg.kind))
            .collect::<Vec<_>>(),
    )
    .map_err(|error| storage_error(error.message))?;
    for (position, leg) in collection.legs.iter().enumerate() {
        if usize::from(leg.position) != position {
            return Err(storage_error(
                "persisted collection leg positions are not contiguous",
            ));
        }
        if leg.updated_at < collection.created_at {
            return Err(storage_error(
                "persisted collection leg update predates aggregate creation",
            ));
        }
        if let Some(transaction_id) = leg.state.transaction_id() {
            validate_transaction_for_collection(collection, transaction_id)
                .map_err(|error| storage_error(error.message))?;
        }
        if let Some(error) = &leg.last_error {
            validate_error(error).map_err(|error| storage_error(error.message))?;
        }
        if let Some(allocation) = &leg.allocation {
            validate_allocation(collection, allocation)
                .map_err(|error| storage_error(error.message))?;
        }
        match leg.kind {
            CollectionLegKind::GasFunding
                if leg.planned_amount.is_none_or(|amount| amount.is_zero()) =>
            {
                return Err(storage_error(
                    "persisted gas-funding leg is missing its positive planned amount",
                ));
            }
            CollectionLegKind::Sweep if leg.planned_amount.is_some() => {
                return Err(storage_error(
                    "persisted sweep leg must not contain a planned gas-funding amount",
                ));
            }
            CollectionLegKind::GasFunding if leg.allocation.is_some() => {
                return Err(storage_error(
                    "persisted gas-funding leg must not contain sweep attribution",
                ));
            }
            CollectionLegKind::Sweep
                if matches!(leg.state, CollectionLegState::Confirmed { .. })
                    && leg.allocation.is_none() =>
            {
                return Err(storage_error(
                    "persisted confirmed sweep is missing attribution",
                ));
            }
            _ => {}
        }
        if leg.watch_id.is_some()
            && matches!(
                leg.state,
                CollectionLegState::Required | CollectionLegState::Signed { .. }
            )
        {
            return Err(storage_error(
                "persisted pre-broadcast collection leg contains an IX watch",
            ));
        }
        if leg.allocation.is_some()
            && !matches!(
                leg.state,
                CollectionLegState::Confirmed { .. } | CollectionLegState::Reorged { .. }
            )
        {
            return Err(storage_error(
                "persisted collection attribution is attached to a non-confirmed leg",
            ));
        }
        if leg.last_error.is_some()
            != matches!(
                leg.state,
                CollectionLegState::Failed { .. } | CollectionLegState::Reorged { .. }
            )
        {
            return Err(storage_error(
                "persisted collection leg safe error does not match its terminal state",
            ));
        }
    }
    if let Some(error) = &collection.last_error {
        validate_error(error).map_err(|error| storage_error(error.message))?;
    }
    let summed_attempts = collection
        .legs
        .iter()
        .try_fold(0_u32, |sum, leg| sum.checked_add(leg.attempt_count))
        .ok_or_else(|| storage_error("persisted collection attempt total overflows"))?;
    if summed_attempts != collection.attempt_count {
        return Err(storage_error(
            "persisted collection attempt total does not match its legs",
        ));
    }
    match collection.state {
        CollectionState::Required => {
            if !matches!(
                collection.reservation.state,
                CollectionReservationState::Active
            ) || collection.last_error.is_some()
            {
                return Err(storage_error(
                    "required collection must have an active reservation and no error",
                ));
            }
        }
        CollectionState::InProgress => {
            if !matches!(
                collection.reservation.state,
                CollectionReservationState::Active
            ) || collection.last_error.is_some()
            {
                return Err(storage_error(
                    "in-progress collection must have an active reservation and no error",
                ));
            }
        }
        CollectionState::Completed => {
            if !all_legs_confirmed(collection)
                || !matches!(
                    collection.reservation.state,
                    CollectionReservationState::Consumed { .. }
                )
                || collection.last_error.is_some()
            {
                return Err(storage_error(
                    "completed collection must have confirmed legs and a consumed reservation",
                ));
            }
        }
        CollectionState::Failed => {
            if !collection
                .legs
                .iter()
                .any(|leg| matches!(leg.state, CollectionLegState::Failed { .. }))
                || collection.last_error.is_none()
                || matches!(
                    collection.reservation.state,
                    CollectionReservationState::Consumed { .. }
                )
            {
                return Err(storage_error(
                    "failed collection has inconsistent leg, error, or reservation state",
                ));
            }
        }
        CollectionState::Reorged => {
            if !collection
                .legs
                .iter()
                .any(|leg| matches!(leg.state, CollectionLegState::Reorged { .. }))
                || collection.last_error.is_none()
                || matches!(
                    collection.reservation.state,
                    CollectionReservationState::Consumed { .. }
                )
            {
                return Err(storage_error(
                    "reorged collection has inconsistent leg, error, or reservation state",
                ));
            }
        }
    }
    Ok(())
}

fn collection_from_create(command: &CreateCollection) -> Result<Collection, DepositError> {
    validate_create(command)?;
    let mut legs = Vec::with_capacity(command.legs.len());
    for (position, leg) in command.legs.iter().enumerate() {
        let position = u16::try_from(position)
            .map_err(|_| invalid("collection contains too many ordered legs"))?;
        legs.push(CollectionLeg {
            id: leg.id.clone(),
            position,
            kind: leg.kind,
            planned_amount: leg.planned_amount,
            state: CollectionLegState::Required,
            watch_id: None,
            attempt_count: 0,
            allocation: None,
            last_error: None,
            updated_at: command.created_at,
        });
    }
    Ok(Collection {
        id: command.id.clone(),
        job_id: command.job_id.clone(),
        user_id: command.user_id.clone(),
        deposit_id: command.deposit_id.clone(),
        mode: command.mode,
        asset: command.asset.clone(),
        destination: command.destination.clone(),
        policy: command.policy.clone(),
        state: CollectionState::Required,
        reservation: CollectionReservation {
            deposit_id: command.deposit_id.clone(),
            asset: command.asset.clone(),
            amount: command.reservation_amount,
            state: CollectionReservationState::Active,
        },
        legs,
        attempt_count: 0,
        last_error: None,
        created_at: command.created_at,
        updated_at: command.created_at,
    })
}

fn validate_page(request: &CollectionPageRequest) -> Result<(), DepositError> {
    if request.limit == 0 || request.limit > MAX_PAGE_SIZE {
        Err(invalid("collection page size must be between 1 and 1000"))
    } else {
        Ok(())
    }
}

fn validate_guard(
    collection: &Collection,
    leg: &CollectionLeg,
    guard: &CollectionTransitionGuard,
) -> Result<(), DepositError> {
    if collection.state != guard.collection_state {
        return Err(conflict("stale expected collection aggregate state"));
    }
    if leg.state != guard.leg_state {
        return Err(conflict("stale expected collection leg state"));
    }
    Ok(())
}

fn validate_transition_time(collection: &Collection, updated_at: u64) -> Result<(), DepositError> {
    if updated_at < collection.updated_at {
        Err(invalid("collection transition timestamp moved backwards"))
    } else {
        Ok(())
    }
}

fn find_leg(collection: &Collection, leg_id: &CollectionLegId) -> Result<usize, DepositError> {
    collection
        .legs
        .iter()
        .position(|leg| &leg.id == leg_id)
        .ok_or_else(|| not_found("collection leg was not found"))
}

fn ensure_previous_legs_confirmed(
    collection: &Collection,
    position: usize,
) -> Result<(), DepositError> {
    if collection.legs[..position]
        .iter()
        .all(|leg| matches!(leg.state, CollectionLegState::Confirmed { .. }))
    {
        Ok(())
    } else {
        Err(invalid_state(
            "collection leg cannot advance before all previous legs are confirmed",
        ))
    }
}

fn all_legs_confirmed(collection: &Collection) -> bool {
    collection
        .legs
        .iter()
        .all(|leg| matches!(leg.state, CollectionLegState::Confirmed { .. }))
}

impl<S> PersistentPaymentRepository<S>
where
    S: Storage,
{
    pub(crate) async fn collection_eligibility_generation_change(
        &self,
        deposit_id: &DepositId,
        asset: &AssetId,
    ) -> Result<(Condition, Operation), DepositError> {
        let key = reservation_key(deposit_id, asset)?;
        let stored = self
            .storage()
            .get(&collection_eligibility_generation_ns(), &key)
            .await
            .map_err(map_storage)?;
        if let Some(stored) = &stored {
            let record: CollectionIndexRecordV1 = decode(stored)?;
            ensure_version(record.version)?;
            if record.collection_id != deposit_id.0 {
                return Err(storage_error(
                    "collection eligibility generation belongs to another deposit",
                ));
            }
        }
        let condition = stored.map_or_else(
            || Condition::Missing {
                namespace: collection_eligibility_generation_ns(),
                key: key.clone(),
            },
            |stored| Condition::Version {
                namespace: collection_eligibility_generation_ns(),
                key: key.clone(),
                expected: stored.version,
            },
        );
        let operation = Operation::Put {
            namespace: collection_eligibility_generation_ns(),
            key,
            value: encode(&CollectionIndexRecordV1 {
                version: RECORD_VERSION,
                collection_id: deposit_id.0.clone(),
            })?,
        };
        Ok((condition, operation))
    }

    async fn stored_collection_record(
        &self,
        id: &CollectionId,
    ) -> Result<Option<(Collection, StoredValue)>, DepositError> {
        self.storage()
            .get(&collection_ns(), &key_text(&id.0))
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: CollectionRecordV1 = decode(&stored)?;
                Ok((record.try_into()?, stored))
            })
            .transpose()
    }

    async fn required_collection_record(
        &self,
        id: &CollectionId,
    ) -> Result<(Collection, StoredValue), DepositError> {
        self.stored_collection_record(id)
            .await?
            .ok_or_else(|| not_found("collection was not found"))
    }

    async fn collection_index(
        &self,
        namespace: Namespace,
        key: &Key,
    ) -> Result<Option<(CollectionId, StoredValue)>, DepositError> {
        self.storage()
            .get(&namespace, key)
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: CollectionIndexRecordV1 = decode(&stored)?;
                ensure_version(record.version)?;
                Ok((CollectionId(record.collection_id), stored))
            })
            .transpose()
    }

    async fn stored_leg_reference(
        &self,
        transaction_id: &CanonicalTransactionId,
    ) -> Result<Option<(CollectionLegReference, StoredValue)>, DepositError> {
        self.storage()
            .get(&transaction_leg_ns(), &transaction_key(transaction_id)?)
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: LegIndexRecordV1 = decode(&stored)?;
                ensure_version(record.version)?;
                Ok((
                    CollectionLegReference {
                        collection_id: CollectionId(record.collection_id),
                        leg_id: CollectionLegId(record.leg_id),
                    },
                    stored,
                ))
            })
            .transpose()
    }

    async fn stored_signed_envelope(
        &self,
        collection_id: &CollectionId,
        leg_id: &CollectionLegId,
    ) -> Result<Option<(SignedCollectionEnvelope, StoredValue)>, DepositError> {
        self.storage()
            .get(&signed_envelope_ns(), &envelope_key(collection_id, leg_id)?)
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: SignedEnvelopeRecordV1 = decode(&stored)?;
                Ok((record.try_into()?, stored))
            })
            .transpose()
    }

    async fn validate_create_replay(
        &self,
        expected: &Collection,
    ) -> Result<Option<Collection>, DepositError> {
        let by_id = self.stored_collection_record(&expected.id).await?;
        let job_key = key_text(&expected.job_id.0);
        let by_job = self.collection_index(collection_job_ns(), &job_key).await?;
        if let Some((collection, _)) = &by_id {
            if collection != expected {
                return Err(conflict(
                    "collection ID was reused with a different aggregate",
                ));
            }
        }
        if let Some((indexed_id, _)) = &by_job {
            if indexed_id != &expected.id {
                return Err(conflict(
                    "collection job is already associated with another collection",
                ));
            }
            let indexed = self
                .stored_collection_record(indexed_id)
                .await?
                .map(|(collection, _)| collection)
                .ok_or_else(|| storage_error("collection job index is dangling"))?;
            if &indexed != expected {
                return Err(conflict(
                    "collection job was replayed with a different aggregate",
                ));
            }
        }
        let Some((collection, _)) = by_id else {
            if by_job.is_some() {
                return Err(storage_error("collection job index is dangling"));
            }
            if let Some((owner, _)) = self
                .collection_index(
                    active_reservation_ns(),
                    &reservation_key(&expected.deposit_id, &expected.asset)?,
                )
                .await?
            {
                return Err(conflict(format!(
                    "deposit and asset are already reserved by collection {}",
                    owner.0
                )));
            }
            return Ok(None);
        };

        let deposit_index = self
            .collection_index(
                deposit_collection_ns(),
                &deposit_collection_key(&expected.deposit_id, &expected.id)?,
            )
            .await?
            .ok_or_else(|| storage_error("collection deposit index is missing"))?;
        if deposit_index.0 != expected.id {
            return Err(storage_error(
                "collection deposit index points to another aggregate",
            ));
        }
        let job_index = by_job.ok_or_else(|| storage_error("collection job index is missing"))?;
        if job_index.0 != expected.id {
            return Err(storage_error(
                "collection job index points to another aggregate",
            ));
        }
        let reservation_index = self
            .collection_index(
                active_reservation_ns(),
                &reservation_key(&expected.deposit_id, &expected.asset)?,
            )
            .await?
            .ok_or_else(|| storage_error("active collection reservation index is missing"))?;
        if reservation_index.0 != expected.id {
            return Err(conflict(
                "deposit and asset are reserved by another collection",
            ));
        }
        Ok(Some(collection))
    }

    async fn active_reservation_record(
        &self,
        collection: &Collection,
    ) -> Result<Option<(CollectionId, StoredValue)>, DepositError> {
        self.collection_index(
            active_reservation_ns(),
            &reservation_key(&collection.deposit_id, &collection.asset)?,
        )
        .await
    }

    pub(crate) async fn validate_migration_collection_indexes(
        &self,
        collection: &Collection,
    ) -> Result<(), DepositError> {
        let (job_owner, _) = self
            .collection_index(collection_job_ns(), &key_text(&collection.job_id.0))
            .await?
            .ok_or_else(|| storage_error("collection job index is missing"))?;
        if job_owner != collection.id {
            return Err(storage_error(
                "collection job index points to another aggregate",
            ));
        }
        if collection.reservation.state == CollectionReservationState::Active {
            let (reservation_owner, _) = self
                .active_reservation_record(collection)
                .await?
                .ok_or_else(|| storage_error("active collection reservation index is missing"))?;
            if reservation_owner != collection.id {
                return Err(storage_error(
                    "active collection reservation index points to another aggregate",
                ));
            }
        }
        Ok(())
    }

    async fn require_owned_active_reservation(
        &self,
        collection: &Collection,
    ) -> Result<StoredValue, DepositError> {
        let (owner, stored) = self
            .active_reservation_record(collection)
            .await?
            .ok_or_else(|| storage_error("active collection reservation index is missing"))?;
        if owner != collection.id {
            return Err(conflict(
                "deposit and asset are reserved by another collection",
            ));
        }
        Ok(stored)
    }

    async fn commit_collection_update(
        &self,
        current_stored: &StoredValue,
        next: &Collection,
        mut conditions: Vec<Condition>,
        mut operations: Vec<Operation>,
    ) -> Result<(), DepositError> {
        conditions.insert(
            0,
            Condition::Version {
                namespace: collection_ns(),
                key: key_text(&next.id.0),
                expected: current_stored.version,
            },
        );
        operations.push(Operation::Put {
            namespace: collection_ns(),
            key: key_text(&next.id.0),
            value: encode(&CollectionRecordV1::from(next))?,
        });
        self.storage()
            .commit(WriteBatch {
                conditions,
                operations,
            })
            .await
            .map_err(map_storage)?;
        Ok(())
    }
}

impl<S> CollectionStore for PersistentPaymentRepository<S>
where
    S: Storage,
{
    fn create_or_replay_collection<'a>(
        &'a self,
        command: CreateCollection,
    ) -> BoxFuture<'a, Result<CreateCollectionOutcome, DepositError>> {
        Box::pin(async move {
            let expected = collection_from_create(&command)?;
            if let Some(collection) = self.validate_create_replay(&expected).await? {
                return Ok(CreateCollectionOutcome::Replayed { collection });
            }

            let collection_key = key_text(&expected.id.0);
            let job_key = key_text(&expected.job_id.0);
            let deposit_key = deposit_collection_key(&expected.deposit_id, &expected.id)?;
            let reservation_key = reservation_key(&expected.deposit_id, &expected.asset)?;
            let (eligibility_condition, eligibility_operation) = self
                .collection_eligibility_generation_change(&expected.deposit_id, &expected.asset)
                .await?;
            let index = CollectionIndexRecordV1 {
                version: RECORD_VERSION,
                collection_id: expected.id.0.clone(),
            };
            let result = self
                .storage()
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: collection_ns(),
                            key: collection_key.clone(),
                        },
                        Condition::Missing {
                            namespace: collection_job_ns(),
                            key: job_key.clone(),
                        },
                        Condition::Missing {
                            namespace: deposit_collection_ns(),
                            key: deposit_key.clone(),
                        },
                        Condition::Missing {
                            namespace: active_reservation_ns(),
                            key: reservation_key.clone(),
                        },
                        eligibility_condition,
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: collection_ns(),
                            key: collection_key,
                            value: encode(&CollectionRecordV1::from(&expected))?,
                        },
                        Operation::Put {
                            namespace: collection_job_ns(),
                            key: job_key,
                            value: encode(&index)?,
                        },
                        Operation::Put {
                            namespace: deposit_collection_ns(),
                            key: deposit_key,
                            value: encode(&index)?,
                        },
                        Operation::Put {
                            namespace: active_reservation_ns(),
                            key: reservation_key,
                            value: encode(&index)?,
                        },
                        eligibility_operation,
                    ],
                })
                .await
                .map_err(map_storage);
            match result {
                Ok(_) => Ok(CreateCollectionOutcome::Created {
                    collection: expected,
                }),
                Err(error) if error.kind == DepositErrorKind::Conflict => self
                    .validate_create_replay(&expected)
                    .await?
                    .map(|collection| CreateCollectionOutcome::Replayed { collection })
                    .ok_or(error),
                Err(error) => Err(error),
            }
        })
    }

    fn collection<'a>(
        &'a self,
        id: &'a CollectionId,
    ) -> BoxFuture<'a, Result<Option<Collection>, DepositError>> {
        Box::pin(async move {
            Ok(self
                .stored_collection_record(id)
                .await?
                .map(|(collection, _)| collection))
        })
    }

    fn collections_for_deposit<'a>(
        &'a self,
        deposit_id: &'a DepositId,
        request: CollectionPageRequest,
    ) -> BoxFuture<'a, Result<CollectionPage, DepositError>> {
        Box::pin(async move {
            validate_page(&request)?;
            let page = self
                .storage()
                .scan(ScanRequest {
                    namespace: deposit_collection_ns(),
                    prefix: deposit_collection_prefix(deposit_id)?,
                    after: request
                        .after
                        .as_ref()
                        .map(|id| deposit_collection_key(deposit_id, id))
                        .transpose()?,
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let has_next = page.next.is_some();
            let mut collections = Vec::with_capacity(page.entries.len());
            for (_, stored) in page.entries {
                let index: CollectionIndexRecordV1 = decode(&stored)?;
                ensure_version(index.version)?;
                collections.push(
                    self.stored_collection_record(&CollectionId(index.collection_id))
                        .await?
                        .map(|(collection, _)| collection)
                        .ok_or_else(|| storage_error("collection deposit index is dangling"))?,
                );
            }
            let next = has_next
                .then(|| collections.last().map(|collection| collection.id.clone()))
                .flatten();
            Ok(CollectionPage { collections, next })
        })
    }

    fn leg_for_transaction<'a>(
        &'a self,
        transaction_id: &'a CanonicalTransactionId,
    ) -> BoxFuture<'a, Result<Option<CollectionLegReference>, DepositError>> {
        Box::pin(async move {
            Ok(self
                .stored_leg_reference(transaction_id)
                .await?
                .map(|(reference, _)| reference))
        })
    }

    fn signed_envelope<'a>(
        &'a self,
        collection_id: &'a CollectionId,
        leg_id: &'a CollectionLegId,
    ) -> BoxFuture<'a, Result<Option<SignedCollectionEnvelope>, DepositError>> {
        Box::pin(async move {
            Ok(self
                .stored_signed_envelope(collection_id, leg_id)
                .await?
                .map(|(envelope, _)| envelope))
        })
    }

    fn record_signed<'a>(
        &'a self,
        command: RecordSignedCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let expected_reference = CollectionLegReference {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let desired_envelope = SignedCollectionEnvelope {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
                expected_transaction_id: command.expected_transaction_id.clone(),
                bytes: command.envelope,
                signed_at: command.signed_at,
                expires_at: command.expires_at,
            };
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.legs[position].state
                == (CollectionLegState::Signed {
                    transaction_id: command.expected_transaction_id.clone(),
                })
            {
                let existing = self
                    .stored_signed_envelope(&command.collection_id, &command.leg_id)
                    .await?
                    .map(|(envelope, _)| envelope)
                    .ok_or_else(|| storage_error("signed leg is missing its durable envelope"))?;
                let reference = self
                    .stored_leg_reference(&command.expected_transaction_id)
                    .await?
                    .map(|(reference, _)| reference)
                    .ok_or_else(|| storage_error("signed leg is missing transaction index"))?;
                if existing == desired_envelope && reference == expected_reference {
                    return Ok(collection);
                }
                return Err(conflict(
                    "signed collection leg was replayed with different durable attribution",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if !matches!(
                collection.state,
                CollectionState::Required | CollectionState::InProgress
            ) {
                return Err(invalid_state(
                    "terminal collection cannot persist a new signed leg",
                ));
            }
            if collection.legs[position].state != CollectionLegState::Required {
                return Err(invalid_state(
                    "only a required collection leg can persist a signed envelope",
                ));
            }
            ensure_previous_legs_confirmed(&collection, position)?;
            validate_transition_time(&collection, command.signed_at)?;
            if command.expires_at <= command.signed_at {
                return Err(invalid(
                    "signed collection envelope expiry must follow signing time",
                ));
            }
            validate_transaction_for_collection(&collection, &command.expected_transaction_id)?;
            if let Some((reference, _)) = self
                .stored_leg_reference(&command.expected_transaction_id)
                .await?
            {
                if reference == expected_reference {
                    return Err(storage_error(
                        "transaction index exists before its collection leg is signed",
                    ));
                }
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            if collection.reservation.state != CollectionReservationState::Active {
                return Err(invalid_state(
                    "collection must hold an active reservation before signing",
                ));
            }
            let reservation = self.require_owned_active_reservation(&collection).await?;
            let next_attempt = collection
                .attempt_count
                .checked_add(1)
                .ok_or_else(|| invalid("collection attempt counter is exhausted"))?;
            let next_leg_attempt = collection.legs[position]
                .attempt_count
                .checked_add(1)
                .ok_or_else(|| invalid("collection leg attempt counter is exhausted"))?;
            collection.state = CollectionState::InProgress;
            collection.attempt_count = next_attempt;
            collection.last_error = None;
            collection.updated_at = command.signed_at;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Signed {
                transaction_id: command.expected_transaction_id.clone(),
            };
            leg.watch_id = None;
            leg.attempt_count = next_leg_attempt;
            leg.allocation = None;
            leg.last_error = None;
            leg.updated_at = command.signed_at;
            let envelope_key = envelope_key(&command.collection_id, &command.leg_id)?;
            let transaction_key = transaction_key(&command.expected_transaction_id)?;
            let result = self
                .commit_collection_update(
                    &stored,
                    &collection,
                    vec![
                        Condition::Version {
                            namespace: active_reservation_ns(),
                            key: reservation_key(&collection.deposit_id, &collection.asset)?,
                            expected: reservation.version,
                        },
                        Condition::Missing {
                            namespace: signed_envelope_ns(),
                            key: envelope_key.clone(),
                        },
                        Condition::Missing {
                            namespace: transaction_leg_ns(),
                            key: transaction_key.clone(),
                        },
                    ],
                    vec![
                        Operation::Put {
                            namespace: signed_envelope_ns(),
                            key: envelope_key,
                            value: encode(&SignedEnvelopeRecordV1::from(&desired_envelope))?,
                        },
                        // Persist transaction attribution before any broadcast
                        // attempt. If the node accepts the transaction but its
                        // response is lost, IX facts can still be classified as
                        // this collection leg while PS recovers the receipt.
                        Operation::Put {
                            namespace: transaction_leg_ns(),
                            key: transaction_key,
                            value: encode(&LegIndexRecordV1 {
                                version: RECORD_VERSION,
                                collection_id: command.collection_id.0.clone(),
                                leg_id: command.leg_id.0.clone(),
                            })?,
                        },
                    ],
                )
                .await;
            match result {
                Ok(()) => Ok(collection),
                Err(error) if error.kind == DepositErrorKind::Conflict => {
                    let current = self
                        .required_collection_record(&command.collection_id)
                        .await?
                        .0;
                    let current_position = find_leg(&current, &command.leg_id)?;
                    let envelope = self
                        .stored_signed_envelope(&command.collection_id, &command.leg_id)
                        .await?
                        .map(|(envelope, _)| envelope);
                    let reference = self
                        .stored_leg_reference(&desired_envelope.expected_transaction_id)
                        .await?
                        .map(|(reference, _)| reference);
                    if current.legs[current_position].state
                        == (CollectionLegState::Signed {
                            transaction_id: desired_envelope.expected_transaction_id.clone(),
                        })
                        && envelope.as_ref() == Some(&desired_envelope)
                        && reference.as_ref() == Some(&expected_reference)
                    {
                        Ok(current)
                    } else {
                        Err(error)
                    }
                }
                Err(error) => Err(error),
            }
        })
    }

    fn accept_broadcast<'a>(
        &'a self,
        command: AcceptCollectionBroadcast,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            let expected_reference = CollectionLegReference {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            if collection.legs[position].state.transaction_id() == Some(&command.transaction_id)
                && !matches!(
                    collection.legs[position].state,
                    CollectionLegState::Required | CollectionLegState::Signed { .. }
                )
            {
                let reference = self
                    .stored_leg_reference(&command.transaction_id)
                    .await?
                    .map(|(reference, _)| reference)
                    .ok_or_else(|| storage_error("broadcast leg is missing transaction index"))?;
                if reference != expected_reference {
                    return Err(conflict(
                        "transaction ID belongs to a different collection leg",
                    ));
                }
                if self
                    .stored_signed_envelope(&command.collection_id, &command.leg_id)
                    .await?
                    .is_some()
                {
                    return Err(storage_error(
                        "accepted broadcast retained its signed envelope",
                    ));
                }
                return Ok(collection);
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if collection.state != CollectionState::InProgress {
                return Err(invalid_state(
                    "only an in-progress collection can accept broadcast",
                ));
            }
            match &collection.legs[position].state {
                CollectionLegState::Signed { transaction_id }
                    if transaction_id == &command.transaction_id => {}
                CollectionLegState::Signed { .. } => {
                    return Err(conflict(
                        "broadcast transaction ID does not match signed envelope hash",
                    ));
                }
                _ => {
                    return Err(invalid_state(
                        "only a signed collection leg can accept broadcast",
                    ));
                }
            }
            validate_transition_time(&collection, command.accepted_at)?;
            validate_transaction_for_collection(&collection, &command.transaction_id)?;
            let (envelope, envelope_stored) = self
                .stored_signed_envelope(&command.collection_id, &command.leg_id)
                .await?
                .ok_or_else(|| storage_error("signed leg is missing its durable envelope"))?;
            if envelope.expected_transaction_id != command.transaction_id {
                return Err(conflict(
                    "broadcast transaction ID does not match durable signed envelope hash",
                ));
            }
            // `expires_at` is a retention/alerting hint. Once PS has durably
            // recorded exact signed bytes it must be able to recover the
            // broadcast-response-loss window without silently re-signing a
            // different transaction.
            let (reference, reference_stored) = self
                .stored_leg_reference(&command.transaction_id)
                .await?
                .ok_or_else(|| storage_error("signed leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            let reservation = self.require_owned_active_reservation(&collection).await?;
            collection.state = CollectionState::InProgress;
            collection.updated_at = command.accepted_at;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Broadcast {
                transaction_id: command.transaction_id.clone(),
            };
            leg.updated_at = command.accepted_at;
            let envelope_key = envelope_key(&command.collection_id, &command.leg_id)?;
            self.commit_collection_update(
                &stored,
                &collection,
                vec![
                    Condition::Version {
                        namespace: active_reservation_ns(),
                        key: reservation_key(&collection.deposit_id, &collection.asset)?,
                        expected: reservation.version,
                    },
                    Condition::Version {
                        namespace: signed_envelope_ns(),
                        key: envelope_key.clone(),
                        expected: envelope_stored.version,
                    },
                    Condition::Version {
                        namespace: transaction_leg_ns(),
                        key: transaction_key(&command.transaction_id)?,
                        expected: reference_stored.version,
                    },
                ],
                vec![
                    // The opaque bytes are deleted only in the same atomic
                    // batch that accepts their exact expected transaction ID.
                    Operation::Delete {
                        namespace: signed_envelope_ns(),
                        key: envelope_key,
                    },
                ],
            )
            .await?;
            Ok(collection)
        })
    }

    fn attach_watch<'a>(
        &'a self,
        command: AttachCollectionWatch,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.legs[position].watch_id.as_ref() == Some(&command.watch_id) {
                if collection.legs[position].state.transaction_id()
                    == command.expected.leg_state.transaction_id()
                {
                    return Ok(collection);
                }
                return Err(conflict(
                    "IX watch is attached to a different transaction revision",
                ));
            }
            if collection.legs[position].watch_id.is_some() {
                return Err(conflict(
                    "collection leg is already attached to another IX watch",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if collection.state != CollectionState::InProgress {
                return Err(invalid_state(
                    "only an in-progress collection can register an IX watch",
                ));
            }
            let transaction_id = match &collection.legs[position].state {
                CollectionLegState::Broadcast { transaction_id } => transaction_id.clone(),
                _ => {
                    return Err(invalid_state(
                        "IX watch can only attach to a broadcast collection leg",
                    ));
                }
            };
            validate_transition_time(&collection, command.updated_at)?;
            let expected_reference = CollectionLegReference {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let reference = self
                .stored_leg_reference(&transaction_id)
                .await?
                .map(|(reference, _)| reference)
                .ok_or_else(|| storage_error("broadcast leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            let reservation = self.require_owned_active_reservation(&collection).await?;
            collection.updated_at = command.updated_at;
            collection.legs[position].watch_id = Some(command.watch_id);
            collection.legs[position].updated_at = command.updated_at;
            self.commit_collection_update(
                &stored,
                &collection,
                vec![Condition::Version {
                    namespace: active_reservation_ns(),
                    key: reservation_key(&collection.deposit_id, &collection.asset)?,
                    expected: reservation.version,
                }],
                Vec::new(),
            )
            .await?;
            Ok(collection)
        })
    }

    fn confirm_leg<'a>(
        &'a self,
        command: ConfirmCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.legs[position].state
                == (CollectionLegState::Confirmed {
                    transaction_id: command.transaction_id.clone(),
                })
            {
                if collection.legs[position].allocation == command.allocation {
                    return Ok(collection);
                }
                return Err(conflict(
                    "confirmed collection leg was replayed with different attribution",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if collection.state != CollectionState::InProgress {
                return Err(invalid_state(
                    "only an in-progress collection can confirm a leg",
                ));
            }
            match &collection.legs[position].state {
                CollectionLegState::Broadcast { transaction_id }
                    if transaction_id == &command.transaction_id => {}
                CollectionLegState::Broadcast { .. } => {
                    return Err(conflict(
                        "confirmation transaction ID does not match broadcast leg",
                    ));
                }
                _ => {
                    return Err(invalid_state(
                        "only a broadcast collection leg can be confirmed",
                    ));
                }
            }
            if collection.legs[position].watch_id.is_none() {
                return Err(invalid_state(
                    "collection leg cannot confirm before durable IX watch registration",
                ));
            }
            validate_transition_time(&collection, command.confirmed_at)?;
            validate_transaction_for_collection(&collection, &command.transaction_id)?;
            match collection.legs[position].kind {
                CollectionLegKind::GasFunding if command.allocation.is_some() => {
                    return Err(invalid(
                        "gas-funding confirmation must not contain sweep attribution",
                    ));
                }
                CollectionLegKind::Sweep => {
                    let allocation = command.allocation.as_ref().ok_or_else(|| {
                        invalid("sweep confirmation requires factual collection attribution")
                    })?;
                    validate_allocation(&collection, allocation)?;
                }
                _ => {}
            }
            let expected_reference = CollectionLegReference {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let reference = self
                .stored_leg_reference(&command.transaction_id)
                .await?
                .map(|(reference, _)| reference)
                .ok_or_else(|| storage_error("broadcast leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            let reservation = self.require_owned_active_reservation(&collection).await?;
            collection.updated_at = command.confirmed_at;
            collection.last_error = None;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Confirmed {
                transaction_id: command.transaction_id.clone(),
            };
            leg.allocation = command.allocation;
            leg.last_error = None;
            leg.updated_at = command.confirmed_at;

            let mut operations = Vec::new();
            if all_legs_confirmed(&collection) {
                collection.state = CollectionState::Completed;
                collection.reservation.state = CollectionReservationState::Consumed {
                    transaction_id: command.transaction_id,
                    consumed_at: command.confirmed_at,
                };
                operations.push(Operation::Delete {
                    namespace: active_reservation_ns(),
                    key: reservation_key(&collection.deposit_id, &collection.asset)?,
                });
            } else {
                collection.state = CollectionState::InProgress;
            }
            self.commit_collection_update(
                &stored,
                &collection,
                vec![Condition::Version {
                    namespace: active_reservation_ns(),
                    key: reservation_key(&collection.deposit_id, &collection.asset)?,
                    expected: reservation.version,
                }],
                operations,
            )
            .await?;
            Ok(collection)
        })
    }

    fn fail_leg<'a>(
        &'a self,
        command: FailCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            validate_error(&command.error)?;
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.legs[position].state
                == (CollectionLegState::Failed {
                    transaction_id: command.transaction_id.clone(),
                })
            {
                if collection.legs[position].last_error.as_ref() == Some(&command.error) {
                    return Ok(collection);
                }
                return Err(conflict(
                    "failed collection leg was replayed with a different safe error",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if collection.state != CollectionState::InProgress {
                return Err(invalid_state(
                    "only an in-progress collection can enter terminal failure",
                ));
            }
            match &collection.legs[position].state {
                CollectionLegState::Broadcast { transaction_id }
                    if transaction_id == &command.transaction_id => {}
                CollectionLegState::Broadcast { .. } => {
                    return Err(conflict(
                        "failure transaction ID does not match broadcast leg",
                    ));
                }
                _ => {
                    return Err(invalid_state(
                        "only a broadcast collection leg can enter terminal failure",
                    ));
                }
            }
            validate_transition_time(&collection, command.failed_at)?;
            let expected_reference = CollectionLegReference {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let reference = self
                .stored_leg_reference(&command.transaction_id)
                .await?
                .map(|(reference, _)| reference)
                .ok_or_else(|| storage_error("broadcast leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }
            let reservation = self.require_owned_active_reservation(&collection).await?;
            collection.state = CollectionState::Failed;
            collection.last_error = Some(command.error.clone());
            collection.updated_at = command.failed_at;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Failed {
                transaction_id: command.transaction_id,
            };
            leg.last_error = Some(command.error);
            leg.updated_at = command.failed_at;
            self.commit_collection_update(
                &stored,
                &collection,
                vec![Condition::Version {
                    namespace: active_reservation_ns(),
                    key: reservation_key(&collection.deposit_id, &collection.asset)?,
                    expected: reservation.version,
                }],
                Vec::new(),
            )
            .await?;
            Ok(collection)
        })
    }

    fn reorg_leg<'a>(
        &'a self,
        command: ReorgCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            validate_error(&command.error)?;
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.legs[position].state
                == (CollectionLegState::Reorged {
                    transaction_id: command.transaction_id.clone(),
                })
            {
                if collection.legs[position].last_error.as_ref() == Some(&command.error) {
                    return Ok(collection);
                }
                return Err(conflict(
                    "reorged collection leg was replayed with a different safe error",
                ));
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if !matches!(
                collection.state,
                CollectionState::InProgress | CollectionState::Completed
            ) {
                return Err(invalid_state(
                    "only an in-progress or completed collection can be reorged",
                ));
            }
            match &collection.legs[position].state {
                CollectionLegState::Confirmed { transaction_id }
                    if transaction_id == &command.transaction_id => {}
                CollectionLegState::Confirmed { .. } => {
                    return Err(conflict(
                        "reorg transaction ID does not match confirmed leg",
                    ));
                }
                _ => {
                    return Err(invalid_state(
                        "only a confirmed collection leg can be reorged",
                    ));
                }
            }
            validate_transition_time(&collection, command.reorged_at)?;
            let expected_reference = CollectionLegReference {
                collection_id: command.collection_id.clone(),
                leg_id: command.leg_id.clone(),
            };
            let reference = self
                .stored_leg_reference(&command.transaction_id)
                .await?
                .map(|(reference, _)| reference)
                .ok_or_else(|| storage_error("confirmed leg is missing transaction index"))?;
            if reference != expected_reference {
                return Err(conflict(
                    "transaction ID belongs to a different collection leg",
                ));
            }

            let reservation_key = reservation_key(&collection.deposit_id, &collection.asset)?;
            let mut conditions = Vec::new();
            let mut operations = Vec::new();
            match &collection.reservation.state {
                CollectionReservationState::Active => {
                    let reservation = self.require_owned_active_reservation(&collection).await?;
                    conditions.push(Condition::Version {
                        namespace: active_reservation_ns(),
                        key: reservation_key.clone(),
                        expected: reservation.version,
                    });
                }
                CollectionReservationState::Consumed { .. } => {
                    if let Some((owner, _)) = self.active_reservation_record(&collection).await? {
                        return Err(conflict(format!(
                            "reorged value cannot be reserved because collection {} already owns the deposit and asset",
                            owner.0
                        )));
                    }
                    conditions.push(Condition::Missing {
                        namespace: active_reservation_ns(),
                        key: reservation_key.clone(),
                    });
                    let (eligibility_condition, eligibility_operation) = self
                        .collection_eligibility_generation_change(
                            &collection.deposit_id,
                            &collection.asset,
                        )
                        .await?;
                    conditions.push(eligibility_condition);
                    operations.push(eligibility_operation);
                    operations.push(Operation::Put {
                        namespace: active_reservation_ns(),
                        key: reservation_key,
                        value: encode(&CollectionIndexRecordV1 {
                            version: RECORD_VERSION,
                            collection_id: collection.id.0.clone(),
                        })?,
                    });
                    collection.reservation.state = CollectionReservationState::Active;
                }
                CollectionReservationState::Released { .. } => {
                    return Err(invalid_state(
                        "released collection reservation cannot be reorged without reconciliation",
                    ));
                }
            }
            collection.state = CollectionState::Reorged;
            collection.last_error = Some(command.error.clone());
            collection.updated_at = command.reorged_at;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Reorged {
                transaction_id: command.transaction_id,
            };
            leg.last_error = Some(command.error);
            leg.updated_at = command.reorged_at;
            self.commit_collection_update(&stored, &collection, conditions, operations)
                .await?;
            Ok(collection)
        })
    }

    fn retry_leg<'a>(
        &'a self,
        command: RetryCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let position = find_leg(&collection, &command.leg_id)?;
            if collection.legs[position].state == CollectionLegState::Required
                && collection.legs[position].updated_at == command.updated_at
                && collection.legs[position].last_error.is_none()
            {
                return Ok(collection);
            }
            validate_guard(&collection, &collection.legs[position], &command.expected)?;
            if !matches!(
                collection.state,
                CollectionState::Failed | CollectionState::Reorged
            ) || !matches!(
                collection.legs[position].state,
                CollectionLegState::Failed { .. } | CollectionLegState::Reorged { .. }
            ) {
                return Err(invalid_state(
                    "only a terminal failed or reorged collection leg can retry",
                ));
            }
            ensure_previous_legs_confirmed(&collection, position)?;
            validate_transition_time(&collection, command.updated_at)?;
            let reservation_key = reservation_key(&collection.deposit_id, &collection.asset)?;
            let mut conditions = Vec::new();
            let mut operations = Vec::new();
            match &collection.reservation.state {
                CollectionReservationState::Active => {
                    let reservation = self.require_owned_active_reservation(&collection).await?;
                    conditions.push(Condition::Version {
                        namespace: active_reservation_ns(),
                        key: reservation_key,
                        expected: reservation.version,
                    });
                }
                CollectionReservationState::Released { .. } => {
                    if let Some((owner, _)) = self.active_reservation_record(&collection).await? {
                        return Err(conflict(format!(
                            "retry cannot reserve value because collection {} already owns the deposit and asset",
                            owner.0
                        )));
                    }
                    conditions.push(Condition::Missing {
                        namespace: active_reservation_ns(),
                        key: reservation_key.clone(),
                    });
                    let (eligibility_condition, eligibility_operation) = self
                        .collection_eligibility_generation_change(
                            &collection.deposit_id,
                            &collection.asset,
                        )
                        .await?;
                    conditions.push(eligibility_condition);
                    operations.push(eligibility_operation);
                    operations.push(Operation::Put {
                        namespace: active_reservation_ns(),
                        key: reservation_key,
                        value: encode(&CollectionIndexRecordV1 {
                            version: RECORD_VERSION,
                            collection_id: collection.id.0.clone(),
                        })?,
                    });
                    collection.reservation.state = CollectionReservationState::Active;
                }
                CollectionReservationState::Consumed { .. } => {
                    return Err(invalid_state(
                        "consumed collection reservation cannot retry",
                    ));
                }
            }
            collection.state = if position == 0 {
                CollectionState::Required
            } else {
                CollectionState::InProgress
            };
            collection.last_error = None;
            collection.updated_at = command.updated_at;
            let leg = &mut collection.legs[position];
            leg.state = CollectionLegState::Required;
            leg.watch_id = None;
            leg.allocation = None;
            leg.last_error = None;
            leg.updated_at = command.updated_at;
            self.commit_collection_update(&stored, &collection, conditions, operations)
                .await?;
            Ok(collection)
        })
    }

    fn release_reservation<'a>(
        &'a self,
        command: ReleaseCollectionReservation,
    ) -> BoxFuture<'a, Result<Collection, DepositError>> {
        Box::pin(async move {
            let (mut collection, stored) = self
                .required_collection_record(&command.collection_id)
                .await?;
            let desired = CollectionReservationState::Released {
                reason: command.reason,
                released_at: command.released_at,
            };
            if collection.reservation.state == desired {
                return Ok(collection);
            }
            if collection.state != command.expected_collection_state {
                return Err(conflict(
                    "stale expected collection state for reservation release",
                ));
            }
            if collection.reservation.state != command.expected_reservation_state {
                return Err(conflict("stale expected collection reservation state"));
            }
            match (collection.state, command.reason) {
                (CollectionState::Failed, ReservationReleaseReason::TerminalFailure)
                | (CollectionState::Reorged, ReservationReleaseReason::Reorg) => {}
                _ => {
                    return Err(invalid_state(
                        "reservation release reason must match terminal failure or reorg state",
                    ));
                }
            }
            if collection.reservation.state != CollectionReservationState::Active {
                return Err(invalid_state(
                    "only an active collection reservation can be released",
                ));
            }
            validate_transition_time(&collection, command.released_at)?;
            let reservation = self.require_owned_active_reservation(&collection).await?;
            let reservation_key = reservation_key(&collection.deposit_id, &collection.asset)?;
            collection.reservation.state = desired;
            collection.updated_at = command.released_at;
            self.commit_collection_update(
                &stored,
                &collection,
                vec![Condition::Version {
                    namespace: active_reservation_ns(),
                    key: reservation_key.clone(),
                    expected: reservation.version,
                }],
                vec![Operation::Delete {
                    namespace: active_reservation_ns(),
                    key: reservation_key,
                }],
            )
            .await?;
            Ok(collection)
        })
    }
}
