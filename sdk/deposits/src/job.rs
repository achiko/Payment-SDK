use base::Decimal;
use indexing::AssetId;
use indexing::IndexScope;

use crate::{
    BoxFuture, CollectionId, DepositError, DepositId, IdempotencyKey, PolicyIdentity, UserId,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandPrincipal(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CommandOperation {
    DepositPlan,
    CloseDeposit,
    CollectionPlan,
    RetryCollection,
    Accounting,
    ResolveReconciliation,
}

/// Hash of the canonical authenticated request body and semantic parameters.
/// Hashing is an application-boundary responsibility; persistence treats the
/// bytes as an opaque equality token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestHash(pub [u8; 32]);

/// A command is unique within its authenticated principal and operation. The
/// request hash detects accidental reuse of a client key for different work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandIdentity {
    pub principal: CommandPrincipal,
    pub operation: CommandOperation,
    pub client_key: IdempotencyKey,
    pub request_hash: RequestHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    DepositPlan,
    CloseDeposit,
    CollectionPlan,
    RetryCollection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositJob {
    pub deposit_id: DepositId,
    pub user_id: UserId,
    pub scope: IndexScope,
    pub asset: AssetId,
    pub expected: Decimal,
    pub expires_at: u64,
    pub created_at: u64,
    /// Opaque custody/provisioning purpose metadata. Never secret material.
    pub key_purpose: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseJob {
    pub deposit_id: DepositId,
    pub user_id: UserId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionJob {
    pub collection_id: CollectionId,
    pub deposit_id: DepositId,
    pub user_id: UserId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryJob {
    pub collection_id: CollectionId,
    pub deposit_id: DepositId,
    pub user_id: UserId,
}

/// Durable job payload for creating one multi-deposit UTXO collection. Deposit
/// IDs are strictly canonical and unique; their actual users are resolved from
/// durable deposit records under the job's common authenticated owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchJob {
    pub collection_id: CollectionId,
    pub deposit_ids: Vec<DepositId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryBatch {
    pub collection_id: CollectionId,
    pub deposit_ids: Vec<DepositId>,
}

/// Typed durable payload retained so a worker can resume after a PS restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobPayload {
    DepositPlan(DepositJob),
    CloseDeposit(CloseJob),
    CollectionPlan(CollectionJob),
    RetryCollection(RetryJob),
    CreateBatch(BatchJob),
    RetryUtxoBatchCollection(RetryBatch),
}

impl JobPayload {
    #[must_use]
    pub const fn kind(&self) -> JobKind {
        match self {
            Self::DepositPlan(_) => JobKind::DepositPlan,
            Self::CloseDeposit(_) => JobKind::CloseDeposit,
            Self::CollectionPlan(_) => JobKind::CollectionPlan,
            Self::RetryCollection(_) => JobKind::RetryCollection,
            Self::CreateBatch(_) => JobKind::CollectionPlan,
            Self::RetryUtxoBatchCollection(_) => JobKind::RetryCollection,
        }
    }

    #[must_use]
    pub const fn operation(&self) -> CommandOperation {
        match self {
            Self::DepositPlan(_) => CommandOperation::DepositPlan,
            Self::CloseDeposit(_) => CommandOperation::CloseDeposit,
            Self::CollectionPlan(_) => CommandOperation::CollectionPlan,
            Self::RetryCollection(_) => CommandOperation::RetryCollection,
            Self::CreateBatch(_) => CommandOperation::CollectionPlan,
            Self::RetryUtxoBatchCollection(_) => CommandOperation::RetryCollection,
        }
    }

    #[must_use]
    pub fn user_id(&self) -> Option<&UserId> {
        match self {
            Self::DepositPlan(payload) => Some(&payload.user_id),
            Self::CloseDeposit(payload) => Some(&payload.user_id),
            Self::CollectionPlan(payload) => Some(&payload.user_id),
            Self::RetryCollection(payload) => Some(&payload.user_id),
            Self::CreateBatch(_) | Self::RetryUtxoBatchCollection(_) => None,
        }
    }

    #[must_use]
    pub fn deposit_ids(&self) -> Option<&[DepositId]> {
        match self {
            Self::CreateBatch(payload) => Some(&payload.deposit_ids),
            Self::RetryUtxoBatchCollection(payload) => Some(&payload.deposit_ids),
            Self::DepositPlan(_)
            | Self::CloseDeposit(_)
            | Self::CollectionPlan(_)
            | Self::RetryCollection(_) => None,
        }
    }

    #[must_use]
    pub fn resource(&self) -> JobResource {
        match self {
            Self::DepositPlan(payload) => JobResource::Deposit(payload.deposit_id.clone()),
            Self::CloseDeposit(payload) => JobResource::Deposit(payload.deposit_id.clone()),
            Self::CollectionPlan(payload) => JobResource::Collection(payload.collection_id.clone()),
            Self::RetryCollection(payload) => {
                JobResource::Collection(payload.collection_id.clone())
            }
            Self::CreateBatch(payload) => JobResource::Collection(payload.collection_id.clone()),
            Self::RetryUtxoBatchCollection(payload) => {
                JobResource::Collection(payload.collection_id.clone())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JobResource {
    Deposit(DepositId),
    Collection(CollectionId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStateKind {
    Queued,
    Running,
    WaitingRetry,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    /// A singleton worker owns the job until this lease expires. An expired
    /// lease can be claimed again after process failure.
    Running {
        lease_expires_at: u64,
    },
    WaitingRetry {
        next_attempt_at: u64,
    },
    Succeeded,
    Failed,
}

impl JobState {
    #[must_use]
    pub const fn kind(&self) -> JobStateKind {
        match self {
            Self::Queued => JobStateKind::Queued,
            Self::Running { .. } => JobStateKind::Running,
            Self::WaitingRetry { .. } => JobStateKind::WaitingRetry,
            Self::Succeeded => JobStateKind::Succeeded,
            Self::Failed => JobStateKind::Failed,
        }
    }
}

/// Safe diagnostic data suitable for an authenticated job-status response.
/// Dependency credentials, signed envelopes, and custody material must never
/// be placed here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: JobId,
    pub command: CommandIdentity,
    pub kind: JobKind,
    pub payload: JobPayload,
    pub resource: JobResource,
    pub user_id: UserId,
    /// Owner of the opaque user association. This is deliberately independent
    /// from the credential principal that submitted the command.
    pub user_owner: CommandPrincipal,
    pub policy: PolicyIdentity,
    pub state: JobState,
    pub attempt_count: u32,
    pub last_error: Option<JobError>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobPlan {
    /// Server-generated after an explicit replay lookup misses and before the
    /// first persistence attempt. The create path still resolves a concurrent
    /// replay race to the already-persisted ID.
    pub id: JobId,
    pub command: CommandIdentity,
    pub payload: JobPayload,
    /// Expected durable owner of every payload-associated user. For a UTXO
    /// batch these users are resolved from its durable deposits.
    pub user_owner: CommandPrincipal,
    pub policy: PolicyIdentity,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateJobOutcome {
    Created { job: Job },
    Replayed { job: Job },
}

impl CreateJobOutcome {
    #[must_use]
    pub const fn job(&self) -> &Job {
        match self {
            Self::Created { job } | Self::Replayed { job } => job,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionJob {
    pub id: JobId,
    pub expected_state: JobState,
    pub next_state: JobState,
    /// Required for `WaitingRetry` and `Failed`; forbidden for success.
    pub error: Option<JobError>,
    pub updated_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimJob {
    pub now: u64,
    pub lease_expires_at: u64,
    /// Bounds stale-index inspection in one worker tick.
    pub scan_limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobQuery {
    pub after: Option<JobId>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobPage {
    pub jobs: Vec<Job>,
    pub next: Option<JobId>,
}

/// Durable job repository. IDs are generated outside this contract, while
/// create-or-replay guarantees that one business command keeps one stable ID.
pub trait JobCommands: Send + Sync {
    /// Looks up an already-accepted command before a caller generates fresh
    /// server IDs or performs any external provisioning side effect. Reuse of
    /// the scoped client key with another request hash is a conflict.
    fn job_for_command<'a>(
        &'a self,
        command: &'a CommandIdentity,
    ) -> BoxFuture<'a, Result<Option<Job>, DepositError>>;

    fn create_or_replay<'a>(
        &'a self,
        command: JobPlan,
    ) -> BoxFuture<'a, Result<CreateJobOutcome, DepositError>>;
}

pub trait JobReader: Send + Sync {
    fn job<'a>(&'a self, id: &'a JobId) -> BoxFuture<'a, Result<Option<Job>, DepositError>>;

    fn jobs<'a>(&'a self, request: JobQuery) -> BoxFuture<'a, Result<JobPage, DepositError>>;
}

pub trait JobAssociations: Send + Sync {
    fn jobs_for_user<'a>(
        &'a self,
        user_id: &'a UserId,
        request: JobQuery,
    ) -> BoxFuture<'a, Result<JobPage, DepositError>>;

    fn jobs_for_resource<'a>(
        &'a self,
        resource: &'a JobResource,
        request: JobQuery,
    ) -> BoxFuture<'a, Result<JobPage, DepositError>>;
}

pub trait JobRunner: Send + Sync {
    /// Claims the oldest due queued/retry job, or reclaims an expired running
    /// lease. Every successful claim increments `attempt_count` atomically.
    fn claim_next<'a>(
        &'a self,
        command: ClaimJob,
    ) -> BoxFuture<'a, Result<Option<Job>, DepositError>>;

    /// Performs an optimistic lifecycle transition. A stale expected state is
    /// a conflict and never overwrites the winner.
    fn transition<'a>(&'a self, command: TransitionJob)
    -> BoxFuture<'a, Result<Job, DepositError>>;
}

pub trait Jobs: JobCommands + JobReader + JobAssociations + JobRunner {}

impl<T> Jobs for T where T: JobCommands + JobReader + JobAssociations + JobRunner {}
