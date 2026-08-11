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
    CollectionAllocation, CollectionLeg, CollectionLegKind, CollectionLegState, CollectionMode,
    CollectionPageRequest, CollectionReservationState, CollectionStore, CollectionTransitionGuard,
    ConfirmCollectionLeg, ConsumerCheckpointName, Deposit, DepositAddressRequest,
    DepositAddressSource, DepositError, DepositErrorKind, DepositId, DepositLedger,
    DepositPageRequest, DepositState, DepositStateKind, DepositStore, DepositWatchCoordinator,
    FailCollectionLeg, GeneratedDepositAddress, InitializePaymentDatabase, Job, JobError,
    JobPayload, JobState, JobStore, LedgerEffect, LedgerObservationTransition,
    MigratePaymentDatabase, MirrorObservation, MirrorOutcome, MirroredObservation,
    ObservationConsumerCheckpoints, ObservationEventLog, ObservationLogRequest,
    PaymentDatabaseMetadataStore, PersistentPaymentRepository, PrincipalScopeMode,
    ProjectObservation, ProjectionFeeTreatment, ReconciliationCase, ReconciliationCaseId,
    ReconciliationReason, ReconciliationState, ReconciliationStore, RecordObservation,
    RegisterDeposit, ReleaseCollectionReservation, ReorgCollectionLeg, ReservationReleaseReason,
    SafeCollectionError, TransitionJob, UtxoBatchProjectionMutation, UtxoBatchProjectionTransition,
    apply_observation_transition,
};
use http_support::{
    AuthenticationMode, BearerToken, HealthState, HttpServerConfig, RequestLimits,
    TransportSecurity,
};
use indexing::{
    EventCursor, IndexError, IndexScope, MovementId, MovementKind, SyncPhase, SyncStatus,
    TransactionStatus,
};
use storage_rocksdb::RocksDbStorage;
use telemetry::{Attribute, PrometheusTelemetry, Telemetry};
use tokio::{sync::watch, task::JoinSet, time::MissedTickBehavior};

use crate::{
    active_policy::ActivePaymentPolicy,
    api::{self, ApiState},
    auth::{Authorizer, Credentials},
    bitcoin_collection_executor,
    bitcoin_policy::BitcoinPaymentPolicy,
    bitcoin_wallet_client::BitcoinWalletClient,
    collection_executor,
    config::{
        BackupOptions, BitcoinServeOptions, IndexerOptions, IngestOptions, MigrationOptions,
        ProjectionStatusOptions, ReconcileOptions, ServeOptions,
    },
    indexer_client::IndexerClient,
    policy::PaymentPolicy,
    wallet_client::WalletClient,
};

type Repository = PersistentPaymentRepository<RocksDbStorage>;

const JOB_LEASE_DURATION: Duration = Duration::from_secs(5 * 60);
const MAX_JOB_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
const MIGRATION_PAGE_SIZE: usize = 1_000;

const fn principal_scope_mode(authentication_mode: AuthenticationMode) -> PrincipalScopeMode {
    match authentication_mode {
        AuthenticationMode::Strict => PrincipalScopeMode::RoleScoped,
        AuthenticationMode::GlobalTrusted => PrincipalScopeMode::GlobalTrusted,
    }
}

fn authorizer(options: &ServeOptions) -> Result<Arc<Authorizer>, RuntimeError> {
    match options.authentication.mode {
        AuthenticationMode::Strict => {
            let ordinary = options.ordinary_bearer_token.as_ref().ok_or_else(|| {
                RuntimeError::configuration(
                    "strict authentication requires an ordinary bearer token",
                )
            })?;
            let administrator = options.admin_bearer_token.as_ref().ok_or_else(|| {
                RuntimeError::configuration(
                    "strict authentication requires an administrator bearer token",
                )
            })?;
            Ok(Arc::new(Authorizer::strict(Credentials::new(
                BearerToken::new(ordinary.expose()).map_err(RuntimeError::configuration)?,
                BearerToken::new(administrator.expose()).map_err(RuntimeError::configuration)?,
            ))))
        }
        AuthenticationMode::GlobalTrusted => Ok(Arc::new(Authorizer::global_trusted())),
    }
}

fn report_authentication_mode(options: &ServeOptions) {
    let mode = options.authentication.mode;
    tracing::info!(
        authentication_mode = mode.as_str(),
        "Payment Service authentication mode selected"
    );
    if mode == AuthenticationMode::GlobalTrusted {
        let mut ignored = Vec::new();
        if options.ordinary_bearer_token.is_some() {
            ignored.push("PS_API_BEARER_TOKEN");
        }
        if options.admin_bearer_token.is_some() {
            ignored.push("PS_ADMIN_BEARER_TOKEN");
        }
        if options.wallet.bearer_token.is_some() {
            ignored.push("PS_WALLET_BEARER_TOKEN");
        }
        if options.indexer.bearer_token.is_some() {
            ignored.push("PS_INDEXER_BEARER_TOKEN");
        }
        tracing::warn!(
            authentication_mode = mode.as_str(),
            ignored_bearer_variables = ignored.join(","),
            "GLOBAL-TRUSTED AUTHENTICATION MODE: every caller with network access has ordinary and administrator authority"
        );
    }
}

fn record_authentication_mode_metric(telemetry: &dyn Telemetry, mode: AuthenticationMode) {
    telemetry.gauge(
        "payment_sdk_strict_authentication_mode",
        if mode.is_strict() { 1.0 } else { 0.0 },
        &[Attribute {
            key: "service".to_owned(),
            value: "payment-service".to_owned(),
        }],
    );
}

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
        .migrate_and_bind_principal_scope(
            MigratePaymentDatabase {
                scope: policy.scope.clone(),
                active_policy: policy.identity(),
                migrated_at: unix_timestamp()?,
                page_size: MIGRATION_PAGE_SIZE,
            },
            principal_scope_mode(options.authentication.mode),
        )
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

pub async fn migrate_bitcoin(options: MigrationOptions) -> Result<(), RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let policy = BitcoinPaymentPolicy::load(&options.policy_path).map_err(|error| {
        RuntimeError::configuration(format!(
            "failed to load Bitcoin Payment Service policy ({:?}): {error}",
            error.kind
        ))
    })?;
    if policy.scope.network != options.network {
        return Err(RuntimeError::configuration(
            "migration network must exactly match the Bitcoin policy scope",
        ));
    }
    let outcome = RocksDbStorage::migrate(&options.database.database_path, &options.backup_path)?;
    tracing::info!(
        backup_id = outcome.backup.backup_id,
        from = outcome.report.previous.0,
        to = outcome.report.current.0,
        "pre-migration Bitcoin Payment Service backup and physical schema validation completed"
    );
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    let scope = policy.scope.clone();
    let active_policy = policy.identity();
    let report = repository
        .migrate_and_bind_principal_scope(
            MigratePaymentDatabase {
                scope,
                active_policy,
                migrated_at: unix_timestamp()?,
                page_size: MIGRATION_PAGE_SIZE,
            },
            principal_scope_mode(options.authentication.mode),
        )
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
        "Bitcoin Payment Service semantic migration validated and bound metadata"
    );
    Ok(())
}

