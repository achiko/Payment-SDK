use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
    time::{Duration, SystemTime},
};

use axum::{Router, extract::State, response::IntoResponse, routing::get};

use chain_identity::{AssetId, AtomicAmount, ChainId};
use deposits::{
    AwaitingWatchPageRequest, BalanceDirection, BoxFuture, ClaimJob, CloseDeposit, Collection,
    CollectionAllocation, CollectionLeg, CollectionLegKind, CollectionLegState,
    CollectionPageRequest, CollectionReservationState, CollectionStore, CollectionTransitionGuard,
    ConfirmCollectionLeg, ConsumerCheckpointName, Deposit, DepositAddressRequest,
    DepositAddressSource, DepositError, DepositErrorKind, DepositId, DepositLedger,
    DepositPageRequest, DepositState, DepositStateKind, DepositStore, DepositWatchCoordinator,
    FailCollectionLeg, GeneratedDepositAddress, InitializePaymentDatabase, Job, JobError,
    JobPayload, JobState, JobStore, LedgerEffect, LedgerObservationTransition,
    MigratePaymentDatabase, MirrorObservation, MirrorOutcome, MirroredObservation,
    ObservationConsumerCheckpoints, ObservationEventLog, ObservationLogRequest,
    PaymentDatabaseMetadataStore, PersistentPaymentRepository, ProjectObservation,
    ReconciliationCase, ReconciliationCaseId, ReconciliationReason, ReconciliationState,
    ReconciliationStore, RecordObservation, RegisterDeposit, ReleaseCollectionReservation,
    ReorgCollectionLeg, ReservationReleaseReason, SafeCollectionError, TransitionJob,
    apply_observation_transition,
};
use http_support::{BearerToken, HealthState, HttpServerConfig, RequestLimits, TransportSecurity};
use indexing::{EventCursor, IndexError, IndexScope, MovementId, SyncPhase, TransactionStatus};
use storage_rocksdb::RocksDbStorage;
use telemetry::PrometheusTelemetry;
use tokio::{sync::watch, task::JoinSet, time::MissedTickBehavior};

use crate::{
    api::{self, ApiState},
    auth::Credentials,
    collection_executor,
    config::{
        BackupOptions, IngestOptions, MigrationOptions, ProjectionStatusOptions, ReconcileOptions,
        ServeOptions,
    },
    indexer_client::IndexerClient,
    policy::PaymentPolicy,
    wallet_client::WalletClient,
};

type Repository = PersistentPaymentRepository<RocksDbStorage>;

const JOB_LEASE_DURATION: Duration = Duration::from_secs(5 * 60);
const MAX_JOB_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
const MIGRATION_PAGE_SIZE: usize = 1_000;

pub async fn backup(options: BackupOptions) -> Result<(), RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let info = storage.create_backup(&options.backup_path).await?;
    tracing::info!(
        backup_id = info.backup_id,
        files = info.file_count,
        "Payment Service RocksDB backup verified"
    );
    Ok(())
}

pub async fn migrate(options: MigrationOptions) -> Result<(), RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let policy = PaymentPolicy::load(&options.policy_path).map_err(|error| {
        RuntimeError::configuration(format!(
            "failed to load Payment Service policy ({:?}): {error}",
            error.kind
        ))
    })?;
    if policy.scope.chain.0 != "ethereum" || policy.scope.network != options.network {
        return Err(RuntimeError::configuration(
            "migration network must exactly match the Ethereum policy scope",
        ));
    }
    let outcome = RocksDbStorage::migrate(&options.database.database_path, &options.backup_path)?;
    tracing::info!(
        backup_id = outcome.backup.backup_id,
        from = outcome.report.previous.0,
        to = outcome.report.current.0,
        "pre-migration Payment Service backup and physical schema validation completed"
    );
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    let report = repository
        .migrate_and_bind(MigratePaymentDatabase {
            scope: policy.scope.clone(),
            active_policy: policy.identity(),
            migrated_at: unix_timestamp()?,
            page_size: MIGRATION_PAGE_SIZE,
        })
        .await?;
    tracing::info!(
        previous_schema = ?report.previous_domain_schema_version,
        current_schema = report.metadata.domain_schema_version,
        deposits = report.deposits,
        ledger_entries = report.ledger_entries,
        mirrored_observations = report.mirrored_observations,
        deposit_observations = report.deposit_observations,
        reconciliation_cases = report.reconciliation_cases,
        users = report.users,
        jobs = report.jobs,
        collections = report.collections,
        deposit_indexes_rebuilt = report.deposit_indexes_rebuilt,
        "Payment Service semantic migration validated and bound metadata"
    );
    Ok(())
}

