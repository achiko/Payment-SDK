use super::*;

#[derive(Clone, Copy, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum KindRecord {
    DepositPlan,
    CloseDeposit,
    CollectionPlan,
    RetryCollection,
}

impl From<JobKind> for KindRecord {
    fn from(value: JobKind) -> Self {
        match value {
            JobKind::DepositPlan => Self::DepositPlan,
            JobKind::CloseDeposit => Self::CloseDeposit,
            JobKind::CollectionPlan => Self::CollectionPlan,
            JobKind::RetryCollection => Self::RetryCollection,
        }
    }
}

impl From<KindRecord> for JobKind {
    fn from(value: KindRecord) -> Self {
        match value {
            KindRecord::DepositPlan => Self::DepositPlan,
            KindRecord::CloseDeposit => Self::CloseDeposit,
            KindRecord::CollectionPlan => Self::CollectionPlan,
            KindRecord::RetryCollection => Self::RetryCollection,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct ScopeRecord {
    chain: String,
    network: String,
}

impl From<&IndexScope> for ScopeRecord {
    fn from(value: &IndexScope) -> Self {
        Self {
            chain: value.chain.0.clone(),
            network: value.network.clone(),
        }
    }
}

impl From<ScopeRecord> for IndexScope {
    fn from(value: ScopeRecord) -> Self {
        Self {
            chain: ChainId(value.chain),
            network: value.network,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct AssetRecord {
    chain: String,
    asset: String,
}

impl From<&AssetId> for AssetRecord {
    fn from(value: &AssetId) -> Self {
        Self {
            chain: value.chain.0.clone(),
            asset: value.asset.clone(),
        }
    }
}

impl From<AssetRecord> for AssetId {
    fn from(value: AssetRecord) -> Self {
        Self {
            chain: ChainId(value.chain),
            asset: value.asset,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum PayloadRecord {
    DepositPlan {
        deposit_id: String,
        user_id: String,
        scope: ScopeRecord,
        asset: AssetRecord,
        expected: [u8; 32],
        expires_at: u64,
        created_at: u64,
        key_purpose: String,
    },
    CloseDeposit {
        deposit_id: String,
        user_id: String,
    },
    CollectionPlan {
        collection_id: String,
        deposit_id: String,
        user_id: String,
    },
    RetryCollection {
        collection_id: String,
        deposit_id: String,
        user_id: String,
    },
    CreateBatch {
        collection_id: String,
        deposit_ids: Vec<String>,
    },
    RetryUtxoBatchCollection {
        collection_id: String,
        deposit_ids: Vec<String>,
    },
}

impl From<&JobPayload> for PayloadRecord {
    fn from(value: &JobPayload) -> Self {
        match value {
            JobPayload::DepositPlan(payload) => Self::DepositPlan {
                deposit_id: payload.deposit_id.0.clone(),
                user_id: payload.user_id.0.clone(),
                scope: ScopeRecord::from(&payload.scope),
                asset: AssetRecord::from(&payload.asset),
                expected: crate::amount::record_bytes(&payload.expected),
                expires_at: payload.expires_at,
                created_at: payload.created_at,
                key_purpose: payload.key_purpose.clone(),
            },
            JobPayload::CloseDeposit(payload) => Self::CloseDeposit {
                deposit_id: payload.deposit_id.0.clone(),
                user_id: payload.user_id.0.clone(),
            },
            JobPayload::CollectionPlan(payload) => Self::CollectionPlan {
                collection_id: payload.collection_id.0.clone(),
                deposit_id: payload.deposit_id.0.clone(),
                user_id: payload.user_id.0.clone(),
            },
            JobPayload::RetryCollection(payload) => Self::RetryCollection {
                collection_id: payload.collection_id.0.clone(),
                deposit_id: payload.deposit_id.0.clone(),
                user_id: payload.user_id.0.clone(),
            },
            JobPayload::CreateBatch(payload) => Self::CreateBatch {
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

impl From<PayloadRecord> for JobPayload {
    fn from(value: PayloadRecord) -> Self {
        match value {
            PayloadRecord::DepositPlan {
                deposit_id,
                user_id,
                scope,
                asset,
                expected,
                expires_at,
                created_at,
                key_purpose,
            } => Self::DepositPlan(DepositJob {
                deposit_id: DepositId(deposit_id),
                user_id: UserId(user_id),
                scope: scope.into(),
                asset: asset.into(),
                expected: crate::amount::from_bytes(expected),
                expires_at,
                created_at,
                key_purpose,
            }),
            PayloadRecord::CloseDeposit {
                deposit_id,
                user_id,
            } => Self::CloseDeposit(CloseJob {
                deposit_id: DepositId(deposit_id),
                user_id: UserId(user_id),
            }),
            PayloadRecord::CollectionPlan {
                collection_id,
                deposit_id,
                user_id,
            } => Self::CollectionPlan(CollectionJob {
                collection_id: CollectionId(collection_id),
                deposit_id: DepositId(deposit_id),
                user_id: UserId(user_id),
            }),
            PayloadRecord::RetryCollection {
                collection_id,
                deposit_id,
                user_id,
            } => Self::RetryCollection(RetryJob {
                collection_id: CollectionId(collection_id),
                deposit_id: DepositId(deposit_id),
                user_id: UserId(user_id),
            }),
            PayloadRecord::CreateBatch {
                collection_id,
                deposit_ids,
            } => Self::CreateBatch(BatchJob {
                collection_id: CollectionId(collection_id),
                deposit_ids: deposit_ids.into_iter().map(DepositId).collect(),
            }),
            PayloadRecord::RetryUtxoBatchCollection {
                collection_id,
                deposit_ids,
            } => Self::RetryUtxoBatchCollection(RetryBatch {
                collection_id: CollectionId(collection_id),
                deposit_ids: deposit_ids.into_iter().map(DepositId).collect(),
            }),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum ResourceRecord {
    Deposit(String),
    Collection(String),
}

impl From<&JobResource> for ResourceRecord {
    fn from(value: &JobResource) -> Self {
        match value {
            JobResource::Deposit(id) => Self::Deposit(id.0.clone()),
            JobResource::Collection(id) => Self::Collection(id.0.clone()),
        }
    }
}

impl From<ResourceRecord> for JobResource {
    fn from(value: ResourceRecord) -> Self {
        match value {
            ResourceRecord::Deposit(id) => Self::Deposit(DepositId(id)),
            ResourceRecord::Collection(id) => Self::Collection(CollectionId(id)),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum StateRecord {
    Queued,
    Running { lease_expires_at: u64 },
    WaitingRetry { next_attempt_at: u64 },
    Succeeded,
    Failed,
}

impl From<&JobState> for StateRecord {
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

impl From<StateRecord> for JobState {
    fn from(value: StateRecord) -> Self {
        match value {
            StateRecord::Queued => Self::Queued,
            StateRecord::Running { lease_expires_at } => Self::Running { lease_expires_at },
            StateRecord::WaitingRetry { next_attempt_at } => Self::WaitingRetry { next_attempt_at },
            StateRecord::Succeeded => Self::Succeeded,
            StateRecord::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct ErrorRecord {
    code: String,
    message: String,
    retryable: bool,
}

impl From<&JobError> for ErrorRecord {
    fn from(value: &JobError) -> Self {
        Self {
            code: value.code.clone(),
            message: value.message.clone(),
            retryable: value.retryable,
        }
    }
}

impl From<ErrorRecord> for JobError {
    fn from(value: ErrorRecord) -> Self {
        Self {
            code: value.code,
            message: value.message,
            retryable: value.retryable,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct JobRecord {
    version: u16,
    id: String,
    command: CommandRecord,
    kind: KindRecord,
    payload: PayloadRecord,
    resource: ResourceRecord,
    user_id: String,
    user_owner: String,
    policy_version: String,
    policy_digest: [u8; 32],
    state: StateRecord,
    attempt_count: u32,
    last_error: Option<ErrorRecord>,
    created_at: u64,
    updated_at: u64,
}

impl From<&Job> for JobRecord {
    fn from(value: &Job) -> Self {
        Self {
            version: RECORD_VERSION,
            id: value.id.0.clone(),
            command: CommandRecord::from(&value.command),
            kind: value.kind.into(),
            payload: PayloadRecord::from(&value.payload),
            resource: ResourceRecord::from(&value.resource),
            user_id: value.user_id.0.clone(),
            user_owner: value.user_owner.0.clone(),
            policy_version: value.policy.version.clone(),
            policy_digest: value.policy.digest,
            state: StateRecord::from(&value.state),
            attempt_count: value.attempt_count,
            last_error: value.last_error.as_ref().map(ErrorRecord::from),
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

impl TryFrom<JobRecord> for Job {
    type Error = DepositError;

    fn try_from(value: JobRecord) -> Result<Self, Self::Error> {
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
pub(super) struct CommandIndex {
    pub(super) version: u16,
    pub(super) command: CommandRecord,
    pub(super) job_id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct JobIndex {
    pub(super) version: u16,
    pub(super) job_id: String,
}
