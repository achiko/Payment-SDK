use std::collections::BTreeSet;

use bincode::{Decode, Encode};
use indexing::IndexScope;
use indexing::{AssetId, ChainId};
use storage::{
    Condition, Error, ErrorKind, Key, Namespace, Operation, ScanRequest, Store, StoredValue,
    WriteBatch,
};

use crate::{
    BatchJob, BoxFuture, ClaimJob, CloseJob, CollectionId, CollectionJob, CommandIdentity,
    CommandOperation, CommandPrincipal, CreateJobOutcome, DatabaseIdentity, DatabaseInitializer,
    DepositError, DepositErrorKind, DepositId, DepositJob, DepositReader, IdempotencyKey,
    InitializeDatabase, Job, JobAssociations, JobCommands, JobError, JobId, JobKind, JobPage,
    JobPayload, JobPlan, JobQuery, JobReader, JobResource, JobRunner, JobState, MetadataReader,
    PAYMENT_DOMAIN_SCHEMA_VERSION, PAYMENT_SERVICE_OWNER, PaymentStore, PolicyIdentity,
    PrincipalScopeMode, RequestHash, RetryBatch, RetryJob, TransitionJob, User, UserId, UserStore,
};

mod metadata;
mod metadata_record;
mod record;
mod repository;
mod validation;
mod wire;

use metadata_record::*;
use record::*;
use validation::*;
use wire::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    async fn create_job(&self, command: JobPlan) -> Result<CreateJobOutcome, DepositError> {
        command.validate()?;
        if let Some(job) = self.idempotent_job(&command.command).await? {
            ensure_job_owner(&job, &command.user_owner)?;
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

        // A concurrent command may create the same opaque user. Re-read once
        // before reporting a genuine ownership or uniqueness conflict.
        if let Some(outcome) = self.try_create_job(&job, &associated_users, false).await? {
            return Ok(outcome);
        }
        self.try_create_job(&job, &associated_users, true)
            .await?
            .ok_or_else(|| storage_error("job creation retry was exhausted"))
    }

    async fn try_create_job(
        &self,
        job: &Job,
        associated_users: &[UserId],
        final_attempt: bool,
    ) -> Result<Option<CreateJobOutcome>, DepositError> {
        let missing_users = self.missing_job_users(job, associated_users).await?;
        match self
            .store_new_job(job, associated_users, &missing_users)
            .await
        {
            Ok(()) => Ok(Some(CreateJobOutcome::Created { job: job.clone() })),
            Err(error) if error.kind == DepositErrorKind::Conflict => {
                self.replay_job(job, error, final_attempt).await
            }
            Err(error) => Err(error),
        }
    }

    async fn missing_job_users(
        &self,
        job: &Job,
        associated_users: &[UserId],
    ) -> Result<Vec<UserId>, DepositError> {
        let mut missing = Vec::new();
        for user_id in associated_users {
            match self.stored_user_record(user_id).await? {
                Some(user) if user.owner != job.user_owner => {
                    return Err(conflict(
                        "opaque user ID is already owned by another authenticated principal",
                    ));
                }
                Some(_) => {}
                None => missing.push(user_id.clone()),
            }
        }
        Ok(missing)
    }

    async fn replay_job(
        &self,
        job: &Job,
        conflict_error: DepositError,
        final_attempt: bool,
    ) -> Result<Option<CreateJobOutcome>, DepositError> {
        if let Some(existing) = self.idempotent_job(&job.command).await? {
            ensure_job_owner(&existing, &job.user_owner)?;
            return Ok(Some(CreateJobOutcome::Replayed { job: existing }));
        }
        if final_attempt {
            return Err(conflict_error);
        }
        Ok(None)
    }

    async fn claim_job_record(
        &self,
        current: &Job,
        stored: &StoredValue,
        command: &ClaimJob,
    ) -> Result<Option<Job>, DepositError> {
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
        match self.update_job_state(current, stored, &claimed).await {
            Ok(()) => Ok(Some(claimed)),
            Err(error) if error.kind == DepositErrorKind::Conflict => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn ensure_job_owner(job: &Job, owner: &CommandPrincipal) -> Result<(), DepositError> {
    if job.user_owner != *owner {
        return Err(conflict(
            "command replay supplied a different opaque user owner",
        ));
    }
    Ok(())
}

impl<S> UserStore for PaymentStore<S>
where
    S: Store,
{
    fn ensure_user<'a>(&'a self, command: User) -> BoxFuture<'a, Result<User, DepositError>> {
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
                        value: encode(&UserRecord::from(&user))?,
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

impl<S> JobCommands for PaymentStore<S>
where
    S: Store,
{
    fn job_for_command<'a>(
        &'a self,
        command: &'a CommandIdentity,
    ) -> BoxFuture<'a, Result<Option<Job>, DepositError>> {
        Box::pin(async move { self.idempotent_job(command).await })
    }

    fn create_or_replay<'a>(
        &'a self,
        command: JobPlan,
    ) -> BoxFuture<'a, Result<CreateJobOutcome, DepositError>> {
        Box::pin(self.create_job(command))
    }
}

impl<S> JobReader for PaymentStore<S>
where
    S: Store,
{
    fn job<'a>(&'a self, id: &'a JobId) -> BoxFuture<'a, Result<Option<Job>, DepositError>> {
        Box::pin(async move { Ok(self.stored_job_record(id).await?.map(|(job, _)| job)) })
    }

    fn jobs<'a>(&'a self, request: JobQuery) -> BoxFuture<'a, Result<JobPage, DepositError>> {
        Box::pin(async move {
            request.validate()?;
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
                .map(|(_, stored)| decode::<JobRecord>(&stored)?.try_into())
                .collect::<Result<Vec<Job>, DepositError>>()?;
            let next = has_next
                .then(|| jobs.last().map(|job| job.id.clone()))
                .flatten();
            Ok(JobPage { jobs, next })
        })
    }
}

impl<S> JobAssociations for PaymentStore<S>
where
    S: Store,
{
    fn jobs_for_user<'a>(
        &'a self,
        user_id: &'a UserId,
        request: JobQuery,
    ) -> BoxFuture<'a, Result<JobPage, DepositError>> {
        Box::pin(async move {
            request.validate()?;
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
        request: JobQuery,
    ) -> BoxFuture<'a, Result<JobPage, DepositError>> {
        Box::pin(async move {
            request.validate()?;
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
}

impl<S> JobRunner for PaymentStore<S>
where
    S: Store,
{
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
                let index: JobIndex = decode(&index_stored)?;
                ensure_version(index.version)?;
                let Some((current, stored)) = self.stored_job_record(&JobId(index.job_id)).await?
                else {
                    return Err(storage_error("ready-job index is dangling"));
                };
                if current.ready_at() != Some(ready_at) {
                    return Err(storage_error(
                        "ready-job index does not match the durable job state",
                    ));
                }
                if let Some(claimed) = self.claim_job_record(&current, &stored, &command).await? {
                    return Ok(Some(claimed));
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
                    error.validate()?;
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
                    error.validate()?;
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
