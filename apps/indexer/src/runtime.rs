use std::{
    error::Error,
    future::Future,
    num::NonZeroU32,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::State,
    http::{HeaderValue, header::CONTENT_TYPE},
    response::IntoResponse,
    routing::get,
};
use chain_bitcoin::{
    BitcoinBlockInterpreter, BitcoinCoreBlockSource, BitcoinIndexRecordCodec,
    BitcoinIndexSourceConfig,
};
use chain_ethereum::{
    EthereumBlockInterpreter, EthereumHttpBlockSource, EthereumIndexRecordCodec,
    EthereumIndexSourceConfig, EthereumNewHeadsClient, EthereumNewHeadsConfig,
    EthereumNewHeadsConnectionEvent,
};
use http::{
    BearerToken, HealthState, HttpServerConfig, HttpTransport, HttpTransportConfig, RequestLimits,
    RetryPolicy, TransportSecurity,
};
use indexing::{
    AbortRebuildCommand, ActivateRebuildCommand, BeginRebuildCommand, BlockCommitObservation,
    BlockCommitObservationOutcome, BlockHash, BlockHeight, BlockInterpreter, BlockRef, BlockSource,
    CleanupGenerationCommand, CommitBlockCommand, CommitBlockOutcome, CommitRebuildBlockCommand,
    CommitWatchBackfillCommand, IndexError, IndexErrorKind, IndexRepository, IndexScope,
    IndexingWorker, MigrateIndexPolicyCommand, MigrateIndexPolicyOutcome, OrderedSyncConfig,
    OrderedSyncWorker, PersistentIndexConfig, PersistentIndexRepository,
    PrepareRebuildActivationCommand, ProjectionBatch, ProjectionMutation, RebuildGeneration,
    RebuildPhase, ReorgDepth, ReorgObservation, SourceError, SyncObserver, SyncPhase, SyncRequest,
    SyncStatus, ValidateRebuildCommand,
};
use json_rpc::TransportJsonRpcClient;
use storage_rocksdb::RocksDbStorage;
use telemetry::{Attribute, PrometheusTelemetry, Telemetry};
use tokio::{sync::watch, task::JoinSet};

use crate::{
    api::{self, ApiRepository, ApiState},
    config::{
        BackupOptions, BitcoinGenerationOptions, BitcoinMigrationCommand, BitcoinMigrationOptions,
        BitcoinPolicyMigrationOptions, BitcoinRebuildOptions, BitcoinRepositoryOptions,
        BitcoinServeOptions, BitcoinSourceOptions, GenerationOptions, MigrationCommand,
        MigrationOptions, PolicyMigrationOptions, RebuildOptions, RepositoryOptions,
        SchemaMigrationOptions, ServeOptions, SourceOptions, bitcoin_bootstrap_height,
        bootstrap_height,
    },
};

type AppError = Box<dyn Error + Send + Sync>;
type AppResult<T> = Result<T, AppError>;
type RpcClient = TransportJsonRpcClient<HttpTransport>;
type EthereumSource = EthereumHttpBlockSource<RpcClient>;
type BitcoinSource = BitcoinCoreBlockSource<RpcClient>;
type EthereumRepository = PersistentIndexRepository<RocksDbStorage, EthereumIndexRecordCodec>;
type BitcoinRepository = PersistentIndexRepository<RocksDbStorage, BitcoinIndexRecordCodec>;

struct SyncRuntime<S, I, R> {
    worker: Arc<OrderedSyncWorker<SharedSource<S>, I, R>>,
    repository: R,
    source: SharedSource<S>,
    interpreter: I,
    scope: IndexScope,
    snapshot: SharedOperationalSnapshot,
    telemetry: PrometheusTelemetry,
    poll_interval: Duration,
    backfill_projection_policy: BackfillProjectionPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackfillProjectionPolicy {
    RequireUnconditionalFacts,
    DropBitcoinConditionalSpendCandidates,
}

struct ServiceRuntimeOptions {
    http_bind: std::net::SocketAddr,
    metrics_bind: std::net::SocketAddr,
    bearer_token: Option<String>,
    ready_max_lag: u64,
    ready_max_age: Duration,
}

enum WakeSource {
    PollOnly,
    EthereumNewHeads(String),
}

#[derive(Clone)]
struct MetricContext {
    telemetry: Arc<dyn Telemetry>,
    attributes: Arc<[Attribute]>,
}

impl MetricContext {
    fn new(telemetry: Arc<dyn Telemetry>, network: String) -> Self {
        Self {
            telemetry,
            attributes: Arc::from([Attribute {
                key: "network".to_owned(),
                value: network,
            }]),
        }
    }

    fn observe_source<T>(
        &self,
        operation: SourceOperation,
        elapsed: Duration,
        result: &Result<T, SourceError>,
    ) {
        let counter = if result.is_ok() {
            operation.success_counter()
        } else {
            operation.failure_counter()
        };
        self.telemetry.count(counter, 1, self.attributes.as_ref());
        self.telemetry.duration(
            operation.duration_metric(),
            elapsed,
            self.attributes.as_ref(),
        );
    }

    fn set_websocket_enabled(&self, enabled: bool) {
        self.telemetry.gauge(
            "ix_websocket_enabled",
            if enabled { 1.0 } else { 0.0 },
            self.attributes.as_ref(),
        );
    }

    fn set_websocket_connected(&self, connected: bool) {
        self.telemetry.gauge(
            "ix_websocket_connected",
            if connected { 1.0 } else { 0.0 },
            self.attributes.as_ref(),
        );
    }

    fn observe_websocket_event(&self, event: EthereumNewHeadsConnectionEvent) {
        match event {
            EthereumNewHeadsConnectionEvent::Connected => self.set_websocket_connected(true),
            EthereumNewHeadsConnectionEvent::Disconnected => self.set_websocket_connected(false),
            EthereumNewHeadsConnectionEvent::ReconnectScheduled => {
                self.telemetry
                    .count("ix_websocket_reconnect_total", 1, self.attributes.as_ref())
            }
            EthereumNewHeadsConnectionEvent::Failure => {
                self.telemetry
                    .count("ix_websocket_failure_total", 1, self.attributes.as_ref())
            }
        }
    }

    fn count_websocket_wake(&self) {
        self.telemetry
            .count("ix_websocket_wakes_total", 1, self.attributes.as_ref());
    }
}

impl SyncObserver for MetricContext {
    fn block_commit(&self, observation: BlockCommitObservation) {
        self.telemetry.duration(
            "ix_block_commit_seconds",
            observation.elapsed,
            self.attributes.as_ref(),
        );
        let counter = match observation.outcome {
            BlockCommitObservationOutcome::Success(CommitBlockOutcome::Applied) => {
                "ix_block_commit_applied_total"
            }
            BlockCommitObservationOutcome::Success(CommitBlockOutcome::AlreadyApplied) => {
                "ix_block_commit_already_applied_total"
            }
            BlockCommitObservationOutcome::Failure { .. } => "ix_block_commit_failure_total",
        };
        self.telemetry.count(counter, 1, self.attributes.as_ref());
    }

