use std::{sync::Arc, time::SystemTime};

use axum::{
    Json, Router,
    extract::{
        Extension, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use chain_ethereum::EthereumAddress;
use chain_identity::{AssetId, AtomicAmount};
use deposits::{
    AccountingCommand, ApplyResult, CloseDepositJob, Collection, CollectionId, CollectionLeg,
    CollectionLegKind, CollectionLegState, CollectionMode, CollectionPageRequest,
    CollectionReservationState, CollectionState, CollectionStore, CommandIdentity,
    CommandOperation, CommandPrincipal, CreateCollectionJob, CreateDepositJob, CreateJob,
    CreateUtxoBatchCollectionJob, Deposit, DepositId, DepositLedger, DepositObservationLogRequest,
    DepositPageRequest, DepositState, DepositStateKind, DepositStore, Job, JobId, JobKind,
    JobPageRequest, JobPayload, JobResource, JobState, JobStore, LedgerEntry, LedgerEntryCause,
    LedgerEntryId, LedgerObservationKind, LedgerPageRequest, ObservationEventLog,
    PersistentPaymentRepository, ProjectionId, ReconciliationCase, ReconciliationCaseId,
    ReconciliationDecision, ReconciliationPageRequest, ReconciliationReason,
    ReconciliationResolution, ReconciliationState, ReconciliationStore, ResolveReconciliation,
    RetryCollectionJob, RetryUtxoBatchCollectionJob, UserId, UserStore,
};
use http_support::{HealthState, RequestLimits};
use indexing::{
    BlockRef, ConfirmationProof, EventCursor, MovementKind, ObservationEvent, TransactionStatus,
    ValueMovement,
};
use serde::{Deserialize, Serialize};
use storage_rocksdb::RocksDbStorage;

use crate::{
    active_policy::ActivePaymentPolicy,
    api_error::ApiError,
    auth::{
        AuthenticatedPrincipal, Credentials, PrincipalRole, administrator_routes, ordinary_routes,
    },
    commands::{idempotency_key, request_hash, validate_opaque_id},
    ids::ServerIdGenerator,
};

const USER_OWNER_PRINCIPAL: &str = "exchange";
const DEPOSIT_KEY_PURPOSE: &str = "payment-service-deposit-address-v1";

type Repository = PersistentPaymentRepository<RocksDbStorage>;

#[derive(Clone)]
pub struct ApiState {
    repository: Repository,
    policy: Arc<ActivePaymentPolicy>,
    limits: RequestLimits,
    ids: ServerIdGenerator,
    health: HealthState,
    indexer_health: HealthState,
    wallet_health: HealthState,
}

impl ApiState {
    #[must_use]
    pub fn new(
        repository: Repository,
        policy: Arc<ActivePaymentPolicy>,
        limits: RequestLimits,
    ) -> Self {
        Self {
            repository,
            policy,
            limits,
            ids: ServerIdGenerator,
            health: HealthState::new(false),
            indexer_health: HealthState::new(false),
            wallet_health: HealthState::new(false),
        }
    }

    #[must_use]
    pub fn with_runtime_health(
        mut self,
        health: HealthState,
        indexer_health: HealthState,
        wallet_health: HealthState,
    ) -> Self {
        self.health = health;
        self.indexer_health = indexer_health;
        self.wallet_health = wallet_health;
        self
    }
}

pub fn router(state: Arc<ApiState>, credentials: Arc<Credentials>) -> Router {
    let administrator = administrator_routes(
        Router::new()
            .route(
                "/v1/deposits/{deposit_id}/accounting",
                post(record_accounting),
            )
            .route("/v1/reconciliations", get(reconciliations))
            .route("/v1/reconciliations/{case_id}", get(reconciliation))
            .route(
                "/v1/reconciliations/{case_id}/resolve",
                post(resolve_reconciliation),
            )
            .route("/v1/admin/status", get(admin_status)),
        Arc::clone(&credentials),
    );
    let routes = Router::new()
        .route("/v1/deposits", post(create_deposit).get(deposits))
        .route("/v1/deposits/{deposit_id}", get(deposit))
        .route("/v1/deposits/{deposit_id}/balances", get(balances))
        .route("/v1/deposits/{deposit_id}/ledger", get(ledger))
        .route("/v1/deposits/{deposit_id}/observations", get(observations))
        .route("/v1/deposits/{deposit_id}/close", post(close_deposit))
        .route("/v1/collections", post(create_collection).get(collections))
        .route("/v1/collections/{collection_id}", get(collection))
        .route(
            "/v1/collections/{collection_id}/retry",
            post(retry_collection),
        )
        .route("/v1/jobs/{job_id}", get(job))
        .merge(administrator)
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state);
    ordinary_routes(routes, credentials)
}

async fn route_not_found() -> ApiError {
    ApiError::not_found(
        "route_not_found",
        "requested Payment Service route does not exist",
    )
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed for this Payment Service route",
        false,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateDepositDto {
    user_id: String,
    scope: ScopeDto,
    asset: String,
    expected_amount: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeDto {
    chain: String,
    network: String,
}

#[derive(Serialize)]
struct AcceptedDepositJobDto {
    job_id: String,
    deposit_id: String,
}

async fn create_deposit(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    headers: HeaderMap,
    body: Result<Json<CreateDepositDto>, JsonRejection>,
) -> Result<(StatusCode, Json<AcceptedDepositJobDto>), ApiError> {
    let client_key = idempotency_key(&headers)?;
    let Json(body) = body.map_err(|_| {
        ApiError::bad_request("invalid_json", "deposit request body is not valid JSON")
    })?;
    validate_opaque_id(&body.user_id, "user_id")?;
    if body.scope.chain != state.policy.scope().chain.0
        || body.scope.network != state.policy.scope().network
    {
        return Err(ApiError::bad_request(
            "scope_mismatch",
            "deposit scope does not match this Payment Service instance",
        ));
    }
    let asset = parse_asset(&body.asset, &state.policy)?;
    let expected = parse_positive_amount(&body.expected_amount, "expected_amount")?;
    if matches!(state.policy.as_ref(), ActivePaymentPolicy::Bitcoin(_))
        && expected.0[..24].iter().any(|byte| *byte != 0)
    {
        return Err(ApiError::bad_request(
            "invalid_amount",
            "Bitcoin expected_amount must fit the native unsigned 64-bit satoshi range",
        ));
    }
    let command = CommandIdentity {
        principal: command_principal(principal),
        operation: CommandOperation::CreateDeposit,
        client_key,
        request_hash: request_hash(
            "create_deposit",
            &[
                &body.user_id,
                &body.scope.chain,
                &body.scope.network,
                &asset.asset,
                &expected.to_string(),
            ],
        ),
    };
    if let Some(job) = state
        .repository
        .job_for_command(&command)
        .await
        .map_err(ApiError::from_deposit)?
    {
        return accepted_deposit_job(&job, JobKind::CreateDeposit);
    }
    let now = unix_timestamp()?;
    let expires_at = now
        .checked_add(state.policy.deposit_ttl().as_secs())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "time_overflow",
                "deposit expiration could not be represented",
                false,
            )
        })?;
    let outcome = state
        .repository
        .create_or_replay(CreateJob {
            id: JobId(state.ids.job_id()),
            command,
            payload: JobPayload::CreateDeposit(CreateDepositJob {
                deposit_id: DepositId(state.ids.deposit_id()),
                user_id: UserId(body.user_id),
                scope: state.policy.scope().clone(),
                asset,
                expected,
                expires_at,
                created_at: now,
                key_purpose: DEPOSIT_KEY_PURPOSE.to_owned(),
            }),
            user_owner: exchange_user_owner(),
            policy: state.policy.identity(),
            created_at: now,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    accepted_deposit_job(outcome.job(), JobKind::CreateDeposit)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DepositPageQuery {
    user_id: Option<String>,
    state: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct DepositPageDto {
    deposits: Vec<DepositDto>,
    next_cursor: Option<String>,
}

async fn deposits(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    query: Result<Query<DepositPageQuery>, QueryRejection>,
) -> Result<Json<DepositPageDto>, ApiError> {
    let Query(query) = valid_query(query)?;
    let user_id = query
        .user_id
        .map(|user_id| {
            validate_opaque_id(&user_id, "user_id")?;
            Ok::<_, ApiError>(UserId(user_id))
        })
        .transpose()?;
    if let Some(user_id) = user_id.as_ref() {
        let Some(user) = state
            .repository
            .user(user_id)
            .await
            .map_err(ApiError::from_deposit)?
        else {
            return Ok(Json(DepositPageDto {
                deposits: Vec::new(),
                next_cursor: None,
            }));
        };
        authorize_owner(principal, &user.owner)?;
    }
    let cursor = query
        .cursor
        .map(|cursor| {
            validate_opaque_id(&cursor, "cursor")?;
            Ok::<_, ApiError>(DepositId(cursor))
        })
        .transpose()?;
    let state_filter = query
        .state
        .as_deref()
        .map(parse_deposit_state_filter)
        .transpose()?;
    let page = state
        .repository
        .deposits(DepositPageRequest {
            after: cursor,
            limit: page_limit(&state.limits, query.limit)?,
            user_id,
            state: state_filter,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    let mut response = Vec::with_capacity(page.deposits.len());
    for deposit in page.deposits {
        authorize_user(&state, principal, &deposit.user_id).await?;
        let balances = state
            .repository
            .current(&deposit.id)
            .await
            .map_err(ApiError::from_deposit)?
            .map(|entry| entry.balances)
            .unwrap_or_default();
        response.push(DepositDto::new(
            deposit,
            balances,
            state.policy.scope().network.clone(),
        ));
    }
    Ok(Json(DepositPageDto {
        deposits: response,
        next_cursor: page.next.map(|cursor| cursor.0),
    }))
}

async fn close_deposit(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(deposit_id): Path<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<AcceptedDepositJobDto>), ApiError> {
    validate_opaque_id(&deposit_id, "deposit_id")?;
    let client_key = idempotency_key(&headers)?;
    let deposit = find_deposit(&state, &deposit_id).await?;
    authorize_user(&state, principal, &deposit.user_id).await?;
    let command = CommandIdentity {
        principal: command_principal(principal),
        operation: CommandOperation::CloseDeposit,
        client_key,
        request_hash: request_hash("close_deposit", &[&deposit_id]),
    };
    if let Some(job) = state
        .repository
        .job_for_command(&command)
        .await
        .map_err(ApiError::from_deposit)?
    {
        return accepted_deposit_job(&job, JobKind::CloseDeposit);
    }
    let now = unix_timestamp()?;
    let outcome = state
        .repository
        .create_or_replay(CreateJob {
            id: JobId(state.ids.job_id()),
            command,
            payload: JobPayload::CloseDeposit(CloseDepositJob {
                deposit_id: deposit.id.clone(),
                user_id: deposit.user_id,
            }),
            user_owner: exchange_user_owner(),
            policy: state.policy.identity(),
            created_at: now,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    accepted_deposit_job(outcome.job(), JobKind::CloseDeposit)
}

fn accepted_deposit_job(
    job: &Job,
    expected_kind: JobKind,
) -> Result<(StatusCode, Json<AcceptedDepositJobDto>), ApiError> {
    if job.kind != expected_kind {
        return Err(internal_invariant(
            "deposit command resolved to another job kind",
        ));
    }
    let deposit_id = match &job.payload {
        JobPayload::CreateDeposit(payload) => &payload.deposit_id,
        JobPayload::CloseDeposit(payload) => &payload.deposit_id,
        JobPayload::CreateCollection(_)
        | JobPayload::RetryCollection(_)
        | JobPayload::CreateUtxoBatchCollection(_)
        | JobPayload::RetryUtxoBatchCollection(_) => {
            return Err(internal_invariant(
                "deposit command resolved to a collection job payload",
            ));
        }
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedDepositJobDto {
            job_id: job.id.0.clone(),
            deposit_id: deposit_id.0.clone(),
        }),
    ))
}

async fn job(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(job_id): Path<String>,
) -> Result<Json<JobDto>, ApiError> {
    validate_opaque_id(&job_id, "job_id")?;
    let job = state
        .repository
        .job(&JobId(job_id))
        .await
        .map_err(ApiError::from_deposit)?
        .ok_or_else(|| ApiError::not_found("job_not_found", "job does not exist"))?;
    authorize_owner(principal, &job.user_owner)?;
    Ok(Json(JobDto::from(&job)))
}

#[derive(Serialize)]
struct JobDto {
    job_id: String,
    kind: &'static str,
    state: &'static str,
    resource_kind: &'static str,
    resource_id: String,
    attempt_count: u32,
    last_error: Option<JobErrorDto>,
    next_attempt_at: Option<String>,
    created_at: String,
    updated_at: String,
    policy_version: String,
}

#[derive(Serialize)]
struct JobErrorDto {
    code: String,
    message: String,
    retryable: bool,
}

impl From<&Job> for JobDto {
    fn from(job: &Job) -> Self {
        let (state, next_attempt_at) = match job.state {
            JobState::Queued => ("queued", None),
            JobState::Running { lease_expires_at } => {
                ("running", Some(lease_expires_at.to_string()))
            }
            JobState::WaitingRetry { next_attempt_at } => {
                ("waiting_retry", Some(next_attempt_at.to_string()))
            }
            JobState::Succeeded => ("succeeded", None),
            JobState::Failed => ("failed", None),
        };
        let (resource_kind, resource_id) = match &job.resource {
            JobResource::Deposit(id) => ("deposit", id.0.clone()),
            JobResource::Collection(id) => ("collection", id.0.clone()),
        };
        Self {
            job_id: job.id.0.clone(),
            kind: match job.kind {
                JobKind::CreateDeposit => "create_deposit",
                JobKind::CloseDeposit => "close_deposit",
                JobKind::CreateCollection => "create_collection",
                JobKind::RetryCollection => "retry_collection",
            },
            state,
            resource_kind,
            resource_id,
            attempt_count: job.attempt_count,
            last_error: job.last_error.as_ref().map(|error| JobErrorDto {
                code: error.code.clone(),
                message: error.message.clone(),
                retryable: error.retryable,
            }),
            next_attempt_at,
            created_at: job.created_at.to_string(),
            updated_at: job.updated_at.to_string(),
            policy_version: job.policy.version.clone(),
        }
    }
}

async fn deposit(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(deposit_id): Path<String>,
) -> Result<Json<DepositDto>, ApiError> {
    validate_opaque_id(&deposit_id, "deposit_id")?;
    let deposit = find_deposit(&state, &deposit_id).await?;
    authorize_user(&state, principal, &deposit.user_id).await?;
    let balances = state
        .repository
        .current(&deposit.id)
        .await
        .map_err(ApiError::from_deposit)?
        .map(|entry| entry.balances)
        .unwrap_or_default();
    Ok(Json(DepositDto::new(
        deposit,
        balances,
        state.policy.scope().network.clone(),
    )))
}

#[derive(Serialize)]
struct DepositDto {
    deposit_id: String,
    user_id: String,
    scope: ScopeResponseDto,
    asset: String,
    expected_amount: String,
    state: &'static str,
    payment_progress: &'static str,
    address: Option<String>,
    birthday: Option<String>,
    expires_at: String,
    created_at: String,
}

#[derive(Serialize)]
struct ScopeResponseDto {
    chain: String,
    network: String,
}

impl DepositDto {
    fn new(deposit: Deposit, balances: deposits::DepositBalances, network: String) -> Self {
        let address_available = !matches!(deposit.state, DepositState::AwaitingWatch);
        Self {
            deposit_id: deposit.id.0,
            user_id: deposit.user_id.0,
            scope: ScopeResponseDto {
                chain: deposit.asset.chain.0.clone(),
                // A PS database is bound to one scope; the deposit's chain is
                // persisted while network identity comes from that binding.
                network,
            },
            asset: deposit.asset.asset,
            expected_amount: deposit.expected.to_string(),
            state: deposit_state_name(&deposit.state),
            payment_progress: payment_progress(balances, deposit.expected),
            address: address_available.then_some(deposit.address.value),
            birthday: address_available.then_some(deposit.birthday.0.to_string()),
            expires_at: deposit.expires_at.to_string(),
            created_at: deposit.created_at.to_string(),
        }
    }
}

async fn balances(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(deposit_id): Path<String>,
) -> Result<Json<BalancesDto>, ApiError> {
    validate_opaque_id(&deposit_id, "deposit_id")?;
    let deposit = find_deposit(&state, &deposit_id).await?;
    authorize_user(&state, principal, &deposit.user_id).await?;
    let ledger = state
        .repository
        .current(&deposit.id)
        .await
        .map_err(ApiError::from_deposit)?
        .ok_or_else(|| internal_invariant("persisted deposit has no absolute ledger head"))?;
    Ok(Json(BalancesDto::from(ledger.balances)))
}

#[derive(Serialize)]
struct BalancesDto {
    received: String,
    confirmed: String,
    balance: String,
    collected: String,
    accounted: String,
}

impl From<deposits::DepositBalances> for BalancesDto {
    fn from(value: deposits::DepositBalances) -> Self {
        Self {
            received: value.received.to_string(),
            confirmed: value.confirmed.to_string(),
            balance: value.balance.to_string(),
            collected: value.collected.to_string(),
            accounted: value.accounted.to_string(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct LedgerPageDto {
    entries: Vec<LedgerEntryDto>,
    next_cursor: Option<String>,
}

async fn ledger(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(deposit_id): Path<String>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<LedgerPageDto>, ApiError> {
    validate_opaque_id(&deposit_id, "deposit_id")?;
    let Query(query) = valid_query(query)?;
    let cursor = query
        .cursor
        .map(|cursor| {
            validate_opaque_id(&cursor, "cursor")?;
            Ok::<_, ApiError>(LedgerEntryId(cursor))
        })
        .transpose()?;
    let deposit = find_deposit(&state, &deposit_id).await?;
    authorize_user(&state, principal, &deposit.user_id).await?;
    let page = state
        .repository
        .entries(LedgerPageRequest {
            deposit_id: deposit.id,
            after: cursor,
            limit: page_limit(&state.limits, query.limit)?,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    Ok(Json(LedgerPageDto {
        entries: page.entries.iter().map(LedgerEntryDto::from).collect(),
        next_cursor: page.next.map(|cursor| cursor.0),
    }))
}

#[derive(Serialize)]
struct LedgerEntryDto {
    ledger_entry_id: String,
    previous_ledger_entry_id: Option<String>,
    cause: LedgerCauseDto,
    balances: BalancesDto,
    recorded_at: String,
}

impl From<&LedgerEntry> for LedgerEntryDto {
    fn from(entry: &LedgerEntry) -> Self {
        Self {
            ledger_entry_id: entry.id.0.clone(),
            previous_ledger_entry_id: entry.previous.as_ref().map(|previous| previous.0.clone()),
            cause: LedgerCauseDto::from(&entry.cause),
            balances: BalancesDto::from(entry.balances),
            recorded_at: entry.recorded_at.to_string(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LedgerCauseDto {
    Opened,
    Observation {
        event_id: String,
        revision: String,
        status: Box<TransactionStatusDto>,
        classification: &'static str,
        movement_ids: Vec<String>,
        network_fee: Option<String>,
    },
    Accounting {
        reason: String,
    },
    ReconciliationResolution {
        case_id: String,
        reason: String,
    },
}

impl From<&LedgerEntryCause> for LedgerCauseDto {
    fn from(cause: &LedgerEntryCause) -> Self {
        match cause {
            LedgerEntryCause::Opened { .. } => Self::Opened,
            LedgerEntryCause::Observation {
                event_id,
                observation_revision,
                status,
                kind,
                movement_ids,
                network_fee,
                ..
            } => Self::Observation {
                event_id: event_id.0.clone(),
                revision: observation_revision.0.to_string(),
                status: Box::new(TransactionStatusDto::from(status)),
                classification: ledger_observation_kind(*kind),
                movement_ids: movement_ids
                    .iter()
                    .map(|movement_id| movement_id.0.clone())
                    .collect(),
                network_fee: network_fee.map(|amount| amount.to_string()),
            },
            LedgerEntryCause::Accounting { reason, .. } => Self::Accounting {
                reason: reason.clone(),
            },
            LedgerEntryCause::ReconciliationResolution {
                case_id, reason, ..
            } => Self::ReconciliationResolution {
                case_id: case_id.0.clone(),
                reason: reason.clone(),
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountingDto {
    next_accounted: String,
    expected_ledger_head: String,
    reason: String,
}

#[derive(Serialize)]
struct AccountingResultDto {
    deposit_id: String,
    ledger_entry: LedgerEntryDto,
}

async fn record_accounting(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(deposit_id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<AccountingDto>, JsonRejection>,
) -> Result<Json<AccountingResultDto>, ApiError> {
    validate_opaque_id(&deposit_id, "deposit_id")?;
    let client_key = idempotency_key(&headers)?;
    let Json(body) = body.map_err(|_| {
        ApiError::bad_request("invalid_json", "accounting request body is not valid JSON")
    })?;
    validate_opaque_id(&body.expected_ledger_head, "expected_ledger_head")?;
    validate_reason(&body.reason)?;
    let next_accounted = parse_amount(&body.next_accounted, "next_accounted")?;
    let deposit = find_deposit(&state, &deposit_id).await?;
    authorize_user(&state, principal, &deposit.user_id).await?;
    let result = state
        .repository
        .record_accounting(AccountingCommand {
            command: CommandIdentity {
                principal: command_principal(principal),
                operation: CommandOperation::Accounting,
                client_key,
                request_hash: request_hash(
                    "accounting",
                    &[
                        &deposit_id,
                        &body.expected_ledger_head,
                        &body.next_accounted,
                        &body.reason,
                    ],
                ),
            },
            deposit_id: deposit.id,
            expected_head: Some(LedgerEntryId(body.expected_ledger_head)),
            next_accounted,
            reason: body.reason,
            recorded_at: unix_timestamp()?,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    let entry = match result {
        ApplyResult::Appended { entry } | ApplyResult::AlreadyPresent { entry } => entry,
    };
    Ok(Json(AccountingResultDto {
        deposit_id,
        ledger_entry: LedgerEntryDto::from(&entry),
    }))
}

#[derive(Serialize)]
struct ObservationPageDto {
    observations: Vec<DepositObservationDto>,
    /// IX event cursor within this deposit's durable observation index.
    next_cursor: Option<String>,
}

async fn observations(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(deposit_id): Path<String>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<ObservationPageDto>, ApiError> {
    validate_opaque_id(&deposit_id, "deposit_id")?;
    let Query(query) = valid_query(query)?;
    let cursor = query
        .cursor
        .map(|cursor| {
            cursor.parse::<u64>().map(EventCursor).map_err(|_| {
                ApiError::bad_request(
                    "invalid_cursor",
                    "observation cursor must be an unsigned decimal integer",
                )
            })
        })
        .transpose()?;
    let deposit = find_deposit(&state, &deposit_id).await?;
    authorize_user(&state, principal, &deposit.user_id).await?;
    let page = state
        .repository
        .observations_for_deposit(DepositObservationLogRequest {
            deposit_id: deposit.id.clone(),
            after: cursor,
            limit: page_limit(&state.limits, query.limit)?,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    let observations = page
        .observations
        .into_iter()
        .map(|mirrored| {
            let relevant_movement_ids = mirrored
                .event
                .transaction
                .movements
                .iter()
                .filter(|movement| {
                    movement.from.as_ref() == Some(&deposit.address)
                        || movement.to.as_ref() == Some(&deposit.address)
                })
                .map(|movement| movement.id.clone())
                .collect::<Vec<_>>();
            let has_ledger_effect = mirrored.event.transaction.movements.iter().any(|movement| {
                movement.asset == deposit.asset
                    && (movement.from.as_ref() == Some(&deposit.address)
                        || movement.to.as_ref() == Some(&deposit.address))
            }) || mirrored.event.transaction.fee.as_ref().is_some_and(
                |fee| fee.asset == deposit.asset && fee.payer.as_ref() == Some(&deposit.address),
            );
            let ledger_entry_id = has_ledger_effect.then(|| {
                LedgerEntryId(format!(
                    "projection:{}",
                    ProjectionId::for_observation(
                        &mirrored.event.id,
                        mirrored.event.transaction.revision,
                        &deposit.id,
                    )
                    .0
                ))
            });
            DepositObservationDto::new(
                ledger_entry_id.as_ref(),
                &mirrored.event,
                mirrored.received_at,
                &relevant_movement_ids,
            )
        })
        .collect();
    Ok(Json(ObservationPageDto {
        observations,
        next_cursor: page.next.map(|cursor| cursor.0.to_string()),
    }))
}

#[derive(Serialize)]
struct DepositObservationDto {
    ledger_entry_id: Option<String>,
    event_id: String,
    cursor: String,
    transaction_id: String,
    revision: String,
    status: TransactionStatusDto,
    previous_status: Option<TransactionStatusDto>,
    movements: Vec<MovementDto>,
    network_fee: Option<NetworkFeeDto>,
    first_seen_at: String,
    observed_at: String,
    received_at: String,
}

impl DepositObservationDto {
    fn new(
        ledger_entry_id: Option<&LedgerEntryId>,
        event: &ObservationEvent,
        received_at: u64,
        relevant_movement_ids: &[indexing::MovementId],
    ) -> Self {
        let movements = event
            .transaction
            .movements
            .iter()
            .filter(|movement| relevant_movement_ids.contains(&movement.id))
            .map(MovementDto::from)
            .collect();
        Self {
            ledger_entry_id: ledger_entry_id.map(|entry_id| entry_id.0.clone()),
            event_id: event.id.0.clone(),
            cursor: event.cursor.0.to_string(),
            transaction_id: event.transaction.transaction_id.value.clone(),
            revision: event.transaction.revision.0.to_string(),
            status: TransactionStatusDto::from(&event.transaction.status),
            previous_status: event
                .previous_status
                .as_ref()
                .map(TransactionStatusDto::from),
            movements,
            network_fee: event.transaction.fee.as_ref().map(NetworkFeeDto::from),
            first_seen_at: event.transaction.first_seen_at.to_string(),
            observed_at: event.transaction.observed_at.to_string(),
            received_at: received_at.to_string(),
        }
    }
}

#[derive(Serialize)]
struct NetworkFeeDto {
    asset: AssetResponseDto,
    amount: String,
    payer: Option<String>,
}

impl From<&indexing::NetworkFee> for NetworkFeeDto {
    fn from(fee: &indexing::NetworkFee) -> Self {
        Self {
            asset: AssetResponseDto::from(&fee.asset),
            amount: fee.amount.to_string(),
            payer: fee.payer.as_ref().map(|address| address.value.clone()),
        }
    }
}

#[derive(Serialize)]
struct MovementDto {
    movement_id: String,
    asset: AssetResponseDto,
    amount: String,
    from: Option<String>,
    to: Option<String>,
    kind: &'static str,
}

impl From<&ValueMovement> for MovementDto {
    fn from(movement: &ValueMovement) -> Self {
        Self {
            movement_id: movement.id.0.clone(),
            asset: AssetResponseDto::from(&movement.asset),
            amount: movement.amount.to_string(),
            from: movement.from.as_ref().map(|address| address.value.clone()),
            to: movement.to.as_ref().map(|address| address.value.clone()),
            kind: movement_kind(movement.kind),
        }
    }
}

#[derive(Serialize)]
struct AssetResponseDto {
    chain: String,
    asset: String,
}

impl From<&AssetId> for AssetResponseDto {
    fn from(asset: &AssetId) -> Self {
        Self {
            chain: asset.chain.0.clone(),
            asset: asset.asset.clone(),
        }
    }
}

#[derive(Serialize)]
struct TransactionStatusDto {
    kind: &'static str,
    block: Option<BlockDto>,
    confirmations: Option<String>,
    confirmation_proof: Option<ConfirmationProofDto>,
    replacement_transaction_id: Option<String>,
}

impl From<&TransactionStatus> for TransactionStatusDto {
    fn from(status: &TransactionStatus) -> Self {
        match status {
            TransactionStatus::Pending => Self::simple("pending"),
            TransactionStatus::Included {
                block,
                confirmations,
            } => Self {
                kind: "included",
                block: Some(BlockDto::from(block)),
                confirmations: Some(confirmations.to_string()),
                confirmation_proof: None,
                replacement_transaction_id: None,
            },
            TransactionStatus::Confirmed { block, proof } => Self {
                kind: "confirmed",
                block: Some(BlockDto::from(block)),
                confirmations: None,
                confirmation_proof: Some(ConfirmationProofDto::from(*proof)),
                replacement_transaction_id: None,
            },
            TransactionStatus::Failed { block, .. } => Self {
                kind: "failed",
                block: block.as_ref().map(BlockDto::from),
                confirmations: None,
                confirmation_proof: None,
                replacement_transaction_id: None,
            },
            TransactionStatus::Replaced { by } => Self {
                kind: "replaced",
                block: None,
                confirmations: None,
                confirmation_proof: None,
                replacement_transaction_id: Some(by.value.clone()),
            },
            TransactionStatus::Dropped => Self::simple("dropped"),
            TransactionStatus::Reorged { previous_block } => Self {
                kind: "reorged",
                block: Some(BlockDto::from(previous_block)),
                confirmations: None,
                confirmation_proof: None,
                replacement_transaction_id: None,
            },
        }
    }
}

impl TransactionStatusDto {
    const fn simple(kind: &'static str) -> Self {
        Self {
            kind,
            block: None,
            confirmations: None,
            confirmation_proof: None,
            replacement_transaction_id: None,
        }
    }
}

#[derive(Serialize)]
struct BlockDto {
    height: String,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<String>,
}

impl From<&BlockRef> for BlockDto {
    fn from(block: &BlockRef) -> Self {
        Self {
            height: block.height.0.to_string(),
            hash: hex_bytes(&block.hash.0),
            parent_hash: block.parent_hash.as_ref().map(|hash| hex_bytes(&hash.0)),
            timestamp: block.timestamp.map(|timestamp| timestamp.to_string()),
        }
    }
}

#[derive(Serialize)]
struct ConfirmationProofDto {
    kind: &'static str,
    required: Option<String>,
    observed: Option<String>,
}

impl From<ConfirmationProof> for ConfirmationProofDto {
    fn from(proof: ConfirmationProof) -> Self {
        match proof {
            ConfirmationProof::Depth { required, observed } => Self {
                kind: "depth",
                required: Some(required.to_string()),
                observed: Some(observed.to_string()),
            },
            ConfirmationProof::ChainFinalized => Self {
                kind: "chain_finalized",
                required: None,
                observed: None,
            },
            ConfirmationProof::DepthAndChainFinalized { required, observed } => Self {
                kind: "depth_and_chain_finalized",
                required: Some(required.to_string()),
                observed: Some(observed.to_string()),
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateCollectionDto {
    deposit_id: Option<String>,
    deposit_ids: Option<Vec<String>>,
}

enum CollectionJobRequest {
    Ethereum {
        deposit_id: DepositId,
        user_id: UserId,
    },
    Bitcoin {
        deposit_ids: Vec<DepositId>,
    },
}

impl CollectionJobRequest {
    fn into_payload(self, collection_id: CollectionId) -> JobPayload {
        match self {
            Self::Ethereum {
                deposit_id,
                user_id,
            } => JobPayload::CreateCollection(CreateCollectionJob {
                collection_id,
                deposit_id,
                user_id,
            }),
            Self::Bitcoin { deposit_ids } => {
                JobPayload::CreateUtxoBatchCollection(CreateUtxoBatchCollectionJob {
                    collection_id,
                    deposit_ids,
                })
            }
        }
    }
}

#[derive(Serialize)]
struct AcceptedCollectionJobDto {
    job_id: String,
    collection_id: String,
}

async fn create_collection(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    headers: HeaderMap,
    body: Result<Json<CreateCollectionDto>, JsonRejection>,
) -> Result<(StatusCode, Json<AcceptedCollectionJobDto>), ApiError> {
    let client_key = idempotency_key(&headers)?;
    let Json(body) = body.map_err(|_| {
        ApiError::bad_request("invalid_json", "collection request body is not valid JSON")
    })?;
    let (payload, hash_fields) = match state.policy.as_ref() {
        ActivePaymentPolicy::Ethereum(_) => {
            let deposit_id = body.deposit_id.ok_or_else(|| {
                ApiError::bad_request(
                    "invalid_collection",
                    "Ethereum collection requires exactly one deposit_id",
                )
            })?;
            if body.deposit_ids.is_some() {
                return Err(ApiError::bad_request(
                    "invalid_collection",
                    "Ethereum collection does not accept deposit_ids",
                ));
            }
            validate_opaque_id(&deposit_id, "deposit_id")?;
            let deposit = find_deposit(&state, &deposit_id).await?;
            authorize_user(&state, principal, &deposit.user_id).await?;
            (
                CollectionJobRequest::Ethereum {
                    deposit_id: deposit.id,
                    user_id: deposit.user_id,
                },
                vec![deposit_id],
            )
        }
        ActivePaymentPolicy::Bitcoin(policy) => {
            if body.deposit_id.is_some() {
                return Err(ApiError::bad_request(
                    "invalid_collection",
                    "Bitcoin collection requires the explicit deposit_ids array",
                ));
            }
            let mut deposit_ids = body.deposit_ids.ok_or_else(|| {
                ApiError::bad_request(
                    "invalid_collection",
                    "Bitcoin collection requires deposit_ids",
                )
            })?;
            if deposit_ids.is_empty() {
                return Err(ApiError::bad_request(
                    "invalid_collection",
                    "Bitcoin collection deposit_ids must not be empty",
                ));
            }
            for deposit_id in &deposit_ids {
                validate_opaque_id(deposit_id, "deposit_ids")?;
            }
            deposit_ids.sort();
            if deposit_ids.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(ApiError::bad_request(
                    "invalid_collection",
                    "Bitcoin collection deposit_ids must be unique",
                ));
            }
            if deposit_ids.len() > policy.maximum_deposits {
                return Err(ApiError::bad_request(
                    "invalid_collection",
                    "Bitcoin collection exceeds the active maximum deposit count",
                ));
            }
            let mut durable_ids = Vec::with_capacity(deposit_ids.len());
            for deposit_id in &deposit_ids {
                let deposit = find_deposit(&state, deposit_id).await?;
                authorize_user(&state, principal, &deposit.user_id).await?;
                if deposit.asset != policy.asset {
                    return Err(ApiError::bad_request(
                        "scope_mismatch",
                        "Bitcoin collection deposit does not belong to this policy",
                    ));
                }
                durable_ids.push(deposit.id);
            }
            (
                CollectionJobRequest::Bitcoin {
                    deposit_ids: durable_ids,
                },
                deposit_ids,
            )
        }
    };
    let command = CommandIdentity {
        principal: command_principal(principal),
        operation: CommandOperation::CreateCollection,
        client_key,
        request_hash: request_hash(
            "create_collection",
            &hash_fields.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
    };
    if let Some(job) = state
        .repository
        .job_for_command(&command)
        .await
        .map_err(ApiError::from_deposit)?
    {
        return accepted_collection_job(&job, JobKind::CreateCollection);
    }
    let outcome = state
        .repository
        .create_or_replay(CreateJob {
            id: JobId(state.ids.job_id()),
            command,
            payload: payload.into_payload(CollectionId(state.ids.collection_id())),
            user_owner: exchange_user_owner(),
            policy: state.policy.identity(),
            created_at: unix_timestamp()?,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    accepted_collection_job(outcome.job(), JobKind::CreateCollection)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionPageQuery {
    deposit_id: String,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct CollectionPageDto {
    collections: Vec<CollectionDto>,
    next_cursor: Option<String>,
}

async fn collections(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    query: Result<Query<CollectionPageQuery>, QueryRejection>,
) -> Result<Json<CollectionPageDto>, ApiError> {
    let Query(query) = valid_query(query)?;
    validate_opaque_id(&query.deposit_id, "deposit_id")?;
    let cursor = query
        .cursor
        .map(|cursor| {
            validate_opaque_id(&cursor, "cursor")?;
            Ok::<_, ApiError>(CollectionId(cursor))
        })
        .transpose()?;
    let deposit = find_deposit(&state, &query.deposit_id).await?;
    authorize_user(&state, principal, &deposit.user_id).await?;
    let page = state
        .repository
        .collections_for_deposit(
            &deposit.id,
            CollectionPageRequest {
                after: cursor,
                limit: page_limit(&state.limits, query.limit)?,
            },
        )
        .await
        .map_err(ApiError::from_deposit)?;
    Ok(Json(CollectionPageDto {
        collections: page.collections.iter().map(CollectionDto::from).collect(),
        next_cursor: page.next.map(|cursor| cursor.0),
    }))
}

async fn collection(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(collection_id): Path<String>,
) -> Result<Json<CollectionDto>, ApiError> {
    validate_opaque_id(&collection_id, "collection_id")?;
    let collection = find_collection(&state, &collection_id).await?;
    if collection.participants.is_empty() {
        return Err(internal_invariant(
            "collection has no durable participant ownership",
        ));
    }
    for participant in &collection.participants {
        authorize_user(&state, principal, &participant.user_id).await?;
    }
    Ok(Json(CollectionDto::from(&collection)))
}

async fn retry_collection(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(collection_id): Path<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<AcceptedCollectionJobDto>), ApiError> {
    validate_opaque_id(&collection_id, "collection_id")?;
    let client_key = idempotency_key(&headers)?;
    let collection = find_collection(&state, &collection_id).await?;
    for participant in &collection.participants {
        authorize_user(&state, principal, &participant.user_id).await?;
    }
    if collection.participants.is_empty() {
        return Err(internal_invariant(
            "collection has no durable participant ownership",
        ));
    }
    let command = CommandIdentity {
        principal: command_principal(principal),
        operation: CommandOperation::RetryCollection,
        client_key,
        request_hash: request_hash("retry_collection", &[&collection_id]),
    };
    if let Some(job) = state
        .repository
        .job_for_command(&command)
        .await
        .map_err(ApiError::from_deposit)?
    {
        return accepted_collection_job(&job, JobKind::RetryCollection);
    }
    let payload = if collection.mode == CollectionMode::UtxoBatch {
        JobPayload::RetryUtxoBatchCollection(RetryUtxoBatchCollectionJob {
            collection_id: collection.id.clone(),
            deposit_ids: collection
                .participants
                .iter()
                .map(|participant| participant.reservation.deposit_id.clone())
                .collect(),
        })
    } else {
        JobPayload::RetryCollection(RetryCollectionJob {
            collection_id: collection.id.clone(),
            deposit_id: collection.deposit_id.clone(),
            user_id: collection.user_id.clone(),
        })
    };
    let outcome = state
        .repository
        .create_or_replay(CreateJob {
            id: JobId(state.ids.job_id()),
            command,
            payload,
            user_owner: exchange_user_owner(),
            policy: state.policy.identity(),
            created_at: unix_timestamp()?,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    accepted_collection_job(outcome.job(), JobKind::RetryCollection)
}

fn accepted_collection_job(
    job: &Job,
    expected_kind: JobKind,
) -> Result<(StatusCode, Json<AcceptedCollectionJobDto>), ApiError> {
    if job.kind != expected_kind {
        return Err(internal_invariant(
            "collection command resolved to another job kind",
        ));
    }
    let collection_id = match &job.payload {
        JobPayload::CreateCollection(payload) => &payload.collection_id,
        JobPayload::RetryCollection(payload) => &payload.collection_id,
        JobPayload::CreateUtxoBatchCollection(payload) => &payload.collection_id,
        JobPayload::RetryUtxoBatchCollection(payload) => &payload.collection_id,
        JobPayload::CreateDeposit(_) | JobPayload::CloseDeposit(_) => {
            return Err(internal_invariant(
                "collection command resolved to a deposit job payload",
            ));
        }
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedCollectionJobDto {
            job_id: job.id.0.clone(),
            collection_id: collection_id.0.clone(),
        }),
    ))
}

#[derive(Serialize)]
struct CollectionDto {
    collection_id: String,
    job_id: String,
    user_id: String,
    deposit_id: String,
    mode: &'static str,
    asset: AssetResponseDto,
    destination: String,
    policy_version: String,
    policy_digest: String,
    state: &'static str,
    reservation: CollectionReservationDto,
    participants: Vec<CollectionParticipantDto>,
    legs: Vec<CollectionLegDto>,
    attempt_count: u32,
    last_error: Option<CollectionErrorDto>,
    created_at: String,
    updated_at: String,
}

impl From<&Collection> for CollectionDto {
    fn from(collection: &Collection) -> Self {
        Self {
            collection_id: collection.id.0.clone(),
            job_id: collection.job_id.0.clone(),
            user_id: collection.user_id.0.clone(),
            deposit_id: collection.deposit_id.0.clone(),
            mode: collection_mode(collection.mode),
            asset: AssetResponseDto::from(&collection.asset),
            destination: collection.destination.value.clone(),
            policy_version: collection.policy.version.clone(),
            policy_digest: hex_bytes(&collection.policy.digest),
            state: collection_state(collection.state),
            reservation: CollectionReservationDto::from(&collection.reservation),
            participants: collection
                .participants
                .iter()
                .map(CollectionParticipantDto::from)
                .collect(),
            legs: collection.legs.iter().map(CollectionLegDto::from).collect(),
            attempt_count: collection.attempt_count,
            last_error: collection.last_error.as_ref().map(CollectionErrorDto::from),
            created_at: collection.created_at.to_string(),
            updated_at: collection.updated_at.to_string(),
        }
    }
}

#[derive(Serialize)]
struct CollectionParticipantDto {
    user_id: String,
    deposit_id: String,
    reservation: CollectionReservationDto,
    spend_resources: Vec<CollectionSpendResourceDto>,
}

impl From<&deposits::CollectionParticipant> for CollectionParticipantDto {
    fn from(participant: &deposits::CollectionParticipant) -> Self {
        Self {
            user_id: participant.user_id.0.clone(),
            deposit_id: participant.reservation.deposit_id.0.clone(),
            reservation: CollectionReservationDto::from(&participant.reservation),
            spend_resources: participant
                .spend_resources
                .iter()
                .map(CollectionSpendResourceDto::from)
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct CollectionSpendResourceDto {
    transaction_id: String,
    output_index: u32,
    amount: String,
}

impl From<&deposits::CollectionSpendResource> for CollectionSpendResourceDto {
    fn from(resource: &deposits::CollectionSpendResource) -> Self {
        Self {
            transaction_id: resource.id.transaction_id.value.clone(),
            output_index: resource.id.output_index,
            amount: resource.amount.to_string(),
        }
    }
}

#[derive(Serialize)]
struct CollectionReservationDto {
    amount: String,
    state: &'static str,
    transaction_id: Option<String>,
    released_reason: Option<&'static str>,
    state_changed_at: Option<String>,
}

impl From<&deposits::CollectionReservation> for CollectionReservationDto {
    fn from(reservation: &deposits::CollectionReservation) -> Self {
        match &reservation.state {
            CollectionReservationState::Active => Self {
                amount: reservation.amount.to_string(),
                state: "active",
                transaction_id: None,
                released_reason: None,
                state_changed_at: None,
            },
            CollectionReservationState::Consumed {
                transaction_id,
                consumed_at,
            } => Self {
                amount: reservation.amount.to_string(),
                state: "consumed",
                transaction_id: Some(transaction_id.value.clone()),
                released_reason: None,
                state_changed_at: Some(consumed_at.to_string()),
            },
            CollectionReservationState::Released {
                reason,
                released_at,
            } => Self {
                amount: reservation.amount.to_string(),
                state: "released",
                transaction_id: None,
                released_reason: Some(match reason {
                    deposits::ReservationReleaseReason::TerminalFailure => "terminal_failure",
                    deposits::ReservationReleaseReason::Reorg => "reorg",
                }),
                state_changed_at: Some(released_at.to_string()),
            },
        }
    }
}

#[derive(Serialize)]
struct CollectionLegDto {
    leg_id: String,
    position: u16,
    kind: &'static str,
    planned_amount: Option<String>,
    state: &'static str,
    transaction_id: Option<String>,
    watch_id: Option<String>,
    attempt_count: u32,
    allocation: Option<CollectionAllocationDto>,
    allocations: Vec<CollectionAllocationDto>,
    last_error: Option<CollectionErrorDto>,
    updated_at: String,
}

impl From<&CollectionLeg> for CollectionLegDto {
    fn from(leg: &CollectionLeg) -> Self {
        Self {
            leg_id: leg.id.0.clone(),
            position: leg.position,
            kind: collection_leg_kind(leg.kind),
            planned_amount: leg.planned_amount.map(|amount| amount.to_string()),
            state: collection_leg_state(&leg.state),
            transaction_id: leg
                .state
                .transaction_id()
                .map(|transaction_id| transaction_id.value.clone()),
            watch_id: leg.watch_id.as_ref().map(|watch_id| watch_id.0.clone()),
            attempt_count: leg.attempt_count,
            allocation: leg.allocation.as_ref().map(CollectionAllocationDto::from),
            allocations: leg
                .allocations
                .iter()
                .map(CollectionAllocationDto::from)
                .collect(),
            last_error: leg.last_error.as_ref().map(CollectionErrorDto::from),
            updated_at: leg.updated_at.to_string(),
        }
    }
}

#[derive(Serialize)]
struct CollectionAllocationDto {
    deposit_id: String,
    asset: AssetResponseDto,
    gross_debit: String,
    master_credit: String,
    allocated_fee_asset: AssetResponseDto,
    allocated_fee: String,
}

impl From<&deposits::CollectionAllocation> for CollectionAllocationDto {
    fn from(allocation: &deposits::CollectionAllocation) -> Self {
        Self {
            deposit_id: allocation.deposit_id.0.clone(),
            asset: AssetResponseDto::from(&allocation.asset),
            gross_debit: allocation.gross_debit.to_string(),
            master_credit: allocation.master_credit.to_string(),
            allocated_fee_asset: AssetResponseDto::from(&allocation.allocated_fee_asset),
            allocated_fee: allocation.allocated_fee.to_string(),
        }
    }
}

#[derive(Serialize)]
struct CollectionErrorDto {
    code: String,
    message: String,
    retryable: bool,
}

impl From<&deposits::SafeCollectionError> for CollectionErrorDto {
    fn from(error: &deposits::SafeCollectionError) -> Self {
        Self {
            code: error.code.clone(),
            message: error.message.clone(),
            retryable: error.retryable,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconciliationPageQuery {
    deposit_id: Option<String>,
    cursor: Option<String>,
    state: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct ReconciliationPageDto {
    reconciliations: Vec<ReconciliationDto>,
    next_cursor: Option<String>,
}

async fn reconciliations(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    query: Result<Query<ReconciliationPageQuery>, QueryRejection>,
) -> Result<Json<ReconciliationPageDto>, ApiError> {
    let Query(query) = valid_query(query)?;
    let deposit_id = query
        .deposit_id
        .map(|deposit_id| {
            validate_opaque_id(&deposit_id, "deposit_id")?;
            Ok::<_, ApiError>(DepositId(deposit_id))
        })
        .transpose()?;
    if let Some(deposit_id) = deposit_id.as_ref() {
        let deposit = find_deposit(&state, &deposit_id.0).await?;
        authorize_user(&state, principal, &deposit.user_id).await?;
    }
    let cursor = query
        .cursor
        .map(|cursor| {
            validate_opaque_id(&cursor, "cursor")?;
            Ok::<_, ApiError>(ReconciliationCaseId(cursor))
        })
        .transpose()?;
    let open_only = match query.state.as_deref() {
        None | Some("open") => true,
        Some("all") => false,
        Some(_) => {
            return Err(ApiError::bad_request(
                "invalid_reconciliation_state",
                "reconciliation state filter must be `open` or `all`",
            ));
        }
    };
    let page = state
        .repository
        .cases(ReconciliationPageRequest {
            deposit_id,
            after: cursor,
            limit: page_limit(&state.limits, query.limit)?,
            open_only,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    Ok(Json(ReconciliationPageDto {
        reconciliations: page.cases.iter().map(ReconciliationDto::from).collect(),
        next_cursor: page.next.map(|cursor| cursor.0),
    }))
}

async fn reconciliation(
    State(state): State<Arc<ApiState>>,
    Path(case_id): Path<String>,
) -> Result<Json<ReconciliationDto>, ApiError> {
    validate_opaque_id(&case_id, "case_id")?;
    let case = state
        .repository
        .case(&ReconciliationCaseId(case_id))
        .await
        .map_err(ApiError::from_deposit)?
        .ok_or_else(|| {
            ApiError::not_found(
                "reconciliation_not_found",
                "reconciliation case does not exist",
            )
        })?;
    Ok(Json(ReconciliationDto::from(&case)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveReconciliationDto {
    resolution: String,
    expected_ledger_head: Option<String>,
    external_reference: Option<String>,
    reason: String,
}

async fn resolve_reconciliation(
    State(state): State<Arc<ApiState>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Path(case_id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<ResolveReconciliationDto>, JsonRejection>,
) -> Result<Json<ReconciliationDto>, ApiError> {
    validate_opaque_id(&case_id, "case_id")?;
    let client_key = idempotency_key(&headers)?;
    let Json(body) = body.map_err(|_| {
        ApiError::bad_request(
            "invalid_json",
            "reconciliation resolution body is not valid JSON",
        )
    })?;
    validate_reason(&body.reason)?;
    let decision = match body.resolution.as_str() {
        "reverse_credit" => {
            let expected = body.expected_ledger_head.as_deref().ok_or_else(|| {
                ApiError::bad_request(
                    "missing_expected_ledger_head",
                    "reverse_credit requires expected_ledger_head",
                )
            })?;
            validate_opaque_id(expected, "expected_ledger_head")?;
            if body.external_reference.is_some() {
                return Err(ApiError::bad_request(
                    "invalid_resolution_fields",
                    "reverse_credit does not accept external_reference",
                ));
            }
            ReconciliationDecision::ReverseCredit {
                expected_head: LedgerEntryId(expected.to_owned()),
                reason: body.reason.clone(),
            }
        }
        "accept_liability" => {
            if body.expected_ledger_head.is_some() || body.external_reference.is_some() {
                return Err(ApiError::bad_request(
                    "invalid_resolution_fields",
                    "accept_liability accepts only a reason",
                ));
            }
            ReconciliationDecision::AcceptLiability {
                reason: body.reason.clone(),
            }
        }
        "external_debt_recorded" => {
            let reference = body.external_reference.as_deref().ok_or_else(|| {
                ApiError::bad_request(
                    "missing_external_reference",
                    "external_debt_recorded requires external_reference",
                )
            })?;
            validate_opaque_id(reference, "external_reference")?;
            if body.expected_ledger_head.is_some() {
                return Err(ApiError::bad_request(
                    "invalid_resolution_fields",
                    "external_debt_recorded does not accept expected_ledger_head",
                ));
            }
            ReconciliationDecision::ExternalDebtRecorded {
                external_reference: reference.to_owned(),
                reason: body.reason.clone(),
            }
        }
        _ => {
            return Err(ApiError::bad_request(
                "invalid_resolution",
                "resolution must be reverse_credit, accept_liability, or external_debt_recorded",
            ));
        }
    };
    let expected_head = body.expected_ledger_head.as_deref().unwrap_or("");
    let external_reference = body.external_reference.as_deref().unwrap_or("");
    let case = state
        .repository
        .resolve_case(ResolveReconciliation {
            command: CommandIdentity {
                principal: command_principal(principal),
                operation: CommandOperation::ResolveReconciliation,
                client_key,
                request_hash: request_hash(
                    "resolve_reconciliation",
                    &[
                        &case_id,
                        &body.resolution,
                        expected_head,
                        external_reference,
                        &body.reason,
                    ],
                ),
            },
            case_id: ReconciliationCaseId(case_id),
            decision,
            resolved_at: unix_timestamp()?,
        })
        .await
        .map_err(ApiError::from_deposit)?;
    Ok(Json(ReconciliationDto::from(&case)))
}

#[derive(Serialize)]
struct ReconciliationDto {
    case_id: String,
    deposit_id: String,
    triggering_event_id: String,
    reason: ReconciliationReasonDto,
    state: &'static str,
    resolution: Option<ReconciliationResolutionDto>,
    resolved_at: Option<String>,
    created_at: String,
}

impl From<&ReconciliationCase> for ReconciliationDto {
    fn from(case: &ReconciliationCase) -> Self {
        let (state, resolution, resolved_at) = match &case.state {
            ReconciliationState::Open => ("open", None, None),
            ReconciliationState::Resolved {
                resolution,
                resolved_at,
            } => (
                "resolved",
                Some(ReconciliationResolutionDto::from(resolution)),
                Some(resolved_at.to_string()),
            ),
            ReconciliationState::LegacyResolved {
                description,
                resolved_at,
            } => (
                "legacy_resolved",
                Some(ReconciliationResolutionDto::Legacy {
                    description: description.clone(),
                }),
                Some(resolved_at.to_string()),
            ),
        };
        Self {
            case_id: case.id.0.clone(),
            deposit_id: case.deposit_id.0.clone(),
            triggering_event_id: case.triggering_event_id.0.clone(),
            reason: ReconciliationReasonDto::from(&case.reason),
            state,
            resolution,
            resolved_at,
            created_at: case.created_at.to_string(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReconciliationResolutionDto {
    ReverseCredit {
        reason: String,
        ledger_entry_id: Option<String>,
    },
    AcceptLiability {
        reason: String,
    },
    ExternalDebtRecorded {
        external_reference: String,
        reason: String,
    },
    Legacy {
        description: String,
    },
}

impl From<&ReconciliationResolution> for ReconciliationResolutionDto {
    fn from(resolution: &ReconciliationResolution) -> Self {
        match &resolution.decision {
            ReconciliationDecision::ReverseCredit { reason, .. } => Self::ReverseCredit {
                reason: reason.clone(),
                ledger_entry_id: resolution.ledger_entry_id.as_ref().map(|id| id.0.clone()),
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

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReconciliationReasonDto {
    PostCreditReorg {
        accounted: String,
        corrected_confirmed: String,
    },
    ReservedSpendConflict {
        collection_id: String,
        transaction_id: String,
    },
}

impl From<&ReconciliationReason> for ReconciliationReasonDto {
    fn from(reason: &ReconciliationReason) -> Self {
        match reason {
            ReconciliationReason::PostCreditReorg {
                accounted,
                corrected_confirmed,
            } => Self::PostCreditReorg {
                accounted: accounted.to_string(),
                corrected_confirmed: corrected_confirmed.to_string(),
            },
            ReconciliationReason::ReservedSpendConflict {
                collection_id,
                transaction_id,
            } => Self::ReservedSpendConflict {
                collection_id: collection_id.0.clone(),
                transaction_id: transaction_id.value.clone(),
            },
        }
    }
}

#[derive(Serialize)]
struct AdminStatusDto {
    service: &'static str,
    scope: AdminScopeResponseDto,
    policy_version: String,
    policy_digest: String,
    ingestion_cursor: Option<String>,
    projection_cursor: Option<String>,
    event_lag: Option<String>,
    ready: bool,
    indexer_ready: bool,
    wallet_ready: bool,
    job_backlog: usize,
    job_backlog_truncated: bool,
    max_page_size: usize,
}

#[derive(Serialize)]
struct AdminScopeResponseDto {
    chain: String,
    network: String,
    chain_id: Option<u64>,
}

async fn admin_status(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<AdminStatusDto>, ApiError> {
    use deposits::{ConsumerCheckpointName, ObservationConsumerCheckpoints};

    let ingestion = state
        .repository
        .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
        .await
        .map_err(ApiError::from_deposit)?;
    let projection = state
        .repository
        .consumer_checkpoint(ConsumerCheckpointName::IxProjection)
        .await
        .map_err(ApiError::from_deposit)?;
    let jobs = state
        .repository
        .jobs(JobPageRequest {
            after: None,
            limit: state.limits.max_page_size(),
        })
        .await
        .map_err(ApiError::from_deposit)?;
    let job_backlog = jobs
        .jobs
        .iter()
        .filter(|job| !matches!(job.state, JobState::Succeeded | JobState::Failed))
        .count();
    let event_lag = match (ingestion.cursor, projection.cursor) {
        (Some(ingestion), Some(projection)) => ingestion
            .0
            .checked_sub(projection.0)
            .map(|lag| lag.to_string()),
        (Some(ingestion), None) => Some(ingestion.0.to_string()),
        (None, None) => Some("0".to_owned()),
        (None, Some(_)) => None,
    };
    Ok(Json(AdminStatusDto {
        service: "payment-service",
        scope: AdminScopeResponseDto {
            chain: state.policy.scope().chain.0.clone(),
            network: state.policy.scope().network.clone(),
            chain_id: state.policy.ethereum_chain_id(),
        },
        policy_version: state.policy.version().to_string(),
        policy_digest: state.policy.digest_hex(),
        ingestion_cursor: ingestion.cursor.map(|cursor| cursor.0.to_string()),
        projection_cursor: projection.cursor.map(|cursor| cursor.0.to_string()),
        event_lag,
        ready: state.health.is_ready(),
        indexer_ready: state.indexer_health.is_ready(),
        wallet_ready: state.wallet_health.is_ready(),
        job_backlog,
        job_backlog_truncated: jobs.next.is_some(),
        max_page_size: state.limits.max_page_size(),
    }))
}

async fn find_deposit(state: &ApiState, id: &str) -> Result<Deposit, ApiError> {
    state
        .repository
        .deposit(&DepositId(id.to_owned()))
        .await
        .map_err(ApiError::from_deposit)?
        .ok_or_else(|| ApiError::not_found("deposit_not_found", "deposit does not exist"))
}

async fn find_collection(state: &ApiState, id: &str) -> Result<Collection, ApiError> {
    state
        .repository
        .collection(&CollectionId(id.to_owned()))
        .await
        .map_err(ApiError::from_deposit)?
        .ok_or_else(|| ApiError::not_found("collection_not_found", "collection does not exist"))
}

fn valid_query<T>(query: Result<Query<T>, QueryRejection>) -> Result<Query<T>, ApiError> {
    query.map_err(|_| {
        ApiError::bad_request("invalid_query", "query parameters are missing or invalid")
    })
}

fn page_limit(limits: &RequestLimits, requested: Option<usize>) -> Result<usize, ApiError> {
    limits.page_size(requested).map_err(|error| {
        ApiError::bad_request("invalid_page_size", format!("invalid page size: {error}"))
    })
}

async fn authorize_user(
    state: &ApiState,
    principal: AuthenticatedPrincipal,
    user_id: &UserId,
) -> Result<(), ApiError> {
    let user = state
        .repository
        .user(user_id)
        .await
        .map_err(ApiError::from_deposit)?
        .ok_or_else(|| internal_invariant("resource points to a missing PS user"))?;
    authorize_owner(principal, &user.owner)
}

fn authorize_owner(
    principal: AuthenticatedPrincipal,
    owner: &CommandPrincipal,
) -> Result<(), ApiError> {
    if principal.role() == PrincipalRole::Administrator || owner.0 == principal.idempotency_scope()
    {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "authenticated principal does not own this Payment Service resource",
            false,
        ))
    }
}

fn command_principal(principal: AuthenticatedPrincipal) -> CommandPrincipal {
    CommandPrincipal(principal.idempotency_scope().to_owned())
}

fn exchange_user_owner() -> CommandPrincipal {
    CommandPrincipal(USER_OWNER_PRINCIPAL.to_owned())
}

fn parse_asset(input: &str, policy: &ActivePaymentPolicy) -> Result<AssetId, ApiError> {
    let canonical = match policy {
        ActivePaymentPolicy::Bitcoin(_) if input == "native" => input.to_owned(),
        ActivePaymentPolicy::Bitcoin(_) => {
            return Err(ApiError::bad_request(
                "invalid_asset",
                "Bitcoin Payment Service supports only the `native` asset",
            ));
        }
        ActivePaymentPolicy::Ethereum(_) if input == "native" => input.to_owned(),
        ActivePaymentPolicy::Ethereum(_) => {
            let address = input.parse::<EthereumAddress>().map_err(|_| {
                ApiError::bad_request(
                    "invalid_asset",
                    "asset must be `native` or a canonical ERC-20 address",
                )
            })?;
            let canonical = address.to_string();
            if input != canonical {
                return Err(ApiError::bad_request(
                    "invalid_asset",
                    "ERC-20 asset address must use lowercase canonical hexadecimal",
                ));
            }
            canonical
        }
    };
    let asset = AssetId {
        chain: policy.scope().chain.clone(),
        asset: canonical,
    };
    if !policy.enabled_asset(&asset) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_asset",
            "asset is not enabled by the active Payment Service policy",
            false,
        ));
    }
    Ok(asset)
}

fn parse_positive_amount(input: &str, name: &str) -> Result<AtomicAmount, ApiError> {
    let amount = parse_amount(input, name)?;
    if amount.is_zero() {
        return Err(ApiError::bad_request(
            "invalid_amount",
            format!("{name} must be greater than zero"),
        ));
    }
    Ok(amount)
}

fn parse_amount(input: &str, name: &str) -> Result<AtomicAmount, ApiError> {
    input.parse::<AtomicAmount>().map_err(|_| {
        ApiError::bad_request(
            "invalid_amount",
            format!("{name} must be a canonical unsigned 256-bit decimal string"),
        )
    })
}

fn validate_reason(reason: &str) -> Result<(), ApiError> {
    if reason.trim().is_empty() || reason.len() > 1_024 {
        return Err(ApiError::bad_request(
            "invalid_reason",
            "accounting reason must be non-blank and at most 1024 bytes",
        ));
    }
    Ok(())
}

fn payment_progress(balances: deposits::DepositBalances, expected: AtomicAmount) -> &'static str {
    if balances.received.is_zero() {
        "unseen"
    } else if balances.confirmed.is_zero() {
        "included"
    } else if balances.confirmed < expected {
        "partial"
    } else if balances.confirmed == expected {
        "paid"
    } else {
        "overpaid"
    }
}

fn deposit_state_name(state: &DepositState) -> &'static str {
    match state {
        DepositState::AwaitingWatch => "awaiting_watch",
        DepositState::Active { .. } => "active",
        DepositState::Expired { .. } => "expired",
        DepositState::Closed => "closed",
    }
}

fn parse_deposit_state_filter(input: &str) -> Result<DepositStateKind, ApiError> {
    match input {
        "awaiting_watch" => Ok(DepositStateKind::AwaitingWatch),
        "active" => Ok(DepositStateKind::Active),
        "expired" => Ok(DepositStateKind::Expired),
        "closed" => Ok(DepositStateKind::Closed),
        _ => Err(ApiError::bad_request(
            "invalid_deposit_state",
            "deposit state filter must be `awaiting_watch`, `active`, `expired`, or `closed`",
        )),
    }
}

const fn ledger_observation_kind(kind: LedgerObservationKind) -> &'static str {
    match kind {
        LedgerObservationKind::Incoming => "incoming",
        LedgerObservationKind::Collection => "collection",
        LedgerObservationKind::GasFunding => "gas_funding",
        LedgerObservationKind::OtherBalanceChange => "other_balance_change",
        LedgerObservationKind::Reorg => "reorg",
    }
}

const fn movement_kind(kind: MovementKind) -> &'static str {
    match kind {
        MovementKind::Transfer => "transfer",
        MovementKind::Input => "input",
        MovementKind::Output => "output",
        MovementKind::InternalTransfer => "internal_transfer",
        MovementKind::Mint => "mint",
        MovementKind::Burn => "burn",
    }
}

const fn collection_mode(mode: CollectionMode) -> &'static str {
    match mode {
        CollectionMode::AccountTransfer => "account_transfer",
        CollectionMode::UtxoBatch => "utxo_batch",
        CollectionMode::TokenWithGas => "token_with_gas",
    }
}

const fn collection_state(state: CollectionState) -> &'static str {
    match state {
        CollectionState::Required => "required",
        CollectionState::InProgress => "in_progress",
        CollectionState::Completed => "completed",
        CollectionState::Failed => "failed",
        CollectionState::Reorged => "reorged",
    }
}

const fn collection_leg_kind(kind: CollectionLegKind) -> &'static str {
    match kind {
        CollectionLegKind::GasFunding => "gas_funding",
        CollectionLegKind::Sweep => "sweep",
    }
}

const fn collection_leg_state(state: &CollectionLegState) -> &'static str {
    match state {
        CollectionLegState::Required => "required",
        CollectionLegState::Signed { .. } => "signed",
        CollectionLegState::Broadcast { .. } => "broadcast",
        CollectionLegState::Confirmed { .. } => "confirmed",
        CollectionLegState::Failed { .. } => "failed",
        CollectionLegState::Reorged { .. } => "reorged",
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn unix_timestamp() -> Result<u64, ApiError> {
    SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|duration| duration.as_secs())
        .map_err(|_| internal_invariant("system clock precedes the Unix epoch"))
}

fn internal_invariant(_detail: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_invariant",
        "Payment Service encountered an internal consistency error",
        false,
    )
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, header},
    };
    use bitcoin::{Address, CompressedPublicKey, Network, PublicKey, secp256k1};
    use chain_identity::{CanonicalAddress, CanonicalTransactionId, ChainId};
    use deposits::{
        CreateDeposit, CreateDepositWithLedger, DepositBalances, EnsureUser, IdempotencyKey,
        InitializePaymentDatabase, PaymentDatabaseMetadataStore,
    };
    use http_support::BearerToken;
    use indexing::{
        BlockHeight, EventCursor, IndexScope, MovementId, ObservationEventId, ObservationRevision,
        ObservedTransaction,
    };
    use serde_json::{Value, json};
    use signer::KeyLocator;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::*;
    use crate::{bitcoin_policy::BitcoinPaymentPolicy, policy::PaymentPolicy};

    fn amount(value: u64) -> AtomicAmount {
        value.to_string().parse().expect("test amount must parse")
    }

    #[test]
    fn progress_uses_canonical_current_balances() {
        let expected = amount(100);
        assert_eq!(
            payment_progress(deposits::DepositBalances::default(), expected),
            "unseen"
        );
        assert_eq!(
            payment_progress(
                deposits::DepositBalances {
                    received: amount(50),
                    ..Default::default()
                },
                expected
            ),
            "included"
        );
        assert_eq!(
            payment_progress(
                deposits::DepositBalances {
                    received: amount(50),
                    confirmed: amount(50),
                    ..Default::default()
                },
                expected
            ),
            "partial"
        );
        assert_eq!(
            payment_progress(
                deposits::DepositBalances {
                    received: expected,
                    confirmed: expected,
                    ..Default::default()
                },
                expected
            ),
            "paid"
        );
        assert_eq!(
            payment_progress(
                deposits::DepositBalances {
                    received: amount(101),
                    confirmed: amount(101),
                    ..Default::default()
                },
                expected
            ),
            "overpaid"
        );
    }

    #[test]
    fn page_limits_and_deposit_state_filters_are_strict() {
        let limits = RequestLimits::new(1024, 25, 100).expect("test limits must be valid");
        assert_eq!(page_limit(&limits, None).expect("default must parse"), 25);
        assert_eq!(
            page_limit(&limits, Some(100)).expect("maximum must parse"),
            100
        );
        assert!(page_limit(&limits, Some(0)).is_err());
        assert!(page_limit(&limits, Some(101)).is_err());
        assert_eq!(
            parse_deposit_state_filter("expired").expect("state must parse"),
            DepositStateKind::Expired
        );
        assert!(parse_deposit_state_filter("Expired").is_err());
    }

    #[test]
    fn ledger_response_does_not_echo_client_idempotency_keys() {
        let entry = LedgerEntry {
            id: LedgerEntryId("ledger-1".to_owned()),
            deposit_id: DepositId("deposit-1".to_owned()),
            previous: None,
            cause: LedgerEntryCause::Opened {
                idempotency_key: IdempotencyKey("client-secret-idempotency-key".to_owned()),
            },
            balances: DepositBalances::default(),
            recorded_at: 10,
        };
        let encoded = serde_json::to_string(&LedgerEntryDto::from(&entry))
            .expect("ledger DTO must serialize");
        assert!(encoded.contains("\"kind\":\"opened\""));
        assert!(!encoded.contains("client-secret-idempotency-key"));
    }

    #[test]
    fn deposit_observation_returns_only_ledger_attributed_movements() {
        let relevant = movement(
            "movement-relevant",
            "0x1111111111111111111111111111111111111111",
        );
        let unrelated = movement(
            "movement-unrelated",
            "0x2222222222222222222222222222222222222222",
        );
        let event = ObservationEvent {
            id: ObservationEventId("event-1".to_owned()),
            cursor: EventCursor(7),
            watch_ids: Vec::new(),
            previous_status: None,
            transaction: ObservedTransaction {
                scope: IndexScope {
                    chain: ChainId("ethereum".to_owned()),
                    network: "test".to_owned(),
                },
                transaction_id: CanonicalTransactionId {
                    chain: ChainId("ethereum".to_owned()),
                    value: "0xtransaction".to_owned(),
                },
                revision: ObservationRevision(3),
                status: TransactionStatus::Pending,
                movements: vec![relevant, unrelated],
                fee: None,
                first_seen_at: 11,
                observed_at: 12,
            },
        };
        let dto = DepositObservationDto::new(
            Some(&LedgerEntryId("ledger-2".to_owned())),
            &event,
            13,
            &[MovementId("movement-relevant".to_owned())],
        );
        let encoded = serde_json::to_value(dto).expect("observation DTO must serialize");
        let movements = encoded["movements"]
            .as_array()
            .expect("movements must be an array");
        assert_eq!(movements.len(), 1);
        assert_eq!(movements[0]["movement_id"], "movement-relevant");
        assert!(!encoded.to_string().contains("movement-unrelated"));
        assert!(
            !encoded
                .to_string()
                .contains("0x2222222222222222222222222222222222222222")
        );
    }

    fn movement(id: &str, to: &str) -> ValueMovement {
        ValueMovement {
            id: MovementId(id.to_owned()),
            asset: AssetId {
                chain: ChainId("ethereum".to_owned()),
                asset: "native".to_owned(),
            },
            amount: amount(10),
            from: None,
            to: Some(CanonicalAddress {
                chain: ChainId("ethereum".to_owned()),
                value: to.to_owned(),
            }),
            kind: MovementKind::Transfer,
        }
    }

    struct HttpFixture {
        _directory: TempDir,
        router: Router,
        repository: Repository,
        first_head: String,
        reconciliation_head: String,
    }

    async fn http_fixture() -> HttpFixture {
        let directory = TempDir::new().expect("temporary directory must be available");
        let repository = PersistentPaymentRepository::new(
            RocksDbStorage::open(directory.path()).expect("test RocksDB must open"),
        );
        let ethereum_policy = test_policy();
        let policy = Arc::new(ActivePaymentPolicy::Ethereum(ethereum_policy));
        repository
            .initialize_or_validate(InitializePaymentDatabase {
                scope: policy.scope().clone(),
                active_policy: policy.identity(),
                initialized_at: 1,
            })
            .await
            .expect("test database metadata must initialize");
        repository
            .ensure_user(EnsureUser {
                id: UserId("user-1".to_owned()),
                owner: exchange_user_owner(),
                first_seen_at: 1,
            })
            .await
            .expect("test user must persist");
        let first = create_test_deposit(&repository, "deposit-1", "idem-deposit-1", 1).await;
        create_test_deposit(&repository, "deposit-2", "idem-deposit-2", 2).await;
        let reconciliation_deposit = create_test_deposit(
            &repository,
            "deposit-reconciliation",
            "idem-deposit-reconciliation",
            3,
        )
        .await;
        repository
            .open_case(ReconciliationCase {
                id: ReconciliationCaseId("reconciliation-1".to_owned()),
                deposit_id: reconciliation_deposit.deposit.id.clone(),
                triggering_event_id: ObservationEventId("event-reconciliation-1".to_owned()),
                reason: ReconciliationReason::PostCreditReorg {
                    accounted: amount(1),
                    corrected_confirmed: amount(0),
                },
                state: ReconciliationState::Open,
                created_at: 4,
            })
            .await
            .expect("typed reconciliation fixture must persist");
        let credentials = Arc::new(Credentials::new(
            BearerToken::new("exchange-secret").expect("test token must parse"),
            BearerToken::new("administrator-secret").expect("test token must parse"),
        ));
        let router = router(
            Arc::new(ApiState::new(
                repository.clone(),
                policy,
                RequestLimits::new(1024 * 1024, 25, 100).expect("test limits must be valid"),
            )),
            credentials,
        );
        HttpFixture {
            _directory: directory,
            router,
            repository,
            first_head: first.ledger.id.0,
            reconciliation_head: reconciliation_deposit.ledger.id.0,
        }
    }

    async fn create_test_deposit(
        repository: &Repository,
        id: &str,
        idempotency_key: &str,
        address_suffix: u8,
    ) -> deposits::CreatedDeposit {
        let address = format!("0x{:040x}", address_suffix);
        repository
            .create_with_ledger(CreateDepositWithLedger {
                deposit: CreateDeposit {
                    id: DepositId(id.to_owned()),
                    idempotency_key: IdempotencyKey(idempotency_key.to_owned()),
                    user_id: UserId("user-1".to_owned()),
                    asset: AssetId {
                        chain: ChainId("ethereum".to_owned()),
                        asset: "native".to_owned(),
                    },
                    address: CanonicalAddress {
                        chain: ChainId("ethereum".to_owned()),
                        value: address,
                    },
                    key: KeyLocator::Identifier(format!("test-key-{address_suffix}")),
                    key_purpose: DEPOSIT_KEY_PURPOSE.to_owned(),
                    expected: amount(100),
                    birthday: BlockHeight(10),
                    expires_at: 1_000,
                    created_at: u64::from(address_suffix),
                },
                ledger_recorded_at: u64::from(address_suffix),
            })
            .await
            .expect("test deposit and ledger must persist")
    }

    fn test_policy() -> PaymentPolicy {
        PaymentPolicy::from_json(
            br#"{
                "version": 1,
                "scope": {"chain": "ethereum", "network": "test", "chain_id": 1},
                "deposit_ttl_seconds": 3600,
                "assets": [{
                    "asset": "native",
                    "master_destination": "0x1111111111111111111111111111111111111111",
                    "minimum_collection_amount": "1"
                }],
                "fees": {
                    "max_fee_per_gas": "100",
                    "max_priority_fee_per_gas": "10",
                    "max_gas_limit": 21000,
                    "max_total_fee": "2100000"
                },
                "gas_funder": {
                    "address": "0x2222222222222222222222222222222222222222",
                    "key_locator": "test:gas-funder",
                    "maximum_funding_amount": "1000000"
                }
            }"#,
        )
        .expect("test policy must parse")
    }

    struct BitcoinHttpFixture {
        _directory: TempDir,
        router: Router,
        repository: Repository,
    }

    async fn bitcoin_http_fixture() -> BitcoinHttpFixture {
        let directory = TempDir::new().expect("temporary directory must be available");
        let repository = PersistentPaymentRepository::new(
            RocksDbStorage::open(directory.path()).expect("test RocksDB must open"),
        );
        let policy = Arc::new(ActivePaymentPolicy::Bitcoin(
            BitcoinPaymentPolicy::from_json(
                br#"{
                    "version": 1,
                    "scope": {"chain": "bitcoin", "network": "regtest"},
                    "deposit_address_kind": "p2wpkh",
                    "deposit_ttl_seconds": 3600,
                    "master_destination": "bcrt1qtwxw3vnj3f29szvhvr84k0aekcrhh9cla5nxa0",
                    "minimum_collection_satoshis": "10000",
                    "minimum_spend_confirmations": 2,
                    "requested_satoshis_per_kvb": "1000",
                    "maximum_satoshis_per_kvb": "5000",
                    "maximum_absolute_fee_satoshis": "50000",
                    "maximum_deposits": 2,
                    "maximum_inputs": 10
                }"#,
            )
            .expect("complete Bitcoin test policy must parse"),
        ));
        repository
            .initialize_or_validate(InitializePaymentDatabase {
                scope: policy.scope().clone(),
                active_policy: policy.identity(),
                initialized_at: 1,
            })
            .await
            .expect("Bitcoin test database metadata must initialize");
        for (index, user_id) in ["bitcoin-user-a", "bitcoin-user-b"].into_iter().enumerate() {
            repository
                .ensure_user(EnsureUser {
                    id: UserId(user_id.to_owned()),
                    owner: exchange_user_owner(),
                    first_seen_at: u64::try_from(index + 1).expect("test index must fit u64"),
                })
                .await
                .expect("Bitcoin test user must persist");
        }
        create_test_bitcoin_deposit(&repository, "bitcoin-deposit-a", "bitcoin-user-a", 1).await;
        create_test_bitcoin_deposit(&repository, "bitcoin-deposit-b", "bitcoin-user-b", 2).await;
        let credentials = Arc::new(Credentials::new(
            BearerToken::new("exchange-secret").expect("test token must parse"),
            BearerToken::new("administrator-secret").expect("test token must parse"),
        ));
        let router = router(
            Arc::new(ApiState::new(
                repository.clone(),
                policy,
                RequestLimits::new(1024 * 1024, 25, 100).expect("test limits must be valid"),
            )),
            credentials,
        );
        BitcoinHttpFixture {
            _directory: directory,
            router,
            repository,
        }
    }

    async fn create_test_bitcoin_deposit(
        repository: &Repository,
        deposit_id: &str,
        user_id: &str,
        key_byte: u8,
    ) {
        let secret = secp256k1::SecretKey::from_slice(&[key_byte; 32])
            .expect("test secret scalar must be valid");
        let secp = secp256k1::Secp256k1::new();
        let public_key = PublicKey::new(secp256k1::PublicKey::from_secret_key(&secp, &secret));
        let compressed =
            CompressedPublicKey::try_from(public_key).expect("test public key must be compressed");
        let address = Address::p2wpkh(&compressed, Network::Regtest).to_string();
        repository
            .create_with_ledger(CreateDepositWithLedger {
                deposit: CreateDeposit {
                    id: DepositId(deposit_id.to_owned()),
                    idempotency_key: IdempotencyKey(format!("create-{deposit_id}")),
                    user_id: UserId(user_id.to_owned()),
                    asset: AssetId {
                        chain: ChainId("bitcoin".to_owned()),
                        asset: "native".to_owned(),
                    },
                    address: CanonicalAddress {
                        chain: ChainId("bitcoin".to_owned()),
                        value: address,
                    },
                    key: KeyLocator::Identifier(format!("bitcoin-test-key-{key_byte}")),
                    key_purpose: DEPOSIT_KEY_PURPOSE.to_owned(),
                    expected: amount(100_000),
                    birthday: BlockHeight(10),
                    expires_at: 1_000,
                    created_at: u64::from(key_byte),
                },
                ledger_recorded_at: u64::from(key_byte),
            })
            .await
            .expect("Bitcoin test deposit and ledger must persist");
    }

    async fn call_json(
        fixture: &HttpFixture,
        method: Method,
        path: &str,
        token: &str,
        idempotency_key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        if let Some(idempotency_key) = idempotency_key {
            builder = builder.header("Idempotency-Key", idempotency_key);
        }
        let body = match body {
            Some(body) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&body).expect("test JSON must encode"))
            }
            None => Body::empty(),
        };
        let response = fixture
            .router
            .clone()
            .oneshot(builder.body(body).expect("test request must build"))
            .await
            .expect("router must respond");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response body must be readable");
        let body = serde_json::from_slice(&bytes).expect("response must be JSON");
        (status, body)
    }

    async fn call_bitcoin_json(
        fixture: &BitcoinHttpFixture,
        method: Method,
        path: &str,
        token: &str,
        idempotency_key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        if let Some(idempotency_key) = idempotency_key {
            builder = builder.header("Idempotency-Key", idempotency_key);
        }
        let body = match body {
            Some(body) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&body).expect("test JSON must encode"))
            }
            None => Body::empty(),
        };
        let response = fixture
            .router
            .clone()
            .oneshot(builder.body(body).expect("test request must build"))
            .await
            .expect("router must respond");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response body must be readable");
        let body = serde_json::from_slice(&bytes).expect("response must be JSON");
        (status, body)
    }

    #[tokio::test]
    async fn collection_command_replay_keeps_stable_job_and_resource_ids() {
        let fixture = http_fixture().await;
        let command = json!({"deposit_id": "deposit-1"});
        let first = call_json(
            &fixture,
            Method::POST,
            "/v1/collections",
            "exchange-secret",
            Some("collect-once"),
            Some(command.clone()),
        )
        .await;
        assert_eq!(first.0, StatusCode::ACCEPTED);
        assert!(
            first.1["job_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("job_"))
        );
        assert!(
            first.1["collection_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("col_"))
        );
        let replay = call_json(
            &fixture,
            Method::POST,
            "/v1/collections",
            "exchange-secret",
            Some("collect-once"),
            Some(command),
        )
        .await;
        assert_eq!(replay, first);

        let conflicting = call_json(
            &fixture,
            Method::POST,
            "/v1/collections",
            "exchange-secret",
            Some("collect-once"),
            Some(json!({"deposit_id": "deposit-2"})),
        )
        .await;
        assert_eq!(conflicting.0, StatusCode::CONFLICT);
        assert_eq!(conflicting.1["code"], "conflict");
    }

    #[tokio::test]
    async fn bitcoin_batch_command_is_canonical_cross_user_and_idempotent() {
        let fixture = bitcoin_http_fixture().await;
        let command = json!({
            "deposit_ids": ["bitcoin-deposit-b", "bitcoin-deposit-a"]
        });
        let first = call_bitcoin_json(
            &fixture,
            Method::POST,
            "/v1/collections",
            "exchange-secret",
            Some("collect-bitcoin-batch-once"),
            Some(command.clone()),
        )
        .await;
        assert_eq!(first.0, StatusCode::ACCEPTED);
        let job_id = first.1["job_id"]
            .as_str()
            .expect("accepted Bitcoin batch must return a job ID");
        let job = fixture
            .repository
            .job(&JobId(job_id.to_owned()))
            .await
            .expect("Bitcoin batch job read must succeed")
            .expect("Bitcoin batch job must persist");
        let JobPayload::CreateUtxoBatchCollection(payload) = job.payload else {
            panic!("Bitcoin batch command must persist a UTXO payload");
        };
        assert_eq!(
            payload.deposit_ids,
            vec![
                DepositId("bitcoin-deposit-a".to_owned()),
                DepositId("bitcoin-deposit-b".to_owned()),
            ]
        );

        let replay = call_bitcoin_json(
            &fixture,
            Method::POST,
            "/v1/collections",
            "exchange-secret",
            Some("collect-bitcoin-batch-once"),
            Some(command),
        )
        .await;
        assert_eq!(replay, first);

        let changed_membership = call_bitcoin_json(
            &fixture,
            Method::POST,
            "/v1/collections",
            "exchange-secret",
            Some("collect-bitcoin-batch-once"),
            Some(json!({"deposit_ids": ["bitcoin-deposit-a"]})),
        )
        .await;
        assert_eq!(changed_membership.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn bitcoin_batch_command_rejects_ambiguous_or_over_limit_membership() {
        let fixture = bitcoin_http_fixture().await;
        for body in [
            json!({"deposit_id": "bitcoin-deposit-a"}),
            json!({"deposit_ids": []}),
            json!({"deposit_ids": ["bitcoin-deposit-a", "bitcoin-deposit-a"]}),
        ] {
            let response = call_bitcoin_json(
                &fixture,
                Method::POST,
                "/v1/collections",
                "exchange-secret",
                Some("invalid-bitcoin-batch"),
                Some(body),
            )
            .await;
            assert_eq!(response.0, StatusCode::BAD_REQUEST);
            assert_eq!(response.1["code"], "invalid_collection");
        }

        create_test_bitcoin_deposit(
            &fixture.repository,
            "bitcoin-deposit-c",
            "bitcoin-user-a",
            3,
        )
        .await;
        let over_limit = call_bitcoin_json(
            &fixture,
            Method::POST,
            "/v1/collections",
            "exchange-secret",
            Some("over-limit-bitcoin-batch"),
            Some(json!({
                "deposit_ids": [
                    "bitcoin-deposit-a",
                    "bitcoin-deposit-b",
                    "bitcoin-deposit-c"
                ]
            })),
        )
        .await;
        assert_eq!(over_limit.0, StatusCode::BAD_REQUEST);
        assert_eq!(over_limit.1["code"], "invalid_collection");
    }

    #[tokio::test]
    async fn awaiting_watch_deposit_never_discloses_address_or_birthday() {
        let fixture = http_fixture().await;
        let (status, body) = call_json(
            &fixture,
            Method::GET,
            "/v1/deposits/deposit-1",
            "exchange-secret",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "awaiting_watch");
        assert!(body["address"].is_null());
        assert!(body["birthday"].is_null());
        assert!(!body.to_string().contains("test-key-1"));
    }

    #[tokio::test]
    async fn accounting_requires_admin_and_replays_the_same_immutable_row() {
        let fixture = http_fixture().await;
        let body = json!({
            "next_accounted": "0",
            "expected_ledger_head": fixture.first_head.clone(),
            "reason": "confirmed manual credit remains zero"
        });
        let forbidden = call_json(
            &fixture,
            Method::POST,
            "/v1/deposits/deposit-1/accounting",
            "exchange-secret",
            Some("account-once"),
            Some(body.clone()),
        )
        .await;
        assert_eq!(forbidden.0, StatusCode::FORBIDDEN);

        let first = call_json(
            &fixture,
            Method::POST,
            "/v1/deposits/deposit-1/accounting",
            "administrator-secret",
            Some("account-once"),
            Some(body.clone()),
        )
        .await;
        assert_eq!(first.0, StatusCode::OK);
        assert_eq!(first.1["ledger_entry"]["balances"]["accounted"], "0");
        assert!(!first.1.to_string().contains("account-once"));

        let replay = call_json(
            &fixture,
            Method::POST,
            "/v1/deposits/deposit-1/accounting",
            "administrator-secret",
            Some("account-once"),
            Some(body),
        )
        .await;
        assert_eq!(replay, first);

        let conflicting = call_json(
            &fixture,
            Method::POST,
            "/v1/deposits/deposit-1/accounting",
            "administrator-secret",
            Some("account-once"),
            Some(json!({
                "next_accounted": "0",
                "expected_ledger_head": fixture.first_head.clone(),
                "reason": "different command content"
            })),
        )
        .await;
        assert_eq!(conflicting.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn reconciliation_resolution_is_admin_only_idempotent_and_reverses_credit_atomically() {
        let fixture = http_fixture().await;
        let body = json!({
            "resolution": "reverse_credit",
            "expected_ledger_head": fixture.reconciliation_head.clone(),
            "reason": "reverse business credit after the canonical reorg"
        });
        let forbidden = call_json(
            &fixture,
            Method::POST,
            "/v1/reconciliations/reconciliation-1/resolve",
            "exchange-secret",
            Some("resolve-reconciliation-once"),
            Some(body.clone()),
        )
        .await;
        assert_eq!(forbidden.0, StatusCode::FORBIDDEN);
        assert_eq!(forbidden.1["code"], "forbidden");

        let first = call_json(
            &fixture,
            Method::POST,
            "/v1/reconciliations/reconciliation-1/resolve",
            "administrator-secret",
            Some("resolve-reconciliation-once"),
            Some(body.clone()),
        )
        .await;
        assert_eq!(first.0, StatusCode::OK);
        assert_eq!(first.1["state"], "resolved");
        assert_eq!(first.1["resolution"]["kind"], "reverse_credit");
        assert_eq!(
            first.1["resolution"]["reason"],
            "reverse business credit after the canonical reorg"
        );
        let ledger_entry_id = first.1["resolution"]["ledger_entry_id"]
            .as_str()
            .expect("reverse-credit response must identify its ledger row");
        assert!(!ledger_entry_id.is_empty());

        let current = fixture
            .repository
            .current(&DepositId("deposit-reconciliation".to_owned()))
            .await
            .expect("ledger read must succeed")
            .expect("reconciliation deposit must retain a ledger head");
        assert_eq!(current.id.0, ledger_entry_id);
        assert_eq!(
            current.previous,
            Some(LedgerEntryId(fixture.reconciliation_head.clone()))
        );
        assert!(current.balances.accounted <= current.balances.confirmed);
        assert!(matches!(
            current.cause,
            LedgerEntryCause::ReconciliationResolution {
                case_id,
                reason,
                ..
            } if case_id == ReconciliationCaseId("reconciliation-1".to_owned())
                && reason == "reverse business credit after the canonical reorg"
        ));

        let replay = call_json(
            &fixture,
            Method::POST,
            "/v1/reconciliations/reconciliation-1/resolve",
            "administrator-secret",
            Some("resolve-reconciliation-once"),
            Some(body),
        )
        .await;
        assert_eq!(replay, first);

        let changed = call_json(
            &fixture,
            Method::POST,
            "/v1/reconciliations/reconciliation-1/resolve",
            "administrator-secret",
            Some("resolve-reconciliation-once"),
            Some(json!({
                "resolution": "reverse_credit",
                "expected_ledger_head": fixture.reconciliation_head.clone(),
                "reason": "different resolution request content"
            })),
        )
        .await;
        assert_eq!(changed.0, StatusCode::CONFLICT);
        assert_eq!(changed.1["code"], "conflict");
    }

    #[tokio::test]
    async fn reconciliation_resolution_rejects_incompatible_field_combinations() {
        let fixture = http_fixture().await;
        let cases = [
            (
                json!({
                    "resolution": "reverse_credit",
                    "reason": "missing the expected ledger head"
                }),
                "missing_expected_ledger_head",
            ),
            (
                json!({
                    "resolution": "reverse_credit",
                    "expected_ledger_head": fixture.reconciliation_head.clone(),
                    "external_reference": "debt-1",
                    "reason": "reverse credit cannot record debt"
                }),
                "invalid_resolution_fields",
            ),
            (
                json!({
                    "resolution": "accept_liability",
                    "expected_ledger_head": fixture.reconciliation_head.clone(),
                    "reason": "accept liability cannot select a ledger head"
                }),
                "invalid_resolution_fields",
            ),
            (
                json!({
                    "resolution": "accept_liability",
                    "external_reference": "debt-2",
                    "reason": "accept liability cannot record external debt"
                }),
                "invalid_resolution_fields",
            ),
            (
                json!({
                    "resolution": "external_debt_recorded",
                    "reason": "missing the external debt reference"
                }),
                "missing_external_reference",
            ),
            (
                json!({
                    "resolution": "external_debt_recorded",
                    "expected_ledger_head": fixture.reconciliation_head.clone(),
                    "external_reference": "debt-3",
                    "reason": "external debt cannot select a ledger head"
                }),
                "invalid_resolution_fields",
            ),
        ];

        for (index, (body, expected_code)) in cases.into_iter().enumerate() {
            let response = call_json(
                &fixture,
                Method::POST,
                "/v1/reconciliations/reconciliation-1/resolve",
                "administrator-secret",
                Some(&format!("invalid-resolution-{index}")),
                Some(body),
            )
            .await;
            assert_eq!(response.0, StatusCode::BAD_REQUEST);
            assert_eq!(response.1["code"], expected_code);
        }
    }

    #[tokio::test]
    async fn admin_status_reports_scope_cursors_readiness_backlog_and_limits() {
        let fixture = http_fixture().await;
        let forbidden = call_json(
            &fixture,
            Method::GET,
            "/v1/admin/status",
            "exchange-secret",
            None,
            None,
        )
        .await;
        assert_eq!(forbidden.0, StatusCode::FORBIDDEN);

        let response = call_json(
            &fixture,
            Method::GET,
            "/v1/admin/status",
            "administrator-secret",
            None,
            None,
        )
        .await;
        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(
            response.1,
            json!({
                "service": "payment-service",
                "scope": {"chain": "ethereum", "network": "test", "chain_id": 1},
                "policy_version": "1",
                "policy_digest": test_policy().digest_hex(),
                "ingestion_cursor": null,
                "projection_cursor": null,
                "event_lag": "0",
                "ready": false,
                "indexer_ready": false,
                "wallet_ready": false,
                "job_backlog": 0,
                "job_backlog_truncated": false,
                "max_page_size": 100
            })
        );
    }
}