pub async fn serve(options: ServeOptions) -> Result<(), RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let policy = Arc::new(PaymentPolicy::load(&options.policy_path).map_err(|error| {
        RuntimeError::configuration(format!(
            "failed to load Payment Service policy ({:?}): {error}",
            error.kind
        ))
    })?);
    if policy.scope.network != options.indexer.network || policy.scope.chain.0 != "ethereum" {
        return Err(RuntimeError::configuration(
            "policy scope must match the configured Ethereum Indexer network",
        ));
    }

    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    repository
        .initialize_or_validate(InitializePaymentDatabase {
            scope: policy.scope.clone(),
            active_policy: policy.identity(),
            initialized_at: unix_timestamp()?,
        })
        .await?;
    // Validate both dependency clients before opening public listeners. Their
    // constructors perform no live transaction or signing operation.
    let indexer = Arc::new(IndexerClient::new(&options.indexer)?);
    let wallet = Arc::new(WalletClient::new(&options.wallet)?);

    let credentials = Arc::new(Credentials::new(
        BearerToken::new(options.ordinary_bearer_token.expose())
            .map_err(RuntimeError::configuration)?,
        BearerToken::new(options.admin_bearer_token.expose())
            .map_err(RuntimeError::configuration)?,
    ));
    let health = HealthState::new(false);
    let indexer_health = HealthState::new(false);
    let wallet_health = HealthState::new(false);
    let limits = RequestLimits::default();
    let api_state = Arc::new(
        ApiState::new(repository.clone(), Arc::clone(&policy), limits.clone()).with_runtime_health(
            health.clone(),
            indexer_health.clone(),
            wallet_health.clone(),
        ),
    );
    let security = if options.http_bind.ip().is_loopback() {
        TransportSecurity::PlaintextLoopback
    } else {
        TransportSecurity::TlsTerminatedUpstream
    };
    let server_config = HttpServerConfig::new(options.http_bind, security, None, limits)
        .with_custom_authentication();
    let application_router = http_support::service_router(
        api::router(api_state, credentials),
        &server_config,
        health.clone(),
    )
    .map_err(RuntimeError::configuration)?;

    let telemetry = PrometheusTelemetry::install()
        .map_err(|error| RuntimeError::invariant(error.to_string()))?;
    let metrics_config = HttpServerConfig::new(
        options.metrics_bind,
        TransportSecurity::PlaintextLoopback,
        None,
        RequestLimits::default(),
    );
    let metrics_router = Router::new()
        .route("/metrics", get(metrics))
        .with_state(telemetry);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = JoinSet::<Result<(), RuntimeError>>::new();

    let worker_repository = repository.clone();
    let worker_indexer = Arc::clone(&indexer);
    let worker_wallet = Arc::clone(&wallet);
    let worker_policy = Arc::clone(&policy);
    let worker_shutdown = shutdown_rx.clone();
    let worker_interval = options.worker_interval();
    let worker_page_size = options.worker_page_size;
    tasks.spawn(async move {
        run_job_worker(
            JobWorkerContext {
                repository: worker_repository,
                indexer: worker_indexer,
                wallet: worker_wallet,
                policy: worker_policy,
                interval: worker_interval,
                page_size: worker_page_size,
            },
            worker_shutdown,
        )
        .await
    });

    let watch_repository = repository.clone();
    let watch_indexer = Arc::clone(&indexer);
    let watch_scope = policy.scope.clone();
    let watch_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        run_watch_reconciliation_worker(
            watch_repository,
            watch_indexer,
            watch_scope,
            worker_interval,
            worker_page_size,
            watch_shutdown,
        )
        .await
    });

    let ingestion_repository = repository.clone();
    let ingestion_indexer = Arc::clone(&indexer);
    let ingestion_scope = policy.scope.clone();
    let ingestion_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        run_ingestion_worker(
            ingestion_repository,
            ingestion_indexer,
            ingestion_scope,
            worker_interval,
            worker_page_size,
            ingestion_shutdown,
        )
        .await
    });

    let projection_repository = repository.clone();
    let projection_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        run_projection_worker(
            projection_repository,
            worker_interval,
            worker_page_size,
            projection_shutdown,
        )
        .await
    });

    let expiration_repository = repository.clone();
    let expiration_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        run_expiration_worker(
            expiration_repository,
            worker_interval,
            worker_page_size,
            expiration_shutdown,
        )
        .await
    });

    let readiness_repository = repository;
    let readiness_indexer = Arc::clone(&indexer);
    let readiness_wallet = Arc::clone(&wallet);
    let readiness_scope = policy.scope.clone();
    let readiness_health = health.clone();
    let readiness_indexer_health = indexer_health;
    let readiness_wallet_health = wallet_health;
    let readiness_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        run_readiness_worker(
            ReadinessWorkerContext {
                repository: readiness_repository,
                indexer: readiness_indexer,
                wallet: readiness_wallet,
                scope: readiness_scope,
                health: readiness_health,
                indexer_health: readiness_indexer_health,
                wallet_health: readiness_wallet_health,
                interval: worker_interval,
            },
            readiness_shutdown,
        )
        .await
    });

    let api_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        http_support::serve(
            application_router,
            &server_config,
            shutdown_signal(api_shutdown),
        )
        .await
        .map_err(|error| RuntimeError::invariant(error.to_string()))
    });
    let metrics_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        http_support::serve(
            metrics_router,
            &metrics_config,
            shutdown_signal(metrics_shutdown),
        )
        .await
        .map_err(|error| RuntimeError::invariant(error.to_string()))
    });

    tracing::info!(
        network = %policy.scope.network,
        policy_version = policy.version,
        "Payment Service HTTP runtime and durable workers started"
    );
    let termination = tokio::select! {
        signal = termination_signal() => {
            signal?;
            None
        }
        task = tasks.join_next() => Some(task),
    };
    health.set_ready(false);
    let _ignored = shutdown_tx.send(true);

    let deadline = tokio::time::sleep(options.shutdown_grace());
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                tasks.abort_all();
                break;
            }
            task = tasks.join_next(), if !tasks.is_empty() => {
                match task {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(error))) => tracing::error!(error = %error, "Payment Service task failed during shutdown"),
                    Some(Err(error)) => tracing::error!(error = %error, "Payment Service task panicked during shutdown"),
                    None => break,
                }
            }
            else => break,
        }
    }

    match termination {
        None => Ok(()),
        Some(Some(Ok(Ok(())))) => Err(RuntimeError::invariant(
            "a supervised Payment Service task stopped unexpectedly",
        )),
        Some(Some(Ok(Err(error)))) => Err(error),
        Some(Some(Err(error))) => Err(RuntimeError::invariant(format!(
            "Payment Service task panicked: {error}"
        ))),
        Some(None) => Err(RuntimeError::invariant(
            "Payment Service supervisor has no running tasks",
        )),
    }
}

struct JobWorkerContext {
    repository: Repository,
    indexer: Arc<IndexerClient>,
    wallet: Arc<WalletClient>,
    policy: Arc<PaymentPolicy>,
    interval: Duration,
    page_size: usize,
}

async fn run_job_worker(
    context: JobWorkerContext,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let JobWorkerContext {
        repository,
        indexer,
        wallet,
        policy,
        interval,
        page_size,
    } = context;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = ticker.tick() => {}
        }

        for _ in 0..page_size {
            if *shutdown.borrow() {
                return Ok(());
            }
            let now = unix_timestamp()?;
            let lease_expires_at = now
                .checked_add(JOB_LEASE_DURATION.as_secs())
                .ok_or_else(|| RuntimeError::invariant("job lease timestamp overflowed"))?;
            let Some(job) = repository
                .claim_next(ClaimJob {
                    now,
                    lease_expires_at,
                    scan_limit: page_size,
                })
                .await?
            else {
                break;
            };

            let result = process_job(
                &repository,
                indexer.as_ref(),
                wallet.as_ref(),
                policy.as_ref(),
                &policy.scope,
                &job,
            )
            .await;
            finish_job_attempt(&repository, job, result).await?;
        }
    }
}