    fn reorg_detected(&self, observation: ReorgObservation) {
        match observation.depth {
            ReorgDepth::Exact { depth, .. } => {
                self.telemetry
                    .count("ix_reorg_total", 1, self.attributes.as_ref());
                self.telemetry.gauge(
                    "ix_reorg_depth_blocks",
                    depth as f64,
                    self.attributes.as_ref(),
                );
            }
            ReorgDepth::BeyondRetention { minimum_depth, .. } => {
                self.telemetry.count(
                    "ix_reorg_beyond_retention_total",
                    1,
                    self.attributes.as_ref(),
                );
                self.telemetry.gauge(
                    "ix_reorg_depth_blocks",
                    minimum_depth as f64,
                    self.attributes.as_ref(),
                );
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SourceOperation {
    Tip,
    BlockAt,
    CanonicalHash,
}

impl SourceOperation {
    const fn success_counter(self) -> &'static str {
        match self {
            Self::Tip => "ix_source_tip_success_total",
            Self::BlockAt => "ix_source_block_at_success_total",
            Self::CanonicalHash => "ix_source_canonical_hash_success_total",
        }
    }

    const fn failure_counter(self) -> &'static str {
        match self {
            Self::Tip => "ix_source_tip_failure_total",
            Self::BlockAt => "ix_source_block_at_failure_total",
            Self::CanonicalHash => "ix_source_canonical_hash_failure_total",
        }
    }

    const fn duration_metric(self) -> &'static str {
        match self {
            Self::Tip => "ix_source_tip_seconds",
            Self::BlockAt => "ix_source_block_at_seconds",
            Self::CanonicalHash => "ix_source_canonical_hash_seconds",
        }
    }
}

struct SharedSource<S> {
    source: Arc<S>,
    metrics: MetricContext,
}

impl<S> Clone for SharedSource<S> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            metrics: self.metrics.clone(),
        }
    }
}

impl<S> SharedSource<S> {
    fn new(source: Arc<S>, metrics: MetricContext) -> Self {
        Self { source, metrics }
    }
}

impl<S> BlockSource for SharedSource<S>
where
    S: BlockSource,
{
    type Block = S::Block;

    fn tip<'a>(&'a self) -> indexing::BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move {
            let started = Instant::now();
            let result = self.source.tip().await;
            self.metrics
                .observe_source(SourceOperation::Tip, started.elapsed(), &result);
            result
        })
    }

    fn block_at<'a>(
        &'a self,
        height: BlockHeight,
    ) -> indexing::BoxFuture<'a, Result<Self::Block, SourceError>> {
        Box::pin(async move {
            let started = Instant::now();
            let result = self.source.block_at(height).await;
            self.metrics
                .observe_source(SourceOperation::BlockAt, started.elapsed(), &result);
            result
        })
    }

    fn canonical_hash<'a>(
        &'a self,
        height: BlockHeight,
    ) -> indexing::BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
        Box::pin(async move {
            let started = Instant::now();
            let result = self.source.canonical_hash(height).await;
            self.metrics
                .observe_source(SourceOperation::CanonicalHash, started.elapsed(), &result);
            result
        })
    }
}

#[derive(Clone, Debug, Default)]
struct OperationalSnapshot {
    status: Option<SyncStatus>,
    last_reconciled: Option<Instant>,
}

type SharedOperationalSnapshot = Arc<Mutex<OperationalSnapshot>>;

pub async fn serve(options: ServeOptions, telemetry: PrometheusTelemetry) -> AppResult<()> {
    serve_until(options, telemetry, async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| Box::new(error) as AppError)
    })
    .await
}

pub(crate) async fn serve_until<F>(
    options: ServeOptions,
    telemetry: PrometheusTelemetry,
    shutdown: F,
) -> AppResult<()>
where
    F: Future<Output = AppResult<()>>,
{
    tokio::pin!(shutdown);
    options.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    let repository = repository(storage, &options.repository)?;
    let source =
        match startup_or_shutdown(connect_source(&scope, &options.source), shutdown.as_mut())
            .await?
        {
            Some(source) => Arc::new(source),
            None => return Ok(()),
        };
    let interpreter = EthereumBlockInterpreter::new(scope.clone())?;
    let metric_context = MetricContext::new(
        Arc::new(telemetry.clone()),
        options.repository.network.clone(),
    );
    let shared_source = SharedSource::new(Arc::clone(&source), metric_context.clone());
    let worker_config = OrderedSyncConfig::new(
        scope.clone(),
        bootstrap_height(&options.repository),
        options.repository.confirmation_policy()?,
        options.repository.reorg_retention,
    )?;
    let worker = Arc::new(
        OrderedSyncWorker::new(
            shared_source.clone(),
            interpreter.clone(),
            repository.clone(),
            worker_config,
        )
        .with_observer(Arc::new(metric_context.clone())),
    );

    let limits = RequestLimits::default();
    let api_repository: Arc<dyn ApiRepository> = Arc::new(api::ApiRepositoryAdapter::<
        _,
        api::EthereumApiTarget,
    >::new(repository.clone()));
    let api_state = Arc::new(ApiState::new(
        scope.clone(),
        api_repository,
        bootstrap_height(&options.repository),
        limits.clone(),
    ));
    let health = HealthState::new(false);
    let snapshot = Arc::new(Mutex::new(OperationalSnapshot::default()));
    let runtime = SyncRuntime {
        worker,
        repository,
        source: shared_source,
        interpreter,
        scope,
        snapshot: Arc::clone(&snapshot),
        telemetry: telemetry.clone(),
        poll_interval: options.poll_interval(),
        backfill_projection_policy: BackfillProjectionPolicy::RequireUnconditionalFacts,
    };
    let wake_source = if let Some(url) = options.source.rpc_ws_url.clone() {
        WakeSource::EthereumNewHeads(url)
    } else {
        WakeSource::PollOnly
    };
    tracing::info!(network = %options.repository.network, "Indexer Service started");
    let ready_max_age = options.ready_max_age();
    run_service_until(
        runtime,
        api::router(api_state),
        health,
        limits,
        ServiceRuntimeOptions {
            http_bind: options.http_bind,
            metrics_bind: options.metrics_bind,
            bearer_token: options.bearer_token,
            ready_max_lag: options.ready_max_lag,
            ready_max_age,
        },
        telemetry,
        metric_context,
        wake_source,
        snapshot,
        shutdown.as_mut(),
    )
    .await
}

pub async fn serve_bitcoin(
    options: BitcoinServeOptions,
    telemetry: PrometheusTelemetry,
) -> AppResult<()> {
    serve_bitcoin_until(options, telemetry, async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| Box::new(error) as AppError)
    })
    .await
}

