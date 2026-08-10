use std::collections::BTreeSet;

use bincode::{Decode, Encode};
use chain_identity::{AssetId, AtomicAmount, ChainId};
use indexing::IndexScope;
use storage::{
    Condition, Key, Namespace, Operation, ScanRequest, Storage, StorageError, StorageErrorKind,
    StoredValue, Value, WriteBatch,
};

use crate::{
    BoxFuture, ClaimJob, CloseDepositJob, CollectionId, CommandIdentity, CommandOperation,
    CommandPrincipal, CreateCollectionJob, CreateDepositJob, CreateJob, CreateJobOutcome,
    CreateUtxoBatchCollectionJob, DepositError, DepositErrorKind, DepositId, DepositStore,
    EnsureUser, IdempotencyKey, InitializePaymentDatabase, Job, JobError, JobId, JobKind, JobPage,
    JobPageRequest, JobPayload, JobResource, JobState, JobStore, MigratePaymentDatabase,
    PAYMENT_DOMAIN_SCHEMA_VERSION, PAYMENT_SERVICE_OWNER, PaymentDatabaseMetadata,
    PaymentDatabaseMetadataStore, PaymentDatabaseMigrationReport, PersistentPaymentRepository,
    PolicyIdentity, RequestHash, RetryCollectionJob, RetryUtxoBatchCollectionJob, TransitionJob,
    User, UserId, UserStore,
};

const RECORD_VERSION: u16 = 1;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 1_000;

fn ns(value: &str) -> Namespace {
    Namespace(value.to_owned())
}

fn user_ns() -> Namespace {
    ns("ps.v1.user")
}

fn database_metadata_ns() -> Namespace {
    ns("ps.v1.database_metadata")
}

fn database_metadata_key() -> Key {
    key_text("identity")
}

fn ix_semantic_ns() -> Namespace {
    ns("ix.semantic.v1")
}

fn job_ns() -> Namespace {
    ns("ps.v1.job")
}

fn command_job_ns() -> Namespace {
    ns("ps.v1.command_job")
}

fn user_job_ns() -> Namespace {
    ns("ps.v1.user_job")
}

fn resource_job_ns() -> Namespace {
    ns("ps.v1.resource_job")
}

fn ready_job_ns() -> Namespace {
    ns("ps.v1.ready_job")
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

const fn operation_tag(operation: CommandOperation) -> &'static [u8] {
    match operation {
        CommandOperation::CreateDeposit => b"create_deposit",
        CommandOperation::CloseDeposit => b"close_deposit",
        CommandOperation::CreateCollection => b"create_collection",
        CommandOperation::RetryCollection => b"retry_collection",
        CommandOperation::Accounting => b"accounting",
        CommandOperation::ResolveReconciliation => b"resolve_reconciliation",
    }
}

fn command_key(identity: &CommandIdentity) -> Result<Key, DepositError> {
    component_key(&[
        identity.principal.0.as_bytes(),
        operation_tag(identity.operation),
        identity.client_key.0.as_bytes(),
    ])
}

fn user_job_key(user_id: &UserId, job_id: &JobId) -> Result<Key, DepositError> {
    component_key(&[user_id.0.as_bytes(), job_id.0.as_bytes()])
}

fn user_job_prefix(user_id: &UserId) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[user_id.0.as_bytes()])?.0)
}

const fn resource_tag(resource: &JobResource) -> &'static [u8] {
    match resource {
        JobResource::Deposit(_) => b"deposit",
        JobResource::Collection(_) => b"collection",
    }
}

fn resource_id(resource: &JobResource) -> &str {
    match resource {
        JobResource::Deposit(id) => &id.0,
        JobResource::Collection(id) => &id.0,
    }
}

fn resource_job_key(resource: &JobResource, job_id: &JobId) -> Result<Key, DepositError> {
    component_key(&[
        resource_tag(resource),
        resource_id(resource).as_bytes(),
        job_id.0.as_bytes(),
    ])
}

fn resource_job_prefix(resource: &JobResource) -> Result<Vec<u8>, DepositError> {
    Ok(component_key(&[resource_tag(resource), resource_id(resource).as_bytes()])?.0)
}

fn ready_job_key(ready_at: u64, job_id: &JobId) -> Key {
    let mut output = Vec::with_capacity(8 + job_id.0.len());
    output.extend_from_slice(&ready_at.to_be_bytes());
    output.extend_from_slice(job_id.0.as_bytes());
    Key(output)
}

fn ready_at_from_key(key: &Key) -> Result<u64, DepositError> {
    let bytes = key
        .0
        .get(..8)
        .ok_or_else(|| storage_error("ready-job index key is truncated"))?;
    let encoded: [u8; 8] = bytes
        .try_into()
        .map_err(|_| storage_error("ready-job index timestamp is invalid"))?;
    Ok(u64::from_be_bytes(encoded))
}

fn encode<T: Encode>(record: &T) -> Result<Value, DepositError> {
    bincode::encode_to_vec(
        record,
        bincode::config::standard()
            .with_fixed_int_encoding()
            .with_big_endian(),
    )
    .map(Value)
    .map_err(|error| storage_error(format!("failed to encode PS job RecordV1: {error}")))
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
    .map_err(|error| storage_error(format!("failed to decode PS job RecordV1: {error}")))?;
    if consumed != stored.value.0.len() {
        return Err(storage_error("PS job RecordV1 contains trailing bytes"));
    }
    Ok(record)
}

