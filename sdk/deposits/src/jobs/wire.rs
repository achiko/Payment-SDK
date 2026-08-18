use bincode::{Decode, Encode};
use storage::{Key, Namespace, StoredValue, Value};

use crate::{CommandIdentity, CommandOperation, DepositError, JobId, JobResource, UserId};

use super::{invalid, storage_error};

pub(super) const RECORD_VERSION: u16 = 1;
pub(super) const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_PAGE_SIZE: usize = 1_000;

pub(super) fn ns(value: &str) -> Namespace {
    Namespace(value.to_owned())
}

pub(super) fn user_ns() -> Namespace {
    ns("ps.v1.user")
}

pub(super) fn database_metadata_ns() -> Namespace {
    ns("ps.v1.database_metadata")
}

pub(super) fn database_metadata_key() -> Key {
    key_text("identity")
}

pub(super) fn ix_semantic_ns() -> Namespace {
    ns("ix.semantic.v1")
}

pub(super) fn job_ns() -> Namespace {
    ns("ps.v1.job")
}

pub(super) fn command_job_ns() -> Namespace {
    ns("ps.v1.command_job")
}

pub(super) fn user_job_ns() -> Namespace {
    ns("ps.v1.user_job")
}

pub(super) fn resource_job_ns() -> Namespace {
    ns("ps.v1.resource_job")
}

pub(super) fn ready_job_ns() -> Namespace {
    ns("ps.v1.ready_job")
}

pub(super) fn key_text(value: &str) -> Key {
    Key(value.as_bytes().to_vec())
}

pub(super) fn component_key(parts: &[&[u8]]) -> Result<Key, DepositError> {
    let mut output = Vec::new();
    for part in parts {
        let length = u32::try_from(part.len()).map_err(|_| invalid("key component is too long"))?;
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(part);
    }
    Ok(Key(output))
}

pub(super) const fn operation_tag(operation: CommandOperation) -> &'static [u8] {
    match operation {
        CommandOperation::DepositPlan => b"create_deposit",
        CommandOperation::CloseDeposit => b"close_deposit",
        CommandOperation::CollectionPlan => b"create_collection",
        CommandOperation::RetryCollection => b"retry_collection",
        CommandOperation::Accounting => b"accounting",
        CommandOperation::ResolveReconciliation => b"resolve_reconciliation",
    }
}

pub(super) fn command_key(identity: &CommandIdentity) -> Result<Key, DepositError> {
    component_key(&[
        identity.principal.0.as_bytes(),
        operation_tag(identity.operation),
        identity.client_key.0.as_bytes(),
    ])
}

pub(super) fn user_job_key(user_id: &UserId, job_id: &JobId) -> Result<Key, DepositError> {
    component_key(&[user_id.0.as_bytes(), job_id.0.as_bytes()])
}

pub(super) fn user_job_prefix(user_id: &UserId) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[user_id.0.as_bytes()])?.0)
}

pub(super) const fn resource_tag(resource: &JobResource) -> &'static [u8] {
    match resource {
        JobResource::Deposit(_) => b"deposit",
        JobResource::Collection(_) => b"collection",
    }
}

pub(super) fn resource_id(resource: &JobResource) -> &str {
    match resource {
        JobResource::Deposit(id) => &id.0,
        JobResource::Collection(id) => &id.0,
    }
}

pub(super) fn resource_job_key(
    resource: &JobResource,
    job_id: &JobId,
) -> Result<Key, DepositError> {
    component_key(&[
        resource_tag(resource),
        resource_id(resource).as_bytes(),
        job_id.0.as_bytes(),
    ])
}

pub(super) fn resource_job_prefix(resource: &JobResource) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[resource_tag(resource), resource_id(resource).as_bytes()])?.0)
}

pub(super) fn ready_job_key(ready_at: u64, job_id: &JobId) -> Key {
    let mut output = Vec::with_capacity(8 + job_id.0.len());
    output.extend_from_slice(&ready_at.to_be_bytes());
    output.extend_from_slice(job_id.0.as_bytes());
    Key(output)
}

pub(super) fn ready_at_from_key(key: &Key) -> Result<u64, DepositError> {
    let bytes = key
        .0
        .get(..8)
        .ok_or_else(|| storage_error("ready-job index key is truncated"))?;
    let encoded: [u8; 8] = bytes
        .try_into()
        .map_err(|_| storage_error("ready-job index timestamp is invalid"))?;
    Ok(u64::from_be_bytes(encoded))
}

pub(super) fn encode<T: Encode>(record: &T) -> Result<Value, DepositError> {
    bincode::encode_to_vec(
        record,
        bincode::config::standard()
            .with_fixed_int_encoding()
            .with_big_endian(),
    )
    .map(Value)
    .map_err(|error| storage_error(format!("failed to encode PS job record: {error}")))
}

pub(super) fn decode<T>(stored: &StoredValue) -> Result<T, DepositError>
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
    .map_err(|error| storage_error(format!("failed to decode PS job record: {error}")))?;
    if consumed != stored.value.0.len() {
        return Err(storage_error("PS job record contains trailing bytes"));
    }
    Ok(record)
}

pub(super) fn ensure_version(version: u16) -> Result<(), DepositError> {
    if version == RECORD_VERSION {
        Ok(())
    } else {
        Err(storage_error(format!(
            "unsupported PS job record version {version}"
        )))
    }
}