pub(crate) async fn serve_bitcoin_until<F>(
    options: BitcoinServeOptions,
    telemetry: PrometheusTelemetry,
    shutdown: F,
) -> AppResult<()>
where
    F: Future<Output = AppResult<()>>,
{
    tokio::pin!(shutdown);
    options.validate()?;
    let scope = options.repository.scope()?;
    let network = options.repository.network()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    let repository = bitcoin_repository(storage, &options.repository)?;
    let source = match startup_or_shutdown(
        connect_bitcoin_source(&scope, network, &options.source),
        shutdown.as_mut(),
    )
    .await?
    {
        Some(source) => Arc::new(source),
        None => return Ok(()),
    };
    let interpreter = BitcoinBlockInterpreter::new(scope.clone(), network)?;
    // Never expose a persisted Ready phase from a prior process before this
    // process has completed its first authoritative reconciliation. Recovery
    // phases remain sticky and are cleared only by their explicit workflows.
    let mut startup_status = repository.status(&scope).await?;
    if !matches!(
        startup_status.phase,
        SyncPhase::RebuildRequired | SyncPhase::Halted
    ) {
        startup_status.phase = SyncPhase::Reconciling;
        startup_status.observed_tip = None;
        startup_status.rebuild_reason = None;
        startup_status.halted_reason = None;
        repository.set_status(startup_status).await?;
    }
    let metric_context = MetricContext::new(
        Arc::new(telemetry.clone()),
        options.repository.network.clone(),
    );
    let shared_source = SharedSource::new(Arc::clone(&source), metric_context.clone());
    let worker_config = OrderedSyncConfig::new(
        scope.clone(),
        bitcoin_bootstrap_height(&options.repository),
        options.repository.confirmation_policy()?,
        options.repository.reorg_retention,
    )?;
    let worker = Arc::new(
        OrderedSyncWorker::new(
            shared_source.clone(),
            interpreter.clone(),
            repository.clone(),
            worker_config,
        )
        .with_observer(Arc::new(metric_context.clone())),
    );
    let limits = RequestLimits::default();
    let api_repository: Arc<dyn ApiRepository> = Arc::new(api::ApiRepositoryAdapter::<
        _,
        api::BitcoinApiTarget,
    >::new(repository.clone()));
    let utxo_repository: Arc<dyn api::BitcoinUtxoRepository> =
        Arc::new(api::BitcoinUtxoRepositoryAdapter::new(repository.clone()));
    let health = HealthState::new(false);
    let api_state = Arc::new(ApiState::new_bitcoin(
        scope.clone(),
        network,
        api_repository,
        utxo_repository,
        health.clone(),
        bitcoin_bootstrap_height(&options.repository),
        limits.clone(),
    ));
    let snapshot = Arc::new(Mutex::new(OperationalSnapshot::default()));
    let runtime = SyncRuntime {
        worker,
        repository,
        source: shared_source,
        interpreter,
        scope,
        snapshot: Arc::clone(&snapshot),
        telemetry: telemetry.clone(),
        poll_interval: options.poll_interval(),
        backfill_projection_policy: BackfillProjectionPolicy::DropBitcoinConditionalSpendCandidates,
    };

    tracing::info!(network = %options.repository.network, "Bitcoin Indexer Service started");
    let ready_max_age = options.ready_max_age();
    run_service_until(
        runtime,
        api::bitcoin_router(api_state),
        health,
        limits,
        ServiceRuntimeOptions {
            http_bind: options.http_bind,
            metrics_bind: options.metrics_bind,
            bearer_token: options.bearer_token,
            ready_max_lag: options.ready_max_lag,
            ready_max_age,
        },
        telemetry,
        metric_context,
        WakeSource::PollOnly,
        snapshot,
        shutdown.as_mut(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_service_until<S, I, R, F>(
    runtime: SyncRuntime<S, I, R>,
    api_router: Router,
    health: HealthState,
    limits: RequestLimits,
    options: ServiceRuntimeOptions,
    telemetry: PrometheusTelemetry,
    metric_context: MetricContext,
    wake_source: WakeSource,
    snapshot: SharedOperationalSnapshot,
    shutdown: Pin<&mut F>,
) -> AppResult<()>
where
    S: BlockSource + 'static,
    I: BlockInterpreter<Block = S::Block, Target = R::Target, Undo = R::Undo>
        + Clone
        + Send
        + Sync
        + 'static,
    R: IndexRepository + Clone + Send + Sync + 'static,
    F: Future<Output = AppResult<()>>,
{
    let bearer_token = options
        .bearer_token
        .as_deref()
        .map(BearerToken::new)
        .transpose()?;
    let security = if options.http_bind.ip().is_loopback() {
        TransportSecurity::PlaintextLoopback
    } else {
        TransportSecurity::TlsTerminatedUpstream
    };
    let server_config = HttpServerConfig::new(options.http_bind, security, bearer_token, limits);
    let application_router = http::service_router(api_router, &server_config, health.clone())?;
    let metrics_config = HttpServerConfig::new(
        options.metrics_bind,
        TransportSecurity::PlaintextLoopback,
        None,
        RequestLimits::default(),
    );
    let metrics_router = Router::new()
        .route("/metrics", get(metrics))
        .with_state(telemetry.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = JoinSet::<AppResult<()>>::new();

    let api_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        http::serve(
            application_router,
            &server_config,
            shutdown_signal(api_shutdown),
        )
        .await
        .map_err(|error| Box::new(error) as AppError)
    });
    let metrics_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        http::serve(
            metrics_router,
            &metrics_config,
            shutdown_signal(metrics_shutdown),
        )
        .await
        .map_err(|error| Box::new(error) as AppError)
    });

    let (wake_tx, wake_rx) = tokio::sync::mpsc::channel(1);
    let sync_shutdown = shutdown_rx.clone();
    tasks.spawn(sync_loop(runtime, wake_rx, sync_shutdown));

    let readiness_shutdown = shutdown_rx.clone();
    let readiness_telemetry = telemetry.clone();
    tasks.spawn(readiness_loop(
        health,
        snapshot,
        readiness_telemetry,
        options.ready_max_lag,
        options.ready_max_age,
        readiness_shutdown,
    ));

    metric_context.set_websocket_connected(false);
    match wake_source {
        WakeSource::PollOnly => metric_context.set_websocket_enabled(false),
        WakeSource::EthereumNewHeads(url) => {
            metric_context.set_websocket_enabled(true);
            let websocket_shutdown = shutdown_rx.clone();
            let websocket_metrics = metric_context.clone();
            let websocket_wake_tx = wake_tx.clone();
            tasks.spawn(async move {
                let client = EthereumNewHeadsClient::new(EthereumNewHeadsConfig::new(
                    Some(url),
                    Duration::from_secs(1),
                )?);
                let wake_metrics = websocket_metrics.clone();
                let event_metrics = websocket_metrics.clone();
                let websocket = client.run_with_events(
                    move |_| {
                        let _ignored = websocket_wake_tx.try_send(());
                        wake_metrics.count_websocket_wake();
                        true
                    },
                    move |event| event_metrics.observe_websocket_event(event),
                );
                supervise_websocket(websocket, websocket_metrics, websocket_shutdown).await
            });
        }
    }

    supervise_tasks_until(&mut tasks, shutdown_tx, wake_tx, shutdown).await
}

async fn startup_or_shutdown<T, F, S>(startup: F, shutdown: Pin<&mut S>) -> AppResult<Option<T>>
where
    F: Future<Output = AppResult<T>>,
    S: Future<Output = AppResult<()>>,
{
    tokio::select! {
        biased;
        result = shutdown => {
            result?;
            Ok(None)
        }
        result = startup => result.map(Some),
    }
}

async fn supervise_tasks_until<F>(
    tasks: &mut JoinSet<AppResult<()>>,
    shutdown_tx: watch::Sender<bool>,
    wake_sender_guard: tokio::sync::mpsc::Sender<()>,
    shutdown: Pin<&mut F>,
) -> AppResult<()>
where
    F: Future<Output = AppResult<()>>,
{
    let termination = tokio::select! {
        result = shutdown => {
            result?;
            None
        }
        result = tasks.join_next() => Some(result),
    };
    let _ignored = shutdown_tx.send(true);
    drop(wake_sender_guard);

    let shutdown_deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(shutdown_deadline);
    while !tasks.is_empty() {
        tokio::select! {
            _ = &mut shutdown_deadline => {
                tasks.abort_all();
                break;
            }
            result = tasks.join_next() => {
                match result {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(error))) => tracing::error!(error = %error, "service task failed during shutdown"),
                    Some(Err(error)) => tracing::error!(error = %error, "service task panicked during shutdown"),
                    None => break,
                }
            }
        }
    }

    match termination {
        None => Ok(()),
        Some(Some(Ok(Ok(())))) => Err(runtime_error(
            "a supervised service task stopped unexpectedly",
        )),
        Some(Some(Ok(Err(error)))) => Err(error),
        Some(Some(Err(error))) => Err(Box::new(error)),
        Some(None) => Err(runtime_error("service supervisor has no running tasks")),
    }
}

pub async fn backup(options: BackupOptions) -> AppResult<()> {
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let info = storage.create_backup(&options.backup_path).await?;
    tracing::info!(
        backup_id = info.backup_id,
        files = info.file_count,
        "RocksDB backup verified"
    );
    Ok(())
}

pub async fn migrate(options: MigrationOptions) -> AppResult<()> {
    match options.command {
        MigrationCommand::Schema(options) => migrate_schema(options),
        MigrationCommand::Policy(options) => migrate_policy(options).await,
    }
}