pub async fn serve(options: ServeOptions) -> Result<(), RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    report_authentication_mode(&options);
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

    // Negotiate dependency modes before any durable bind, public listener, or
    // effectful worker. Client construction and readiness are read-only.
    let indexer = Arc::new(IndexerClient::new(
        &options.indexer,
        options.authentication.mode,
    )?);
    let wallet = Arc::new(WalletClient::new(
        &options.wallet,
        options.authentication.mode,
    )?);
    let (indexer_mode_probe, wallet_mode_probe) =
        tokio::join!(indexer.readiness(), wallet.readiness());
    let _indexer_ready = indexer_mode_probe?;
    let _wallet_ready = wallet_mode_probe?;

    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    repository
        .initialize_or_validate_principal_scope(
            InitializePaymentDatabase {
                scope: policy.scope.clone(),
                active_policy: policy.identity(),
                initialized_at: unix_timestamp()?,
            },
            principal_scope_mode(options.authentication.mode),
        )
        .await?;

    let authorizer = authorizer(&options)?;
    let health = HealthState::new(false);
    let indexer_health = HealthState::new(false);
    let wallet_health = HealthState::new(false);
    let limits = RequestLimits::default();
    let api_policy = Arc::new(ActivePaymentPolicy::Ethereum(policy.as_ref().clone()));
    let api_state = Arc::new(
        ApiState::new(repository.clone(), api_policy, limits.clone())
            .with_authentication_mode(options.authentication.mode)
            .with_runtime_health(
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
        .with_authentication_mode(options.authentication.mode)
        .with_custom_authentication();
    let application_router = http_support::service_router(
        api::router(api_state, authorizer),
        &server_config,
        health.clone(),
    )
    .map_err(RuntimeError::configuration)?;

    let telemetry = PrometheusTelemetry::install()
        .map_err(|error| RuntimeError::invariant(error.to_string()))?;
    record_authentication_mode_metric(&telemetry, options.authentication.mode);
    let metrics_config = HttpServerConfig::new(
        options.metrics_bind,
        TransportSecurity::PlaintextLoopback,
        None,
        RequestLimits::default(),
    )
    .with_authentication_mode(AuthenticationMode::GlobalTrusted);
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

/// Run one native-Bitcoin Payment Service scope.
///
/// The process owns one Bitcoin network and one PS database. IX remains the
/// source of canonical chain facts, while WS remains stateless and receives
/// only exact PS-selected inputs for signing and broadcast.
pub async fn serve_bitcoin(options: BitcoinServeOptions) -> Result<(), RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let options = options.common;
    report_authentication_mode(&options);
    let policy = Arc::new(
        BitcoinPaymentPolicy::load(&options.policy_path).map_err(|error| {
            RuntimeError::configuration(format!(
                "failed to load Bitcoin Payment Service policy ({:?}): {error}",
                error.kind
            ))
        })?,
    );
    if policy.scope.network != options.indexer.network || policy.scope.chain.0 != "bitcoin" {
        return Err(RuntimeError::configuration(
            "policy scope must match the configured Bitcoin Indexer network",
        ));
    }

    // Negotiate dependency modes before any durable bind, public listener, or
    // effectful worker. Client construction and readiness are read-only.
    let indexer = Arc::new(IndexerClient::new(
        &options.indexer,
        options.authentication.mode,
    )?);
    let wallet = Arc::new(BitcoinWalletClient::new(
        &options.wallet,
        options.authentication.mode,
        policy.network,
        policy.deposit_address_kind,
    )?);
    let (indexer_mode_probe, wallet_mode_probe) =
        tokio::join!(indexer.readiness(), wallet.readiness());
    let _indexer_ready = indexer_mode_probe?;
    let _wallet_ready = wallet_mode_probe?;

    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    repository
        .initialize_or_validate_principal_scope(
            InitializePaymentDatabase {
                scope: policy.scope.clone(),
                active_policy: policy.identity(),
                initialized_at: unix_timestamp()?,
            },
            principal_scope_mode(options.authentication.mode),
        )
        .await?;

    let authorizer = authorizer(&options)?;
    let health = HealthState::new(false);
    let indexer_health = HealthState::new(false);
    let wallet_health = HealthState::new(false);
    let limits = RequestLimits::default();
    let api_policy = Arc::new(ActivePaymentPolicy::Bitcoin(policy.as_ref().clone()));
    let api_state = Arc::new(
        ApiState::new(repository.clone(), api_policy, limits.clone())
            .with_authentication_mode(options.authentication.mode)
            .with_runtime_health(
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
        .with_authentication_mode(options.authentication.mode)
        .with_custom_authentication();
    let application_router = http_support::service_router(
        api::router(api_state, authorizer),
        &server_config,
        health.clone(),
    )
    .map_err(RuntimeError::configuration)?;

    let telemetry = PrometheusTelemetry::install()
        .map_err(|error| RuntimeError::invariant(error.to_string()))?;
    record_authentication_mode_metric(&telemetry, options.authentication.mode);
    let metrics_config = HttpServerConfig::new(
        options.metrics_bind,
        TransportSecurity::PlaintextLoopback,
        None,
        RequestLimits::default(),
    )
    .with_authentication_mode(AuthenticationMode::GlobalTrusted);
    let metrics_router = Router::new()
        .route("/metrics", get(metrics))
        .with_state(telemetry);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = JoinSet::<Result<(), RuntimeError>>::new();
    let worker_interval = options.worker_interval();
    let worker_page_size = options.worker_page_size;

    let worker_repository = repository.clone();
    let worker_indexer = Arc::clone(&indexer);
    let worker_wallet = Arc::clone(&wallet);
    let worker_policy = Arc::clone(&policy);
    let worker_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        run_bitcoin_job_worker(
            BitcoinJobWorkerContext {
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
        run_bitcoin_readiness_worker(
            BitcoinReadinessWorkerContext {
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
        "Bitcoin Payment Service HTTP runtime and durable workers started"
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
                    Some(Ok(Err(error))) => tracing::error!(error = %error, "Bitcoin Payment Service task failed during shutdown"),
                    Some(Err(error)) => tracing::error!(error = %error, "Bitcoin Payment Service task panicked during shutdown"),
                    None => break,
                }
            }
            else => break,
        }
    }

    match termination {
        None => Ok(()),
        Some(Some(Ok(Ok(())))) => Err(RuntimeError::invariant(
            "a supervised Bitcoin Payment Service task stopped unexpectedly",
        )),
        Some(Some(Ok(Err(error)))) => Err(error),
        Some(Some(Err(error))) => Err(RuntimeError::invariant(format!(
            "Bitcoin Payment Service task panicked: {error}"
        ))),
        Some(None) => Err(RuntimeError::invariant(
            "Bitcoin Payment Service supervisor has no running tasks",
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

struct BitcoinJobWorkerContext {
    repository: Repository,
    indexer: Arc<IndexerClient>,
    wallet: Arc<BitcoinWalletClient>,
    policy: Arc<BitcoinPaymentPolicy>,
    interval: Duration,
    page_size: usize,
}

async fn run_bitcoin_job_worker(
    context: BitcoinJobWorkerContext,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let BitcoinJobWorkerContext {
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

            let result = process_bitcoin_job(
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
        JobPayload::CreateUtxoBatchCollection(_) | JobPayload::RetryUtxoBatchCollection(_) => Err(
            domain_invariant("Bitcoin collection job reached the Ethereum Payment Service worker"),
        ),
    }
}

async fn process_bitcoin_job(
    repository: &Repository,
    indexer: &IndexerClient,
    wallet: &BitcoinWalletClient,
    policy: &BitcoinPaymentPolicy,
    scope: &IndexScope,
    job: &Job,
) -> Result<(), DepositError> {
    match &job.payload {
        JobPayload::CreateDeposit(payload) => {
            if &payload.scope != scope {
                return Err(domain_invariant(
                    "deposit job scope differs from the running Bitcoin Payment Service scope",
                ));
            }
            if payload.asset != policy.asset {
                return Err(domain_invariant(
                    "deposit job asset differs from the active Bitcoin native asset",
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
                    "Bitcoin deposit job completed without an active IX watch",
                ));
            }
            Ok(())
        }
        JobPayload::CloseDeposit(payload) => close_deposit_workflow(repository, payload).await,
        JobPayload::CreateUtxoBatchCollection(_) | JobPayload::RetryUtxoBatchCollection(_) => {
            bitcoin_collection_executor::process_bitcoin_collection_job(
                repository, indexer, wallet, policy, scope, job,
            )
            .await
        }
        JobPayload::CreateCollection(_) | JobPayload::RetryCollection(_) => Err(domain_invariant(
            "Ethereum collection job reached the Bitcoin Payment Service worker",
        )),
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
        let (indexer_status, indexer_transport_ready, wallet_ready) = tokio::join!(
            indexer.status(&scope),
            indexer.readiness(),
            wallet.readiness()
        );
        let indexer_ready = indexer_transport_ready.as_ref().is_ok_and(|ready| *ready)
            && indexer_status
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

struct BitcoinReadinessWorkerContext {
    repository: Repository,
    indexer: Arc<IndexerClient>,
    wallet: Arc<BitcoinWalletClient>,
    scope: IndexScope,
    health: HealthState,
    indexer_health: HealthState,
    wallet_health: HealthState,
    interval: Duration,
}

async fn run_bitcoin_readiness_worker(
    context: BitcoinReadinessWorkerContext,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let BitcoinReadinessWorkerContext {
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
        let (indexer_status, indexer_transport_ready, wallet_ready) = tokio::join!(
            indexer.status(&scope),
            indexer.readiness(),
            wallet.readiness()
        );
        let indexer_ready = indexer_transport_ready.as_ref().is_ok_and(|ready| *ready)
            && indexer_status
                .as_ref()
                .is_ok_and(|status| bitcoin_indexer_is_ready(status, &scope));
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

fn bitcoin_indexer_is_ready(status: &SyncStatus, scope: &IndexScope) -> bool {
    status.scope == *scope
        && status.phase == SyncPhase::Ready
        && status.confirmation_policy.minimum_confirmations > 0
        && status
            .checkpoint
            .as_ref()
            .zip(status.observed_tip.as_ref())
            .is_some_and(|(checkpoint, observed_tip)| {
                checkpoint.hash.0.len() == 32
                    && observed_tip.hash.0.len() == 32
                    && checkpoint.height <= observed_tip.height
            })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EffectClass {
    Incoming,
    Collection,
    GasFunding,
    OtherDebit,
    NetBalanceChange,
}

type ClassifiedDeposit = (Deposit, EffectClass, Vec<MovementId>, Vec<MovementId>);
type ClassifiedDeposits = BTreeMap<DepositId, ClassifiedDeposit>;

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
            let Some((affected_deposits, updates, cases, fee_treatment, utxo_batch_transition)) =
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
                    fee_treatment,
                    utxo_batch_transition,
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

struct UtxoBatchFactClassification {
    classified: ClassifiedDeposits,
    affected_deposits: BTreeSet<DepositId>,
    transition: Option<UtxoBatchProjectionMutation>,
}

async fn classify_utxo_batch_fact(
    repository: &Repository,
    event: &indexing::ObservationEvent,
    collection: &Collection,
    leg: &CollectionLeg,
    received_at: u64,
) -> Result<Option<UtxoBatchFactClassification>, DepositError> {
    if leg.kind != CollectionLegKind::Sweep
        || collection.participants.is_empty()
        || leg.allocations.len() != collection.participants.len()
        || leg.watch_id.is_none()
        || leg.state.transaction_id() != Some(&event.transaction.transaction_id)
    {
        return Ok(None);
    }

    let mut classified = BTreeMap::new();
    let mut affected_deposits = BTreeSet::new();
    let mut participant_addresses = BTreeSet::new();
    for (participant, allocation) in collection.participants.iter().zip(&leg.allocations) {
        let deposit_id = &participant.reservation.deposit_id;
        if &allocation.deposit_id != deposit_id
            || allocation.asset != collection.asset
            || allocation.allocated_fee_asset != collection.asset
            || participant.reservation.asset != collection.asset
            || participant.reservation.amount != allocation.gross_debit
        {
            return Ok(None);
        }
        let deposit = repository
            .deposit(deposit_id)
            .await?
            .ok_or_else(|| domain_invariant("UTXO-batch participant deposit is missing"))?;
        if deposit.user_id != participant.user_id || deposit.asset != collection.asset {
            return Err(domain_invariant(
                "UTXO-batch participant differs from its durable deposit",
            ));
        }
        if !participant_addresses.insert(deposit.address.clone()) {
            return Err(domain_invariant(
                "UTXO-batch participant addresses must be unique",
            ));
        }
        let movement_ids = event
            .transaction
            .movements
            .iter()
            .filter(|movement| {
                movement.kind == MovementKind::Input
                    && movement.asset == collection.asset
                    && movement.from.as_ref() == Some(&deposit.address)
            })
            .map(|movement| movement.id.clone())
            .collect::<Vec<_>>();
        if movement_ids.is_empty() {
            return Ok(None);
        }
        let gross_debit = sum_movement_amounts(event, |movement| {
            movement.kind == MovementKind::Input
                && movement.asset == collection.asset
                && movement.from.as_ref() == Some(&deposit.address)
        })?;
        if gross_debit != allocation.gross_debit {
            return Ok(None);
        }
        affected_deposits.insert(deposit.id.clone());
        classified.insert(
            deposit.id.clone(),
            (deposit, EffectClass::Collection, movement_ids, Vec::new()),
        );
    }

    let every_input_is_reserved = event.transaction.movements.iter().all(|movement| {
        movement.kind != MovementKind::Input
            || (movement.asset == collection.asset
                && movement
                    .from
                    .as_ref()
                    .is_some_and(|address| participant_addresses.contains(address)))
    });
    let outputs = event
        .transaction
        .movements
        .iter()
        .filter(|movement| movement.kind == MovementKind::Output)
        .collect::<Vec<_>>();
    if !every_input_is_reserved
        || outputs.len() != 1
        || outputs[0].asset != collection.asset
        || outputs[0].to.as_ref() != Some(&collection.destination)
    {
        return Ok(None);
    }
    let master_credit =
        leg.allocations
            .iter()
            .try_fold(AtomicAmount::ZERO, |total, allocation| {
                total
                    .checked_add(&allocation.master_credit)
                    .map_err(|_| domain_invariant("UTXO-batch master-credit total overflowed"))
            })?;
    if outputs[0].amount != master_credit {
        return Ok(None);
    }
    let allocated_fee =
        leg.allocations
            .iter()
            .try_fold(AtomicAmount::ZERO, |total, allocation| {
                total
                    .checked_add(&allocation.allocated_fee)
                    .map_err(|_| domain_invariant("UTXO-batch allocated-fee total overflowed"))
            })?;
    if !event
        .transaction
        .fee
        .as_ref()
        .is_some_and(|fee| fee.asset == collection.asset && fee.amount == allocated_fee)
    {
        return Ok(None);
    }

    let transition_at = collection.updated_at.max(received_at);
    let transition = match &event.transaction.status {
        TransactionStatus::Pending => {
            if !matches!(leg.state, CollectionLegState::Broadcast { .. }) {
                return Ok(None);
            }
            None
        }
        TransactionStatus::Included { .. } => match &leg.state {
            CollectionLegState::Broadcast { .. } => None,
            CollectionLegState::Reorged { .. } => Some(UtxoBatchProjectionTransition::Reincluded {
                included_at: transition_at,
            }),
            // IX rolls back a confirmation before it rolls back the block
            // that first included the transaction. A PS that was offline for
            // both revisions must advance through Confirmed -> Included so it
            // can consume the following Included -> Reorged revision. The
            // observation projection reverses confirmation-qualified
            // accounting here; the durable leg remains Confirmed until the
            // subsequent Reorged fact changes its lifecycle state.
            CollectionLegState::Confirmed { .. } => None,
            _ => return Ok(None),
        },
        TransactionStatus::Confirmed { .. } => match &leg.state {
            CollectionLegState::Broadcast { .. } | CollectionLegState::Reorged { .. } => {
                Some(UtxoBatchProjectionTransition::Confirmed {
                    allocations: leg.allocations.clone(),
                    confirmed_at: transition_at,
                })
            }
            CollectionLegState::Confirmed { .. } => None,
            _ => return Ok(None),
        },
        TransactionStatus::Reorged { .. } => match &leg.state {
            CollectionLegState::Confirmed { .. } => Some(UtxoBatchProjectionTransition::Reorged {
                error: SafeCollectionError {
                    code: "ix_reorged".to_owned(),
                    message: "Indexer corrected a confirmed Bitcoin collection transaction"
                        .to_owned(),
                    retryable: false,
                },
                reorged_at: transition_at,
            }),
            CollectionLegState::Broadcast { .. } | CollectionLegState::Reorged { .. } => None,
            _ => return Ok(None),
        },
        TransactionStatus::Failed { .. }
        | TransactionStatus::Dropped
        | TransactionStatus::Replaced { .. } => {
            // Bitcoin v1 has no replacement-signing or automatic release path.
            // Keep the exact outpoints and signed bytes reserved and stop the
            // projection until an operator resolves the unsupported fact.
            return Ok(None);
        }
    };
    let transition = transition.map(|transition| UtxoBatchProjectionMutation {
        collection_id: collection.id.clone(),
        leg_id: leg.id.clone(),
        expected: collection_guard(collection, leg),
        transaction_id: event.transaction.transaction_id.clone(),
        transition,
    });
    Ok(Some(UtxoBatchFactClassification {
        classified,
        affected_deposits,
        transition,
    }))
}

async fn classify_projection(
    repository: &Repository,
    mirrored: &MirroredObservation,
) -> Result<
    Option<(
        Vec<DepositId>,
        Vec<RecordObservation>,
        Vec<ReconciliationCase>,
        ProjectionFeeTreatment,
        Option<UtxoBatchProjectionMutation>,
    )>,
    DepositError,
> {
    let event = &mirrored.event;
    let transaction_id = &event.transaction.transaction_id;
    let mut classified = ClassifiedDeposits::new();
    let mut affected_deposits = BTreeSet::<DepositId>::new();
    let mut fee_included_in_movement = false;
    let mut utxo_batch_transition = None;

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
        if collection.mode == CollectionMode::UtxoBatch {
            let Some(batch) =
                classify_utxo_batch_fact(repository, event, &collection, leg, mirrored.received_at)
                    .await?
            else {
                return Ok(None);
            };
            fee_included_in_movement = true;
            classified = batch.classified;
            affected_deposits = batch.affected_deposits;
            utxo_batch_transition = batch.transition;
        } else {
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
            // deposit ledger. Its collection leg is still resolved from IX,
            // but no token-ledger row is appropriate for that event.
            let deposit_paid_fee = event.transaction.fee.as_ref().is_some_and(|fee| {
                fee.asset == deposit.asset && fee.payer.as_ref() == Some(&deposit.address)
            });
            if !movements.is_empty() || deposit_paid_fee {
                classified.insert(deposit.id.clone(), (deposit, class, movements, Vec::new()));
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
                    event.transaction.scope.chain.0 == "bitcoin"
                        && movement.kind == MovementKind::Output,
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
                    event.transaction.scope.chain.0 == "bitcoin"
                        && movement.kind == MovementKind::Input,
                ) {
                    return Ok(None);
                }
                if event.transaction.scope.chain.0 == "bitcoin"
                    && movement.kind == MovementKind::Input
                {
                    fee_included_in_movement = true;
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
                Some((
                    _existing,
                    EffectClass::OtherDebit | EffectClass::NetBalanceChange,
                    _,
                    _,
                )) => {}
                Some(_) => return Ok(None),
                None => {
                    classified.insert(
                        deposit.id.clone(),
                        (deposit, EffectClass::OtherDebit, Vec::new(), Vec::new()),
                    );
                }
            }
        }
    }

    let mut updates = Vec::with_capacity(classified.len());
    let mut cases = Vec::new();
    for (deposit_id, (deposit, class, movement_ids, credit_movement_ids)) in classified {
        let head = repository
            .current(&deposit_id)
            .await?
            .ok_or_else(|| domain_invariant("classified deposit has no ledger head"))?;
        let effect = movement_effect(class, movement_ids, credit_movement_ids);
        let resolved = resolve_runtime_effect(event, &effect)?;
        let next_balances = apply_observation_transition(
            head.balances,
            &LedgerObservationTransition {
                status: event.transaction.status.clone(),
                previous_status: event.previous_status.clone(),
                effect: resolved,
                network_fee: (!fee_included_in_movement)
                    .then(|| {
                        event.transaction.fee.as_ref().and_then(|fee| {
                            (fee.asset == deposit.asset
                                && fee.payer.as_ref() == Some(&deposit.address))
                            .then_some(fee.amount)
                        })
                    })
                    .flatten(),
            },
        )
        .map_err(|error| {
            domain_invariant(format!(
                "classified IX event cannot update its ledger: {error}"
            ))
        })?;
        if matches!(
            class,
            EffectClass::OtherDebit | EffectClass::NetBalanceChange
        ) && let Some(collection_id) =
            retained_utxo_batch_reservation(repository, &deposit_id).await?
        {
            cases.push(ReconciliationCase {
                id: reserved_spend_reconciliation_case_id(event, &deposit_id),
                deposit_id: deposit_id.clone(),
                triggering_event_id: event.id.clone(),
                reason: ReconciliationReason::ReservedSpendConflict {
                    collection_id,
                    transaction_id: event.transaction.transaction_id.clone(),
                },
                state: ReconciliationState::Open,
                created_at: mirrored.received_at,
            });
        }
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
        if fee_included_in_movement {
            ProjectionFeeTreatment::IncludedInMovementEffect
        } else {
            ProjectionFeeTreatment::Separate
        },
        utxo_batch_transition,
    )))
}

async fn retained_utxo_batch_reservation(
    repository: &Repository,
    deposit_id: &DepositId,
) -> Result<Option<deposits::CollectionId>, DepositError> {
    let deposit = repository
        .deposit(deposit_id)
        .await?
        .ok_or_else(|| domain_invariant("classified deposit disappeared"))?;
    let Some(collection) = repository
        .retained_collection_for(deposit_id, &deposit.asset)
        .await?
    else {
        return Ok(None);
    };
    let participant = collection.participant(deposit_id).ok_or_else(|| {
        domain_invariant("retained collection does not contain its indexed participant")
    })?;
    if collection.mode != CollectionMode::UtxoBatch
        || participant.reservation.asset != deposit.asset
        || matches!(
            participant.reservation.state,
            CollectionReservationState::Released { .. }
        )
    {
        return Err(domain_invariant(
            "retained Bitcoin ownership index points to an incompatible collection",
        ));
    }
    Ok(Some(collection.id))
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
    classifications: &mut ClassifiedDeposits,
    deposit: Deposit,
    class: EffectClass,
    movement_id: MovementId,
    allow_net_balance_change: bool,
) -> bool {
    match classifications.get_mut(&deposit.id) {
        Some((_existing, existing_class, movement_ids, _credit_movement_ids))
            if *existing_class == class =>
        {
            if !movement_ids.contains(&movement_id) {
                movement_ids.push(movement_id);
            }
            true
        }
        Some((_existing, EffectClass::NetBalanceChange, debit_movements, credit_movements))
            if allow_net_balance_change =>
        {
            let target = match class {
                EffectClass::Incoming => credit_movements,
                EffectClass::OtherDebit => debit_movements,
                _ => return false,
            };
            if !target.contains(&movement_id) {
                target.push(movement_id);
            }
            true
        }
        Some((_existing, existing_class, primary_movements, credit_movements))
            if allow_net_balance_change
                && matches!(
                    (*existing_class, class),
                    (EffectClass::Incoming, EffectClass::OtherDebit)
                        | (EffectClass::OtherDebit, EffectClass::Incoming)
                ) =>
        {
            if *existing_class == EffectClass::Incoming {
                *credit_movements = std::mem::take(primary_movements);
                primary_movements.push(movement_id);
            } else if !credit_movements.contains(&movement_id) {
                credit_movements.push(movement_id);
            }
            *existing_class = EffectClass::NetBalanceChange;
            true
        }
        Some(_) => false,
        None => {
            let class = if allow_net_balance_change && class == EffectClass::OtherDebit {
                EffectClass::NetBalanceChange
            } else {
                class
            };
            classifications.insert(
                deposit.id.clone(),
                (deposit, class, vec![movement_id], Vec::new()),
            );
            true
        }
    }
}

fn movement_effect(
    class: EffectClass,
    movements: Vec<MovementId>,
    credit_movements: Vec<MovementId>,
) -> LedgerEffect<MovementId> {
    match class {
        EffectClass::Incoming => LedgerEffect::Incoming { movements },
        EffectClass::Collection => LedgerEffect::Collection { movements },
        EffectClass::GasFunding => LedgerEffect::GasFunding { movements },
        EffectClass::OtherDebit => LedgerEffect::OtherBalanceChange {
            direction: BalanceDirection::Debit,
            movements,
        },
        EffectClass::NetBalanceChange => LedgerEffect::NetBalanceChange {
            debit_movements: movements,
            credit_movements,
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
        LedgerEffect::NetBalanceChange {
            debit_movements,
            credit_movements,
        } => LedgerEffect::NetBalanceChange {
            debit_movements: resolve(debit_movements)?,
            credit_movements: resolve(credit_movements)?,
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

fn reserved_spend_reconciliation_case_id(
    event: &indexing::ObservationEvent,
    deposit_id: &DepositId,
) -> ReconciliationCaseId {
    ReconciliationCaseId(format!(
        "reconciliation:reserved-spend:{}:{}:{}:{}:{}",
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
    reconcile_watches_for_scope(options, ethereum_scope(&options.indexer.network)).await
}

pub async fn reconcile_bitcoin_watches(
    options: &ReconcileOptions,
) -> Result<ReconcileReport, RuntimeError> {
    let scope = validated_bitcoin_indexer_scope(&options.indexer, options.authentication.mode)?;
    reconcile_watches_for_scope(options, scope).await
}

async fn reconcile_watches_for_scope(
    options: &ReconcileOptions,
    scope: IndexScope,
) -> Result<ReconcileReport, RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    validate_maintenance_database(&repository, &scope, options.authentication.mode).await?;
    let client = IndexerClient::new(&options.indexer, options.authentication.mode)?;
    let _ready = client.readiness().await?;
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
    ingest_events_for_scope(options, ethereum_scope(&options.indexer.network)).await
}

pub async fn ingest_bitcoin_events(
    options: &IngestOptions,
) -> Result<IngestionReport, RuntimeError> {
    let scope = validated_bitcoin_indexer_scope(&options.indexer, options.authentication.mode)?;
    ingest_events_for_scope(options, scope).await
}

async fn ingest_events_for_scope(
    options: &IngestOptions,
    scope: IndexScope,
) -> Result<IngestionReport, RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    validate_maintenance_database(&repository, &scope, options.authentication.mode).await?;
    let client = IndexerClient::new(&options.indexer, options.authentication.mode)?;
    let _ready = client.readiness().await?;
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

async fn validate_maintenance_database(
    repository: &Repository,
    scope: &IndexScope,
    authentication_mode: AuthenticationMode,
) -> Result<(), RuntimeError> {
    let metadata = repository.database_metadata().await?.ok_or_else(|| {
        RuntimeError::configuration(
            "Payment Service maintenance requires explicitly bound database metadata",
        )
    })?;
    if metadata.scope != *scope {
        return Err(RuntimeError::configuration(
            "Payment Service database scope does not match the maintenance command",
        ));
    }
    if metadata.principal_scope_mode != principal_scope_mode(authentication_mode) {
        return Err(RuntimeError::configuration(
            "Payment Service database principal-scope mode does not match the selected authentication mode",
        ));
    }
    Ok(())
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

fn bitcoin_scope(network: &str) -> Result<IndexScope, RuntimeError> {
    if !matches!(
        network,
        "mainnet" | "testnet3" | "testnet4" | "signet" | "regtest"
    ) {
        return Err(RuntimeError::configuration(
            "Bitcoin network must be mainnet, testnet3, testnet4, signet, or regtest",
        ));
    }
    Ok(IndexScope {
        chain: ChainId("bitcoin".to_owned()),
        network: network.to_owned(),
    })
}

fn validated_bitcoin_indexer_scope(
    indexer: &IndexerOptions,
    authentication_mode: AuthenticationMode,
) -> Result<IndexScope, RuntimeError> {
    if authentication_mode.is_strict() && indexer.bearer_token.is_none() {
        return Err(RuntimeError::configuration(
            "Bitcoin Indexer Service authentication is required even on loopback",
        ));
    }
    bitcoin_scope(&indexer.network)
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
    use std::{collections::HashMap, sync::Mutex};

    use axum::{Json, Router, extract::Query, http::StatusCode, routing::get};
    use chain_identity::{CanonicalAddress, CanonicalTransactionId};
    use deposits::{
        AcceptCollectionBroadcast, AttachCollectionWatch, CollectionId, CollectionLegId,
        CollectionLegKind, CollectionSpendResource, CollectionSpendResourceEvidence,
        CollectionSpendResourceId, CollectionState, CommandIdentity, CommandOperation,
        CommandPrincipal, ConsumerCheckpointName, CreateCollectionLeg, CreateDeposit,
        CreateDepositWithLedger, CreateJob, CreateUtxoBatchCollection,
        CreateUtxoBatchCollectionJob, CreateUtxoBatchParticipant, EnsureUser, IdempotencyKey,
        JobId, ObservationConsumerCheckpoints, PolicyIdentity, RecordSignedCollectionLeg,
        RequestHash, SignedEnvelopeBytes, UserId, UserStore,
    };
    use indexing::{
        BlockHash, BlockHeight, BlockRef, ConfirmationPolicy, ConfirmationProof, MovementKind,
        NetworkFee, ObservationEvent, ObservationEventId, ObservationRevision, ObservedTransaction,
        ValueMovement, WatchId,
    };
    use serde_json::{Value, json};
    use signer::KeyLocator;
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    use super::*;
    use crate::config::{
        AuthenticationOptions, BearerSecret, DatabaseOptions, IndexerOptions, WalletOptions,
    };

    #[derive(Default)]
    struct RecordingTelemetry {
        gauges: Mutex<Vec<(&'static str, f64, Vec<Attribute>)>>,
    }

    impl Telemetry for RecordingTelemetry {
        fn count(&self, _name: &'static str, _value: u64, _attributes: &[Attribute]) {}

        fn gauge(&self, name: &'static str, value: f64, attributes: &[Attribute]) {
            self.gauges
                .lock()
                .expect("recording telemetry lock must be healthy")
                .push((name, value, attributes.to_vec()));
        }

        fn duration(&self, _name: &'static str, _value: Duration, _attributes: &[Attribute]) {}
    }

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

    #[test]
    fn authentication_mode_metric_is_persistent_and_service_scoped() {
        let telemetry = RecordingTelemetry::default();
        record_authentication_mode_metric(&telemetry, AuthenticationMode::Strict);
        record_authentication_mode_metric(&telemetry, AuthenticationMode::GlobalTrusted);

        assert_eq!(
            *telemetry
                .gauges
                .lock()
                .expect("recording telemetry lock must be healthy"),
            vec![
                (
                    "payment_sdk_strict_authentication_mode",
                    1.0,
                    vec![Attribute {
                        key: "service".to_owned(),
                        value: "payment-service".to_owned(),
                    }],
                ),
                (
                    "payment_sdk_strict_authentication_mode",
                    0.0,
                    vec![Attribute {
                        key: "service".to_owned(),
                        value: "payment-service".to_owned(),
                    }],
                ),
            ]
        );
    }

    #[tokio::test]
    async fn dependency_mode_mismatch_does_not_bind_a_new_database()
    -> Result<(), Box<dyn std::error::Error>> {
        async fn global_readiness() -> (StatusCode, Json<Value>) {
            (
                StatusCode::OK,
                Json(json!({
                    "status": "ready",
                    "authentication_mode": "global_trusted"
                })),
            )
        }

        let endpoint = spawn(Router::new().route("/health/ready", get(global_readiness))).await;
        let directory = TempDir::new()?;
        let policy_path = directory.path().join("policy.json");
        std::fs::write(
            &policy_path,
            br#"{
                "version": 1,
                "scope": {"chain": "ethereum", "network": "test", "chain_id": 1},
                "deposit_ttl_seconds": 3600,
                "assets": [{
                    "asset": "native",
                    "master_destination": "0x1111111111111111111111111111111111111111",
                    "minimum_collection_amount": "1000"
                }],
                "fees": {
                    "max_fee_per_gas": "100",
                    "max_priority_fee_per_gas": "10",
                    "max_gas_limit": 200000,
                    "max_total_fee": "20000000"
                },
                "gas_funder": {
                    "address": "0x4444444444444444444444444444444444444444",
                    "key_locator": "test:gas-funder",
                    "maximum_funding_amount": "5000000"
                }
            }"#,
        )?;
        let database_path = directory.path().join("payment-service");
        let token = || "test-secret".parse::<BearerSecret>().expect("test token");
        let options = ServeOptions {
            authentication: AuthenticationOptions {
                mode: AuthenticationMode::Strict,
            },
            database: DatabaseOptions {
                database_path: database_path.clone(),
            },
            indexer: IndexerOptions {
                indexer_url: endpoint.parse()?,
                network: "test".to_owned(),
                bearer_token: Some(token()),
                request_timeout_seconds: 2,
                retry_attempts: 1,
                retry_initial_millis: 0,
                retry_max_millis: 0,
            },
            wallet: WalletOptions {
                wallet_url: endpoint.parse()?,
                bearer_token: Some(token()),
                request_timeout_seconds: 2,
                retry_attempts: 1,
                retry_initial_millis: 0,
                retry_max_millis: 0,
            },
            policy_path,
            http_bind: "127.0.0.1:0".parse()?,
            metrics_bind: "127.0.0.1:0".parse()?,
            tls_terminated_upstream: false,
            ordinary_bearer_token: Some("ordinary-secret".parse()?),
            admin_bearer_token: Some("admin-secret".parse()?),
            worker_interval_millis: 1_000,
            worker_page_size: 100,
            shutdown_grace_seconds: 10,
        };

        let error = serve(options)
            .await
            .expect_err("strict PS must reject global-trusted dependencies");
        assert!(error.to_string().contains("does not match"));
        assert!(
            !database_path.exists(),
            "dependency mismatch must fail before creating or binding the database"
        );
        Ok(())
    }

    #[test]
    fn bitcoin_maintenance_requires_authentication_and_a_canonical_network() {
        let mut options = indexer("http://127.0.0.1:8080".to_owned());
        options.network = "regtest".to_owned();
        let missing = validated_bitcoin_indexer_scope(&options, AuthenticationMode::Strict)
            .expect_err("Bitcoin maintenance must reject an unauthenticated IX");
        assert!(missing.to_string().contains("authentication is required"));

        assert_eq!(
            validated_bitcoin_indexer_scope(&options, AuthenticationMode::GlobalTrusted)
                .expect("global-trusted maintenance does not send a bearer"),
            IndexScope {
                chain: ChainId("bitcoin".to_owned()),
                network: "regtest".to_owned(),
            }
        );

        options.bearer_token = Some(
            "opaque-bitcoin-ix-token"
                .parse::<BearerSecret>()
                .expect("test token is valid"),
        );
        assert_eq!(
            validated_bitcoin_indexer_scope(&options, AuthenticationMode::Strict)
                .expect("authenticated canonical Bitcoin scope is valid"),
            IndexScope {
                chain: ChainId("bitcoin".to_owned()),
                network: "regtest".to_owned(),
            }
        );
        options.network = "testnet".to_owned();
        let noncanonical = validated_bitcoin_indexer_scope(&options, AuthenticationMode::Strict)
            .expect_err("Bitcoin maintenance must reject ambiguous network aliases");
        assert!(noncanonical.to_string().contains("testnet3"));
    }

    #[test]
    fn bitcoin_readiness_requires_canonical_checkpoint_identity() {
        let scope = IndexScope {
            chain: ChainId("bitcoin".to_owned()),
            network: "regtest".to_owned(),
        };
        let block = |height, byte| BlockRef {
            height: BlockHeight(height),
            hash: BlockHash(vec![byte; 32]),
            parent_hash: None,
            timestamp: Some(1_000 + height),
        };
        let mut status = SyncStatus {
            scope: scope.clone(),
            checkpoint: Some(block(10, 10)),
            observed_tip: Some(block(11, 11)),
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: 1,
                require_chain_finality: false,
            },
            phase: SyncPhase::Ready,
            rebuild_reason: None,
            halted_reason: None,
        };
        assert!(bitcoin_indexer_is_ready(&status, &scope));

        status.checkpoint = None;
        assert!(!bitcoin_indexer_is_ready(&status, &scope));
        status.checkpoint = Some(block(12, 12));
        assert!(!bitcoin_indexer_is_ready(&status, &scope));
        status.checkpoint = Some(BlockRef {
            hash: BlockHash(vec![13; 31]),
            ..block(10, 13)
        });
        assert!(!bitcoin_indexer_is_ready(&status, &scope));
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

    fn bitcoin_test_chain() -> ChainId {
        ChainId("bitcoin".to_owned())
    }

    fn bitcoin_test_asset() -> AssetId {
        AssetId {
            chain: bitcoin_test_chain(),
            asset: "native".to_owned(),
        }
    }

    fn bitcoin_test_address(value: &str) -> CanonicalAddress {
        CanonicalAddress {
            chain: bitcoin_test_chain(),
            value: value.to_owned(),
        }
    }

    fn bitcoin_test_transaction(byte: u8) -> CanonicalTransactionId {
        CanonicalTransactionId {
            chain: bitcoin_test_chain(),
            value: format!("{byte:02x}").repeat(32),
        }
    }

    fn bitcoin_test_block(height: u64, byte: u8) -> BlockRef {
        BlockRef {
            height: BlockHeight(height),
            hash: BlockHash(vec![byte; 32]),
            parent_hash: Some(BlockHash(vec![byte.saturating_sub(1); 32])),
            timestamp: Some(1_000 + height),
        }
    }

    fn bitcoin_test_policy() -> PolicyIdentity {
        PolicyIdentity {
            version: "bitcoin-test-v1".to_owned(),
            digest: [7; 32],
        }
    }

    fn bitcoin_test_deposit() -> CreateDepositWithLedger {
        CreateDepositWithLedger {
            deposit: CreateDeposit {
                id: DepositId("bitcoin-deposit".to_owned()),
                idempotency_key: IdempotencyKey("create-bitcoin-deposit".to_owned()),
                user_id: UserId("bitcoin-user".to_owned()),
                asset: bitcoin_test_asset(),
                address: bitcoin_test_address("bcrt1q-runtime-deposit"),
                key: KeyLocator::Identifier("opaque-bitcoin-key".to_owned()),
                key_purpose: "bitcoin-runtime-projection-test".to_owned(),
                expected: test_amount(1_000),
                birthday: BlockHeight(1),
                expires_at: 10_000,
                created_at: 2,
            },
            ledger_recorded_at: 2,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bitcoin_test_observation(
        id: &str,
        cursor: u64,
        transaction_id: CanonicalTransactionId,
        status: TransactionStatus,
        previous_status: Option<TransactionStatus>,
        movements: Vec<ValueMovement>,
        fee: Option<NetworkFee>,
    ) -> MirroredObservation {
        MirroredObservation {
            event: ObservationEvent {
                id: ObservationEventId(id.to_owned()),
                cursor: EventCursor(cursor),
                watch_ids: vec![WatchId("bitcoin-runtime-watch".to_owned())],
                previous_status,
                transaction: ObservedTransaction {
                    scope: IndexScope {
                        chain: bitcoin_test_chain(),
                        network: "regtest".to_owned(),
                    },
                    transaction_id,
                    revision: ObservationRevision(cursor),
                    status,
                    movements,
                    fee,
                    first_seen_at: 100 + cursor,
                    observed_at: 100 + cursor,
                },
            },
            received_at: 200 + cursor,
        }
    }

    struct BitcoinProjectionFixture {
        _directory: TempDir,
        repository: Repository,
        deposit: Deposit,
        collection: Collection,
    }

    async fn bitcoin_projection_fixture()
    -> Result<BitcoinProjectionFixture, Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let repository = Repository::new(RocksDbStorage::open(directory.path())?);
        let scope = IndexScope {
            chain: bitcoin_test_chain(),
            network: "regtest".to_owned(),
        };
        let owner = CommandPrincipal("bitcoin-exchange".to_owned());
        repository
            .initialize_or_validate(InitializePaymentDatabase {
                scope,
                active_policy: bitcoin_test_policy(),
                initialized_at: 1,
            })
            .await?;
        repository
            .ensure_user(EnsureUser {
                id: UserId("bitcoin-user".to_owned()),
                owner: owner.clone(),
                first_seen_at: 1,
            })
            .await?;
        let created = repository
            .create_with_ledger(bitcoin_test_deposit())
            .await?;
        let deposit = created.deposit;

        let funding = bitcoin_test_observation(
            "bitcoin-funding-event",
            1,
            bitcoin_test_transaction(0x11),
            TransactionStatus::Confirmed {
                block: bitcoin_test_block(10, 10),
                proof: ConfirmationProof::Depth {
                    required: 1,
                    observed: 1,
                },
            },
            None,
            vec![ValueMovement {
                id: MovementId("bitcoin-funding-output".to_owned()),
                asset: bitcoin_test_asset(),
                amount: test_amount(1_000),
                from: None,
                to: Some(deposit.address.clone()),
                kind: MovementKind::Output,
            }],
            None,
        );
        repository
            .mirror_and_advance(MirrorObservation {
                expected_cursor: None,
                observation: funding.clone(),
            })
            .await?;
        let funded = repository
            .project_and_advance(ProjectObservation {
                expected_cursor: None,
                through: funding.event.cursor,
                affected_deposits: vec![deposit.id.clone()],
                ledger_updates: vec![RecordObservation {
                    event_id: funding.event.id,
                    effect: LedgerEffect::Incoming {
                        movements: vec![MovementId("bitcoin-funding-output".to_owned())],
                    },
                    deposit_id: deposit.id.clone(),
                    expected_head: Some(created.ledger.id),
                    recorded_at: 10,
                }],
                reconciliation_cases: Vec::new(),
                fee_treatment: ProjectionFeeTreatment::Separate,
                utxo_batch_transition: None,
            })
            .await?;
        let funded_head = match funded
            .ledger_results
            .first()
            .expect("funding projection has one ledger result")
        {
            deposits::ApplyResult::Appended { entry }
            | deposits::ApplyResult::AlreadyPresent { entry } => entry.id.clone(),
        };

        let collection_id = CollectionId("bitcoin-batch".to_owned());
        let job_id = JobId("bitcoin-batch-job".to_owned());
        repository
            .create_or_replay(CreateJob {
                id: job_id.clone(),
                command: CommandIdentity {
                    principal: owner.clone(),
                    operation: CommandOperation::CreateCollection,
                    client_key: IdempotencyKey("bitcoin-batch-command".to_owned()),
                    request_hash: RequestHash([9; 32]),
                },
                payload: JobPayload::CreateUtxoBatchCollection(CreateUtxoBatchCollectionJob {
                    collection_id: collection_id.clone(),
                    deposit_ids: vec![deposit.id.clone()],
                }),
                user_owner: owner,
                policy: bitcoin_test_policy(),
                created_at: 20,
            })
            .await?;
        let resource = CollectionSpendResource {
            id: CollectionSpendResourceId {
                transaction_id: bitcoin_test_transaction(0x11),
                output_index: 0,
            },
            amount: test_amount(1_000),
            evidence: CollectionSpendResourceEvidence::new(b"runtime-utxo-evidence".to_vec())?,
        };
        let created = repository
            .create_or_replay_utxo_batch(CreateUtxoBatchCollection {
                id: collection_id,
                job_id,
                asset: bitcoin_test_asset(),
                destination: bitcoin_test_address("bcrt1q-runtime-master"),
                policy: bitcoin_test_policy(),
                participants: vec![CreateUtxoBatchParticipant {
                    user_id: deposit.user_id.clone(),
                    deposit_id: deposit.id.clone(),
                    expected_ledger_head: funded_head,
                    reservation_amount: test_amount(1_000),
                    spend_resources: vec![resource],
                }],
                leg: CreateCollectionLeg {
                    id: CollectionLegId("bitcoin-sweep".to_owned()),
                    kind: CollectionLegKind::Sweep,
                    planned_amount: None,
                },
                created_at: 20,
            })
            .await?
            .collection()
            .clone();
        let signed = repository
            .record_signed(RecordSignedCollectionLeg {
                collection_id: created.id.clone(),
                leg_id: created.legs[0].id.clone(),
                expected: collection_guard(&created, &created.legs[0]),
                expected_transaction_id: bitcoin_test_transaction(0x22),
                envelope: SignedEnvelopeBytes::new(vec![1, 2, 3, 4])?,
                allocations: vec![CollectionAllocation {
                    deposit_id: deposit.id.clone(),
                    asset: bitcoin_test_asset(),
                    gross_debit: test_amount(1_000),
                    master_credit: test_amount(990),
                    allocated_fee_asset: bitcoin_test_asset(),
                    allocated_fee: test_amount(10),
                }],
                signed_at: 21,
                expires_at: 10_000,
            })
            .await?;
        let broadcast = repository
            .accept_broadcast(AcceptCollectionBroadcast {
                collection_id: signed.id.clone(),
                leg_id: signed.legs[0].id.clone(),
                expected: collection_guard(&signed, &signed.legs[0]),
                transaction_id: bitcoin_test_transaction(0x22),
                accepted_at: 22,
            })
            .await?;
        let collection = repository
            .attach_watch(AttachCollectionWatch {
                collection_id: broadcast.id.clone(),
                leg_id: broadcast.legs[0].id.clone(),
                expected: collection_guard(&broadcast, &broadcast.legs[0]),
                watch_id: WatchId("bitcoin-collection-watch".to_owned()),
                updated_at: 23,
            })
            .await?;
        Ok(BitcoinProjectionFixture {
            _directory: directory,
            repository,
            deposit,
            collection,
        })
    }

    async fn mirror_and_project_bitcoin_event(
        repository: &Repository,
        expected_cursor: EventCursor,
        event: &MirroredObservation,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        repository
            .mirror_and_advance(MirrorObservation {
                expected_cursor: Some(expected_cursor),
                observation: event.clone(),
            })
            .await?;
        let Some((affected, updates, cases, fee_treatment, transition)) =
            classify_projection(repository, event).await?
        else {
            return Err("the retained Bitcoin collection event must classify".into());
        };
        let has_transition = transition.is_some();
        repository
            .project_and_advance(ProjectObservation {
                expected_cursor: Some(expected_cursor),
                through: event.event.cursor,
                affected_deposits: affected,
                ledger_updates: updates,
                reconciliation_cases: cases,
                fee_treatment,
                utxo_batch_transition: transition,
            })
            .await?;
        Ok(has_transition)
    }

    #[tokio::test]
    async fn bitcoin_first_inclusion_does_not_double_debit_the_factual_fee()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = bitcoin_projection_fixture().await?;
        let event = bitcoin_test_observation(
            "bitcoin-collection-included",
            2,
            bitcoin_test_transaction(0x22),
            TransactionStatus::Included {
                block: bitcoin_test_block(20, 20),
                confirmations: 1,
            },
            None,
            vec![
                ValueMovement {
                    id: MovementId("bitcoin-collection-input".to_owned()),
                    asset: bitcoin_test_asset(),
                    amount: test_amount(1_000),
                    from: Some(fixture.deposit.address.clone()),
                    to: None,
                    kind: MovementKind::Input,
                },
                ValueMovement {
                    id: MovementId("bitcoin-master-output".to_owned()),
                    asset: bitcoin_test_asset(),
                    amount: test_amount(990),
                    from: None,
                    to: Some(fixture.collection.destination.clone()),
                    kind: MovementKind::Output,
                },
            ],
            Some(NetworkFee {
                asset: bitcoin_test_asset(),
                amount: test_amount(10),
                payer: Some(fixture.deposit.address.clone()),
            }),
        );
        fixture
            .repository
            .mirror_and_advance(MirrorObservation {
                expected_cursor: Some(EventCursor(1)),
                observation: event.clone(),
            })
            .await?;
        let Some((affected, updates, cases, fee_treatment, transition)) =
            classify_projection(&fixture.repository, &event).await?
        else {
            panic!("the exact retained Bitcoin collection must classify");
        };
        assert_eq!(
            fee_treatment,
            ProjectionFeeTreatment::IncludedInMovementEffect
        );
        assert!(transition.is_none());
        fixture
            .repository
            .project_and_advance(ProjectObservation {
                expected_cursor: Some(EventCursor(1)),
                through: EventCursor(2),
                affected_deposits: affected,
                ledger_updates: updates,
                reconciliation_cases: cases,
                fee_treatment,
                utxo_batch_transition: transition,
            })
            .await?;

        let ledger = fixture
            .repository
            .current(&fixture.deposit.id)
            .await?
            .expect("Bitcoin deposit ledger remains open");
        assert_eq!(ledger.balances.balance, AtomicAmount::ZERO);
        assert_eq!(ledger.balances.collected, AtomicAmount::ZERO);
        Ok(())
    }

    #[tokio::test]
    async fn bitcoin_offline_projection_replays_confirmed_included_reorged_in_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = bitcoin_projection_fixture().await?;
        let transaction_id = bitcoin_test_transaction(0x22);
        let movements = vec![
            ValueMovement {
                id: MovementId("bitcoin-collection-input".to_owned()),
                asset: bitcoin_test_asset(),
                amount: test_amount(1_000),
                from: Some(fixture.deposit.address.clone()),
                to: None,
                kind: MovementKind::Input,
            },
            ValueMovement {
                id: MovementId("bitcoin-master-output".to_owned()),
                asset: bitcoin_test_asset(),
                amount: test_amount(990),
                from: None,
                to: Some(fixture.collection.destination.clone()),
                kind: MovementKind::Output,
            },
        ];
        let fee = Some(NetworkFee {
            asset: bitcoin_test_asset(),
            amount: test_amount(10),
            payer: Some(fixture.deposit.address.clone()),
        });
        let included_status = TransactionStatus::Included {
            block: bitcoin_test_block(20, 20),
            confirmations: 1,
        };
        let confirmed_status = TransactionStatus::Confirmed {
            block: bitcoin_test_block(20, 20),
            proof: ConfirmationProof::Depth {
                required: 2,
                observed: 2,
            },
        };
        let included = bitcoin_test_observation(
            "bitcoin-collection-included",
            2,
            transaction_id.clone(),
            included_status.clone(),
            None,
            movements.clone(),
            fee.clone(),
        );
        assert!(
            !mirror_and_project_bitcoin_event(&fixture.repository, EventCursor(1), &included,)
                .await?
        );
        let confirmed = bitcoin_test_observation(
            "bitcoin-collection-confirmed",
            3,
            transaction_id.clone(),
            confirmed_status.clone(),
            Some(included_status.clone()),
            movements.clone(),
            fee.clone(),
        );
        assert!(
            mirror_and_project_bitcoin_event(&fixture.repository, EventCursor(2), &confirmed,)
                .await?
        );
        assert_eq!(
            fixture
                .repository
                .collection(&fixture.collection.id)
                .await?
                .expect("collection remains durable")
                .state,
            CollectionState::Completed
        );

        // Core/IX unwind confirmation depth before orphaning the inclusion
        // block. Both revisions can be waiting when PS comes back online.
        let confirmation_rollback = bitcoin_test_observation(
            "bitcoin-collection-confirmation-rollback",
            4,
            transaction_id.clone(),
            included_status.clone(),
            Some(confirmed_status),
            movements.clone(),
            fee.clone(),
        );
        assert!(
            !mirror_and_project_bitcoin_event(
                &fixture.repository,
                EventCursor(3),
                &confirmation_rollback,
            )
            .await?
        );
        let after_confirmation_rollback = fixture
            .repository
            .current(&fixture.deposit.id)
            .await?
            .expect("Bitcoin deposit ledger remains open");
        assert_eq!(
            after_confirmation_rollback.balances.collected,
            AtomicAmount::ZERO
        );
        assert_eq!(
            fixture
                .repository
                .collection(&fixture.collection.id)
                .await?
                .expect("collection remains durable")
                .state,
            CollectionState::Completed
        );

        let reorged = bitcoin_test_observation(
            "bitcoin-collection-reorged",
            5,
            transaction_id,
            TransactionStatus::Reorged {
                previous_block: bitcoin_test_block(20, 20),
            },
            Some(included_status),
            movements,
            fee,
        );
        assert!(
            mirror_and_project_bitcoin_event(&fixture.repository, EventCursor(4), &reorged,)
                .await?
        );
        let reorged_collection = fixture
            .repository
            .collection(&fixture.collection.id)
            .await?
            .expect("collection remains durable");
        assert_eq!(reorged_collection.state, CollectionState::Reorged);
        assert!(matches!(
            reorged_collection.legs[0].state,
            CollectionLegState::Reorged { .. }
        ));
        let reorged_ledger = fixture
            .repository
            .current(&fixture.deposit.id)
            .await?
            .expect("Bitcoin deposit ledger remains open");
        assert_eq!(reorged_ledger.balances.collected, AtomicAmount::ZERO);
        assert_eq!(reorged_ledger.balances.balance, test_amount(1_000));
        Ok(())
    }

    #[tokio::test]
    async fn unknown_reserved_spend_with_change_projects_net_and_opens_conflict()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = bitcoin_projection_fixture().await?;
        let event = bitcoin_test_observation(
            "bitcoin-conflicting-spend",
            2,
            bitcoin_test_transaction(0x33),
            TransactionStatus::Included {
                block: bitcoin_test_block(20, 20),
                confirmations: 1,
            },
            None,
            vec![
                ValueMovement {
                    id: MovementId("bitcoin-conflict-input".to_owned()),
                    asset: bitcoin_test_asset(),
                    amount: test_amount(1_000),
                    from: Some(fixture.deposit.address.clone()),
                    to: None,
                    kind: MovementKind::Input,
                },
                ValueMovement {
                    id: MovementId("bitcoin-conflict-change".to_owned()),
                    asset: bitcoin_test_asset(),
                    amount: test_amount(400),
                    from: None,
                    to: Some(fixture.deposit.address.clone()),
                    kind: MovementKind::Output,
                },
                ValueMovement {
                    id: MovementId("bitcoin-conflict-external".to_owned()),
                    asset: bitcoin_test_asset(),
                    amount: test_amount(590),
                    from: None,
                    to: Some(bitcoin_test_address("bcrt1q-external")),
                    kind: MovementKind::Output,
                },
            ],
            Some(NetworkFee {
                asset: bitcoin_test_asset(),
                amount: test_amount(10),
                payer: Some(fixture.deposit.address.clone()),
            }),
        );
        fixture
            .repository
            .mirror_and_advance(MirrorObservation {
                expected_cursor: Some(EventCursor(1)),
                observation: event.clone(),
            })
            .await?;
        let Some((affected, updates, cases, fee_treatment, transition)) =
            classify_projection(&fixture.repository, &event).await?
        else {
            panic!("the conflicting Bitcoin spend must classify conservatively");
        };
        assert_eq!(
            fee_treatment,
            ProjectionFeeTreatment::IncludedInMovementEffect
        );
        assert!(transition.is_none());
        assert_eq!(cases.len(), 1);
        assert!(matches!(
            &cases[0].reason,
            ReconciliationReason::ReservedSpendConflict { collection_id, transaction_id }
                if collection_id == &fixture.collection.id
                    && transaction_id == &event.event.transaction.transaction_id
        ));
        fixture
            .repository
            .project_and_advance(ProjectObservation {
                expected_cursor: Some(EventCursor(1)),
                through: EventCursor(2),
                affected_deposits: affected,
                ledger_updates: updates,
                reconciliation_cases: cases,
                fee_treatment,
                utxo_batch_transition: transition,
            })
            .await?;

        let ledger = fixture
            .repository
            .current(&fixture.deposit.id)
            .await?
            .expect("Bitcoin deposit ledger remains open");
        assert_eq!(ledger.balances.balance, test_amount(400));
        assert!(
            fixture
                .repository
                .automatic_actions_blocked(&fixture.deposit.id)
                .await?
        );
        Ok(())
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

        async fn readiness() -> (StatusCode, Json<Value>) {
            (
                StatusCode::OK,
                Json(json!({
                    "status": "ready",
                    "authentication_mode": "global_trusted"
                })),
            )
        }

        let endpoint = spawn(
            Router::new()
                .route("/health/ready", get(readiness))
                .route("/v1/events", get(events)),
        )
        .await;
        let directory = TempDir::new()?;
        let options = IngestOptions {
            authentication: crate::config::AuthenticationOptions {
                mode: AuthenticationMode::GlobalTrusted,
            },
            database: DatabaseOptions {
                database_path: directory.path().join("payment-service"),
            },
            indexer: indexer(endpoint),
            page_size: 10,
            max_pages: 2,
        };
        let repository = PersistentPaymentRepository::new(RocksDbStorage::open(
            &options.database.database_path,
        )?);
        repository
            .initialize_or_validate_principal_scope(
                InitializePaymentDatabase {
                    scope: ethereum_scope("test"),
                    active_policy: deposits::PolicyIdentity {
                        version: "test-policy".to_owned(),
                        digest: [7; 32],
                    },
                    initialized_at: 1,
                },
                PrincipalScopeMode::GlobalTrusted,
            )
            .await?;
        drop(repository);

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