async fn run_watch_reconciliation_worker(
    repository: Repository,
    indexer: Arc<IndexerClient>,
    scope: IndexScope,
    interval: Duration,
    page_size: usize,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let disabled_addresses = DisabledAddressGeneration;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = ticker.tick() => {}
        }
        let coordinator = DepositWatchCoordinator::new(
            &repository,
            indexer.as_ref(),
            &disabled_addresses,
            scope.clone(),
        );
        match coordinator.resume_awaiting(page_size).await {
            Ok(activated) if activated > 0 => {
                tracing::info!(activated, "resumed durable AwaitingWatch deposits");
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind,
                    DepositErrorKind::Other | DepositErrorKind::InvalidState
                ) =>
            {
                tracing::warn!(kind = ?error.kind, "IX watch reconciliation is temporarily unavailable");
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn process_job(
    repository: &Repository,
    indexer: &IndexerClient,
    wallet: &WalletClient,
    policy: &PaymentPolicy,
    scope: &IndexScope,
    job: &Job,
) -> Result<(), DepositError> {
    match &job.payload {
        JobPayload::CreateDeposit(payload) => {
            if &payload.scope != scope {
                return Err(domain_invariant(
                    "deposit job scope differs from the running Payment Service scope",
                ));
            }
            let coordinator =
                DepositWatchCoordinator::new(repository, indexer, wallet, scope.clone());
            let deposit = coordinator
                .register(RegisterDeposit {
                    scope: payload.scope.clone(),
                    id: payload.deposit_id.clone(),
                    idempotency_key: job.command.client_key.clone(),
                    user_id: payload.user_id.clone(),
                    asset: payload.asset.clone(),
                    expected: payload.expected,
                    key_purpose: payload.key_purpose.clone(),
                    expires_at: payload.expires_at,
                    created_at: payload.created_at,
                })
                .await?;
            if !matches!(deposit.state, DepositState::Active { .. }) {
                return Err(domain_invariant(
                    "deposit job completed without an active IX watch",
                ));
            }
            Ok(())
        }
        JobPayload::CloseDeposit(payload) => close_deposit_workflow(repository, payload).await,
        JobPayload::CreateCollection(_) | JobPayload::RetryCollection(_) => {
            collection_executor::process_collection_job(
                repository, indexer, wallet, policy, scope, job,
            )
            .await
        }
    }
}

async fn close_deposit_workflow(
    repository: &Repository,
    payload: &deposits::CloseDepositJob,
) -> Result<(), DepositError> {
    let deposit = repository
        .deposit(&payload.deposit_id)
        .await?
        .ok_or_else(|| DepositError {
            kind: DepositErrorKind::NotFound,
            message: "close job deposit does not exist".to_owned(),
        })?;
    if deposit.user_id != payload.user_id {
        return Err(domain_invariant(
            "close job user association differs from the durable deposit",
        ));
    }
    if matches!(deposit.state, DepositState::Closed) {
        return Ok(());
    }

    let ledger = repository
        .current(&deposit.id)
        .await?
        .ok_or_else(|| domain_invariant("close job deposit has no ledger head"))?;
    if !ledger.balances.balance.is_zero() {
        return Err(DepositError {
            kind: DepositErrorKind::InvalidState,
            message: "deposit cannot close while its current balance is non-zero".to_owned(),
        });
    }
    if repository.automatic_actions_blocked(&deposit.id).await? {
        return Err(DepositError {
            kind: DepositErrorKind::InvalidState,
            message: "deposit cannot close while reconciliation is unresolved".to_owned(),
        });
    }
    let collections = repository
        .collections_for_deposit(
            &deposit.id,
            CollectionPageRequest {
                after: None,
                limit: 1_000,
            },
        )
        .await?;
    if collections.collections.iter().any(|collection| {
        matches!(
            collection.reservation.state,
            CollectionReservationState::Active
        )
    }) {
        return Err(DepositError {
            kind: DepositErrorKind::InvalidState,
            message: "deposit cannot close while a collection reservation is active".to_owned(),
        });
    }

    match repository
        .close(CloseDeposit {
            deposit_id: deposit.id.clone(),
            expected_state: deposit.state,
            expected_ledger_head: ledger.id,
        })
        .await
    {
        Ok(()) => Ok(()),
        Err(error) if is_optimistic_conflict(&error) => {
            let current = repository
                .deposit(&deposit.id)
                .await?
                .ok_or_else(|| domain_invariant("close job deposit disappeared during retry"))?;
            resolve_close_state_race(&current.state)
        }
        Err(error) => Err(error),
    }
}

fn resolve_close_state_race(current_state: &DepositState) -> Result<(), DepositError> {
    if matches!(current_state, DepositState::Closed) {
        return Ok(());
    }
    Err(DepositError {
        kind: DepositErrorKind::InvalidState,
        message: "deposit close eligibility changed concurrently; close will retry".to_owned(),
    })
}

async fn finish_job_attempt(
    repository: &Repository,
    job: Job,
    result: Result<(), DepositError>,
) -> Result<(), RuntimeError> {
    let expected_state = job.state.clone();
    if !matches!(expected_state, JobState::Running { .. }) {
        return Err(RuntimeError::invariant(
            "claimed Payment Service job is not running",
        ));
    }
    let now = unix_timestamp()?;
    let (next_state, error) = match result {
        Ok(()) => (JobState::Succeeded, None),
        Err(error) if retryable_job_error(&error) => {
            let next_attempt_at = now
                .checked_add(retry_delay(job.attempt_count).as_secs())
                .ok_or_else(|| RuntimeError::invariant("job retry timestamp overflowed"))?;
            (
                JobState::WaitingRetry { next_attempt_at },
                Some(safe_job_error(&error, true)),
            )
        }
        Err(error) => (JobState::Failed, Some(safe_job_error(&error, false))),
    };
    repository
        .transition(TransitionJob {
            id: job.id,
            expected_state,
            next_state,
            error,
            updated_at: now,
        })
        .await?;
    Ok(())
}

fn retry_delay(attempt_count: u32) -> Duration {
    let exponent = attempt_count.saturating_sub(1).min(8);
    Duration::from_secs(1_u64 << exponent).min(MAX_JOB_RETRY_DELAY)
}

fn retryable_job_error(error: &DepositError) -> bool {
    matches!(
        error.kind,
        DepositErrorKind::Storage | DepositErrorKind::Other | DepositErrorKind::InvalidState
    )
}

fn safe_job_error(error: &DepositError, retryable: bool) -> JobError {
    let (code, message) = match error.kind {
        DepositErrorKind::NotFound => ("resource_not_found", "required resource does not exist"),
        DepositErrorKind::Conflict => ("conflict", "durable state changed concurrently"),
        DepositErrorKind::InvalidState => ("dependency_not_ready", error.message.as_str()),
        DepositErrorKind::InvariantViolation => ("invalid_job", "durable job input is invalid"),
        DepositErrorKind::Storage => (
            "storage_unavailable",
            "Payment Service storage is unavailable",
        ),
        DepositErrorKind::Other => (
            "dependency_unavailable",
            "a Payment Service dependency is unavailable",
        ),
    };
    JobError {
        code: code.to_owned(),
        message: message.to_owned(),
        retryable,
    }
}

fn domain_invariant(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}

async fn run_ingestion_worker(
    repository: Repository,
    indexer: Arc<IndexerClient>,
    scope: IndexScope,
    interval: Duration,
    page_size: usize,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = ticker.tick() => {}
        }
        let checkpoint = repository
            .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
            .await?;
        let page = match indexer.events(&scope, checkpoint.cursor, page_size).await {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(kind = ?error.kind, "IX event ingestion is temporarily unavailable");
                continue;
            }
        };
        let mut expected_cursor = checkpoint.cursor;
        for event in page.events {
            if event.transaction.scope != scope {
                return Err(RuntimeError::invariant(
                    "Indexer event does not belong to the configured PS scope",
                ));
            }
            let existing = repository.observation(&event.id).await?;
            let received_at = match &existing {
                Some(existing) if existing.event == event => existing.received_at,
                Some(_) => {
                    return Err(RuntimeError::invariant(
                        "mirrored IX event ID was reused with a different payload",
                    ));
                }
                None => unix_timestamp()?,
            };
            if expected_cursor.is_some_and(|cursor| event.cursor < cursor) {
                if existing.is_none() {
                    return Err(RuntimeError::invariant(
                        "Indexer delivered an unknown event behind the ingestion cursor",
                    ));
                }
                continue;
            }
            let cursor = event.cursor;
            repository
                .mirror_and_advance(MirrorObservation {
                    expected_cursor,
                    observation: MirroredObservation { event, received_at },
                })
                .await?;
            expected_cursor = Some(cursor);
        }
    }
}