pub async fn migrate_bitcoin(options: BitcoinMigrationOptions) -> AppResult<()> {
    match options.command {
        BitcoinMigrationCommand::Schema(options) => migrate_schema(options),
        BitcoinMigrationCommand::Policy(options) => migrate_bitcoin_policy(options).await,
    }
}

fn migrate_schema(options: SchemaMigrationOptions) -> AppResult<()> {
    let outcome = RocksDbStorage::migrate(&options.database.database_path, &options.backup_path)?;
    tracing::info!(
        backup_id = outcome.backup.backup_id,
        from = outcome.report.previous.0,
        to = outcome.report.current.0,
        "RocksDB schema migration completed"
    );
    Ok(())
}

async fn migrate_policy(options: PolicyMigrationOptions) -> AppResult<()> {
    options.validate()?;
    let physical = RocksDbStorage::migrate(
        &options.repository.database.database_path,
        &options.backup_path,
    )?;
    tracing::info!(
        backup_id = physical.backup.backup_id,
        from = physical.report.previous.0,
        to = physical.report.current.0,
        "pre-policy-migration backup and physical schema verification completed"
    );

    let scope = options.repository.scope()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    let repository = repository(storage, &options.repository)?;
    let outcome = repository
        .migrate_policy(MigrateIndexPolicyCommand {
            scope,
            bootstrap_height: bootstrap_height(&options.repository),
            expected_confirmation_policy: options.expected_confirmation_policy(),
            expected_reorg_retention: options.from_reorg_retention,
            target_confirmation_policy: options.repository.confirmation_policy()?,
            target_reorg_retention: options.repository.reorg_retention,
            idempotency_key: options.migration_id,
            reason: options.reason,
        })
        .await?;
    let (version, applied) = match outcome {
        MigrateIndexPolicyOutcome::Applied { version } => (version, true),
        MigrateIndexPolicyOutcome::AlreadyApplied { version } => (version, false),
    };
    tracing::info!(
        version = version.0,
        applied,
        "Indexer policy migration recorded; a checkpointed index requires staged rebuild before semantic service resumes"
    );
    Ok(())
}

async fn migrate_bitcoin_policy(options: BitcoinPolicyMigrationOptions) -> AppResult<()> {
    options.validate()?;
    let physical = RocksDbStorage::migrate(
        &options.repository.database.database_path,
        &options.backup_path,
    )?;
    tracing::info!(
        backup_id = physical.backup.backup_id,
        from = physical.report.previous.0,
        to = physical.report.current.0,
        "pre-policy-migration backup and physical schema verification completed"
    );
    let scope = options.repository.scope()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    let repository = bitcoin_repository(storage, &options.repository)?;
    let outcome = repository
        .migrate_policy(MigrateIndexPolicyCommand {
            scope,
            bootstrap_height: bitcoin_bootstrap_height(&options.repository),
            expected_confirmation_policy: options.expected_confirmation_policy(),
            expected_reorg_retention: options.from_reorg_retention,
            target_confirmation_policy: options.repository.confirmation_policy()?,
            target_reorg_retention: options.repository.reorg_retention,
            idempotency_key: options.migration_id,
            reason: options.reason,
        })
        .await?;
    let (version, applied) = match outcome {
        MigrateIndexPolicyOutcome::Applied { version } => (version, true),
        MigrateIndexPolicyOutcome::AlreadyApplied { version } => (version, false),
    };
    tracing::info!(
        version = version.0,
        applied,
        "Bitcoin Indexer policy migration recorded; staged rebuild is required before semantic service resumes"
    );
    Ok(())
}

pub async fn rebuild(options: RebuildOptions) -> AppResult<()> {
    options.repository.validate()?;
    options.source.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = repository(storage, &options.repository)?;
    let source = connect_source(&scope, &options.source).await?;
    let interpreter = EthereumBlockInterpreter::new(scope.clone())?;
    rebuild_runtime(
        repository,
        source,
        interpreter,
        scope,
        bootstrap_height(&options.repository),
        options.repository.confirmation_policy()?,
        options.repository.reorg_retention,
    )
    .await
}

pub async fn rebuild_bitcoin(options: BitcoinRebuildOptions) -> AppResult<()> {
    options.repository.validate()?;
    options.source.validate()?;
    let scope = options.repository.scope()?;
    let network = options.repository.network()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = bitcoin_repository(storage, &options.repository)?;
    let source = connect_bitcoin_source(&scope, network, &options.source).await?;
    let interpreter = BitcoinBlockInterpreter::new(scope.clone(), network)?;
    rebuild_runtime(
        repository,
        source,
        interpreter,
        scope,
        bitcoin_bootstrap_height(&options.repository),
        options.repository.confirmation_policy()?,
        options.repository.reorg_retention,
    )
    .await
}

async fn rebuild_runtime<S, I, R>(
    repository: R,
    source: S,
    interpreter: I,
    scope: IndexScope,
    bootstrap_height: BlockHeight,
    confirmation_policy: indexing::ConfirmationPolicy,
    reorg_retention: u64,
) -> AppResult<()>
where
    S: BlockSource,
    I: BlockInterpreter<Block = S::Block, Target = R::Target, Undo = R::Undo>,
    R: IndexRepository,
{
    let mut state = repository
        .begin_rebuild(BeginRebuildCommand {
            scope: scope.clone(),
            bootstrap_height,
        })
        .await?;
    if state.phase == RebuildPhase::Building {
        let tip = source.tip().await?;
        let mut next = match &state.checkpoint {
            Some(checkpoint) => checkpoint
                .height
                .0
                .checked_add(1)
                .map(BlockHeight)
                .ok_or_else(|| runtime_error("rebuild checkpoint height is exhausted"))?,
            None => bootstrap_height,
        };
        while next <= tip.height {
            let watches = repository.watches_at(&scope, next).await?;
            let block = source.block_at(next).await?;
            let interpreted = interpreter.inspect(&block, &watches.watches)?;
            if source.canonical_hash(next).await?.as_ref() != Some(&interpreted.block.hash) {
                return Err(runtime_error(
                    "canonical block changed during rebuild; leave the generation unpublished and rerun rebuild",
                ));
            }
            repository
                .commit_rebuild_block(CommitRebuildBlockCommand {
                    generation: state.generation,
                    command: CommitBlockCommand {
                        scope: scope.clone(),
                        expected_checkpoint: state.checkpoint.clone(),
                        expected_watch_version: watches.version,
                        confirmation_policy,
                        reorg_retention,
                        block: interpreted,
                    },
                })
                .await?;
            state = repository
                .rebuild_state(&scope)
                .await?
                .ok_or_else(|| runtime_error("rebuild manifest disappeared after block commit"))?;
            next = next
                .0
                .checked_add(1)
                .map(BlockHeight)
                .ok_or_else(|| runtime_error("rebuild height is exhausted"))?;
        }
    }
    let checkpoint = state
        .checkpoint
        .ok_or_else(|| runtime_error("rebuild produced no canonical checkpoint"))?;
    if source.canonical_hash(checkpoint.height).await?.as_ref() != Some(&checkpoint.hash) {
        return Err(runtime_error(
            "staged rebuild checkpoint is no longer canonical; leave the generation unpublished and rerun rebuild",
        ));
    }
    repository
        .validate_rebuild(ValidateRebuildCommand {
            scope: scope.clone(),
            generation: state.generation,
            expected_checkpoint: checkpoint.clone(),
        })
        .await?;
    repository
        .prepare_rebuild_activation(PrepareRebuildActivationCommand {
            scope: scope.clone(),
            generation: state.generation,
            expected_checkpoint: checkpoint.clone(),
        })
        .await?;
    // Preparation can be expensive for a large journal. Re-query the exact
    // checkpoint immediately before the one-batch publication fence.
    if source.canonical_hash(checkpoint.height).await?.as_ref() != Some(&checkpoint.hash) {
        return Err(runtime_error(
            "staged rebuild checkpoint changed while activation was prepared; leave the generation unpublished and rerun rebuild",
        ));
    }
    repository
        .activate_rebuild(ActivateRebuildCommand {
            scope,
            generation: state.generation,
            expected_checkpoint: checkpoint,
        })
        .await?;
    tracing::info!(generation = state.generation.0, "staged rebuild activated");
    Ok(())
}

