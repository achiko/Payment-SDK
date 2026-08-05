use chain_identity::{AssetId, AtomicAmount};
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
    CreateDeposit,
    CloseDeposit,
    CreateCollection,
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
    CreateDeposit,
    CloseDeposit,
    CreateCollection,
    RetryCollection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDepositJob {
    pub deposit_id: DepositId,
    pub user_id: UserId,
    pub scope: IndexScope,
    pub asset: AssetId,
    pub expected: AtomicAmount,
    pub expires_at: u64,
    pub created_at: u64,
    /// Opaque custody/provisioning purpose metadata. Never secret material.
    pub key_purpose: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseDepositJob {
    pub deposit_id: DepositId,
    pub user_id: UserId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateCollectionJob {
    pub collection_id: CollectionId,
    pub deposit_id: DepositId,
    pub user_id: UserId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryCollectionJob {
    pub collection_id: CollectionId,
    pub deposit_id: DepositId,
    pub user_id: UserId,
}

/// Typed durable payload retained so a worker can resume after a PS restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobPayload {
    CreateDeposit(CreateDepositJob),
    CloseDeposit(CloseDepositJob),
    CreateCollection(CreateCollectionJob),
    RetryCollection(RetryCollectionJob),
}

impl JobPayload {
    #[must_use]
    pub const fn kind(&self) -> JobKind {
        match self {
            Self::CreateDeposit(_) => JobKind::CreateDeposit,
            Self::CloseDeposit(_) => JobKind::CloseDeposit,
            Self::CreateCollection(_) => JobKind::CreateCollection,
            Self::RetryCollection(_) => JobKind::RetryCollection,
        }
    }

    #[must_use]
    pub const fn operation(&self) -> CommandOperation {
        match self {
            Self::CreateDeposit(_) => CommandOperation::CreateDeposit,
            Self::CloseDeposit(_) => CommandOperation::CloseDeposit,
            Self::CreateCollection(_) => CommandOperation::CreateCollection,
            Self::RetryCollection(_) => CommandOperation::RetryCollection,
        }
    }

    #[must_use]
    pub fn user_id(&self) -> &UserId {
        match self {
            Self::CreateDeposit(payload) => &payload.user_id,
            Self::CloseDeposit(payload) => &payload.user_id,
            Self::CreateCollection(payload) => &payload.user_id,
            Self::RetryCollection(payload) => &payload.user_id,
        }
    }

    #[must_use]
    pub fn resource(&self) -> JobResource {
        match self {
            Self::CreateDeposit(payload) => JobResource::Deposit(payload.deposit_id.clone()),
            Self::CloseDeposit(payload) => JobResource::Deposit(payload.deposit_id.clone()),
            Self::CreateCollection(payload) => {
                JobResource::Collection(payload.collection_id.clone())
            }
            Self::RetryCollection(payload) => {
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
pub struct CreateJob {
    /// Server-generated after an explicit replay lookup misses and before the
    /// first persistence attempt. The create path still resolves a concurrent
    /// replay race to the already-persisted ID.
    pub id: JobId,
    pub command: CommandIdentity,
    pub payload: JobPayload,
    /// Expected durable owner of `payload.user_id()`. An administrator may
    /// submit a command while retaining the exchange principal as user owner.
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
pub struct JobPageRequest {
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
pub trait JobStore: Send + Sync {
    /// Looks up an already-accepted command before a caller generates fresh
    /// server IDs or performs any external provisioning side effect. Reuse of
    /// the scoped client key with another request hash is a conflict.
    fn job_for_command<'a>(
        &'a self,
        command: &'a CommandIdentity,
    ) -> BoxFuture<'a, Result<Option<Job>, DepositError>>;

    fn create_or_replay<'a>(
        &'a self,
        command: CreateJob,
    ) -> BoxFuture<'a, Result<CreateJobOutcome, DepositError>>;

    fn job<'a>(&'a self, id: &'a JobId) -> BoxFuture<'a, Result<Option<Job>, DepositError>>;

    fn jobs<'a>(&'a self, request: JobPageRequest) -> BoxFuture<'a, Result<JobPage, DepositError>>;

    fn jobs_for_user<'a>(
        &'a self,
        user_id: &'a UserId,
        request: JobPageRequest,
    ) -> BoxFuture<'a, Result<JobPage, DepositError>>;

    fn jobs_for_resource<'a>(
        &'a self,
        resource: &'a JobResource,
        request: JobPageRequest,
    ) -> BoxFuture<'a, Result<JobPage, DepositError>>;

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
