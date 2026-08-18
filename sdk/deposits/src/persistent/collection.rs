use std::collections::BTreeSet;

use base::Decimal;
use bincode::{Decode, Encode};
use indexing::WatchId;
use indexing::{AssetId, ChainId, IndexScope, TransactionRef};
use storage::{
    Condition, Error, ErrorKind, Key, Namespace, Operation, ScanRequest, Store, StoredValue, Value,
    WriteBatch,
};

use crate::{
    AcceptBroadcast, AttachWatch, BoxFuture, Collection, CollectionAllocation, CollectionCreator,
    CollectionError, CollectionHistory, CollectionId, CollectionLeg, CollectionLegKind,
    CollectionLegState, CollectionMode, CollectionPage, CollectionParticipant, CollectionPlan,
    CollectionQuery, CollectionReader, CollectionReservation, CollectionReservationState,
    CollectionRetry, CollectionState, ConfirmLeg, CreateBatch, CreateCollectionOutcome,
    DepositError, DepositErrorKind, DepositId, DepositReader, FailLeg, JobId, JobPayload,
    JobReader, JobResource, LegId, LegOutcome, LegRef, MAX_COLLECTION_PARTICIPANTS,
    MAX_COLLECTION_SPEND_RESOURCES, MAX_TOTAL_SPEND_RESOURCE_EVIDENCE_BYTES, PolicyIdentity,
    RecordSignature, ReleaseReservation, ReorgLeg, ReservationReleaseReason, ResourceId,
    ResourceProof, RetryLeg, SignedBytes, SignedEnvelope, SpendResource, SubmissionWriter,
    TransitionGuard, UserId, UserStore, UtxoBatchProjectionTransition,
};

use super::PaymentStore;

mod create;
mod leg_record;
mod model;
mod outcome;
mod read;
mod record;
mod repository;
mod reservation;
mod retry;
mod state_validation;
mod submission;
mod transition;
mod validation;
mod value_record;

use leg_record::*;
use model::*;
use record::*;
use validation::*;
use value_record::*;

const RECORD_VERSION: u16 = 1;
const COLLECTION_RECORD_VERSION: u16 = 3;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 1_000;
const MAX_SAFE_ERROR_CODE_BYTES: usize = 128;
const MAX_SAFE_ERROR_MESSAGE_BYTES: usize = 4_096;

pub(crate) struct CloseFence {
    pub conditions: Vec<Condition>,
    pub operations: Vec<Operation>,
}

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

pub(crate) fn active_spend_resource_ns() -> Namespace {
    ns("ps.v2.active_collection_spend_resource")
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

fn transaction_key(transaction_id: &TransactionRef) -> Result<Key, DepositError> {
    component_key(&[
        transaction_id.scope.chain.0.as_bytes(),
        transaction_id.scope.network.as_bytes(),
        transaction_id.value.as_bytes(),
    ])
}

pub(crate) fn spend_resource_key(resource: &ResourceId) -> Result<Key, DepositError> {
    component_key(&[
        resource.transaction_id.scope.chain.0.as_bytes(),
        resource.transaction_id.scope.network.as_bytes(),
        resource.transaction_id.value.as_bytes(),
        &resource.output_index.to_be_bytes(),
    ])
}

fn envelope_key(collection_id: &CollectionId, leg_id: &LegId) -> Result<Key, DepositError> {
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
    .map_err(|error| storage_error(format!("failed to encode PS collection record: {error}")))
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
    .map_err(|error| storage_error(format!("failed to decode PS collection record: {error}")))?;
    if consumed != stored.value.0.len() {
        return Err(storage_error(
            "PS collection record contains trailing bytes",
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

fn map_storage(error: Error) -> DepositError {
    let kind = match error.kind {
        ErrorKind::Conflict => DepositErrorKind::Conflict,
        ErrorKind::CorruptData | ErrorKind::InvalidRequest => DepositErrorKind::InvariantViolation,
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