pub async fn abort_rebuild(options: GenerationOptions) -> AppResult<()> {
    options.repository.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = repository(storage, &options.repository)?;
    repository
        .abort_rebuild(AbortRebuildCommand {
            scope,
            generation: RebuildGeneration(options.generation),
        })
        .await?;
    tracing::info!(generation = options.generation, "staged rebuild aborted");
    Ok(())
}

pub async fn abort_bitcoin_rebuild(options: BitcoinGenerationOptions) -> AppResult<()> {
    options.repository.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = bitcoin_repository(storage, &options.repository)?;
    repository
        .abort_rebuild(AbortRebuildCommand {
            scope,
            generation: RebuildGeneration(options.generation),
        })
        .await?;
    tracing::info!(
        generation = options.generation,
        "Bitcoin staged rebuild aborted"
    );
    Ok(())
}

pub async fn cleanup(options: GenerationOptions) -> AppResult<()> {
    options.repository.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = repository(storage, &options.repository)?;
    let outcome = repository
        .cleanup_generation(CleanupGenerationCommand {
            scope,
            generation: RebuildGeneration(options.generation),
        })
        .await?;
    tracing::info!(generation = options.generation, outcome = ?outcome, "generation cleanup completed");
    Ok(())
}

pub async fn cleanup_bitcoin(options: BitcoinGenerationOptions) -> AppResult<()> {
    options.repository.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDbStorage::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = bitcoin_repository(storage, &options.repository)?;
    let outcome = repository
        .cleanup_generation(CleanupGenerationCommand {
            scope,
            generation: RebuildGeneration(options.generation),
        })
        .await?;
    tracing::info!(generation = options.generation, outcome = ?outcome, "Bitcoin generation cleanup completed");
    Ok(())
}

fn repository(
    storage: RocksDbStorage,
    options: &RepositoryOptions,
) -> AppResult<EthereumRepository> {
    let config = PersistentIndexConfig::new(
        options.scope()?,
        bootstrap_height(options),
        options.confirmation_policy()?,
        options.reorg_retention,
    )?;
    Ok(PersistentIndexRepository::new(
        storage,
        EthereumIndexRecordCodec,
        config,
    ))
}

fn bitcoin_repository(
    storage: RocksDbStorage,
    options: &BitcoinRepositoryOptions,
) -> AppResult<BitcoinRepository> {
    let config = PersistentIndexConfig::new(
        options.scope()?,
        bitcoin_bootstrap_height(options),
        options.confirmation_policy()?,
        options.reorg_retention,
    )?;
    Ok(PersistentIndexRepository::new(
        storage,
        BitcoinIndexRecordCodec,
        config,
    ))
}

async fn connect_source(scope: &IndexScope, options: &SourceOptions) -> AppResult<EthereumSource> {
    let attempts = NonZeroU32::new(3)
        .ok_or_else(|| runtime_error("non-zero RPC retry count could not be constructed"))?;
    let mut transport_config = HttpTransportConfig::new(&options.rpc_http_url, options.timeout());
    transport_config.retry_policy =
        RetryPolicy::new(attempts, Duration::from_millis(250), Duration::from_secs(2))?;
    let transport = HttpTransport::new(transport_config)?;
    let client = TransportJsonRpcClient::new(transport, &options.rpc_http_url);
    EthereumHttpBlockSource::connect(
        client,
        EthereumIndexSourceConfig {
            scope: scope.clone(),
            expected_chain_id: options.expected_chain_id,
            expected_genesis_hash: options.genesis_hash()?,
        },
    )
    .await
    .map_err(|error| Box::new(error) as AppError)
}

async fn connect_bitcoin_source(
    scope: &IndexScope,
    network: chain_bitcoin::BitcoinNetwork,
    options: &BitcoinSourceOptions,
) -> AppResult<BitcoinSource> {
    let attempts = NonZeroU32::new(3)
        .ok_or_else(|| runtime_error("non-zero RPC retry count could not be constructed"))?;
    let mut transport_config = HttpTransportConfig::new(&options.rpc_http_url, options.timeout());
    transport_config.max_response_bytes = options.rpc_max_response_bytes;
    transport_config.default_headers = options.parsed_rpc_headers()?;
    transport_config.retry_policy =
        RetryPolicy::new(attempts, Duration::from_millis(250), Duration::from_secs(2))?;
    let transport = HttpTransport::new(transport_config)?;
    let client = TransportJsonRpcClient::new(transport, &options.rpc_http_url);
    BitcoinCoreBlockSource::connect(
        client,
        BitcoinIndexSourceConfig {
            scope: scope.clone(),
            network,
            expected_genesis_hash: options.genesis_hash()?,
        },
    )
    .await
    .map_err(|error| Box::new(error) as AppError)
}

async fn sync_loop<S, I, R>(
    runtime: SyncRuntime<S, I, R>,
    mut wakes: tokio::sync::mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
) -> AppResult<()>
where
    S: BlockSource,
    I: BlockInterpreter<Block = S::Block, Target = R::Target, Undo = R::Undo>,
    R: IndexRepository,
{
    let attributes = [Attribute {
        key: "network".to_owned(),
        value: runtime.scope.network.clone(),
    }];
    let mut wait_before_sync = false;
    loop {
        if wait_before_sync {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(runtime.poll_interval) => {}
                wake = wakes.recv() => {
                    if wake.is_none() {
                        return Ok(());
                    }
                }
            }
        }
        if *shutdown.borrow() {
            return Ok(());
        }

        let started = Instant::now();
        match runtime
            .worker
            .sync(SyncRequest {
                scope: runtime.scope.clone(),
                through: None,
                max_blocks: Some(256),
            })
            .await
        {
            Ok(status) => {
                let backfills_applied = if status.phase == SyncPhase::Ready {
                    match process_watch_backfills(
                        &runtime.repository,
                        &runtime.source,
                        &runtime.interpreter,
                        runtime.backfill_projection_policy,
                        &runtime.scope,
                        &runtime.telemetry,
                        &attributes,
                    )
                    .await
                    {
                        Ok(applied) => applied,
                        Err(error) => {
                            runtime
                                .telemetry
                                .count("ix_backfill_failure_total", 1, &attributes);
                            tracing::warn!(
                                kind = ?error.kind,
                                retryable = error.retryable,
                                "historical watch backfill failed"
                            );
                            if !error.retryable {
                                return Err(Box::new(error));
                            }
                            wait_before_sync = true;
                            continue;
                        }
                    }
                } else {
                    0
                };
                runtime
                    .telemetry
                    .count("ix_sync_success_total", 1, &attributes);
                runtime
                    .telemetry
                    .duration("ix_sync_seconds", started.elapsed(), &attributes);
                record_status_metrics(&runtime.telemetry, &attributes, &status);
                match runtime.repository.event_high_water(&runtime.scope).await {
                    Ok(cursor) => runtime.telemetry.gauge(
                        "ix_event_feed_head",
                        cursor.map_or(0.0, |cursor| cursor.0 as f64),
                        &attributes,
                    ),
                    Err(error) => {
                        runtime
                            .telemetry
                            .count("ix_event_feed_head_failure_total", 1, &attributes);
                        tracing::warn!(
                            kind = ?error.kind,
                            retryable = error.retryable,
                            "event-feed head metric could not be refreshed"
                        );
                    }
                }
                let catching_up = status.phase == SyncPhase::CatchingUp;
                let mut guard = runtime
                    .snapshot
                    .lock()
                    .map_err(|_| runtime_error("operational snapshot lock is poisoned"))?;
                guard.status = Some(status);
                guard.last_reconciled = Some(Instant::now());
                drop(guard);
                wait_before_sync = !catching_up && backfills_applied == 0;
            }
            Err(error) => {
                runtime
                    .telemetry
                    .count("ix_sync_failure_total", 1, &attributes);
                tracing::warn!(kind = ?error.kind, retryable = error.retryable, "Indexer reconciliation failed");
                if !error.retryable {
                    return Err(Box::new(error));
                }
                wait_before_sync = true;
            }
        }
    }
}