async fn run_expiration_worker(
    repository: Repository,
    interval: Duration,
    page_size: usize,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut after = None;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = ticker.tick() => {}
        }
        let now = unix_timestamp()?;
        let page = repository
            .deposits(DepositPageRequest {
                after: after.clone(),
                limit: page_size,
                user_id: None,
                state: Some(DepositStateKind::Active),
            })
            .await?;
        for deposit in &page.deposits {
            expire_deposit_if_due(&repository, deposit, now).await?;
        }
        after = page.next;
    }
}

async fn expire_deposit_if_due(
    repository: &Repository,
    deposit: &Deposit,
    now: u64,
) -> Result<(), DepositError> {
    let DepositState::Active { watch_id } = &deposit.state else {
        return Ok(());
    };
    if deposit.expires_at > now {
        return Ok(());
    }
    match repository
        .set_state(
            &deposit.id,
            DepositState::Expired {
                watch_id: watch_id.clone(),
            },
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(error)
            if is_optimistic_conflict(&error) || error.kind == DepositErrorKind::InvalidState =>
        {
            let current = repository
                .deposit(&deposit.id)
                .await?
                .ok_or_else(|| domain_invariant("expiring deposit disappeared during retry"))?;
            if current.state == deposit.state {
                return Err(error);
            }
            tracing::debug!(
                kind = ?error.kind,
                "deposit lifecycle changed during expiration; reloaded durable state"
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

struct ReadinessWorkerContext {
    repository: Repository,
    indexer: Arc<IndexerClient>,
    wallet: Arc<WalletClient>,
    scope: IndexScope,
    health: HealthState,
    indexer_health: HealthState,
    wallet_health: HealthState,
    interval: Duration,
}

async fn run_readiness_worker(
    context: ReadinessWorkerContext,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let ReadinessWorkerContext {
        repository,
        indexer,
        wallet,
        scope,
        health,
        indexer_health,
        wallet_health,
        interval,
    } = context;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                health.set_ready(false);
                indexer_health.set_ready(false);
                wallet_health.set_ready(false);
                return Ok(());
            }
            _ = ticker.tick() => {}
        }
        let (indexer_status, wallet_ready) =
            tokio::join!(indexer.status(&scope), wallet.readiness());
        let indexer_ready = indexer_status
            .as_ref()
            .is_ok_and(|status| status.scope == scope && status.phase == SyncPhase::Ready);
        let wallet_is_ready = wallet_ready.as_ref().is_ok_and(|ready| *ready);
        indexer_health.set_ready(indexer_ready);
        wallet_health.set_ready(wallet_is_ready);
        let ingestion = repository
            .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
            .await?;
        let projection = repository
            .consumer_checkpoint(ConsumerCheckpointName::IxProjection)
            .await?;
        let projection_caught_up = ingestion.cursor == projection.cursor;
        health.set_ready(indexer_ready && wallet_is_ready && projection_caught_up);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EffectClass {
    Incoming,
    Collection,
    GasFunding,
    OtherDebit,
}

async fn run_projection_worker(
    repository: Repository,
    interval: Duration,
    page_size: usize,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = ticker.tick() => {}
        }
        let checkpoint = repository
            .consumer_checkpoint(ConsumerCheckpointName::IxProjection)
            .await?;
        let page = repository
            .observations(ObservationLogRequest {
                after: checkpoint.cursor,
                limit: page_size,
            })
            .await?;
        let mut expected_cursor = checkpoint.cursor;
        for mirrored in page.observations {
            let Some((affected_deposits, updates, cases)) =
                classify_projection(&repository, &mirrored).await?
            else {
                tracing::warn!(
                    cursor = mirrored.event.cursor.0,
                    "PS projection stopped at a relevant but unresolved IX event"
                );
                break;
            };
            let through = mirrored.event.cursor;
            let projection = repository
                .project_and_advance(ProjectObservation {
                    expected_cursor,
                    through,
                    affected_deposits,
                    ledger_updates: updates,
                    reconciliation_cases: cases,
                })
                .await;
            match projection {
                Ok(_) => expected_cursor = Some(through),
                Err(error) if is_optimistic_conflict(&error) => {
                    tracing::debug!(
                        cursor = through.0,
                        "PS projection state changed concurrently; reloading cursor and ledger heads"
                    );
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn is_optimistic_conflict(error: &DepositError) -> bool {
    error.kind == DepositErrorKind::Conflict
}

async fn classify_projection(
    repository: &Repository,
    mirrored: &MirroredObservation,
) -> Result<
    Option<(
        Vec<DepositId>,
        Vec<RecordObservation>,
        Vec<ReconciliationCase>,
    )>,
    DepositError,
> {
    let event = &mirrored.event;
    let transaction_id = &event.transaction.transaction_id;
    let mut classified = BTreeMap::<DepositId, (Deposit, EffectClass, Vec<MovementId>)>::new();
    let mut affected_deposits = BTreeSet::<DepositId>::new();

    if let Some(reference) = repository.leg_for_transaction(transaction_id).await? {
        let collection = repository
            .collection(&reference.collection_id)
            .await?
            .ok_or_else(|| domain_invariant("collection transaction index is dangling"))?;
        let leg = collection
            .legs
            .iter()
            .find(|leg| leg.id == reference.leg_id)
            .ok_or_else(|| domain_invariant("collection transaction points to a missing leg"))?;
        let deposit = repository
            .deposit(&collection.deposit_id)
            .await?
            .ok_or_else(|| domain_invariant("collection points to a missing deposit"))?;
        affected_deposits.insert(deposit.id.clone());
        if !project_collection_leg_fact(
            repository,
            event,
            &collection,
            leg,
            &deposit,
            mirrored.received_at,
        )
        .await?
        {
            return Ok(None);
        }
        let (class, movements) = match leg.kind {
            CollectionLegKind::Sweep => (
                EffectClass::Collection,
                event
                    .transaction
                    .movements
                    .iter()
                    .filter(|movement| {
                        movement.asset == deposit.asset
                            && movement.from.as_ref() == Some(&deposit.address)
                    })
                    .map(|movement| movement.id.clone())
                    .collect::<Vec<_>>(),
            ),
            CollectionLegKind::GasFunding => (
                EffectClass::GasFunding,
                event
                    .transaction
                    .movements
                    .iter()
                    .filter(|movement| {
                        movement.asset == deposit.asset
                            && movement.to.as_ref() == Some(&deposit.address)
                    })
                    .map(|movement| movement.id.clone())
                    .collect::<Vec<_>>(),
            ),
        };
        // Token gas funding changes a native balance outside the token
        // deposit ledger. Its collection leg is still resolved from IX, but
        // no token-ledger row is appropriate for that event.
        let deposit_paid_fee = event.transaction.fee.as_ref().is_some_and(|fee| {
            fee.asset == deposit.asset && fee.payer.as_ref() == Some(&deposit.address)
        });
        if !movements.is_empty() || deposit_paid_fee {
            classified.insert(deposit.id.clone(), (deposit, class, movements));
        } else if leg.kind == CollectionLegKind::Sweep
            && !matches!(
                event.transaction.status,
                TransactionStatus::Pending
                    | TransactionStatus::Failed { .. }
                    | TransactionStatus::Dropped
                    | TransactionStatus::Replaced { .. }
            )
        {
            return Ok(None);
        }
    } else {
        for movement in &event.transaction.movements {
            if let Some(address) = &movement.to
                && let Some(deposit) = repository.by_address(address).await?
                && movement.asset == deposit.asset
            {
                affected_deposits.insert(deposit.id.clone());
                if !insert_classification(
                    &mut classified,
                    deposit,
                    EffectClass::Incoming,
                    movement.id.clone(),
                ) {
                    return Ok(None);
                }
            }
            if let Some(address) = &movement.from
                && let Some(deposit) = repository.by_address(address).await?
                && movement.asset == deposit.asset
            {
                affected_deposits.insert(deposit.id.clone());
                if !insert_classification(
                    &mut classified,
                    deposit,
                    EffectClass::OtherDebit,
                    movement.id.clone(),
                ) {
                    return Ok(None);
                }
            }
        }
        if let Some(fee) = &event.transaction.fee
            && let Some(payer) = &fee.payer
            && let Some(deposit) = repository.by_address(payer).await?
            && fee.asset == deposit.asset
        {
            affected_deposits.insert(deposit.id.clone());
            match classified.get(&deposit.id) {
                Some((_existing, EffectClass::OtherDebit, _)) => {}
                Some(_) => return Ok(None),
                None => {
                    classified.insert(
                        deposit.id.clone(),
                        (deposit, EffectClass::OtherDebit, Vec::new()),
                    );
                }
            }
        }
    }

    let mut updates = Vec::with_capacity(classified.len());
    let mut cases = Vec::new();
    for (deposit_id, (deposit, class, movement_ids)) in classified {
        let head = repository
            .current(&deposit_id)
            .await?
            .ok_or_else(|| domain_invariant("classified deposit has no ledger head"))?;
        let effect = movement_effect(class, movement_ids);
        let resolved = resolve_runtime_effect(event, &effect)?;
        let next_balances = apply_observation_transition(
            head.balances,
            &LedgerObservationTransition {
                status: event.transaction.status.clone(),
                previous_status: event.previous_status.clone(),
                effect: resolved,
                network_fee: event.transaction.fee.as_ref().and_then(|fee| {
                    (fee.asset == deposit.asset && fee.payer.as_ref() == Some(&deposit.address))
                        .then_some(fee.amount)
                }),
            },
        )
        .map_err(|error| {
            domain_invariant(format!(
                "classified IX event cannot update its ledger: {error}"
            ))
        })?;
        if head.balances.accounted <= head.balances.confirmed
            && next_balances.accounted > next_balances.confirmed
        {
            cases.push(ReconciliationCase {
                id: reconciliation_case_id(event, &deposit_id),
                deposit_id: deposit_id.clone(),
                triggering_event_id: event.id.clone(),
                reason: ReconciliationReason::PostCreditReorg {
                    accounted: next_balances.accounted,
                    corrected_confirmed: next_balances.confirmed,
                },
                state: ReconciliationState::Open,
                created_at: mirrored.received_at,
            });
        }
        updates.push(RecordObservation {
            event_id: event.id.clone(),
            effect,
            deposit_id,
            expected_head: Some(head.id),
            recorded_at: mirrored.received_at,
        });
    }
    Ok(Some((
        affected_deposits.into_iter().collect(),
        updates,
        cases,
    )))
}

async fn project_collection_leg_fact(
    repository: &Repository,
    event: &indexing::ObservationEvent,
    collection: &Collection,
    leg: &CollectionLeg,
    deposit: &Deposit,
    received_at: u64,
) -> Result<bool, DepositError> {
    let transaction_id = &event.transaction.transaction_id;
    let transition_at = collection.updated_at.max(received_at);
    match &event.transaction.status {
        TransactionStatus::Pending | TransactionStatus::Included { .. } => Ok(true),
        TransactionStatus::Confirmed { .. } => {
            if matches!(leg.state, CollectionLegState::Confirmed { .. }) {
                return Ok(leg.state.transaction_id() == Some(transaction_id));
            }
            if !matches!(leg.state, CollectionLegState::Broadcast { .. })
                || leg.state.transaction_id() != Some(transaction_id)
                || leg.watch_id.is_none()
            {
                return Ok(false);
            }
            let allocation = if leg.kind == CollectionLegKind::Sweep {
                Some(collection_allocation(event, collection, deposit)?)
            } else {
                None
            };
            repository
                .confirm_leg(ConfirmCollectionLeg {
                    collection_id: collection.id.clone(),
                    leg_id: leg.id.clone(),
                    expected: collection_guard(collection, leg),
                    transaction_id: transaction_id.clone(),
                    allocation,
                    confirmed_at: transition_at,
                })
                .await?;
            Ok(true)
        }
        TransactionStatus::Failed { .. }
        | TransactionStatus::Dropped
        | TransactionStatus::Replaced { .. } => {
            let error = SafeCollectionError {
                code: match &event.transaction.status {
                    TransactionStatus::Failed { .. } => "ix_failed",
                    TransactionStatus::Dropped => "ix_dropped",
                    TransactionStatus::Replaced { .. } => "ix_replaced",
                    _ => return Ok(false),
                }
                .to_owned(),
                message: "Indexer reported a terminal collection transaction fact".to_owned(),
                retryable: false,
            };
            let failed = match &leg.state {
                CollectionLegState::Failed {
                    transaction_id: existing,
                } if existing == transaction_id => collection.clone(),
                CollectionLegState::Broadcast {
                    transaction_id: existing,
                } if existing == transaction_id => {
                    repository
                        .fail_leg(FailCollectionLeg {
                            collection_id: collection.id.clone(),
                            leg_id: leg.id.clone(),
                            expected: collection_guard(collection, leg),
                            transaction_id: transaction_id.clone(),
                            error,
                            failed_at: transition_at,
                        })
                        .await?
                }
                _ => return Ok(false),
            };
            release_collection_reservation(
                repository,
                failed,
                ReservationReleaseReason::TerminalFailure,
                transition_at,
            )
            .await?;
            Ok(true)
        }
        TransactionStatus::Reorged { .. } => {
            // An included-but-not-confirmed orphan leaves the leg broadcast so
            // it can be re-included without a new signature. A confirmed
            // orphan reverses attribution and requires explicit retry.
            if matches!(leg.state, CollectionLegState::Broadcast { .. }) {
                return Ok(leg.state.transaction_id() == Some(transaction_id));
            }
            let reorged = match &leg.state {
                CollectionLegState::Reorged {
                    transaction_id: existing,
                } if existing == transaction_id => collection.clone(),
                CollectionLegState::Confirmed {
                    transaction_id: existing,
                } if existing == transaction_id => {
                    repository
                        .reorg_leg(ReorgCollectionLeg {
                            collection_id: collection.id.clone(),
                            leg_id: leg.id.clone(),
                            expected: collection_guard(collection, leg),
                            transaction_id: transaction_id.clone(),
                            error: SafeCollectionError {
                                code: "ix_reorged".to_owned(),
                                message: "Indexer corrected a confirmed collection transaction"
                                    .to_owned(),
                                retryable: false,
                            },
                            reorged_at: transition_at,
                        })
                        .await?
                }
                _ => return Ok(false),
            };
            release_collection_reservation(
                repository,
                reorged,
                ReservationReleaseReason::Reorg,
                transition_at,
            )
            .await?;
            Ok(true)
        }
    }
}

async fn release_collection_reservation(
    repository: &Repository,
    collection: Collection,
    reason: ReservationReleaseReason,
    released_at: u64,
) -> Result<(), DepositError> {
    if collection.reservation.state != CollectionReservationState::Active {
        return Ok(());
    }
    repository
        .release_reservation(ReleaseCollectionReservation {
            collection_id: collection.id,
            expected_collection_state: collection.state,
            expected_reservation_state: CollectionReservationState::Active,
            reason,
            released_at: collection.updated_at.max(released_at),
        })
        .await?;
    Ok(())
}

fn collection_allocation(
    event: &indexing::ObservationEvent,
    collection: &Collection,
    deposit: &Deposit,
) -> Result<CollectionAllocation, DepositError> {
    let deposit_debit = sum_movement_amounts(event, |movement| {
        movement.asset == collection.asset && movement.from.as_ref() == Some(&deposit.address)
    })?;
    let master_credit = sum_movement_amounts(event, |movement| {
        movement.asset == collection.asset && movement.to.as_ref() == Some(&collection.destination)
    })?;
    if deposit_debit.is_zero() || master_credit.is_zero() {
        return Err(domain_invariant(
            "confirmed sweep is missing deposit debit or master credit attribution",
        ));
    }
    let (allocated_fee_asset, allocated_fee) = event
        .transaction
        .fee
        .as_ref()
        .filter(|fee| fee.payer.as_ref() == Some(&deposit.address))
        .map_or_else(
            || {
                (
                    AssetId {
                        chain: collection.asset.chain.clone(),
                        asset: "native".to_owned(),
                    },
                    AtomicAmount::ZERO,
                )
            },
            |fee| (fee.asset.clone(), fee.amount),
        );
    let gross_debit = if allocated_fee_asset == collection.asset {
        deposit_debit
            .checked_add(&allocated_fee)
            .map_err(|_| domain_invariant("collection gross debit overflowed"))?
    } else {
        deposit_debit
    };
    Ok(CollectionAllocation {
        deposit_id: deposit.id.clone(),
        asset: collection.asset.clone(),
        gross_debit,
        master_credit,
        allocated_fee_asset,
        allocated_fee,
    })
}

fn sum_movement_amounts(
    event: &indexing::ObservationEvent,
    mut predicate: impl FnMut(&indexing::ValueMovement) -> bool,
) -> Result<AtomicAmount, DepositError> {
    event
        .transaction
        .movements
        .iter()
        .filter(|movement| predicate(movement))
        .try_fold(AtomicAmount::ZERO, |sum, movement| {
            sum.checked_add(&movement.amount)
                .map_err(|_| domain_invariant("collection attribution amount overflowed"))
        })
}

fn collection_guard(collection: &Collection, leg: &CollectionLeg) -> CollectionTransitionGuard {
    CollectionTransitionGuard {
        collection_state: collection.state,
        leg_state: leg.state.clone(),
    }
}

fn insert_classification(
    classifications: &mut BTreeMap<DepositId, (Deposit, EffectClass, Vec<MovementId>)>,
    deposit: Deposit,
    class: EffectClass,
    movement_id: MovementId,
) -> bool {
    match classifications.get_mut(&deposit.id) {
        Some((_existing, existing_class, movement_ids)) if *existing_class == class => {
            if !movement_ids.contains(&movement_id) {
                movement_ids.push(movement_id);
            }
            true
        }
        Some(_) => false,
        None => {
            classifications.insert(deposit.id.clone(), (deposit, class, vec![movement_id]));
            true
        }
    }
}

fn movement_effect(class: EffectClass, movements: Vec<MovementId>) -> LedgerEffect<MovementId> {
    match class {
        EffectClass::Incoming => LedgerEffect::Incoming { movements },
        EffectClass::Collection => LedgerEffect::Collection { movements },
        EffectClass::GasFunding => LedgerEffect::GasFunding { movements },
        EffectClass::OtherDebit => LedgerEffect::OtherBalanceChange {
            direction: BalanceDirection::Debit,
            movements,
        },
    }
}

fn resolve_runtime_effect(
    event: &indexing::ObservationEvent,
    effect: &LedgerEffect<MovementId>,
) -> Result<LedgerEffect<AtomicAmount>, DepositError> {
    let resolve = |ids: &[MovementId]| -> Result<Vec<AtomicAmount>, DepositError> {
        ids.iter()
            .map(|id| {
                event
                    .transaction
                    .movements
                    .iter()
                    .find(|movement| &movement.id == id)
                    .map(|movement| movement.amount)
                    .ok_or_else(|| {
                        domain_invariant(
                            "classification references a movement outside its mirrored event",
                        )
                    })
            })
            .collect()
    };
    Ok(match effect {
        LedgerEffect::Incoming { movements } => LedgerEffect::Incoming {
            movements: resolve(movements)?,
        },
        LedgerEffect::Collection { movements } => LedgerEffect::Collection {
            movements: resolve(movements)?,
        },
        LedgerEffect::GasFunding { movements } => LedgerEffect::GasFunding {
            movements: resolve(movements)?,
        },
        LedgerEffect::OtherBalanceChange {
            direction,
            movements,
        } => LedgerEffect::OtherBalanceChange {
            direction: *direction,
            movements: resolve(movements)?,
        },
    })
}

fn reconciliation_case_id(
    event: &indexing::ObservationEvent,
    deposit_id: &DepositId,
) -> ReconciliationCaseId {
    ReconciliationCaseId(format!(
        "reconciliation:{}:{}:{}:{}:{}",
        event.id.0.len(),
        event.id.0,
        event.transaction.revision.0,
        deposit_id.0.len(),
        deposit_id.0
    ))
}

async fn metrics(State(telemetry): State<PrometheusTelemetry>) -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        telemetry.render(),
    )
}

async fn shutdown_signal(mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    let _ignored = shutdown.changed().await;
}

#[cfg(unix)]
async fn termination_signal() -> Result<(), RuntimeError> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| {
        RuntimeError::invariant(format!("failed to install SIGTERM handler: {error}"))
    })?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result
            .map_err(|error| RuntimeError::invariant(format!("failed to receive SIGINT: {error}"))),
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn termination_signal() -> Result<(), RuntimeError> {
    tokio::signal::ctrl_c().await.map_err(|error| {
        RuntimeError::invariant(format!("failed to receive shutdown signal: {error}"))
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub batches: usize,
    pub activated: usize,
    pub exhausted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestionReport {
    pub pages: usize,
    pub appended: usize,
    pub duplicates: usize,
    pub checkpoint: Option<EventCursor>,
    pub exhausted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectionStatusReport {
    pub ingestion_cursor: Option<EventCursor>,
    pub projection_cursor: Option<EventCursor>,
    pub pending_sample: usize,
    pub more_pending: bool,
}

/// Retry the durable half of the deposit-address/watch handshake.
///
/// This path never asks WS for a key or address. It only scans PS-owned
/// `AwaitingWatch` rows and uses their captured birthday/address to perform the
/// idempotent IX acknowledgement.
pub async fn reconcile_watches(
    options: &ReconcileOptions,
) -> Result<ReconcileReport, RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    let client = IndexerClient::new(&options.indexer)?;
    let scope = ethereum_scope(&options.indexer.network);
    let no_address_generation = DisabledAddressGeneration;
    let coordinator =
        DepositWatchCoordinator::new(&repository, &client, &no_address_generation, scope);

    let mut report = ReconcileReport::default();
    while report.batches < options.max_batches {
        let activated = coordinator.resume_awaiting(options.page_size).await?;
        report.batches = report
            .batches
            .checked_add(1)
            .ok_or_else(|| RuntimeError::invariant("reconcile batch counter overflowed"))?;
        report.activated = report
            .activated
            .checked_add(activated)
            .ok_or_else(|| RuntimeError::invariant("reconcile activation counter overflowed"))?;
        if activated < options.page_size {
            return Ok(report);
        }
    }

    let remaining = repository
        .awaiting_watch(AwaitingWatchPageRequest {
            after: None,
            limit: 1,
        })
        .await?;
    report.exhausted = !remaining.deposits.is_empty();
    Ok(report)
}

/// Mirror IX facts and the ingestion cursor atomically without assigning
/// deposit, accounting, collection, or other Payment Service semantics.
pub async fn ingest_events(options: &IngestOptions) -> Result<IngestionReport, RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    let client = IndexerClient::new(&options.indexer)?;
    let scope = ethereum_scope(&options.indexer.network);
    let checkpoint = repository
        .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
        .await?;
    let mut report = IngestionReport {
        checkpoint: checkpoint.cursor,
        ..IngestionReport::default()
    };

    while report.pages < options.max_pages {
        let page = client
            .events(&scope, report.checkpoint, options.page_size)
            .await?;
        report.pages = report
            .pages
            .checked_add(1)
            .ok_or_else(|| RuntimeError::invariant("ingestion page counter overflowed"))?;
        if page.events.is_empty() {
            return Ok(report);
        }

        for event in page.events {
            if event.transaction.scope != scope {
                return Err(RuntimeError::invariant(
                    "Indexer event does not belong to the configured PS scope",
                ));
            }
            let existing = repository.observation(&event.id).await?;
            let received_at = match &existing {
                Some(existing) if existing.event == event => existing.received_at,
                Some(_) => {
                    return Err(RuntimeError::invariant(
                        "mirrored IX event ID was reused with a different payload",
                    ));
                }
                None => unix_timestamp()?,
            };

            if report
                .checkpoint
                .is_some_and(|checkpoint| event.cursor < checkpoint)
            {
                // A stale at-least-once delivery is harmless only when the
                // immutable event is already mirrored byte-for-byte.
                if existing.is_none() {
                    return Err(RuntimeError::invariant(
                        "Indexer delivered an unknown event behind the ingestion cursor",
                    ));
                }
                report.duplicates = report
                    .duplicates
                    .checked_add(1)
                    .ok_or_else(|| RuntimeError::invariant("duplicate counter overflowed"))?;
                continue;
            }

            let cursor = event.cursor;
            match repository
                .mirror_and_advance(MirrorObservation {
                    expected_cursor: report.checkpoint,
                    observation: MirroredObservation { event, received_at },
                })
                .await?
            {
                MirrorOutcome::Appended { .. } => {
                    report.appended = report
                        .appended
                        .checked_add(1)
                        .ok_or_else(|| RuntimeError::invariant("append counter overflowed"))?;
                }
                MirrorOutcome::AlreadyPresent { .. } => {
                    report.duplicates = report
                        .duplicates
                        .checked_add(1)
                        .ok_or_else(|| RuntimeError::invariant("duplicate counter overflowed"))?;
                }
            }
            report.checkpoint = Some(cursor);
        }

        if page.next_cursor.is_none() {
            return Ok(report);
        }
    }

    report.exhausted = true;
    Ok(report)
}

/// Projection intentionally remains separate: classifying mirrored IX facts
/// requires PS deposit/collection/accounting policy that this maintenance
/// runtime is not configured to invent. This command exposes the independent
/// cursor and a bounded backlog sample so an operator can supervise that gap.
pub async fn projection_status(
    options: &ProjectionStatusOptions,
) -> Result<ProjectionStatusReport, RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    let ingestion = repository
        .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
        .await?;
    let projection = repository
        .consumer_checkpoint(ConsumerCheckpointName::IxProjection)
        .await?;
    if projection.cursor > ingestion.cursor {
        return Err(RuntimeError::invariant(
            "PS projection cursor is ahead of its ingestion cursor",
        ));
    }
    let pending = repository
        .observations(ObservationLogRequest {
            after: projection.cursor,
            limit: options.sample_limit,
        })
        .await?;
    Ok(ProjectionStatusReport {
        ingestion_cursor: ingestion.cursor,
        projection_cursor: projection.cursor,
        pending_sample: pending.observations.len(),
        more_pending: pending.next.is_some(),
    })
}

fn ethereum_scope(network: &str) -> IndexScope {
    IndexScope {
        chain: ChainId("ethereum".to_owned()),
        network: network.to_owned(),
    }
}

fn unix_timestamp() -> Result<u64, RuntimeError> {
    SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|duration| duration.as_secs())
        .map_err(|_| RuntimeError::invariant("system clock precedes the Unix epoch"))
}

struct DisabledAddressGeneration;

impl DepositAddressSource for DisabledAddressGeneration {
    fn address<'a>(
        &'a self,
        _request: DepositAddressRequest,
    ) -> BoxFuture<'a, Result<GeneratedDepositAddress, DepositError>> {
        Box::pin(async {
            Err(DepositError {
                kind: DepositErrorKind::InvalidState,
                message: "AwaitingWatch reconciliation must never generate a new key or address"
                    .to_owned(),
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    fn configuration(error: impl fmt::Display) -> Self {
        Self {
            message: format!("invalid Payment Service configuration: {error}"),
        }
    }

    fn invariant(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

impl From<storage::StorageError> for RuntimeError {
    fn from(error: storage::StorageError) -> Self {
        Self {
            message: error.message,
        }
    }
}

impl From<DepositError> for RuntimeError {
    fn from(error: DepositError) -> Self {
        Self {
            message: error.message,
        }
    }
}

impl From<IndexError> for RuntimeError {
    fn from(error: IndexError) -> Self {
        Self {
            message: error.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::{Json, Router, extract::Query, http::StatusCode, routing::get};
    use chain_identity::CanonicalAddress;
    use deposits::{
        ConsumerCheckpointName, CreateDeposit, CreateDepositWithLedger, IdempotencyKey,
        ObservationConsumerCheckpoints, UserId,
    };
    use indexing::{BlockHeight, WatchId};
    use serde_json::{Value, json};
    use signer::KeyLocator;
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    use super::*;
    use crate::config::{DatabaseOptions, IndexerOptions};

    async fn spawn(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener must bind");
        let address = listener.local_addr().expect("listener address must exist");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test server must run");
        });
        format!("http://{address}")
    }

    fn indexer(endpoint: String) -> IndexerOptions {
        IndexerOptions {
            indexer_url: endpoint.parse().expect("test endpoint must parse"),
            network: "test".to_owned(),
            bearer_token: None,
            request_timeout_seconds: 2,
            retry_attempts: 1,
            retry_initial_millis: 0,
            retry_max_millis: 0,
        }
    }

    fn test_amount(value: u64) -> AtomicAmount {
        let mut bytes = [0_u8; 32];
        bytes[24..].copy_from_slice(&value.to_be_bytes());
        AtomicAmount(bytes)
    }

    fn test_deposit() -> CreateDepositWithLedger {
        let chain = ChainId("ethereum".to_owned());
        CreateDepositWithLedger {
            deposit: CreateDeposit {
                id: DepositId("deposit-expiration-race".to_owned()),
                idempotency_key: IdempotencyKey("create-expiration-race".to_owned()),
                user_id: UserId("user-expiration-race".to_owned()),
                asset: AssetId {
                    chain: chain.clone(),
                    asset: "native".to_owned(),
                },
                address: CanonicalAddress {
                    chain,
                    value: "0x1111111111111111111111111111111111111111".to_owned(),
                },
                key: KeyLocator::Identifier("opaque-expiration-key".to_owned()),
                key_purpose: "expiration-race-test".to_owned(),
                expected: test_amount(1),
                birthday: BlockHeight(1),
                expires_at: 2,
                created_at: 1,
            },
            ledger_recorded_at: 1,
        }
    }

    #[tokio::test]
    async fn expiration_skips_a_candidate_closed_after_its_page_was_read()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
        let command = test_deposit();
        let id = command.deposit.id.clone();
        let idempotency_key = command.deposit.idempotency_key.clone();
        let created = repository.create_with_ledger(command).await?;
        let stale_active = repository
            .activate_watch(
                &id,
                &idempotency_key,
                WatchId("watch-expiration-race".to_owned()),
            )
            .await?;
        repository
            .close(CloseDeposit {
                deposit_id: id.clone(),
                expected_state: stale_active.state.clone(),
                expected_ledger_head: created.ledger.id,
            })
            .await?;

        expire_deposit_if_due(&repository, &stale_active, 2).await?;

        assert_eq!(
            repository
                .deposit(&id)
                .await?
                .expect("closed test deposit must exist")
                .state,
            DepositState::Closed
        );
        Ok(())
    }

    #[test]
    fn close_conflicts_skip_a_concurrent_close_and_retry_a_concurrent_expiration() {
        assert_eq!(resolve_close_state_race(&DepositState::Closed), Ok(()));

        let retry = resolve_close_state_race(&DepositState::Expired {
            watch_id: WatchId("watch-close-race".to_owned()),
        })
        .expect_err("a concurrently expired deposit must retry its close job");
        assert_eq!(retry.kind, DepositErrorKind::InvalidState);
        assert!(retryable_job_error(&retry));

        let active = DepositState::Active {
            watch_id: WatchId("watch-close-race".to_owned()),
        };
        let changed_head = resolve_close_state_race(&active)
            .expect_err("a concurrent ledger or reservation change must retry");
        assert_eq!(changed_head.kind, DepositErrorKind::InvalidState);
        assert!(retryable_job_error(&changed_head));
    }

    #[test]
    fn only_typed_conflicts_restart_projection() {
        assert!(is_optimistic_conflict(&optimistic_conflict()));
        assert!(!is_optimistic_conflict(&DepositError {
            kind: DepositErrorKind::Storage,
            message: "test storage failure".to_owned(),
        }));
        assert!(!is_optimistic_conflict(&DepositError {
            kind: DepositErrorKind::InvariantViolation,
            message: "test invariant failure".to_owned(),
        }));
    }

    fn optimistic_conflict() -> DepositError {
        DepositError {
            kind: DepositErrorKind::Conflict,
            message: "test optimistic conflict".to_owned(),
        }
    }

    #[tokio::test]
    async fn ingestion_reuses_the_durable_cursor_and_accepts_identical_redelivery()
    -> Result<(), Box<dyn std::error::Error>> {
        async fn events(Query(query): Query<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
            // Deliberately return event 1 even after cursor 1 to exercise the
            // at-least-once duplicate/restart path.
            assert!(matches!(
                query.get("after_cursor").map(String::as_str),
                None | Some("1")
            ));
            (
                StatusCode::OK,
                Json(json!({
                    "events": [event_json()],
                    "next_cursor": null
                })),
            )
        }

        let endpoint = spawn(Router::new().route("/v1/events", get(events))).await;
        let directory = TempDir::new()?;
        let options = IngestOptions {
            database: DatabaseOptions {
                database_path: directory.path().join("payment-service"),
            },
            indexer: indexer(endpoint),
            page_size: 10,
            max_pages: 2,
        };

        let first = ingest_events(&options).await?;
        assert_eq!(first.appended, 1);
        assert_eq!(first.checkpoint, Some(EventCursor(1)));

        let second = ingest_events(&options).await?;
        assert_eq!(second.appended, 0);
        assert_eq!(second.duplicates, 1);
        assert_eq!(second.checkpoint, Some(EventCursor(1)));

        let storage = RocksDbStorage::open(&options.database.database_path)?;
        let repository = PersistentPaymentRepository::new(storage);
        let durable = repository
            .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
            .await?;
        assert_eq!(durable.cursor, Some(EventCursor(1)));
        Ok(())
    }

    fn event_json() -> Value {
        json!({
            "id": "event-1",
            "cursor": "1",
            "watch_ids": ["watch-1"],
            "previous_status": null,
            "transaction": {
                "scope": {"chain": "ethereum", "network": "test"},
                "transaction_id": format!("0x{}", "22".repeat(32)),
                "revision": "1",
                "status": {
                    "kind": "included",
                    "block": {
                        "height": "42",
                        "hash": format!("0x{}", "11".repeat(32)),
                        "parent_hash": format!("0x{}", "10".repeat(32)),
                        "timestamp": "1000"
                    },
                    "confirmations": "1"
                },
                "movements": [],
                "fee": null,
                "first_seen_at": "1000",
                "observed_at": "1001"
            }
        })
    }
}