fn ensure_version(version: u16) -> Result<(), DepositError> {
    if version == RECORD_VERSION {
        Ok(())
    } else {
        Err(storage_error(format!(
            "unsupported PS job record version {version}"
        )))
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct UserRecordV1 {
    version: u16,
    id: String,
    owner: String,
    first_seen_at: u64,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct DatabaseMetadataRecordV1 {
    record_version: u16,
    service_owner: String,
    domain_schema_version: u16,
    scope: ScopeRecordV1,
    active_policy_version: String,
    active_policy_digest: [u8; 32],
    initialized_at: u64,
}

impl From<&PaymentDatabaseMetadata> for DatabaseMetadataRecordV1 {
    fn from(value: &PaymentDatabaseMetadata) -> Self {
        Self {
            record_version: RECORD_VERSION,
            service_owner: value.service_owner.clone(),
            domain_schema_version: value.domain_schema_version,
            scope: ScopeRecordV1::from(&value.scope),
            active_policy_version: value.active_policy.version.clone(),
            active_policy_digest: value.active_policy.digest,
            initialized_at: value.initialized_at,
        }
    }
}

impl TryFrom<DatabaseMetadataRecordV1> for PaymentDatabaseMetadata {
    type Error = DepositError;

    fn try_from(value: DatabaseMetadataRecordV1) -> Result<Self, Self::Error> {
        ensure_version(value.record_version)?;
        Ok(Self {
            service_owner: value.service_owner,
            domain_schema_version: value.domain_schema_version,
            scope: value.scope.into(),
            active_policy: PolicyIdentity {
                version: value.active_policy_version,
                digest: value.active_policy_digest,
            },
            initialized_at: value.initialized_at,
        })
    }
}

impl From<&User> for UserRecordV1 {
    fn from(value: &User) -> Self {
        Self {
            version: RECORD_VERSION,
            id: value.id.0.clone(),
            owner: value.owner.0.clone(),
            first_seen_at: value.first_seen_at,
        }
    }
}

impl TryFrom<UserRecordV1> for User {
    type Error = DepositError;

    fn try_from(value: UserRecordV1) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        Ok(Self {
            id: UserId(value.id),
            owner: CommandPrincipal(value.owner),
            first_seen_at: value.first_seen_at,
        })
    }
}

#[derive(Clone, Copy, Debug, Decode, Encode, PartialEq, Eq)]
enum CommandOperationRecordV1 {
    CreateDeposit,
    CloseDeposit,
    CreateCollection,
    RetryCollection,
    Accounting,
    ResolveReconciliation,
}

impl From<CommandOperation> for CommandOperationRecordV1 {
    fn from(value: CommandOperation) -> Self {
        match value {
            CommandOperation::CreateDeposit => Self::CreateDeposit,
            CommandOperation::CloseDeposit => Self::CloseDeposit,
            CommandOperation::CreateCollection => Self::CreateCollection,
            CommandOperation::RetryCollection => Self::RetryCollection,
            CommandOperation::Accounting => Self::Accounting,
            CommandOperation::ResolveReconciliation => Self::ResolveReconciliation,
        }
    }
}

impl From<CommandOperationRecordV1> for CommandOperation {
    fn from(value: CommandOperationRecordV1) -> Self {
        match value {
            CommandOperationRecordV1::CreateDeposit => Self::CreateDeposit,
            CommandOperationRecordV1::CloseDeposit => Self::CloseDeposit,
            CommandOperationRecordV1::CreateCollection => Self::CreateCollection,
            CommandOperationRecordV1::RetryCollection => Self::RetryCollection,
            CommandOperationRecordV1::Accounting => Self::Accounting,
            CommandOperationRecordV1::ResolveReconciliation => Self::ResolveReconciliation,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct CommandIdentityRecordV1 {
    principal: String,
    operation: CommandOperationRecordV1,
    client_key: String,
    request_hash: [u8; 32],
}

impl From<&CommandIdentity> for CommandIdentityRecordV1 {
    fn from(value: &CommandIdentity) -> Self {
        Self {
            principal: value.principal.0.clone(),
            operation: value.operation.into(),
            client_key: value.client_key.0.clone(),
            request_hash: value.request_hash.0,
        }
    }
}

impl From<CommandIdentityRecordV1> for CommandIdentity {
    fn from(value: CommandIdentityRecordV1) -> Self {
        Self {
            principal: CommandPrincipal(value.principal),
            operation: value.operation.into(),
            client_key: IdempotencyKey(value.client_key),
            request_hash: RequestHash(value.request_hash),
        }
    }
}

#[derive(Clone, Copy, Debug, Decode, Encode, PartialEq, Eq)]
enum JobKindRecordV1 {
    CreateDeposit,
    CloseDeposit,
    CreateCollection,
    RetryCollection,
}

impl From<JobKind> for JobKindRecordV1 {
    fn from(value: JobKind) -> Self {
        match value {
            JobKind::CreateDeposit => Self::CreateDeposit,
            JobKind::CloseDeposit => Self::CloseDeposit,
            JobKind::CreateCollection => Self::CreateCollection,
            JobKind::RetryCollection => Self::RetryCollection,
        }
    }
}

impl From<JobKindRecordV1> for JobKind {
    fn from(value: JobKindRecordV1) -> Self {
        match value {
            JobKindRecordV1::CreateDeposit => Self::CreateDeposit,
            JobKindRecordV1::CloseDeposit => Self::CloseDeposit,
            JobKindRecordV1::CreateCollection => Self::CreateCollection,
            JobKindRecordV1::RetryCollection => Self::RetryCollection,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct ScopeRecordV1 {
    chain: String,
    network: String,
}

impl From<&IndexScope> for ScopeRecordV1 {
    fn from(value: &IndexScope) -> Self {
        Self {
            chain: value.chain.0.clone(),
            network: value.network.clone(),
        }
    }
}

impl From<ScopeRecordV1> for IndexScope {
    fn from(value: ScopeRecordV1) -> Self {
        Self {
            chain: ChainId(value.chain),
            network: value.network,
        }
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
enum JobPayloadRecordV1 {
    CreateDeposit {
        deposit_id: String,
        user_id: String,
        scope: ScopeRecordV1,
        asset: AssetRecordV1,
        expected: [u8; 32],
        expires_at: u64,
        created_at: u64,
        key_purpose: String,
    },
    CloseDeposit {
        deposit_id: String,
        user_id: String,
    },
    CreateCollection {
        collection_id: String,
        deposit_id: String,
        user_id: String,
    },
    RetryCollection {
        collection_id: String,
        deposit_id: String,
        user_id: String,
    },
    CreateUtxoBatchCollection {
        collection_id: String,
        deposit_ids: Vec<String>,
    },
    RetryUtxoBatchCollection {
        collection_id: String,
        deposit_ids: Vec<String>,
    },
}

impl From<&JobPayload> for JobPayloadRecordV1 {
    fn from(value: &JobPayload) -> Self {
        match value {
            JobPayload::CreateDeposit(payload) => Self::CreateDeposit {
                deposit_id: payload.deposit_id.0.clone(),
                user_id: payload.user_id.0.clone(),
                scope: ScopeRecordV1::from(&payload.scope),
                asset: AssetRecordV1::from(&payload.asset),
                expected: payload.expected.0,
                expires_at: payload.expires_at,
                created_at: payload.created_at,
                key_purpose: payload.key_purpose.clone(),
            },
            JobPayload::CloseDeposit(payload) => Self::CloseDeposit {
                deposit_id: payload.deposit_id.0.clone(),
                user_id: payload.user_id.0.clone(),
            },
            JobPayload::CreateCollection(payload) => Self::CreateCollection {
                collection_id: payload.collection_id.0.clone(),
                deposit_id: payload.deposit_id.0.clone(),
                user_id: payload.user_id.0.clone(),
            },
            JobPayload::RetryCollection(payload) => Self::RetryCollection {
                collection_id: payload.collection_id.0.clone(),
                deposit_id: payload.deposit_id.0.clone(),
                user_id: payload.user_id.0.clone(),
            },
            JobPayload::CreateUtxoBatchCollection(payload) => Self::CreateUtxoBatchCollection {
                collection_id: payload.collection_id.0.clone(),
                deposit_ids: payload
                    .deposit_ids
                    .iter()
                    .map(|deposit_id| deposit_id.0.clone())
                    .collect(),
            },
            JobPayload::RetryUtxoBatchCollection(payload) => Self::RetryUtxoBatchCollection {
                collection_id: payload.collection_id.0.clone(),
                deposit_ids: payload
                    .deposit_ids
                    .iter()
                    .map(|deposit_id| deposit_id.0.clone())
                    .collect(),
            },
        }
    }
}

impl From<JobPayloadRecordV1> for JobPayload {
    fn from(value: JobPayloadRecordV1) -> Self {
        match value {
            JobPayloadRecordV1::CreateDeposit {
                deposit_id,
                user_id,
                scope,
                asset,
                expected,
                expires_at,
                created_at,
                key_purpose,
            } => Self::CreateDeposit(CreateDepositJob {
                deposit_id: DepositId(deposit_id),
                user_id: UserId(user_id),
                scope: scope.into(),
                asset: asset.into(),
                expected: AtomicAmount(expected),
                expires_at,
                created_at,
                key_purpose,
            }),
            JobPayloadRecordV1::CloseDeposit {
                deposit_id,
                user_id,
            } => Self::CloseDeposit(CloseDepositJob {
                deposit_id: DepositId(deposit_id),
                user_id: UserId(user_id),
            }),
            JobPayloadRecordV1::CreateCollection {
                collection_id,
                deposit_id,
                user_id,
            } => Self::CreateCollection(CreateCollectionJob {
                collection_id: CollectionId(collection_id),
                deposit_id: DepositId(deposit_id),
                user_id: UserId(user_id),
            }),
            JobPayloadRecordV1::RetryCollection {
                collection_id,
                deposit_id,
                user_id,
            } => Self::RetryCollection(RetryCollectionJob {
                collection_id: CollectionId(collection_id),
                deposit_id: DepositId(deposit_id),
                user_id: UserId(user_id),
            }),
            JobPayloadRecordV1::CreateUtxoBatchCollection {
                collection_id,
                deposit_ids,
            } => Self::CreateUtxoBatchCollection(CreateUtxoBatchCollectionJob {
                collection_id: CollectionId(collection_id),
                deposit_ids: deposit_ids.into_iter().map(DepositId).collect(),
            }),
            JobPayloadRecordV1::RetryUtxoBatchCollection {
                collection_id,
                deposit_ids,
            } => Self::RetryUtxoBatchCollection(RetryUtxoBatchCollectionJob {
                collection_id: CollectionId(collection_id),
                deposit_ids: deposit_ids.into_iter().map(DepositId).collect(),
            }),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum JobResourceRecordV1 {
    Deposit(String),
    Collection(String),
}

impl From<&JobResource> for JobResourceRecordV1 {
    fn from(value: &JobResource) -> Self {
        match value {
            JobResource::Deposit(id) => Self::Deposit(id.0.clone()),
            JobResource::Collection(id) => Self::Collection(id.0.clone()),
        }
    }
}

impl From<JobResourceRecordV1> for JobResource {
    fn from(value: JobResourceRecordV1) -> Self {
        match value {
            JobResourceRecordV1::Deposit(id) => Self::Deposit(DepositId(id)),
            JobResourceRecordV1::Collection(id) => Self::Collection(CollectionId(id)),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
enum JobStateRecordV1 {
    Queued,
    Running { lease_expires_at: u64 },
    WaitingRetry { next_attempt_at: u64 },
    Succeeded,
    Failed,
}

impl From<&JobState> for JobStateRecordV1 {
    fn from(value: &JobState) -> Self {
        match value {
            JobState::Queued => Self::Queued,
            JobState::Running { lease_expires_at } => Self::Running {
                lease_expires_at: *lease_expires_at,
            },
            JobState::WaitingRetry { next_attempt_at } => Self::WaitingRetry {
                next_attempt_at: *next_attempt_at,
            },
            JobState::Succeeded => Self::Succeeded,
            JobState::Failed => Self::Failed,
        }
    }
}

impl From<JobStateRecordV1> for JobState {
    fn from(value: JobStateRecordV1) -> Self {
        match value {
            JobStateRecordV1::Queued => Self::Queued,
            JobStateRecordV1::Running { lease_expires_at } => Self::Running { lease_expires_at },
            JobStateRecordV1::WaitingRetry { next_attempt_at } => {
                Self::WaitingRetry { next_attempt_at }
            }
            JobStateRecordV1::Succeeded => Self::Succeeded,
            JobStateRecordV1::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct JobErrorRecordV1 {
    code: String,
    message: String,
    retryable: bool,
}

impl From<&JobError> for JobErrorRecordV1 {
    fn from(value: &JobError) -> Self {
        Self {
            code: value.code.clone(),
            message: value.message.clone(),
            retryable: value.retryable,
        }
    }
}

impl From<JobErrorRecordV1> for JobError {
    fn from(value: JobErrorRecordV1) -> Self {
        Self {
            code: value.code,
            message: value.message,
            retryable: value.retryable,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct JobRecordV1 {
    version: u16,
    id: String,
    command: CommandIdentityRecordV1,
    kind: JobKindRecordV1,
    payload: JobPayloadRecordV1,
    resource: JobResourceRecordV1,
    user_id: String,
    user_owner: String,
    policy_version: String,
    policy_digest: [u8; 32],
    state: JobStateRecordV1,
    attempt_count: u32,
    last_error: Option<JobErrorRecordV1>,
    created_at: u64,
    updated_at: u64,
}

impl From<&Job> for JobRecordV1 {
    fn from(value: &Job) -> Self {
        Self {
            version: RECORD_VERSION,
            id: value.id.0.clone(),
            command: CommandIdentityRecordV1::from(&value.command),
            kind: value.kind.into(),
            payload: JobPayloadRecordV1::from(&value.payload),
            resource: JobResourceRecordV1::from(&value.resource),
            user_id: value.user_id.0.clone(),
            user_owner: value.user_owner.0.clone(),
            policy_version: value.policy.version.clone(),
            policy_digest: value.policy.digest,
            state: JobStateRecordV1::from(&value.state),
            attempt_count: value.attempt_count,
            last_error: value.last_error.as_ref().map(JobErrorRecordV1::from),
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

impl TryFrom<JobRecordV1> for Job {
    type Error = DepositError;

    fn try_from(value: JobRecordV1) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        let payload: JobPayload = value.payload.into();
        let kind: JobKind = value.kind.into();
        let resource: JobResource = value.resource.into();
        let user_id = UserId(value.user_id);
        if payload.kind() != kind
            || payload.resource() != resource
            || payload
                .user_id()
                .is_some_and(|payload_user| payload_user != &user_id)
        {
            return Err(storage_error(
                "PS job record payload associations are inconsistent",
            ));
        }
        Ok(Self {
            id: JobId(value.id),
            command: value.command.into(),
            kind,
            payload,
            resource,
            user_id,
            user_owner: CommandPrincipal(value.user_owner),
            policy: PolicyIdentity {
                version: value.policy_version,
                digest: value.policy_digest,
            },
            state: value.state.into(),
            attempt_count: value.attempt_count,
            last_error: value.last_error.map(Into::into),
            created_at: value.created_at,
            updated_at: value.updated_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct CommandJobRecordV1 {
    version: u16,
    command: CommandIdentityRecordV1,
    job_id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
struct JobIndexRecordV1 {
    version: u16,
    job_id: String,
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

fn validate_non_empty(value: &str, name: &str) -> Result<(), DepositError> {
    if value.trim().is_empty() {
        Err(invalid(format!("{name} must not be empty")))
    } else {
        Ok(())
    }
}

fn validate_create(command: &CreateJob) -> Result<(), DepositError> {
    validate_non_empty(&command.id.0, "job ID")?;
    validate_non_empty(&command.command.principal.0, "command principal")?;
    validate_non_empty(&command.command.client_key.0, "command idempotency key")?;
    if let Some(user_id) = command.payload.user_id() {
        validate_non_empty(&user_id.0, "user ID")?;
    }
    validate_non_empty(&command.user_owner.0, "user owner principal")?;
    validate_non_empty(&command.policy.version, "job policy version")?;
    if command.command.operation != command.payload.operation() {
        return Err(invalid(
            "command operation does not match the durable job payload",
        ));
    }
    match &command.payload {
        JobPayload::CreateDeposit(payload) => {
            validate_non_empty(&payload.deposit_id.0, "deposit ID")?;
            validate_non_empty(&payload.scope.chain.0, "Indexer scope chain")?;
            validate_non_empty(&payload.scope.network, "Indexer scope network")?;
            validate_non_empty(&payload.asset.asset, "asset ID")?;
            validate_non_empty(&payload.key_purpose, "key purpose")?;
            if payload.asset.chain != payload.scope.chain {
                return Err(invalid(
                    "deposit job asset and Indexer scope must share a chain",
                ));
            }
            if payload.created_at != command.created_at {
                return Err(invalid(
                    "deposit payload creation time must match the job creation time",
                ));
            }
            if payload.expires_at < payload.created_at {
                return Err(invalid("deposit job expiration precedes creation"));
            }
        }
        JobPayload::CloseDeposit(payload) => {
            validate_non_empty(&payload.deposit_id.0, "deposit ID")?;
        }
        JobPayload::CreateCollection(payload) => {
            validate_non_empty(&payload.collection_id.0, "collection ID")?;
            validate_non_empty(&payload.deposit_id.0, "deposit ID")?;
        }
        JobPayload::RetryCollection(payload) => {
            validate_non_empty(&payload.collection_id.0, "collection ID")?;
            validate_non_empty(&payload.deposit_id.0, "deposit ID")?;
        }
        JobPayload::CreateUtxoBatchCollection(payload) => {
            validate_non_empty(&payload.collection_id.0, "collection ID")?;
            validate_canonical_deposit_ids(&payload.deposit_ids)?;
        }
        JobPayload::RetryUtxoBatchCollection(payload) => {
            validate_non_empty(&payload.collection_id.0, "collection ID")?;
            validate_canonical_deposit_ids(&payload.deposit_ids)?;
        }
    }
    Ok(())
}

fn validate_canonical_deposit_ids(deposit_ids: &[DepositId]) -> Result<(), DepositError> {
    if deposit_ids.is_empty() {
        return Err(invalid("UTXO-batch job must contain at least one deposit"));
    }
    let mut previous = None;
    for deposit_id in deposit_ids {
        validate_non_empty(&deposit_id.0, "UTXO-batch deposit ID")?;
        if previous
            .as_ref()
            .is_some_and(|current| current >= deposit_id)
        {
            return Err(invalid(
                "UTXO-batch job deposit IDs must be strictly canonical and unique",
            ));
        }
        previous = Some(deposit_id.clone());
    }
    Ok(())
}

fn validate_page(request: &JobPageRequest) -> Result<(), DepositError> {
    if request.limit == 0 || request.limit > MAX_PAGE_SIZE {
        Err(invalid("job page size must be between 1 and 1000"))
    } else {
        Ok(())
    }
}

fn validate_job_error(error: &JobError) -> Result<(), DepositError> {
    validate_non_empty(&error.code, "job error code")?;
    validate_non_empty(&error.message, "job error message")
}

fn validate_metadata_command(command: &InitializePaymentDatabase) -> Result<(), DepositError> {
    validate_non_empty(&command.scope.chain.0, "Payment Service scope chain")?;
    validate_non_empty(&command.scope.network, "Payment Service scope network")?;
    validate_non_empty(&command.active_policy.version, "active policy version")
}

fn validate_migration_command(command: &MigratePaymentDatabase) -> Result<(), DepositError> {
    if !matches!(command.scope.chain.0.as_str(), "ethereum" | "bitcoin") {
        return Err(invalid(
            "Payment Service semantic migration supports only Ethereum or Bitcoin scope",
        ));
    }
    validate_non_empty(&command.scope.network, "Payment Service scope network")?;
    validate_non_empty(&command.active_policy.version, "active policy version")?;
    if command.page_size == 0 || command.page_size > MAX_PAGE_SIZE {
        return Err(invalid("migration page size must be between 1 and 1000"));
    }
    Ok(())
}

fn expected_metadata(command: InitializePaymentDatabase) -> PaymentDatabaseMetadata {
    PaymentDatabaseMetadata {
        service_owner: PAYMENT_SERVICE_OWNER.to_owned(),
        domain_schema_version: PAYMENT_DOMAIN_SCHEMA_VERSION,
        scope: command.scope,
        active_policy: command.active_policy,
        initialized_at: command.initialized_at,
    }
}

fn validate_persisted_metadata(
    persisted: PaymentDatabaseMetadata,
    expected: &PaymentDatabaseMetadata,
) -> Result<PaymentDatabaseMetadata, DepositError> {
    if persisted.service_owner != PAYMENT_SERVICE_OWNER {
        return Err(conflict(format!(
            "database is owned by {}, not Payment Service",
            persisted.service_owner
        )));
    }
    if persisted.domain_schema_version != PAYMENT_DOMAIN_SCHEMA_VERSION {
        return Err(conflict(format!(
            "Payment Service domain schema version {} does not match runtime version {}",
            persisted.domain_schema_version, PAYMENT_DOMAIN_SCHEMA_VERSION
        )));
    }
    if persisted.scope != expected.scope {
        return Err(conflict(
            "Payment Service database is bound to a different Indexer scope",
        ));
    }
    if persisted.active_policy != expected.active_policy {
        return Err(conflict(
            "Payment Service database is bound to a different active policy",
        ));
    }
    Ok(persisted)
}

fn state_ready_at(job: &Job) -> Option<u64> {
    match &job.state {
        JobState::Queued => Some(job.created_at),
        JobState::Running { lease_expires_at } => Some(*lease_expires_at),
        JobState::WaitingRetry { next_attempt_at } => Some(*next_attempt_at),
        JobState::Succeeded | JobState::Failed => None,
    }
}

impl<S> PersistentPaymentRepository<S>
where
    S: Storage,
{
    async fn resolve_job_users(
        &self,
        payload: &JobPayload,
        owner: &CommandPrincipal,
    ) -> Result<(UserId, Vec<UserId>), DepositError> {
        if let Some(user_id) = payload.user_id() {
            return Ok((user_id.clone(), vec![user_id.clone()]));
        }
        let deposit_ids = payload
            .deposit_ids()
            .ok_or_else(|| invalid("job payload has no user or deposit associations"))?;
        let mut primary = None;
        let mut associated = BTreeSet::new();
        for deposit_id in deposit_ids {
            let deposit = self
                .deposit(deposit_id)
                .await?
                .ok_or_else(|| not_found("UTXO-batch job deposit was not found"))?;
            let user = self
                .stored_user_record(&deposit.user_id)
                .await?
                .ok_or_else(|| storage_error("UTXO-batch deposit user record is missing"))?;
            if &user.owner != owner {
                return Err(conflict(
                    "UTXO-batch deposit user belongs to another authenticated owner",
                ));
            }
            primary.get_or_insert_with(|| deposit.user_id.clone());
            associated.insert(deposit.user_id);
        }
        let primary = primary.ok_or_else(|| invalid("UTXO-batch job has no participant user"))?;
        Ok((primary, associated.into_iter().collect()))
    }

    async fn stored_database_metadata_with_value(
        &self,
    ) -> Result<Option<(PaymentDatabaseMetadata, StoredValue)>, DepositError> {
        self.storage()
            .get(&database_metadata_ns(), &database_metadata_key())
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let metadata = decode::<DatabaseMetadataRecordV1>(&stored)?.try_into()?;
                Ok((metadata, stored))
            })
            .transpose()
    }

    async fn stored_database_metadata(
        &self,
    ) -> Result<Option<PaymentDatabaseMetadata>, DepositError> {
        Ok(self
            .stored_database_metadata_with_value()
            .await?
            .map(|(metadata, _)| metadata))
    }

    async fn namespace_has_records(&self, namespace: Namespace) -> Result<bool, DepositError> {
        Ok(!self
            .storage()
            .scan(ScanRequest {
                namespace,
                prefix: Vec::new(),
                after: None,
                limit: 1,
            })
            .await
            .map_err(map_storage)?
            .entries
            .is_empty())
    }

    async fn has_unbound_ps_records(&self) -> Result<bool, DepositError> {
        // A missing owner record alongside any semantic PS state represents an
        // older database and must go through the explicit migration workflow.
        for namespace in [
            "ps.v1.deposit",
            "ps.v1.deposit_address",
            "ps.v1.deposit_idem",
            "ps.v1.awaiting_watch",
            "ps.v1.closed_deposit_watch",
            "ps.v1.user_deposit",
            "ps.v1.deposit_state",
            "ps.v1.user_deposit_state",
            "ps.v1.deposit_index_metadata",
            "ps.v1.ledger_head",
            "ps.v1.ledger_entry",
            "ps.v1.projection",
            "ps.v1.accounting_idem",
            "ps.v1.observation",
            "ps.v1.observation_cursor",
            "ps.v1.deposit_observation",
            "ps.v1.consumer_checkpoint",
            "ps.v1.reconciliation",
            "ps.v1.reconciliation_deposit",
            "ps.v1.reconciliation_resolution_idem",
            "ps.v1.reconciliation_generation",
            "ps.v1.user",
            "ps.v1.job",
            "ps.v1.command_job",
            "ps.v1.user_job",
            "ps.v1.resource_job",
            "ps.v1.ready_job",
            "ps.v1.collection",
            "ps.v1.collection_job",
            "ps.v1.deposit_collection",
            "ps.v1.active_collection_reservation",
            "ps.v1.collection_eligibility_generation",
            "ps.v1.collection_transaction",
            "ps.v1.signed_collection_envelope",
            "ps.v2.active_collection_spend_resource",
        ] {
            if self.namespace_has_records(ns(namespace)).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn migration_users(
        &self,
        page_size: usize,
    ) -> Result<Vec<User>, DepositError> {
        let mut after = None;
        let mut users = Vec::new();
        loop {
            let page = self
                .storage()
                .scan(ScanRequest {
                    namespace: user_ns(),
                    prefix: Vec::new(),
                    after: after.clone(),
                    limit: page_size,
                })
                .await
                .map_err(map_storage)?;
            for (key, stored) in page.entries {
                let user: User = decode::<UserRecordV1>(&stored)?.try_into()?;
                if key != key_text(&user.id.0) {
                    return Err(storage_error("user row key does not match its record ID"));
                }
                users.push(user);
            }
            let Some(next) = page.next else {
                break;
            };
            if Some(&next) == after.as_ref() {
                return Err(storage_error("user scan cursor did not advance"));
            }
            after = Some(next);
        }
        Ok(users)
    }

    pub(crate) async fn validate_migration_job_indexes(
        &self,
        jobs: &[Job],
    ) -> Result<(), DepositError> {
        for job in jobs {
            let indexed_command = self
                .idempotent_job(&job.command)
                .await?
                .ok_or_else(|| storage_error("job command idempotency index is missing"))?;
            if indexed_command != *job {
                return Err(storage_error(
                    "job command idempotency index points to another job",
                ));
            }
            if let Some(ready_at) = state_ready_at(job) {
                let stored = self
                    .storage()
                    .get(&ready_job_ns(), &ready_job_key(ready_at, &job.id))
                    .await
                    .map_err(map_storage)?
                    .ok_or_else(|| storage_error("ready-job index is missing"))?;
                let index: JobIndexRecordV1 = decode(&stored)?;
                ensure_version(index.version)?;
                if index.job_id != job.id.0 {
                    return Err(storage_error("ready-job index points to another job"));
                }
            }
        }
        Ok(())
    }

    async fn stored_user_record(&self, id: &UserId) -> Result<Option<User>, DepositError> {
        self.storage()
            .get(&user_ns(), &key_text(&id.0))
            .await
            .map_err(map_storage)?
            .map(|stored| decode::<UserRecordV1>(&stored)?.try_into())
            .transpose()
    }

    async fn stored_job_record(
        &self,
        id: &JobId,
    ) -> Result<Option<(Job, StoredValue)>, DepositError> {
        self.storage()
            .get(&job_ns(), &key_text(&id.0))
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: JobRecordV1 = decode(&stored)?;
                Ok((record.try_into()?, stored))
            })
            .transpose()
    }

    async fn idempotent_job(
        &self,
        identity: &CommandIdentity,
    ) -> Result<Option<Job>, DepositError> {
        let Some(stored) = self
            .storage()
            .get(&command_job_ns(), &command_key(identity)?)
            .await
            .map_err(map_storage)?
        else {
            return Ok(None);
        };
        let record: CommandJobRecordV1 = decode(&stored)?;
        ensure_version(record.version)?;
        let persisted_identity: CommandIdentity = record.command.into();
        if persisted_identity.principal != identity.principal
            || persisted_identity.operation != identity.operation
            || persisted_identity.client_key != identity.client_key
        {
            return Err(storage_error(
                "command idempotency index identity does not match its key",
            ));
        }
        if persisted_identity.request_hash != identity.request_hash {
            return Err(conflict(
                "command idempotency key was reused with a different request",
            ));
        }
        let job = self
            .stored_job_record(&JobId(record.job_id))
            .await?
            .map(|(job, _)| job)
            .ok_or_else(|| storage_error("command idempotency index points to a missing job"))?;
        if job.command != persisted_identity {
            return Err(storage_error(
                "command idempotency index points to a different job command",
            ));
        }
        Ok(Some(job))
    }

    async fn indexed_jobs(
        &self,
        namespace: Namespace,
        prefix: Vec<u8>,
        after: Option<Key>,
        limit: usize,
    ) -> Result<JobPage, DepositError> {
        let page = self
            .storage()
            .scan(ScanRequest {
                namespace,
                prefix,
                after,
                limit,
            })
            .await
            .map_err(map_storage)?;
        let has_next = page.next.is_some();
        let mut jobs = Vec::with_capacity(page.entries.len());
        for (_, stored) in page.entries {
            let index: JobIndexRecordV1 = decode(&stored)?;
            ensure_version(index.version)?;
            jobs.push(
                self.stored_job_record(&JobId(index.job_id))
                    .await?
                    .map(|(job, _)| job)
                    .ok_or_else(|| storage_error("job association index is dangling"))?,
            );
        }
        let next = has_next
            .then(|| jobs.last().map(|job| job.id.clone()))
            .flatten();
        Ok(JobPage { jobs, next })
    }

    async fn store_new_job(
        &self,
        job: &Job,
        associated_users: &[UserId],
        missing_users: &[UserId],
    ) -> Result<(), DepositError> {
        let job_key = key_text(&job.id.0);
        let command_key = command_key(&job.command)?;
        let resource_job_key = resource_job_key(&job.resource, &job.id)?;
        let ready_key = ready_job_key(job.created_at, &job.id);
        if associated_users.is_empty()
            || !associated_users.contains(&job.user_id)
            || associated_users
                .windows(2)
                .any(|users| users[0] >= users[1])
        {
            return Err(storage_error(
                "job associated users must be canonical and include the primary user",
            ));
        }
        let mut conditions = vec![
            Condition::Missing {
                namespace: job_ns(),
                key: job_key.clone(),
            },
            Condition::Missing {
                namespace: command_job_ns(),
                key: command_key.clone(),
            },
            Condition::Missing {
                namespace: resource_job_ns(),
                key: resource_job_key.clone(),
            },
            Condition::Missing {
                namespace: ready_job_ns(),
                key: ready_key.clone(),
            },
        ];
        let index = JobIndexRecordV1 {
            version: RECORD_VERSION,
            job_id: job.id.0.clone(),
        };
        let mut operations = vec![
            Operation::Put {
                namespace: job_ns(),
                key: job_key,
                value: encode(&JobRecordV1::from(job))?,
            },
            Operation::Put {
                namespace: command_job_ns(),
                key: command_key,
                value: encode(&CommandJobRecordV1 {
                    version: RECORD_VERSION,
                    command: CommandIdentityRecordV1::from(&job.command),
                    job_id: job.id.0.clone(),
                })?,
            },
            Operation::Put {
                namespace: resource_job_ns(),
                key: resource_job_key,
                value: encode(&index)?,
            },
            Operation::Put {
                namespace: ready_job_ns(),
                key: ready_key,
                value: encode(&index)?,
            },
        ];
        for user_id in associated_users {
            let key = user_job_key(user_id, &job.id)?;
            conditions.push(Condition::Missing {
                namespace: user_job_ns(),
                key: key.clone(),
            });
            operations.push(Operation::Put {
                namespace: user_job_ns(),
                key,
                value: encode(&index)?,
            });
        }
        for user_id in missing_users {
            if !associated_users.contains(user_id) {
                return Err(storage_error(
                    "missing job user is not one of the associated users",
                ));
            }
            let user = User {
                id: user_id.clone(),
                owner: job.user_owner.clone(),
                first_seen_at: job.created_at,
            };
            conditions.push(Condition::Missing {
                namespace: user_ns(),
                key: key_text(&user.id.0),
            });
            operations.push(Operation::Put {
                namespace: user_ns(),
                key: key_text(&user.id.0),
                value: encode(&UserRecordV1::from(&user))?,
            });
        }
        self.storage()
            .commit(WriteBatch {
                conditions,
                operations,
            })
            .await
            .map_err(map_storage)?;
        Ok(())
    }

    async fn update_job_state(
        &self,
        current: &Job,
        stored: &StoredValue,
        next: &Job,
    ) -> Result<(), DepositError> {
        let mut conditions = vec![Condition::Version {
            namespace: job_ns(),
            key: key_text(&current.id.0),
            expected: stored.version,
        }];
        let mut operations = Vec::new();
        let current_ready_key =
            state_ready_at(current).map(|ready_at| ready_job_key(ready_at, &current.id));
        if let Some(key) = &current_ready_key {
            operations.push(Operation::Delete {
                namespace: ready_job_ns(),
                key: key.clone(),
            });
        }
        if let Some(ready_at) = state_ready_at(next) {
            let key = ready_job_key(ready_at, &next.id);
            if current_ready_key.as_ref() != Some(&key) {
                conditions.push(Condition::Missing {
                    namespace: ready_job_ns(),
                    key: key.clone(),
                });
            }
            operations.push(Operation::Put {
                namespace: ready_job_ns(),
                key,
                value: encode(&JobIndexRecordV1 {
                    version: RECORD_VERSION,
                    job_id: next.id.0.clone(),
                })?,
            });
        }
        operations.push(Operation::Put {
            namespace: job_ns(),
            key: key_text(&next.id.0),
            value: encode(&JobRecordV1::from(next))?,
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

impl<S> PaymentDatabaseMetadataStore for PersistentPaymentRepository<S>
where
    S: Storage,
{
    fn initialize_or_validate<'a>(
        &'a self,
        command: InitializePaymentDatabase,
    ) -> BoxFuture<'a, Result<PaymentDatabaseMetadata, DepositError>> {
        Box::pin(async move {
            validate_metadata_command(&command)?;
            let expected = expected_metadata(command);
            // Bound PS metadata does not make a mixed-owner database safe.
            // Re-check IX ownership on every startup before the metadata fast path.
            if self.namespace_has_records(ix_semantic_ns()).await? {
                return Err(conflict(
                    "database contains Indexer Service records and cannot be owned by Payment Service",
                ));
            }
            if let Some(persisted) = self.stored_database_metadata().await? {
                return validate_persisted_metadata(persisted, &expected);
            }
            if self.has_unbound_ps_records().await? {
                return Err(conflict(
                    "existing Payment Service records require explicit metadata migration",
                ));
            }
            let commit = self
                .storage()
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: database_metadata_ns(),
                            key: database_metadata_key(),
                        },
                        // IX writes this path-global identity key with its first
                        // semantic mutation. Refuse an already-owned IX path in
                        // the same atomic check as PS initialization.
                        Condition::Missing {
                            namespace: ix_semantic_ns(),
                            key: Key(vec![1, 1]),
                        },
                    ],
                    operations: vec![Operation::Put {
                        namespace: database_metadata_ns(),
                        key: database_metadata_key(),
                        value: encode(&DatabaseMetadataRecordV1::from(&expected))?,
                    }],
                })
                .await
                .map_err(map_storage);
            match commit {
                Ok(_) => Ok(expected),
                Err(error) if error.kind == DepositErrorKind::Conflict => self
                    .stored_database_metadata()
                    .await?
                    .ok_or(error)
                    .and_then(|persisted| validate_persisted_metadata(persisted, &expected)),
                Err(error) => Err(error),
            }
        })
    }

    fn database_metadata(
        &self,
    ) -> BoxFuture<'_, Result<Option<PaymentDatabaseMetadata>, DepositError>> {
        Box::pin(async move { self.stored_database_metadata().await })
    }

    fn migrate_and_bind<'a>(
        &'a self,
        command: MigratePaymentDatabase,
    ) -> BoxFuture<'a, Result<PaymentDatabaseMigrationReport, DepositError>> {
        Box::pin(async move {
            validate_migration_command(&command)?;
            let stored_metadata = self.stored_database_metadata_with_value().await?;
            let previous_domain_schema_version = stored_metadata
                .as_ref()
                .map(|(metadata, _)| metadata.domain_schema_version);

            if let Some((metadata, _)) = &stored_metadata {
                if metadata.service_owner != PAYMENT_SERVICE_OWNER {
                    return Err(conflict(format!(
                        "database is owned by {}, not Payment Service",
                        metadata.service_owner
                    )));
                }
                if metadata.domain_schema_version > PAYMENT_DOMAIN_SCHEMA_VERSION {
                    return Err(conflict(format!(
                        "Payment Service domain schema version {} is newer than runtime version {}",
                        metadata.domain_schema_version, PAYMENT_DOMAIN_SCHEMA_VERSION
                    )));
                }
                if metadata.domain_schema_version == 0 {
                    return Err(conflict(
                        "Payment Service domain schema version 0 is not a supported legacy schema",
                    ));
                }
                if metadata.scope != command.scope {
                    return Err(conflict(
                        "Payment Service database is bound to a different Indexer scope",
                    ));
                }
                if metadata.domain_schema_version == PAYMENT_DOMAIN_SCHEMA_VERSION
                    && metadata.active_policy != command.active_policy
                {
                    return Err(conflict(
                        "current Payment Service metadata cannot be silently rebound to another policy",
                    ));
                }
            }

            if self.namespace_has_records(ix_semantic_ns()).await? {
                return Err(conflict(
                    "database contains Indexer Service records and cannot be owned by Payment Service",
                ));
            }

            let validation =
                crate::migration::validate_and_rebuild(self, &command.scope, command.page_size)
                    .await?;
            let expected = PaymentDatabaseMetadata {
                service_owner: PAYMENT_SERVICE_OWNER.to_owned(),
                domain_schema_version: PAYMENT_DOMAIN_SCHEMA_VERSION,
                scope: command.scope,
                active_policy: command.active_policy,
                initialized_at: stored_metadata
                    .as_ref()
                    .map_or(command.migrated_at, |(metadata, _)| metadata.initialized_at),
            };

            let metadata_condition = match &stored_metadata {
                Some((_, stored)) => Condition::Version {
                    namespace: database_metadata_ns(),
                    key: database_metadata_key(),
                    expected: stored.version,
                },
                None => Condition::Missing {
                    namespace: database_metadata_ns(),
                    key: database_metadata_key(),
                },
            };
            self.storage()
                .commit(WriteBatch {
                    conditions: vec![
                        metadata_condition,
                        Condition::Missing {
                            namespace: ix_semantic_ns(),
                            key: Key(vec![1, 1]),
                        },
                    ],
                    operations: vec![Operation::Put {
                        namespace: database_metadata_ns(),
                        key: database_metadata_key(),
                        value: encode(&DatabaseMetadataRecordV1::from(&expected))?,
                    }],
                })
                .await
                .map_err(map_storage)?;

            Ok(PaymentDatabaseMigrationReport {
                metadata: expected,
                previous_domain_schema_version,
                deposits: validation.deposits,
                ledger_entries: validation.ledger_entries,
                mirrored_observations: validation.mirrored_observations,
                deposit_observations: validation.deposit_observations,
                reconciliation_cases: validation.reconciliation_cases,
                users: validation.users,
                jobs: validation.jobs,
                collections: validation.collections,
                deposit_indexes_rebuilt: validation.deposit_indexes_rebuilt,
            })
        })
    }
}

impl<S> UserStore for PersistentPaymentRepository<S>
where
    S: Storage,
{
    fn ensure_user<'a>(&'a self, command: EnsureUser) -> BoxFuture<'a, Result<User, DepositError>> {
        Box::pin(async move {
            validate_non_empty(&command.id.0, "user ID")?;
            if let Some(user) = self.stored_user_record(&command.id).await? {
                if user.owner != command.owner {
                    return Err(conflict(
                        "opaque user ID is already owned by another authenticated principal",
                    ));
                }
                return Ok(user);
            }
            let user = User {
                id: command.id,
                owner: command.owner,
                first_seen_at: command.first_seen_at,
            };
            match self
                .storage()
                .commit(WriteBatch {
                    conditions: vec![Condition::Missing {
                        namespace: user_ns(),
                        key: key_text(&user.id.0),
                    }],
                    operations: vec![Operation::Put {
                        namespace: user_ns(),
                        key: key_text(&user.id.0),
                        value: encode(&UserRecordV1::from(&user))?,
                    }],
                })
                .await
                .map_err(map_storage)
            {
                Ok(_) => Ok(user),
                Err(error) if error.kind == DepositErrorKind::Conflict => {
                    self.stored_user_record(&user.id).await?.ok_or(error)
                }
                Err(error) => Err(error),
            }
        })
    }

    fn user<'a>(&'a self, id: &'a UserId) -> BoxFuture<'a, Result<Option<User>, DepositError>> {
        Box::pin(async move { self.stored_user_record(id).await })
    }
}

impl<S> JobStore for PersistentPaymentRepository<S>
where
    S: Storage,
{
    fn job_for_command<'a>(
        &'a self,
        command: &'a CommandIdentity,
    ) -> BoxFuture<'a, Result<Option<Job>, DepositError>> {
        Box::pin(async move { self.idempotent_job(command).await })
    }

    fn create_or_replay<'a>(
        &'a self,
        command: CreateJob,
    ) -> BoxFuture<'a, Result<CreateJobOutcome, DepositError>> {
        Box::pin(async move {
            validate_create(&command)?;
            if let Some(job) = self.idempotent_job(&command.command).await? {
                if job.user_owner != command.user_owner {
                    return Err(conflict(
                        "command replay supplied a different opaque user owner",
                    ));
                }
                return Ok(CreateJobOutcome::Replayed { job });
            }
            if let Some(metadata) = self.stored_database_metadata().await?
                && metadata.active_policy != command.policy
            {
                return Err(conflict(
                    "job policy does not match the database active policy",
                ));
            }
            let (primary_user, associated_users) = self
                .resolve_job_users(&command.payload, &command.user_owner)
                .await?;
            let job = Job {
                id: command.id,
                command: command.command,
                kind: command.payload.kind(),
                resource: command.payload.resource(),
                user_id: primary_user,
                user_owner: command.user_owner,
                policy: command.policy,
                payload: command.payload,
                state: JobState::Queued,
                attempt_count: 0,
                last_error: None,
                created_at: command.created_at,
                updated_at: command.created_at,
            };

            // A different command may concurrently create the same opaque user.
            // One retry lets that harmless race settle without weakening any job
            // or command uniqueness condition.
            for attempt in 0..2 {
                let mut missing_users = Vec::new();
                for user_id in &associated_users {
                    match self.stored_user_record(user_id).await? {
                        Some(user) if user.owner != job.user_owner => {
                            return Err(conflict(
                                "opaque user ID is already owned by another authenticated principal",
                            ));
                        }
                        Some(_) => {}
                        None => missing_users.push(user_id.clone()),
                    }
                }
                match self
                    .store_new_job(&job, &associated_users, &missing_users)
                    .await
                {
                    Ok(()) => return Ok(CreateJobOutcome::Created { job }),
                    Err(error) if error.kind == DepositErrorKind::Conflict => {
                        if let Some(existing) = self.idempotent_job(&job.command).await? {
                            if existing.user_owner != job.user_owner {
                                return Err(conflict(
                                    "command replay supplied a different opaque user owner",
                                ));
                            }
                            return Ok(CreateJobOutcome::Replayed { job: existing });
                        }
                        if attempt == 1 {
                            return Err(error);
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(storage_error("job creation retry was exhausted"))
        })
    }

    fn job<'a>(&'a self, id: &'a JobId) -> BoxFuture<'a, Result<Option<Job>, DepositError>> {
        Box::pin(async move { Ok(self.stored_job_record(id).await?.map(|(job, _)| job)) })
    }

    fn jobs<'a>(&'a self, request: JobPageRequest) -> BoxFuture<'a, Result<JobPage, DepositError>> {
        Box::pin(async move {
            validate_page(&request)?;
            let page = self
                .storage()
                .scan(ScanRequest {
                    namespace: job_ns(),
                    prefix: Vec::new(),
                    after: request.after.as_ref().map(|id| key_text(&id.0)),
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let has_next = page.next.is_some();
            let jobs = page
                .entries
                .into_iter()
                .map(|(_, stored)| decode::<JobRecordV1>(&stored)?.try_into())
                .collect::<Result<Vec<Job>, DepositError>>()?;
            let next = has_next
                .then(|| jobs.last().map(|job| job.id.clone()))
                .flatten();
            Ok(JobPage { jobs, next })
        })
    }

    fn jobs_for_user<'a>(
        &'a self,
        user_id: &'a UserId,
        request: JobPageRequest,
    ) -> BoxFuture<'a, Result<JobPage, DepositError>> {
        Box::pin(async move {
            validate_page(&request)?;
            self.indexed_jobs(
                user_job_ns(),
                user_job_prefix(user_id)?,
                request
                    .after
                    .as_ref()
                    .map(|id| user_job_key(user_id, id))
                    .transpose()?,
                request.limit,
            )
            .await
        })
    }

    fn jobs_for_resource<'a>(
        &'a self,
        resource: &'a JobResource,
        request: JobPageRequest,
    ) -> BoxFuture<'a, Result<JobPage, DepositError>> {
        Box::pin(async move {
            validate_page(&request)?;
            self.indexed_jobs(
                resource_job_ns(),
                resource_job_prefix(resource)?,
                request
                    .after
                    .as_ref()
                    .map(|id| resource_job_key(resource, id))
                    .transpose()?,
                request.limit,
            )
            .await
        })
    }

    fn claim_next<'a>(
        &'a self,
        command: ClaimJob,
    ) -> BoxFuture<'a, Result<Option<Job>, DepositError>> {
        Box::pin(async move {
            if command.scan_limit == 0 || command.scan_limit > MAX_PAGE_SIZE {
                return Err(invalid("job claim scan limit must be between 1 and 1000"));
            }
            if command.lease_expires_at <= command.now {
                return Err(invalid("job lease must expire after the claim time"));
            }
            let page = self
                .storage()
                .scan(ScanRequest {
                    namespace: ready_job_ns(),
                    prefix: Vec::new(),
                    after: None,
                    limit: command.scan_limit,
                })
                .await
                .map_err(map_storage)?;
            for (ready_key, index_stored) in page.entries {
                let ready_at = ready_at_from_key(&ready_key)?;
                if ready_at > command.now {
                    break;
                }
                let index: JobIndexRecordV1 = decode(&index_stored)?;
                ensure_version(index.version)?;
                let Some((current, stored)) = self.stored_job_record(&JobId(index.job_id)).await?
                else {
                    return Err(storage_error("ready-job index is dangling"));
                };
                if state_ready_at(&current) != Some(ready_at) {
                    return Err(storage_error(
                        "ready-job index does not match the durable job state",
                    ));
                }
                let attempt_count = current
                    .attempt_count
                    .checked_add(1)
                    .ok_or_else(|| invalid("job attempt counter overflowed"))?;
                let mut claimed = current.clone();
                claimed.state = JobState::Running {
                    lease_expires_at: command.lease_expires_at,
                };
                claimed.attempt_count = attempt_count;
                claimed.updated_at = command.now;
                match self.update_job_state(&current, &stored, &claimed).await {
                    Ok(()) => return Ok(Some(claimed)),
                    Err(error) if error.kind == DepositErrorKind::Conflict => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(None)
        })
    }

    fn transition<'a>(
        &'a self,
        command: TransitionJob,
    ) -> BoxFuture<'a, Result<Job, DepositError>> {
        Box::pin(async move {
            let (current, stored) = self
                .stored_job_record(&command.id)
                .await?
                .ok_or_else(|| not_found("job does not exist"))?;
            if current.state != command.expected_state {
                return Err(conflict("job expected state does not match current state"));
            }
            if command.updated_at < current.updated_at {
                return Err(invalid("job transition time moved backwards"));
            }
            if !matches!(&current.state, JobState::Running { .. })
                || !matches!(
                    &command.next_state,
                    JobState::WaitingRetry { .. } | JobState::Succeeded | JobState::Failed
                )
            {
                return Err(invalid_state("job lifecycle transition is not allowed"));
            }
            let last_error = match &command.next_state {
                JobState::WaitingRetry { next_attempt_at } => {
                    if *next_attempt_at < command.updated_at {
                        return Err(invalid("job retry time precedes its transition"));
                    }
                    let error = command
                        .error
                        .as_ref()
                        .ok_or_else(|| invalid("waiting-retry job requires a safe error"))?;
                    validate_job_error(error)?;
                    if !error.retryable {
                        return Err(invalid("waiting-retry job error must be retryable"));
                    }
                    Some(error.clone())
                }
                JobState::Failed => {
                    let error = command
                        .error
                        .as_ref()
                        .ok_or_else(|| invalid("failed job requires a safe error"))?;
                    validate_job_error(error)?;
                    Some(error.clone())
                }
                JobState::Succeeded => {
                    if command.error.is_some() {
                        return Err(invalid("succeeded job cannot retain an error"));
                    }
                    None
                }
                JobState::Queued | JobState::Running { .. } => {
                    return Err(invalid_state("job lifecycle transition is not allowed"));
                }
            };
            let mut next = current.clone();
            next.state = command.next_state;
            next.last_error = last_error;
            next.updated_at = command.updated_at;
            self.update_job_state(&current, &stored, &next).await?;
            Ok(next)
        })
    }
}