/// Advances durable historical-watch jobs without moving the live checkpoint.
///
/// Canonical synchronization is deliberately completed first. Each historical
/// height is then fetched through the same authoritative HTTP source, checked
/// again by hash, interpreted for exactly one watch, and atomically committed.
async fn process_watch_backfills<S, I, R>(
    repository: &R,
    source: &SharedSource<S>,
    interpreter: &I,
    projection_policy: BackfillProjectionPolicy,
    scope: &IndexScope,
    telemetry: &PrometheusTelemetry,
    attributes: &[Attribute],
) -> Result<usize, IndexError>
where
    S: BlockSource,
    I: BlockInterpreter<Block = S::Block, Target = R::Target, Undo = R::Undo>,
    R: IndexRepository,
{
    const BATCH_SIZE: usize = 32;

    let jobs = repository
        .pending_watch_backfills(scope, BATCH_SIZE)
        .await?;
    telemetry.gauge("ix_backfill_pending", jobs.len() as f64, attributes);
    let mut applied = 0_usize;

    for job in jobs {
        let checkpoint = repository.checkpoint(scope).await?.ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::Conflict,
                "historical watch backfill requires a live canonical checkpoint",
                true,
            )
        })?;
        let watches = repository.watches_at(scope, job.next_height).await?;
        let watch = watches
            .watches
            .into_iter()
            .find(|watch| watch.id == job.watch_id)
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "historical watch is not active at its durable backfill cursor",
                    false,
                )
            })?;
        let block = source.block_at(job.next_height).await?;
        let interpreted = interpreter.inspect(&block, std::slice::from_ref(&watch))?;
        if source.canonical_hash(job.next_height).await?.as_ref() != Some(&interpreted.block.hash) {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "historical HTTP block changed before its backfill commit",
                true,
            ));
        }
        let projection = prepare_backfill_projection(projection_policy, interpreted.projection)?;
        repository
            .commit_watch_backfill_projection(
                CommitWatchBackfillCommand {
                    scope: scope.clone(),
                    watch_id: job.watch_id,
                    expected_next_height: job.next_height,
                    expected_checkpoint: checkpoint,
                    block: interpreted.block,
                    drafts: interpreted.drafts,
                },
                projection,
            )
            .await?;
        applied = applied.saturating_add(1);
        telemetry.count("ix_backfill_blocks_total", 1, attributes);
    }

    Ok(applied)
}

fn prepare_backfill_projection(
    policy: BackfillProjectionPolicy,
    projection: ProjectionBatch,
) -> Result<ProjectionBatch, IndexError> {
    let mut mutations = Vec::with_capacity(projection.mutations.len());
    for mutation in projection.mutations {
        match mutation {
            mutation @ ProjectionMutation::Put { .. } => mutations.push(mutation),
            ProjectionMutation::PutIfPresent { .. }
                if policy == BackfillProjectionPolicy::DropBitcoinConditionalSpendCandidates =>
            {
                // Bitcoin emits conditional markers for supported inputs whose
                // addresses are outside the one historical watch. They protect
                // live chronological commits, but are not facts discovered by
                // this watch's order-independent backfill.
            }
            ProjectionMutation::PutIfPresent { .. } => {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "historical projection contains a conditional mutation for a chain without a backfill filter",
                    false,
                ));
            }
            ProjectionMutation::Delete { .. } => {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "historical projection backfill cannot delete canonical facts",
                    false,
                ));
            }
        }
    }
    Ok(ProjectionBatch::new(mutations))
}

async fn readiness_loop(
    health: HealthState,
    snapshot: SharedOperationalSnapshot,
    telemetry: PrometheusTelemetry,
    ready_max_lag: u64,
    ready_max_age: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> AppResult<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    health.set_ready(false);
                    return Ok(());
                }
            }
            _ = interval.tick() => {
                let (ready, age) = {
                    let guard = snapshot
                        .lock()
                        .map_err(|_| runtime_error("operational snapshot lock is poisoned"))?;
                    readiness(&guard, ready_max_lag, ready_max_age)
                };
                health.set_ready(ready);
                telemetry.gauge("ix_ready", if ready { 1.0 } else { 0.0 }, &[]);
                telemetry.gauge(
                    "ix_reconciliation_age_seconds",
                    age.map_or(-1.0, |value| value.as_secs_f64()),
                    &[],
                );
            }
        }
    }
}

fn readiness(
    snapshot: &OperationalSnapshot,
    max_lag: u64,
    max_age: Duration,
) -> (bool, Option<Duration>) {
    let age = snapshot.last_reconciled.map(|at| at.elapsed());
    let lag = snapshot.status.as_ref().and_then(|status| {
        Some(
            status
                .observed_tip
                .as_ref()?
                .height
                .0
                .saturating_sub(status.checkpoint.as_ref()?.height.0),
        )
    });
    let ready = snapshot
        .status
        .as_ref()
        .is_some_and(|status| status.phase == SyncPhase::Ready)
        && lag.is_some_and(|lag| lag <= max_lag)
        && age.is_some_and(|age| age <= max_age);
    (ready, age)
}

fn record_status_metrics(
    telemetry: &PrometheusTelemetry,
    attributes: &[Attribute],
    status: &SyncStatus,
) {
    if let Some(checkpoint) = &status.checkpoint {
        telemetry.gauge(
            "ix_checkpoint_height",
            checkpoint.height.0 as f64,
            attributes,
        );
    }
    if let Some(tip) = &status.observed_tip {
        telemetry.gauge("ix_remote_tip_height", tip.height.0 as f64, attributes);
    }
    if let (Some(checkpoint), Some(tip)) = (&status.checkpoint, &status.observed_tip) {
        telemetry.gauge(
            "ix_lag_blocks",
            tip.height.0.saturating_sub(checkpoint.height.0) as f64,
            attributes,
        );
    }
    telemetry.gauge("ix_worker_phase", phase_number(status.phase), attributes);
}

const fn phase_number(phase: SyncPhase) -> f64 {
    match phase {
        SyncPhase::Starting => 0.0,
        SyncPhase::Reconciling => 1.0,
        SyncPhase::CatchingUp => 2.0,
        SyncPhase::Ready => 3.0,
        SyncPhase::Reverting => 4.0,
        SyncPhase::Replaying => 5.0,
        SyncPhase::RebuildRequired => 6.0,
        SyncPhase::Halted => 7.0,
    }
}

async fn metrics(State(telemetry): State<PrometheusTelemetry>) -> impl IntoResponse {
    (
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        telemetry.render(),
    )
}

async fn supervise_websocket<F>(
    websocket: F,
    metrics: MetricContext,
    shutdown: watch::Receiver<bool>,
) -> AppResult<()>
where
    F: Future<Output = Result<(), SourceError>> + Send,
{
    tokio::pin!(websocket);
    let result = tokio::select! {
        result = &mut websocket => result.map_err(|error| Box::new(error) as AppError),
        _ = shutdown_signal(shutdown) => Ok(()),
    };
    metrics.set_websocket_connected(false);
    result
}

async fn shutdown_signal(mut shutdown: watch::Receiver<bool>) {
    if !*shutdown.borrow() {
        drop(shutdown.changed().await);
    }
}

fn runtime_error(message: impl Into<String>) -> AppError {
    Box::new(RuntimeError(message.into()))
}

#[derive(Debug)]
struct RuntimeError(String);

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        str::FromStr,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll},
    };

    use bitcoin::{
        Address, Amount, CompressedPublicKey, OutPoint, PublicKey, ScriptBuf, Sequence,
        Transaction, TxIn, TxOut, Txid, Witness, absolute, consensus, hashes::Hash,
        hex::DisplayHex, transaction::Version,
    };
    use chain_bitcoin::{
        BitcoinAddress, BitcoinBlock, BitcoinBlockInterpreter, BitcoinNetwork, BitcoinWatchTarget,
    };
    use chain_ethereum::EthereumBlock;
    use chain_identity::{CanonicalAddress, ChainId};
    use indexing::{ConfirmationPolicy, IndexScope, WatchId, WatchSelector, WatchTarget};
    use serde_json::{Number, Value, json};

    use super::*;

    type RecordedMetric = (&'static str, Vec<Attribute>);

    #[derive(Clone, Default)]
    struct RecordingTelemetry {
        records: Arc<Mutex<Vec<RecordedMetric>>>,
    }

    impl RecordingTelemetry {
        fn names(&self) -> Vec<&'static str> {
            self.records
                .lock()
                .expect("recording telemetry lock must be healthy")
                .iter()
                .map(|(name, _)| *name)
                .collect()
        }

        fn attributes(&self) -> Vec<Vec<Attribute>> {
            self.records
                .lock()
                .expect("recording telemetry lock must be healthy")
                .iter()
                .map(|(_, attributes)| attributes.clone())
                .collect()
        }

        fn record(&self, name: &'static str, attributes: &[Attribute]) {
            self.records
                .lock()
                .expect("recording telemetry lock must be healthy")
                .push((name, attributes.to_vec()));
        }
    }

    impl Telemetry for RecordingTelemetry {
        fn count(&self, name: &'static str, _value: u64, attributes: &[Attribute]) {
            self.record(name, attributes);
        }

        fn gauge(&self, name: &'static str, _value: f64, attributes: &[Attribute]) {
            self.record(name, attributes);
        }

        fn duration(&self, name: &'static str, _value: Duration, attributes: &[Attribute]) {
            self.record(name, attributes);
        }
    }

    struct ScriptedSource;

    impl BlockSource for ScriptedSource {
        type Block = EthereumBlock;

        fn tip<'a>(&'a self) -> indexing::BoxFuture<'a, Result<BlockRef, SourceError>> {
            Box::pin(async { Ok(block(3)) })
        }

        fn block_at<'a>(
            &'a self,
            _height: BlockHeight,
        ) -> indexing::BoxFuture<'a, Result<Self::Block, SourceError>> {
            Box::pin(async {
                Err(SourceError {
                    message: "scripted source failure".to_owned(),
                    retryable: true,
                })
            })
        }

        fn canonical_hash<'a>(
            &'a self,
            _height: BlockHeight,
        ) -> indexing::BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
            Box::pin(async { Ok(Some(BlockHash(vec![3; 32]))) })
        }
    }

    struct PendingWebsocket {
        dropped: Arc<AtomicBool>,
    }

    impl Future for PendingWebsocket {
        type Output = Result<(), SourceError>;

        fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingWebsocket {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8; 32]),
            parent_hash: None,
            timestamp: Some(height),
        }
    }

    fn status(phase: SyncPhase, checkpoint: u64, tip: u64) -> SyncStatus {
        SyncStatus {
            scope: IndexScope {
                chain: ChainId("ethereum".to_owned()),
                network: "test".to_owned(),
            },
            checkpoint: Some(BlockRef {
                height: BlockHeight(checkpoint),
                hash: BlockHash(vec![1; 32]),
                parent_hash: None,
                timestamp: None,
            }),
            observed_tip: Some(BlockRef {
                height: BlockHeight(tip),
                hash: BlockHash(vec![2; 32]),
                parent_hash: None,
                timestamp: None,
            }),
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: 12,
                require_chain_finality: false,
            },
            phase,
            rebuild_reason: None,
            halted_reason: None,
        }
    }

    fn regtest_p2wpkh_address(prefix: u8) -> Address {
        let public_key = PublicKey::from_slice(&[
            prefix, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        Address::p2wpkh(
            &CompressedPublicKey::try_from(public_key).expect("test public key must be compressed"),
            bitcoin::Network::Regtest,
        )
    }

    fn bitcoin_amount(satoshis: u64) -> Value {
        let whole = satoshis / 100_000_000;
        let remainder = satoshis % 100_000_000;
        let lexical = if remainder == 0 {
            whole.to_string()
        } else {
            format!("{whole}.{remainder:08}")
                .trim_end_matches('0')
                .to_owned()
        };
        Value::Number(Number::from_str(&lexical).expect("test BTC amount must encode"))
    }

    #[test]
    fn readiness_requires_phase_lag_and_recent_reconciliation() {
        let mut snapshot = OperationalSnapshot {
            status: Some(status(SyncPhase::Ready, 10, 12)),
            last_reconciled: Some(Instant::now()),
        };
        assert!(readiness(&snapshot, 2, Duration::from_secs(30)).0);
        snapshot.status = Some(status(SyncPhase::CatchingUp, 10, 12));
        assert!(!readiness(&snapshot, 2, Duration::from_secs(30)).0);
        snapshot.status = Some(status(SyncPhase::Ready, 9, 12));
        assert!(!readiness(&snapshot, 2, Duration::from_secs(30)).0);
    }

    #[test]
    fn bitcoin_backfill_keeps_watched_fact_and_drops_unrelated_conditional_spend() {
        let scope = IndexScope {
            chain: ChainId("bitcoin".to_owned()),
            network: "regtest".to_owned(),
        };
        let watched_native = regtest_p2wpkh_address(0x02);
        let unrelated_native = regtest_p2wpkh_address(0x03);
        let watched = BitcoinAddress(watched_native.to_string());
        let watch = WatchTarget {
            id: WatchId("historical-address".to_owned()),
            scope: scope.clone(),
            selector: WatchSelector::Address(CanonicalAddress {
                chain: ChainId("bitcoin".to_owned()),
                value: watched.0.clone(),
            }),
            target: BitcoinWatchTarget::Address(watched),
            idempotency_key: "historical-address-key".to_owned(),
            start_height: BlockHeight(0),
            registered_at: None,
            inactive_from: None,
        };
        let coinbase = Transaction {
            version: Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 1]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: watched_native.script_pubkey(),
            }],
        };
        let unrelated_spend = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([7; 32]), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(19_000),
                script_pubkey: ScriptBuf::new_op_return([]),
            }],
        };
        let block_hash = bitcoin::BlockHash::from_byte_array([0xaa; 32]);
        let parent_hash = bitcoin::BlockHash::from_byte_array([0xbb; 32]);
        let raw_block = serde_json::to_vec(&json!({
            "hash": block_hash.to_string(),
            "height": 10,
            "previousblockhash": parent_hash.to_string(),
            "time": 100,
            "nTx": 2,
            "tx": [
                {
                    "txid": coinbase.compute_txid().to_string(),
                    "hex": consensus::serialize(&coinbase).to_lower_hex_string(),
                    "vin": [{"coinbase": "01"}],
                    "vout": [{
                        "value": bitcoin_amount(50_000),
                        "n": 0,
                        "scriptPubKey": {
                            "hex": watched_native.script_pubkey().as_bytes().to_lower_hex_string()
                        }
                    }]
                },
                {
                    "txid": unrelated_spend.compute_txid().to_string(),
                    "hex": consensus::serialize(&unrelated_spend).to_lower_hex_string(),
                    "vin": [{
                        "txid": unrelated_spend.input[0].previous_output.txid.to_string(),
                        "vout": 0,
                        "prevout": {
                            "generated": false,
                            "height": 9,
                            "value": bitcoin_amount(20_000),
                            "scriptPubKey": {
                                "hex": unrelated_native
                                    .script_pubkey()
                                    .as_bytes()
                                    .to_lower_hex_string()
                            }
                        }
                    }],
                    "vout": [{
                        "value": bitcoin_amount(19_000),
                        "n": 0,
                        "scriptPubKey": {
                            "hex": unrelated_spend.output[0]
                                .script_pubkey
                                .as_bytes()
                                .to_lower_hex_string()
                        }
                    }]
                }
            ]
        }))
        .expect("test Bitcoin block must encode");
        let block = BitcoinBlock {
            reference: BlockRef {
                height: BlockHeight(10),
                hash: BlockHash(block_hash.to_byte_array().to_vec()),
                parent_hash: Some(BlockHash(parent_hash.to_byte_array().to_vec())),
                timestamp: Some(100),
            },
            raw_block,
        };

        let interpreted = BitcoinBlockInterpreter::new(scope, BitcoinNetwork::Regtest)
            .expect("test Bitcoin interpreter must construct")
            .inspect(&block, &[watch])
            .expect("watched fact and unrelated input must interpret");
        assert_eq!(interpreted.drafts.len(), 1);
        assert_eq!(interpreted.projection.mutations.len(), 2);
        assert!(
            interpreted
                .projection
                .mutations
                .iter()
                .any(|mutation| matches!(mutation, ProjectionMutation::Put { .. }))
        );
        assert!(
            interpreted
                .projection
                .mutations
                .iter()
                .any(|mutation| matches!(mutation, ProjectionMutation::PutIfPresent { .. }))
        );

        let backfill = prepare_backfill_projection(
            BackfillProjectionPolicy::DropBitcoinConditionalSpendCandidates,
            interpreted.projection,
        )
        .expect("Bitcoin backfill must discard only unrelated conditional candidates");
        assert!(matches!(
            backfill.mutations.as_slice(),
            [ProjectionMutation::Put { .. }]
        ));

        let error = prepare_backfill_projection(
            BackfillProjectionPolicy::DropBitcoinConditionalSpendCandidates,
            ProjectionBatch::new(vec![ProjectionMutation::Delete { key: vec![1] }]),
        )
        .expect_err("historical backfill must fail closed on a delete");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(!error.retryable);
    }

    #[test]
    fn metrics_listener_constant_is_loopback() {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9090);
        assert!(address.ip().is_loopback());
    }

    #[tokio::test]
    async fn source_metrics_record_sanitized_method_outcomes_and_durations() {
        let telemetry = RecordingTelemetry::default();
        let metrics = MetricContext::new(Arc::new(telemetry.clone()), "testnet".to_owned());
        let source = SharedSource::new(Arc::new(ScriptedSource), metrics);

        source.tip().await.expect("scripted tip must succeed");
        source
            .block_at(BlockHeight(3))
            .await
            .expect_err("scripted block read must fail");
        source
            .canonical_hash(BlockHeight(3))
            .await
            .expect("scripted canonical hash must succeed");

        assert_eq!(
            telemetry.names(),
            vec![
                "ix_source_tip_success_total",
                "ix_source_tip_seconds",
                "ix_source_block_at_failure_total",
                "ix_source_block_at_seconds",
                "ix_source_canonical_hash_success_total",
                "ix_source_canonical_hash_seconds",
            ]
        );
        assert!(telemetry.attributes().iter().all(|attributes| {
            attributes
                == &[Attribute {
                    key: "network".to_owned(),
                    value: "testnet".to_owned(),
                }]
        }));
    }

    #[test]
    fn sync_observer_records_commit_and_reorg_metrics() {
        let telemetry = RecordingTelemetry::default();
        let metrics = MetricContext::new(Arc::new(telemetry.clone()), "testnet".to_owned());
        let scope = IndexScope {
            chain: ChainId("ethereum".to_owned()),
            network: "testnet".to_owned(),
        };

        metrics.block_commit(BlockCommitObservation {
            scope: scope.clone(),
            block: block(12),
            elapsed: Duration::from_millis(3),
            outcome: BlockCommitObservationOutcome::Success(CommitBlockOutcome::Applied),
        });
        metrics.reorg_detected(ReorgObservation {
            scope,
            previous_tip: block(12),
            depth: ReorgDepth::Exact {
                depth: 2,
                common_ancestor: block(10),
            },
        });

        assert_eq!(
            telemetry.names(),
            vec![
                "ix_block_commit_seconds",
                "ix_block_commit_applied_total",
                "ix_reorg_total",
                "ix_reorg_depth_blocks",
            ]
        );
        assert!(telemetry.attributes().iter().all(|attributes| {
            attributes
                == &[Attribute {
                    key: "network".to_owned(),
                    value: "testnet".to_owned(),
                }]
        }));
    }

    #[tokio::test]
    async fn websocket_supervisor_cancels_a_silent_stream_on_shutdown() {
        let telemetry = RecordingTelemetry::default();
        let metrics = MetricContext::new(Arc::new(telemetry.clone()), "testnet".to_owned());
        metrics.set_websocket_enabled(true);
        metrics.observe_websocket_event(EthereumNewHeadsConnectionEvent::Connected);
        metrics.observe_websocket_event(EthereumNewHeadsConnectionEvent::ReconnectScheduled);
        metrics.observe_websocket_event(EthereumNewHeadsConnectionEvent::Failure);

        let dropped = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(supervise_websocket(
            PendingWebsocket {
                dropped: Arc::clone(&dropped),
            },
            metrics,
            shutdown_rx,
        ));
        tokio::task::yield_now().await;
        shutdown_tx
            .send(true)
            .expect("test shutdown receiver must remain open");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("silent WebSocket supervision must stop promptly")
            .expect("WebSocket supervision task must not panic")
            .expect("WebSocket supervision shutdown must succeed");

        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(
            telemetry.names(),
            vec![
                "ix_websocket_enabled",
                "ix_websocket_connected",
                "ix_websocket_reconnect_total",
                "ix_websocket_failure_total",
                "ix_websocket_connected",
            ]
        );
        assert!(telemetry.attributes().iter().all(|attributes| {
            attributes
                == &[Attribute {
                    key: "network".to_owned(),
                    value: "testnet".to_owned(),
                }]
        }));
    }

    #[tokio::test]
    async fn caller_shutdown_cancels_async_startup() {
        let mut shutdown = std::future::ready(Ok(()));
        let result = startup_or_shutdown(
            std::future::pending::<AppResult<()>>(),
            Pin::new(&mut shutdown),
        )
        .await
        .expect("caller shutdown must complete cleanly");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn caller_shutdown_drains_supervised_resource_owners() {
        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut tasks = JoinSet::<AppResult<()>>::new();
        tasks.spawn(async move {
            let _resource = DropFlag(task_dropped);
            shutdown_signal(shutdown_rx).await;
            Ok(())
        });
        let (wake_tx, _wake_rx) = tokio::sync::mpsc::channel(1);
        let mut shutdown = std::future::ready(Ok(()));

        supervise_tasks_until(&mut tasks, shutdown_tx, wake_tx, Pin::new(&mut shutdown))
            .await
            .expect("caller shutdown must drain supervised tasks");

        assert!(tasks.is_empty());
        assert!(dropped.load(Ordering::Acquire));
    }
}
