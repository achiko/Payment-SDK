use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::{marker::PhantomData, str::FromStr};

use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use chain_bitcoin::{
    BitcoinAddress, BitcoinIndexRecordCodec, BitcoinIndexedOutput, BitcoinNetwork,
    BitcoinTransactionId, BitcoinWatchTarget, format_bitcoin_block_hash,
};
use chain_ethereum::{EthereumAddress, EthereumTransactionId, EthereumWatchTarget};
use chain_identity::{CanonicalAddress, CanonicalTransactionId};
use indexing::{
    BlockHeight, BoxFuture, EventCursor, IndexError, IndexErrorKind, IndexRepository, IndexScope,
    ObservationEvent, ObservationEventPage, ObservationEventRequest, ObservedTransaction,
    ProjectionCursor, ProjectionGetRequest, ProjectionQuery, ProjectionScanRequest,
    ProjectionSnapshot, RegisterWatchCommand, RegisterWatchOutcome, SyncPhase, SyncStatus,
    TransactionPage, TransactionPageRequest, TransactionRequest, TransactionStatus, UnwatchCommand,
    UnwatchOutcome, WatchReceipt, WatchRequest, WatchSelector,
};
use serde::{Deserialize, Serialize};

pub trait ApiRepository: Send + Sync {
    fn status<'a>(&'a self, scope: &'a IndexScope)
    -> BoxFuture<'a, Result<SyncStatus, IndexError>>;

    fn register<'a>(
        &'a self,
        request: WatchRequest,
        target: ApiWatchTarget,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>>;

    fn unwatch<'a>(
        &'a self,
        command: UnwatchCommand,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>>;

    fn transaction<'a>(
        &'a self,
        request: TransactionRequest,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>>;

    fn transactions_by_address<'a>(
        &'a self,
        request: TransactionPageRequest,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;

    fn events<'a>(
        &'a self,
        request: ObservationEventRequest,
    ) -> BoxFuture<'a, Result<ObservationEventPage, IndexError>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiWatchTarget {
    Ethereum(EthereumWatchTarget),
    Bitcoin(BitcoinWatchTarget),
}

pub trait ApiTarget: Send + Sync + 'static {
    type Target: Clone + Send + Sync + 'static;

    fn convert(target: ApiWatchTarget) -> Result<Self::Target, IndexError>;
}

pub enum EthereumApiTarget {}

impl ApiTarget for EthereumApiTarget {
    type Target = EthereumWatchTarget;

    fn convert(target: ApiWatchTarget) -> Result<Self::Target, IndexError> {
        match target {
            ApiWatchTarget::Ethereum(target) => Ok(target),
            ApiWatchTarget::Bitcoin(_) => Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Bitcoin watch target cannot be stored in an Ethereum index",
                false,
            )),
        }
    }
}

pub enum BitcoinApiTarget {}

impl ApiTarget for BitcoinApiTarget {
    type Target = BitcoinWatchTarget;

    fn convert(target: ApiWatchTarget) -> Result<Self::Target, IndexError> {
        match target {
            ApiWatchTarget::Bitcoin(target) => Ok(target),
            ApiWatchTarget::Ethereum(_) => Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum watch target cannot be stored in a Bitcoin index",
                false,
            )),
        }
    }
}

pub struct ApiRepositoryAdapter<R, T> {
    repository: R,
    target: PhantomData<T>,
}

impl<R, T> ApiRepositoryAdapter<R, T> {
    #[must_use]
    pub const fn new(repository: R) -> Self {
        Self {
            repository,
            target: PhantomData,
        }
    }
}

impl<R, T> ApiRepository for ApiRepositoryAdapter<R, T>
where
    R: IndexRepository<Target = T::Target>,
    T: ApiTarget,
{
    fn status<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
        self.repository.status(scope)
    }

    fn register<'a>(
        &'a self,
        request: WatchRequest,
        target: ApiWatchTarget,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            let registered_at = self.repository.checkpoint(&request.scope).await?;
            let outcome = self
                .repository
                .register_watch(RegisterWatchCommand {
                    request,
                    target: T::convert(target)?,
                    registered_at,
                })
                .await?;
            Ok(match outcome {
                RegisterWatchOutcome::Registered(receipt)
                | RegisterWatchOutcome::Existing(receipt) => receipt,
            })
        })
    }

    fn unwatch<'a>(
        &'a self,
        command: UnwatchCommand,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        self.repository.unwatch(command)
    }

    fn transaction<'a>(
        &'a self,
        request: TransactionRequest,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        self.repository.transaction(request)
    }

    fn transactions_by_address<'a>(
        &'a self,
        request: TransactionPageRequest,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        self.repository.transactions_by_address(request)
    }

    fn events<'a>(
        &'a self,
        request: ObservationEventRequest,
    ) -> BoxFuture<'a, Result<ObservationEventPage, IndexError>> {
        self.repository.events(request)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinUtxoPageRequest {
    pub scope: IndexScope,
    pub address: BitcoinAddress,
    pub after: Option<ProjectionCursor>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinUtxoPage {
    pub snapshot: ProjectionSnapshot,
    pub outputs: Vec<BitcoinIndexedOutput>,
    pub next: Option<ProjectionCursor>,
}

pub trait BitcoinUtxoRepository: Send + Sync {
    fn utxos<'a>(
        &'a self,
        request: BitcoinUtxoPageRequest,
    ) -> BoxFuture<'a, Result<BitcoinUtxoPage, IndexError>>;
}

pub struct BitcoinUtxoRepositoryAdapter<R> {
    repository: R,
}

impl<R> BitcoinUtxoRepositoryAdapter<R> {
    #[must_use]
    pub const fn new(repository: R) -> Self {
        Self { repository }
    }
}

impl<R> BitcoinUtxoRepository for BitcoinUtxoRepositoryAdapter<R>
where
    R: ProjectionQuery + IndexRepository<Target = BitcoinWatchTarget>,
{
    fn utxos<'a>(
        &'a self,
        request: BitcoinUtxoPageRequest,
    ) -> BoxFuture<'a, Result<BitcoinUtxoPage, IndexError>> {
        Box::pin(async move {
            // A historical watch is materialized height-by-height. Serving a
            // UTXO page before every durable backfill finishes could expose a
            // creation before its later historical spend has been applied.
            // Gate the full Bitcoin projection conservatively until no job is
            // pending; imported-address rescans trade availability for safety.
            if !self
                .repository
                .pending_watch_backfills(&request.scope, 1)
                .await?
                .is_empty()
            {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "Bitcoin UTXOs are unavailable while historical watch backfill is pending",
                    true,
                ));
            }
            let prefix = BitcoinIndexRecordCodec::utxo_key_prefix(&request.address)?;
            let page = self
                .repository
                .projection_scan(ProjectionScanRequest {
                    scope: request.scope.clone(),
                    prefix,
                    after: request.after,
                    limit: request.limit,
                })
                .await?;
            let mut outputs = Vec::with_capacity(page.entries.len());
            for entry in page.entries {
                let output = BitcoinIndexRecordCodec::decode_utxo_entry(&entry.key, &entry.value)?;
                let marker_key =
                    BitcoinIndexRecordCodec::spent_marker_key(&output.address, output.outpoint)?;
                let marker = self
                    .repository
                    .projection_get(ProjectionGetRequest {
                        scope: request.scope.clone(),
                        key: marker_key.clone(),
                        expected_snapshot: Some(page.snapshot.clone()),
                    })
                    .await?;
                if marker.snapshot != page.snapshot {
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "active Bitcoin UTXO projection changed during pagination",
                        true,
                    ));
                }
                if let Some(value) = marker.value {
                    BitcoinIndexRecordCodec::decode_spent_marker_entry(&marker_key, &value)?;
                } else {
                    outputs.push(output);
                }
            }
            Ok(BitcoinUtxoPage {
                snapshot: page.snapshot,
                outputs,
                next: page.next,
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApiChain {
    Ethereum,
    Bitcoin(BitcoinNetwork),
}

pub struct ApiState {
    scope: IndexScope,
    chain: ApiChain,
    repository: Arc<dyn ApiRepository>,
    bitcoin_utxos: Option<Arc<dyn BitcoinUtxoRepository>>,
    operational_health: Option<http::HealthState>,
    bootstrap_height: BlockHeight,
    limits: http::RequestLimits,
    request_counter: AtomicU64,
}

impl ApiState {
    #[must_use]
    pub fn new(
        scope: IndexScope,
        repository: Arc<dyn ApiRepository>,
        bootstrap_height: BlockHeight,
        limits: http::RequestLimits,
    ) -> Self {
        Self {
            scope,
            chain: ApiChain::Ethereum,
            repository,
            bitcoin_utxos: None,
            operational_health: None,
            bootstrap_height,
            limits,
            request_counter: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn new_bitcoin(
        scope: IndexScope,
        network: BitcoinNetwork,
        repository: Arc<dyn ApiRepository>,
        bitcoin_utxos: Arc<dyn BitcoinUtxoRepository>,
        operational_health: http::HealthState,
        bootstrap_height: BlockHeight,
        limits: http::RequestLimits,
    ) -> Self {
        Self {
            scope,
            chain: ApiChain::Bitcoin(network),
            repository,
            bitcoin_utxos: Some(bitcoin_utxos),
            operational_health: Some(operational_health),
            bootstrap_height,
            limits,
            request_counter: AtomicU64::new(0),
        }
    }

    fn request_id(&self) -> String {
        let next = self.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("ix-request-{next:020}")
    }

    fn validate_network(&self, network: &str) -> Result<(), ApiError> {
        if network == self.scope.network {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "scope_not_found",
                "requested Indexer scope does not exist",
                false,
                self.request_id(),
            ))
        }
    }

    async fn semantic_status(&self) -> Result<SyncStatus, ApiError> {
        let status = self
            .repository
            .status(&self.scope)
            .await
            .map_err(|error| ApiError::from_index(error, self.request_id()))?;
        if matches!(status.phase, SyncPhase::RebuildRequired | SyncPhase::Halted) {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "semantic_surface_unavailable",
                "semantic operations are unavailable until Indexer recovery completes",
                true,
                self.request_id(),
            ));
        }
        Ok(status)
    }
}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/scopes/ethereum/{network}/status", get(status))
        .route(
            "/v1/scopes/ethereum/{network}/watches",
            post(register_watch),
        )
        .route(
            "/v1/scopes/ethereum/{network}/watches/{watch_id}",
            delete(unwatch),
        )
        .route(
            "/v1/scopes/ethereum/{network}/transactions/{tx_hash}",
            get(transaction),
        )
        .route(
            "/v1/scopes/ethereum/{network}/addresses/{address}/transactions",
            get(transactions_by_address),
        )
        .route("/v1/events", get(events))
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}

pub fn bitcoin_router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/scopes/bitcoin/{network}/status", get(status))
        .route("/v1/scopes/bitcoin/{network}/watches", post(register_watch))
        .route(
            "/v1/scopes/bitcoin/{network}/watches/{watch_id}",
            delete(unwatch),
        )
        .route(
            "/v1/scopes/bitcoin/{network}/transactions/{tx_hash}",
            get(transaction),
        )
        .route(
            "/v1/scopes/bitcoin/{network}/addresses/{address}/transactions",
            get(transactions_by_address),
        )
        .route(
            "/v1/scopes/bitcoin/{network}/addresses/{address}/utxos",
            get(bitcoin_utxos),
        )
        .route("/v1/events", get(events))
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}

async fn route_not_found(State(state): State<Arc<ApiState>>) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "route_not_found",
        "requested Indexer route does not exist",
        false,
        state.request_id(),
    )
}

async fn method_not_allowed(State(state): State<Arc<ApiState>>) -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed for this Indexer route",
        false,
        state.request_id(),
    )
}

async fn status(
    State(state): State<Arc<ApiState>>,
    Path(network): Path<String>,
) -> Result<Json<StatusDto>, ApiError> {
    state.validate_network(&network)?;
    let status = state
        .repository
        .status(&state.scope)
        .await
        .map_err(|error| ApiError::from_index(error, state.request_id()))?;
    Ok(Json(StatusDto::try_from_status(status).map_err(
        |error| ApiError::from_index(error, state.request_id()),
    )?))
}

async fn register_watch(
    State(state): State<Arc<ApiState>>,
    Path(network): Path<String>,
    body: Result<Json<CreateWatchDto>, JsonRejection>,
) -> Result<(StatusCode, Json<WatchDto>), ApiError> {
    state.validate_network(&network)?;
    state.semantic_status().await?;
    let Json(body) = body.map_err(|_| {
        ApiError::bad_request(
            "invalid_json",
            "watch request body is not valid JSON",
            state.request_id(),
        )
    })?;
    let start_height = parse_decimal(&body.start_height, "start_height", &state)?;
    if start_height < state.bootstrap_height.0 {
        return Err(ApiError::bad_request(
            "invalid_start_height",
            "watch start height precedes the configured bootstrap height",
            state.request_id(),
        ));
    }
    if body.idempotency_key.trim().is_empty() {
        return Err(ApiError::bad_request(
            "invalid_idempotency_key",
            "watch idempotency key must not be empty",
            state.request_id(),
        ));
    }
    let (selector, target) = parse_selector(&state, body.selector)?;
    let receipt = state
        .repository
        .register(
            WatchRequest {
                scope: state.scope.clone(),
                selector,
                start_height: BlockHeight(start_height),
                idempotency_key: body.idempotency_key.clone(),
            },
            target,
        )
        .await
        .map_err(|error| ApiError::from_index(error, state.request_id()))?;
    Ok((
        StatusCode::OK,
        Json(
            WatchDto::try_from_receipt(receipt)
                .map_err(|error| ApiError::from_index(error, state.request_id()))?,
        ),
    ))
}

async fn unwatch(
    State(state): State<Arc<ApiState>>,
    Path((network, watch_id)): Path<(String, String)>,
) -> Result<Json<UnwatchDto>, ApiError> {
    state.validate_network(&network)?;
    let status = state.semantic_status().await?;
    let expected_checkpoint = status.checkpoint;
    let inactive_from = match expected_checkpoint.as_ref() {
        Some(checkpoint) => checkpoint.height.0.checked_add(1).ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "height_exhausted",
                "watch cannot be deactivated after the maximum block height",
                false,
                state.request_id(),
            )
        })?,
        None => state.bootstrap_height.0,
    };
    let outcome = state
        .repository
        .unwatch(UnwatchCommand {
            scope: state.scope.clone(),
            watch_id: indexing::WatchId(watch_id),
            inactive_from: BlockHeight(inactive_from),
            expected_checkpoint,
        })
        .await
        .map_err(|error| ApiError::from_index(error, state.request_id()))?;
    Ok(Json(UnwatchDto {
        outcome: match outcome {
            UnwatchOutcome::Deactivated => "deactivated",
            UnwatchOutcome::AlreadyInactive => "already_inactive",
        },
    }))
}

async fn transaction(
    State(state): State<Arc<ApiState>>,
    Path((network, tx_hash)): Path<(String, String)>,
) -> Result<Json<TransactionDto>, ApiError> {
    state.validate_network(&network)?;
    state.semantic_status().await?;
    let (_, transaction_id) = parse_transaction(&state, &tx_hash)?;
    let transaction = state
        .repository
        .transaction(TransactionRequest {
            scope: state.scope.clone(),
            transaction_id,
        })
        .await
        .map_err(|error| ApiError::from_index(error, state.request_id()))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "transaction_not_found",
                "indexed transaction does not exist",
                false,
                state.request_id(),
            )
        })?;
    Ok(Json(
        TransactionDto::try_from_transaction(transaction)
            .map_err(|error| ApiError::from_index(error, state.request_id()))?,
    ))
}

#[derive(Deserialize)]
struct TransactionQuery {
    after: Option<String>,
    limit: Option<usize>,
}

async fn transactions_by_address(
    State(state): State<Arc<ApiState>>,
    Path((network, address)): Path<(String, String)>,
    query: Result<Query<TransactionQuery>, QueryRejection>,
) -> Result<Json<TransactionPageDto>, ApiError> {
    state.validate_network(&network)?;
    state.semantic_status().await?;
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request(
            "invalid_query",
            "transaction page query is invalid",
            state.request_id(),
        )
    })?;
    let (_, address) = parse_address(&state, &address)?;
    let after = query
        .after
        .as_deref()
        .map(|value| parse_transaction(&state, value).map(|(_, canonical)| canonical))
        .transpose()?;
    let limit = state.limits.page_size(query.limit).map_err(|error| {
        ApiError::bad_request("invalid_page_size", error.to_string(), state.request_id())
    })?;
    let page = state
        .repository
        .transactions_by_address(TransactionPageRequest {
            scope: state.scope.clone(),
            address,
            after,
            limit,
        })
        .await
        .map_err(|error| ApiError::from_index(error, state.request_id()))?;
    let transactions = page
        .transactions
        .into_iter()
        .map(TransactionDto::try_from_transaction)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ApiError::from_index(error, state.request_id()))?;
    Ok(Json(TransactionPageDto {
        transactions,
        next: page.next.map(|next| next.value),
    }))
}

#[derive(Deserialize)]
struct BitcoinUtxoQuery {
    after: Option<String>,
    limit: Option<usize>,
}

async fn bitcoin_utxos(
    State(state): State<Arc<ApiState>>,
    Path((network, address)): Path<(String, String)>,
    query: Result<Query<BitcoinUtxoQuery>, QueryRejection>,
) -> Result<Json<BitcoinUtxoPageDto>, ApiError> {
    state.validate_network(&network)?;
    if !state
        .operational_health
        .as_ref()
        .is_some_and(http::HealthState::is_ready)
    {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "bitcoin_utxo_snapshot_unavailable",
            "Bitcoin UTXOs are unavailable until the Indexer is operationally ready",
            true,
            state.request_id(),
        ));
    }
    let status = state.semantic_status().await?;
    if status.phase != SyncPhase::Ready {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "bitcoin_utxo_snapshot_unavailable",
            "Bitcoin UTXOs are available only while the Indexer is ready",
            true,
            state.request_id(),
        ));
    }
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request(
            "invalid_query",
            "UTXO page query is invalid",
            state.request_id(),
        )
    })?;
    let native = match parse_address(&state, &address)?.0 {
        ParsedAddress::Bitcoin(address) => address,
        ParsedAddress::Ethereum(_) => {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Bitcoin UTXO route is attached to a non-Bitcoin scope",
                false,
                state.request_id(),
            ));
        }
    };
    let after = query
        .after
        .as_deref()
        .map(|value| decode_projection_cursor(value, &state))
        .transpose()?;
    let limit = state.limits.page_size(query.limit).map_err(|error| {
        ApiError::bad_request("invalid_page_size", error.to_string(), state.request_id())
    })?;
    let repository = state.bitcoin_utxos.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Bitcoin UTXO projection is not configured",
            false,
            state.request_id(),
        )
    })?;
    let page = repository
        .utxos(BitcoinUtxoPageRequest {
            scope: state.scope.clone(),
            address: native,
            after,
            limit,
        })
        .await
        .map_err(|error| ApiError::from_index(error, state.request_id()))?;
    let snapshot = page.snapshot.clone();
    let checkpoint_height = snapshot.checkpoint.as_ref().map(|block| block.height.0);
    let outputs = page
        .outputs
        .into_iter()
        .map(|output| BitcoinUtxoDto::try_from_indexed(output, checkpoint_height, &state))
        .collect::<Result<Vec<_>, _>>()?;
    let after = state.semantic_status().await?;
    if after.phase != SyncPhase::Ready || after.checkpoint != snapshot.checkpoint {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "bitcoin_utxo_snapshot_changed",
            "Bitcoin canonical state changed during the UTXO read",
            true,
            state.request_id(),
        ));
    }
    let checkpoint = snapshot
        .checkpoint
        .clone()
        .map(|block| BlockDto::try_from_block(block, &state.scope.chain))
        .transpose()
        .map_err(|error| ApiError::from_index(error, state.request_id()))?;
    Ok(Json(BitcoinUtxoPageDto {
        generation: snapshot.generation.0.to_string(),
        revision: snapshot.revision.to_string(),
        checkpoint,
        outputs,
        next: page.next.as_ref().map(encode_projection_cursor),
    }))
}

fn decode_projection_cursor(input: &str, state: &ApiState) -> Result<ProjectionCursor, ApiError> {
    let parts = input.split(':').collect::<Vec<_>>();
    if parts.len() != 7 {
        return Err(ApiError::bad_request(
            "invalid_cursor",
            "UTXO cursor does not contain a complete projection snapshot",
            state.request_id(),
        ));
    }
    let generation = parse_decimal(parts[0], "cursor generation", state)?;
    let revision = parse_decimal(parts[1], "cursor revision", state)?;
    let checkpoint = if parts[2] == "-" {
        if parts[3..6].iter().any(|part| *part != "-") {
            return Err(ApiError::bad_request(
                "invalid_cursor",
                "UTXO cursor has an inconsistent empty checkpoint",
                state.request_id(),
            ));
        }
        None
    } else {
        let height = parse_decimal(parts[2], "cursor checkpoint height", state)?;
        let hash = decode_hex(parts[3]).map_err(|message| {
            ApiError::bad_request("invalid_cursor", message, state.request_id())
        })?;
        if hash.len() != 32 {
            return Err(ApiError::bad_request(
                "invalid_cursor",
                "UTXO cursor checkpoint hash must contain 32 bytes",
                state.request_id(),
            ));
        }
        let parent_hash = if parts[4] == "-" {
            None
        } else {
            let parent = decode_hex(parts[4]).map_err(|message| {
                ApiError::bad_request("invalid_cursor", message, state.request_id())
            })?;
            if parent.len() != 32 {
                return Err(ApiError::bad_request(
                    "invalid_cursor",
                    "UTXO cursor parent hash must contain 32 bytes",
                    state.request_id(),
                ));
            }
            Some(indexing::BlockHash(parent))
        };
        let timestamp = if parts[5] == "-" {
            None
        } else {
            Some(parse_decimal(
                parts[5],
                "cursor checkpoint timestamp",
                state,
            )?)
        };
        Some(indexing::BlockRef {
            height: BlockHeight(height),
            hash: indexing::BlockHash(hash),
            parent_hash,
            timestamp,
        })
    };
    let key = decode_hex(parts[6])
        .map_err(|message| ApiError::bad_request("invalid_cursor", message, state.request_id()))?;
    if key.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_cursor",
            "UTXO cursor key must not be empty",
            state.request_id(),
        ));
    }
    Ok(ProjectionCursor {
        snapshot: ProjectionSnapshot {
            generation: indexing::RebuildGeneration(generation),
            revision,
            checkpoint,
        },
        key,
    })
}

fn encode_projection_cursor(cursor: &ProjectionCursor) -> String {
    let (height, hash, parent, timestamp) = cursor.snapshot.checkpoint.as_ref().map_or_else(
        || {
            (
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
            )
        },
        |checkpoint| {
            (
                checkpoint.height.0.to_string(),
                encode_hex(&checkpoint.hash.0),
                checkpoint
                    .parent_hash
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |hash| encode_hex(&hash.0)),
                checkpoint
                    .timestamp
                    .map_or_else(|| "-".to_owned(), |timestamp| timestamp.to_string()),
            )
        },
    );
    format!(
        "{}:{}:{height}:{hash}:{parent}:{timestamp}:{}",
        cursor.snapshot.generation.0,
        cursor.snapshot.revision,
        encode_hex(&cursor.key)
    )
}

#[derive(Deserialize)]
struct EventsQuery {
    after_cursor: Option<String>,
    limit: Option<usize>,
}

async fn events(
    State(state): State<Arc<ApiState>>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Result<Json<EventPageDto>, ApiError> {
    state.semantic_status().await?;
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request(
            "invalid_query",
            "event page query is invalid",
            state.request_id(),
        )
    })?;
    let after = query
        .after_cursor
        .as_deref()
        .map(|value| parse_decimal(value, "after_cursor", &state).map(EventCursor))
        .transpose()?;
    let limit = state.limits.page_size(query.limit).map_err(|error| {
        ApiError::bad_request("invalid_page_size", error.to_string(), state.request_id())
    })?;
    let page = state
        .repository
        .events(ObservationEventRequest {
            scope: state.scope.clone(),
            after,
            limit,
        })
        .await
        .map_err(|error| ApiError::from_index(error, state.request_id()))?;
    let events = page
        .events
        .into_iter()
        .map(EventDto::try_from_event)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ApiError::from_index(error, state.request_id()))?;
    Ok(Json(EventPageDto {
        events,
        next_cursor: page.next.map(|cursor| cursor.0.to_string()),
    }))
}

#[derive(Deserialize)]
struct CreateWatchDto {
    selector: SelectorDto,
    start_height: String,
    idempotency_key: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum SelectorDto {
    Address(String),
    Transaction(String),
}

fn parse_selector(
    state: &ApiState,
    selector: SelectorDto,
) -> Result<(WatchSelector, ApiWatchTarget), ApiError> {
    match selector {
        SelectorDto::Address(value) => {
            let (native, canonical) = parse_address(state, &value)?;
            let target = match native {
                ParsedAddress::Ethereum(address) => {
                    ApiWatchTarget::Ethereum(EthereumWatchTarget::Address(address))
                }
                ParsedAddress::Bitcoin(address) => {
                    ApiWatchTarget::Bitcoin(BitcoinWatchTarget::Address(address))
                }
            };
            Ok((WatchSelector::Address(canonical), target))
        }
        SelectorDto::Transaction(value) => {
            let (native, canonical) = parse_transaction(state, &value)?;
            let target = match native {
                ParsedTransaction::Ethereum(transaction) => {
                    ApiWatchTarget::Ethereum(EthereumWatchTarget::Transaction(transaction))
                }
                ParsedTransaction::Bitcoin(transaction) => {
                    ApiWatchTarget::Bitcoin(BitcoinWatchTarget::Transaction(transaction))
                }
            };
            Ok((WatchSelector::Transaction(canonical), target))
        }
    }
}

enum ParsedAddress {
    Ethereum(EthereumAddress),
    Bitcoin(BitcoinAddress),
}

fn parse_address(
    state: &ApiState,
    input: &str,
) -> Result<(ParsedAddress, CanonicalAddress), ApiError> {
    match state.chain {
        ApiChain::Ethereum => {
            let bytes = decode_fixed::<20>(input).map_err(|message| {
                ApiError::bad_request("invalid_address", message, state.request_id())
            })?;
            let value = encode_hex(&bytes);
            Ok((
                ParsedAddress::Ethereum(EthereumAddress(bytes)),
                CanonicalAddress {
                    chain: state.scope.chain.clone(),
                    value,
                },
            ))
        }
        ApiChain::Bitcoin(network) => {
            let native = BitcoinAddress::parse_for_network(input, network).map_err(|error| {
                ApiError::bad_request("invalid_address", error.message, state.request_id())
            })?;
            let script = native.script_pubkey_for_network(network).map_err(|error| {
                ApiError::bad_request("invalid_address", error.message, state.request_id())
            })?;
            if !script.is_p2wpkh() && !script.is_p2tr() {
                return Err(ApiError::bad_request(
                    "unsupported_address",
                    "Bitcoin v1 watches support P2WPKH and P2TR addresses only",
                    state.request_id(),
                ));
            }
            let canonical = CanonicalAddress {
                chain: state.scope.chain.clone(),
                value: native.0.clone(),
            };
            Ok((ParsedAddress::Bitcoin(native), canonical))
        }
    }
}

enum ParsedTransaction {
    Ethereum(EthereumTransactionId),
    Bitcoin(BitcoinTransactionId),
}

fn parse_transaction(
    state: &ApiState,
    input: &str,
) -> Result<(ParsedTransaction, CanonicalTransactionId), ApiError> {
    match state.chain {
        ApiChain::Ethereum => {
            let bytes = decode_fixed::<32>(input).map_err(|message| {
                ApiError::bad_request("invalid_transaction_hash", message, state.request_id())
            })?;
            let value = encode_hex(&bytes);
            Ok((
                ParsedTransaction::Ethereum(EthereumTransactionId(bytes)),
                CanonicalTransactionId {
                    chain: state.scope.chain.clone(),
                    value,
                },
            ))
        }
        ApiChain::Bitcoin(_) => {
            let native = BitcoinTransactionId::from_str(input).map_err(|error| {
                ApiError::bad_request(
                    "invalid_transaction_hash",
                    error.to_string(),
                    state.request_id(),
                )
            })?;
            let canonical = CanonicalTransactionId {
                chain: state.scope.chain.clone(),
                value: native.to_string(),
            };
            Ok((ParsedTransaction::Bitcoin(native), canonical))
        }
    }
}

fn parse_decimal(input: &str, field: &str, state: &ApiState) -> Result<u64, ApiError> {
    input.parse::<u64>().map_err(|_| {
        ApiError::bad_request(
            "invalid_decimal",
            format!("{field} must be an unsigned decimal string"),
            state.request_id(),
        )
    })
}

fn decode_fixed<const N: usize>(input: &str) -> Result<[u8; N], &'static str> {
    let hex = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .ok_or("Ethereum value must have a 0x prefix")?;
    if hex.len() != N * 2 {
        return Err("Ethereum value has an invalid byte length");
    }
    let mut output = [0_u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| "Ethereum value contains non-hex characters")?;
    }
    Ok(output)
}

fn decode_hex(input: &str) -> Result<Vec<u8>, &'static str> {
    let hex = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .ok_or("hexadecimal value must have a 0x prefix")?;
    if hex.is_empty() || hex.len() % 2 != 0 {
        return Err("hexadecimal value must contain a non-empty whole number of bytes");
    }
    let mut output = Vec::with_capacity(hex.len() / 2);
    for index in (0..hex.len()).step_by(2) {
        output.push(
            u8::from_str_radix(&hex[index..index + 2], 16)
                .map_err(|_| "hexadecimal value contains non-hex characters")?,
        );
    }
    Ok(output)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(2 + bytes.len() * 2);
    output.push_str("0x");
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[derive(Serialize)]
struct StatusDto {
    scope: ScopeDto,
    phase: &'static str,
    checkpoint: Option<BlockDto>,
    observed_tip: Option<BlockDto>,
    confirmation_depth: String,
    rebuild_reason: Option<String>,
    halted_reason: Option<String>,
}

impl StatusDto {
    fn try_from_status(status: SyncStatus) -> Result<Self, IndexError> {
        let chain = status.scope.chain.clone();
        Ok(Self {
            scope: ScopeDto::from(&status.scope),
            phase: phase_name(status.phase),
            checkpoint: status
                .checkpoint
                .map(|block| BlockDto::try_from_block(block, &chain))
                .transpose()?,
            observed_tip: status
                .observed_tip
                .map(|block| BlockDto::try_from_block(block, &chain))
                .transpose()?,
            confirmation_depth: status.confirmation_policy.minimum_confirmations.to_string(),
            rebuild_reason: status.rebuild_reason.map(|reason| reason.message),
            halted_reason: status.halted_reason,
        })
    }
}

#[derive(Serialize)]
struct ScopeDto {
    chain: String,
    network: String,
}

impl From<&IndexScope> for ScopeDto {
    fn from(scope: &IndexScope) -> Self {
        Self {
            chain: scope.chain.0.clone(),
            network: scope.network.clone(),
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

impl BlockDto {
    fn try_from_block(
        block: indexing::BlockRef,
        chain: &chain_identity::ChainId,
    ) -> Result<Self, IndexError> {
        Ok(Self {
            height: block.height.0.to_string(),
            hash: encode_chain_block_hash(chain, &block.hash)?,
            parent_hash: block
                .parent_hash
                .as_ref()
                .map(|hash| encode_chain_block_hash(chain, hash))
                .transpose()?,
            timestamp: block.timestamp.map(|value| value.to_string()),
        })
    }
}

fn encode_chain_block_hash(
    chain: &chain_identity::ChainId,
    hash: &indexing::BlockHash,
) -> Result<String, IndexError> {
    if chain.0 == "bitcoin" {
        format_bitcoin_block_hash(hash).map_err(|error| {
            IndexError::new(
                IndexErrorKind::Other,
                format!("Bitcoin block hash could not be encoded: {}", error.message),
                false,
            )
        })
    } else {
        Ok(encode_hex(&hash.0))
    }
}

#[derive(Serialize)]
struct WatchDto {
    id: String,
    scope: ScopeDto,
    selector: SelectorResponseDto,
    start_height: String,
    registered_at: Option<BlockDto>,
    inactive_from: Option<String>,
    confirmation_depth: String,
}

impl WatchDto {
    fn try_from_receipt(receipt: WatchReceipt) -> Result<Self, IndexError> {
        let chain = receipt.scope.chain.clone();
        Ok(Self {
            id: receipt.id.0,
            scope: ScopeDto::from(&receipt.scope),
            selector: SelectorResponseDto::from(receipt.selector),
            start_height: receipt.start_height.0.to_string(),
            registered_at: receipt
                .registered_at
                .map(|block| BlockDto::try_from_block(block, &chain))
                .transpose()?,
            inactive_from: receipt.inactive_from.map(|height| height.0.to_string()),
            confirmation_depth: receipt
                .confirmation_policy
                .minimum_confirmations
                .to_string(),
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum SelectorResponseDto {
    Address(String),
    Transaction(String),
}

impl From<WatchSelector> for SelectorResponseDto {
    fn from(selector: WatchSelector) -> Self {
        match selector {
            WatchSelector::Address(address) => Self::Address(address.value),
            WatchSelector::Transaction(transaction) => Self::Transaction(transaction.value),
        }
    }
}

#[derive(Serialize)]
struct UnwatchDto {
    outcome: &'static str,
}

#[derive(Serialize)]
struct TransactionPageDto {
    transactions: Vec<TransactionDto>,
    next: Option<String>,
}

#[derive(Serialize)]
struct BitcoinUtxoPageDto {
    generation: String,
    revision: String,
    checkpoint: Option<BlockDto>,
    outputs: Vec<BitcoinUtxoDto>,
    next: Option<String>,
}

#[derive(Serialize)]
struct BitcoinUtxoDto {
    transaction_id: String,
    output_index: String,
    value_sats: String,
    script_pubkey: String,
    address: String,
    created_height: String,
    coinbase: bool,
    confirmations: String,
}

impl BitcoinUtxoDto {
    fn try_from_indexed(
        output: BitcoinIndexedOutput,
        checkpoint_height: Option<u64>,
        state: &ApiState,
    ) -> Result<Self, ApiError> {
        let confirmations = match checkpoint_height {
            Some(height) => height
                .checked_sub(output.created_height.0)
                .and_then(|depth| depth.checked_add(1))
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        "UTXO creation height is ahead of the canonical checkpoint",
                        false,
                        state.request_id(),
                    )
                })?,
            None => 0,
        };
        Ok(Self {
            transaction_id: output.outpoint.transaction_id.to_string(),
            output_index: output.outpoint.output_index.to_string(),
            value_sats: output.value.0.to_string(),
            script_pubkey: encode_hex(&output.script_pubkey),
            address: output.address.0,
            created_height: output.created_height.0.to_string(),
            coinbase: output.coinbase,
            confirmations: confirmations.to_string(),
        })
    }
}

#[derive(Serialize)]
struct TransactionDto {
    scope: ScopeDto,
    transaction_id: String,
    revision: String,
    status: TransactionStatusDto,
    movements: Vec<MovementDto>,
    fee: Option<FeeDto>,
    first_seen_at: String,
    observed_at: String,
}

impl TransactionDto {
    fn try_from_transaction(transaction: ObservedTransaction) -> Result<Self, IndexError> {
        let chain = transaction.scope.chain.clone();
        Ok(Self {
            scope: ScopeDto::from(&transaction.scope),
            transaction_id: transaction.transaction_id.value,
            revision: transaction.revision.0.to_string(),
            status: TransactionStatusDto::try_from_status(transaction.status, &chain)?,
            movements: transaction
                .movements
                .into_iter()
                .map(MovementDto::from)
                .collect(),
            fee: transaction.fee.map(FeeDto::from),
            first_seen_at: transaction.first_seen_at.to_string(),
            observed_at: transaction.observed_at.to_string(),
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TransactionStatusDto {
    Pending,
    Included {
        block: BlockDto,
        confirmations: String,
    },
    Confirmed {
        block: BlockDto,
        proof: ConfirmationProofDto,
    },
    Failed {
        block: Option<BlockDto>,
        reason: Option<String>,
    },
    Replaced {
        by: String,
    },
    Dropped,
    Reorged {
        previous_block: BlockDto,
    },
}

impl TransactionStatusDto {
    fn try_from_status(
        status: TransactionStatus,
        chain: &chain_identity::ChainId,
    ) -> Result<Self, IndexError> {
        Ok(match status {
            TransactionStatus::Pending => Self::Pending,
            TransactionStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: BlockDto::try_from_block(block, chain)?,
                confirmations: confirmations.to_string(),
            },
            TransactionStatus::Confirmed { block, proof } => Self::Confirmed {
                block: BlockDto::try_from_block(block, chain)?,
                proof: proof.into(),
            },
            TransactionStatus::Failed { block, reason } => Self::Failed {
                block: block
                    .map(|block| BlockDto::try_from_block(block, chain))
                    .transpose()?,
                reason,
            },
            TransactionStatus::Replaced { by } => Self::Replaced { by: by.value },
            TransactionStatus::Dropped => Self::Dropped,
            TransactionStatus::Reorged { previous_block } => Self::Reorged {
                previous_block: BlockDto::try_from_block(previous_block, chain)?,
            },
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ConfirmationProofDto {
    Depth { required: String, observed: String },
    ChainFinalized,
    DepthAndChainFinalized { required: String, observed: String },
}

impl From<indexing::ConfirmationProof> for ConfirmationProofDto {
    fn from(proof: indexing::ConfirmationProof) -> Self {
        match proof {
            indexing::ConfirmationProof::Depth { required, observed } => Self::Depth {
                required: required.to_string(),
                observed: observed.to_string(),
            },
            indexing::ConfirmationProof::ChainFinalized => Self::ChainFinalized,
            indexing::ConfirmationProof::DepthAndChainFinalized { required, observed } => {
                Self::DepthAndChainFinalized {
                    required: required.to_string(),
                    observed: observed.to_string(),
                }
            }
        }
    }
}

#[derive(Serialize)]
struct MovementDto {
    id: String,
    asset: String,
    amount: String,
    from: Option<String>,
    to: Option<String>,
    kind: &'static str,
}

impl From<indexing::ValueMovement> for MovementDto {
    fn from(movement: indexing::ValueMovement) -> Self {
        Self {
            id: movement.id.0,
            asset: movement.asset.asset,
            amount: encode_hex(&movement.amount.0),
            from: movement.from.map(|address| address.value),
            to: movement.to.map(|address| address.value),
            kind: match movement.kind {
                indexing::MovementKind::Transfer => "transfer",
                indexing::MovementKind::Input => "input",
                indexing::MovementKind::Output => "output",
                indexing::MovementKind::InternalTransfer => "internal_transfer",
                indexing::MovementKind::Mint => "mint",
                indexing::MovementKind::Burn => "burn",
            },
        }
    }
}

#[derive(Serialize)]
struct FeeDto {
    asset: String,
    amount: String,
    payer: Option<String>,
}

impl From<indexing::NetworkFee> for FeeDto {
    fn from(fee: indexing::NetworkFee) -> Self {
        Self {
            asset: fee.asset.asset,
            amount: encode_hex(&fee.amount.0),
            payer: fee.payer.map(|address| address.value),
        }
    }
}

#[derive(Serialize)]
struct EventPageDto {
    events: Vec<EventDto>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct EventDto {
    id: String,
    cursor: String,
    watch_ids: Vec<String>,
    previous_status: Option<TransactionStatusDto>,
    transaction: TransactionDto,
}

impl EventDto {
    fn try_from_event(event: ObservationEvent) -> Result<Self, IndexError> {
        let chain = event.transaction.scope.chain.clone();
        Ok(Self {
            id: event.id.0,
            cursor: event.cursor.0.to_string(),
            watch_ids: event.watch_ids.into_iter().map(|id| id.0).collect(),
            previous_status: event
                .previous_status
                .map(|status| TransactionStatusDto::try_from_status(status, &chain))
                .transpose()?,
            transaction: TransactionDto::try_from_transaction(event.transaction)?,
        })
    }
}

fn phase_name(phase: SyncPhase) -> &'static str {
    match phase {
        SyncPhase::Starting => "starting",
        SyncPhase::Reconciling => "reconciling",
        SyncPhase::CatchingUp => "catching_up",
        SyncPhase::Ready => "ready",
        SyncPhase::Reverting => "reverting",
        SyncPhase::Replaying => "replaying",
        SyncPhase::RebuildRequired => "rebuild_required",
        SyncPhase::Halted => "halted",
    }
}

#[derive(Serialize)]
struct ErrorDto {
    code: &'static str,
    message: String,
    retryable: bool,
    request_id: String,
}

struct ApiError {
    status: StatusCode,
    body: ErrorDto,
}

impl ApiError {
    fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        retryable: bool,
        request_id: String,
    ) -> Self {
        Self {
            status,
            body: ErrorDto {
                code,
                message: message.into(),
                retryable,
                request_id,
            },
        }
    }

    fn bad_request(code: &'static str, message: impl Into<String>, request_id: String) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, false, request_id)
    }

    fn from_index(error: IndexError, request_id: String) -> Self {
        let (status, code) = match error.kind {
            IndexErrorKind::Conflict => (StatusCode::CONFLICT, "conflict"),
            IndexErrorKind::ScopeMismatch => (StatusCode::NOT_FOUND, "scope_not_found"),
            IndexErrorKind::PolicyMismatch => (StatusCode::CONFLICT, "policy_mismatch"),
            IndexErrorKind::InvalidWatch => (StatusCode::BAD_REQUEST, "invalid_watch"),
            IndexErrorKind::InvalidRequest | IndexErrorKind::InvalidBlock => {
                (StatusCode::BAD_REQUEST, "invalid_request")
            }
            IndexErrorKind::ReorgBeyondRetention | IndexErrorKind::RebuildRequired => {
                (StatusCode::SERVICE_UNAVAILABLE, "rebuild_required")
            }
            IndexErrorKind::Halted => (StatusCode::SERVICE_UNAVAILABLE, "indexer_halted"),
            IndexErrorKind::Source | IndexErrorKind::CannotConnect if error.retryable => {
                (StatusCode::SERVICE_UNAVAILABLE, "source_unavailable")
            }
            IndexErrorKind::Storage if error.retryable => {
                (StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable")
            }
            IndexErrorKind::Source
            | IndexErrorKind::Storage
            | IndexErrorKind::CannotConnect
            | IndexErrorKind::Other => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            "Indexer operation failed".to_owned()
        } else {
            error.message
        };
        Self::new(status, code, message, error.retryable, request_id)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use chain_bitcoin::{BitcoinOutPoint, Satoshi};
    use chain_identity::ChainId;
    use indexing::{BlockHash, ConfirmationPolicy, RebuildReason};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct FakeRepository {
        status: SyncStatus,
    }

    struct FakeBitcoinUtxos;

    impl BitcoinUtxoRepository for FakeBitcoinUtxos {
        fn utxos<'a>(
            &'a self,
            request: BitcoinUtxoPageRequest,
        ) -> BoxFuture<'a, Result<BitcoinUtxoPage, IndexError>> {
            Box::pin(async move {
                Ok(BitcoinUtxoPage {
                    snapshot: ProjectionSnapshot {
                        generation: indexing::RebuildGeneration(7),
                        revision: 9,
                        checkpoint: Some(indexing::BlockRef {
                            height: BlockHeight(42),
                            hash: BlockHash(vec![0x11; 32]),
                            parent_hash: Some(BlockHash(vec![0x10; 32])),
                            timestamp: Some(1_000),
                        }),
                    },
                    outputs: vec![BitcoinIndexedOutput {
                        outpoint: BitcoinOutPoint {
                            transaction_id: BitcoinTransactionId([0x22; 32]),
                            output_index: 1,
                        },
                        value: Satoshi(75_000),
                        script_pubkey: vec![0x51, 0x20],
                        address: request.address,
                        created_height: BlockHeight(40),
                        coinbase: false,
                    }],
                    next: None,
                })
            })
        }
    }

    impl ApiRepository for FakeRepository {
        fn status<'a>(
            &'a self,
            _scope: &'a IndexScope,
        ) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
            let status = self.status.clone();
            Box::pin(async move { Ok(status) })
        }

        fn register<'a>(
            &'a self,
            _request: WatchRequest,
            _target: ApiWatchTarget,
        ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
            unexpected_call()
        }

        fn unwatch<'a>(
            &'a self,
            _command: UnwatchCommand,
        ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
            unexpected_call()
        }

        fn transaction<'a>(
            &'a self,
            _request: TransactionRequest,
        ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
            unexpected_call()
        }

        fn transactions_by_address<'a>(
            &'a self,
            _request: TransactionPageRequest,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            unexpected_call()
        }

        fn events<'a>(
            &'a self,
            _request: ObservationEventRequest,
        ) -> BoxFuture<'a, Result<ObservationEventPage, IndexError>> {
            unexpected_call()
        }
    }

    fn unexpected_call<'a, T>() -> BoxFuture<'a, Result<T, IndexError>> {
        Box::pin(async {
            Err(IndexError::new(
                IndexErrorKind::Other,
                "unexpected fake repository call",
                false,
            ))
        })
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("ethereum".to_owned()),
            network: "test".to_owned(),
        }
    }

    fn status(phase: SyncPhase) -> SyncStatus {
        let checkpoint = indexing::BlockRef {
            height: BlockHeight(42),
            hash: BlockHash(vec![0x11; 32]),
            parent_hash: Some(BlockHash(vec![0x10; 32])),
            timestamp: Some(1_000),
        };
        SyncStatus {
            scope: scope(),
            checkpoint: Some(checkpoint.clone()),
            observed_tip: Some(checkpoint.clone()),
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: 12,
                require_chain_finality: false,
            },
            phase,
            rebuild_reason: (phase == SyncPhase::RebuildRequired).then_some(RebuildReason {
                checkpoint,
                oldest_retained: BlockHeight(1),
                message: "operator rebuild required".to_owned(),
            }),
            halted_reason: None,
        }
    }

    fn bitcoin_scope() -> IndexScope {
        IndexScope {
            chain: ChainId("bitcoin".to_owned()),
            network: "regtest".to_owned(),
        }
    }

    fn bitcoin_status() -> SyncStatus {
        let mut value = status(SyncPhase::Ready);
        value.scope = bitcoin_scope();
        value
    }

    fn regtest_address() -> String {
        let public_key = bitcoin::PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        bitcoin::Address::p2wpkh(
            &bitcoin::CompressedPublicKey::try_from(public_key)
                .expect("test public key must be compressed"),
            bitcoin::Network::Regtest,
        )
        .to_string()
    }

    fn regtest_legacy_address() -> String {
        let public_key = bitcoin::PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        bitcoin::Address::p2pkh(public_key, bitcoin::Network::Regtest).to_string()
    }

    fn app(phase: SyncPhase) -> Router {
        let state = Arc::new(ApiState::new(
            scope(),
            Arc::new(FakeRepository {
                status: status(phase),
            }),
            BlockHeight(10),
            http::RequestLimits::default(),
        ));
        router(state)
    }

    fn bitcoin_app() -> Router {
        bitcoin_app_with_health(true)
    }

    fn bitcoin_app_with_health(ready: bool) -> Router {
        let state = Arc::new(ApiState::new_bitcoin(
            bitcoin_scope(),
            BitcoinNetwork::Regtest,
            Arc::new(FakeRepository {
                status: bitcoin_status(),
            }),
            Arc::new(FakeBitcoinUtxos),
            http::HealthState::new(ready),
            BlockHeight(0),
            http::RequestLimits::default(),
        ));
        bitcoin_router(state)
    }

    async fn response_json(response: Response) -> Value {
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("test response body must be readable");
        serde_json::from_slice(&body).expect("test response must be JSON")
    }

    #[test]
    fn fixed_hex_parser_is_strict() {
        assert_eq!(
            decode_fixed::<2>("0x00ff").expect("valid bytes must decode"),
            [0, 255]
        );
        assert!(decode_fixed::<2>("00ff").is_err());
        assert!(decode_fixed::<2>("0x0ff").is_err());
        assert!(decode_fixed::<2>("0x00fg").is_err());
    }

    #[tokio::test]
    async fn status_encodes_large_fields_as_strings() {
        let response = app(SyncPhase::Ready)
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["checkpoint"]["height"], "42");
        assert_eq!(body["confirmation_depth"], "12");
    }

    #[tokio::test]
    async fn validation_errors_use_the_structured_contract() {
        let response = app(SyncPhase::Ready)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scopes/ethereum/test/watches")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"selector":{{"type":"address","value":"0x{}"}},"start_height":"9","idempotency_key":"deposit-1"}}"#,
                        "11".repeat(20)
                    )))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response_json(response).await;
        assert_eq!(body["code"], "invalid_start_height");
        assert_eq!(body["retryable"], false);
        assert!(
            body["request_id"]
                .as_str()
                .is_some_and(|value| value.starts_with("ix-request-"))
        );
    }

    #[tokio::test]
    async fn rebuild_required_keeps_status_available_and_blocks_semantic_queries() {
        let application = app(SyncPhase::RebuildRequired);
        let status_response = application
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(status_response.status(), StatusCode::OK);

        let semantic_response = application
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/scopes/ethereum/test/transactions/0x{}",
                        "22".repeat(32)
                    ))
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(semantic_response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response_json(semantic_response).await;
        assert_eq!(body["code"], "semantic_surface_unavailable");
        assert_eq!(body["retryable"], true);
    }

    #[tokio::test]
    async fn pagination_above_the_public_maximum_is_rejected() {
        let response = app(SyncPhase::Ready)
            .oneshot(
                Request::builder()
                    .uri("/v1/events?limit=1001")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response_json(response).await;
        assert_eq!(body["code"], "invalid_page_size");
    }

    #[tokio::test]
    async fn routing_errors_use_the_structured_contract() {
        let application = app(SyncPhase::Ready);
        let not_found = application
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/unknown")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        assert_eq!(response_json(not_found).await["code"], "route_not_found");

        let method = application
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scopes/ethereum/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response_json(method).await["code"], "method_not_allowed");
    }

    #[tokio::test]
    async fn bitcoin_utxo_route_returns_decimal_values_and_confirmations() {
        let address = regtest_address();
        let response = bitcoin_app()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/scopes/bitcoin/regtest/addresses/{address}/utxos?limit=10"
                    ))
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["generation"], "7");
        assert_eq!(body["revision"], "9");
        assert_eq!(body["checkpoint"]["height"], "42");
        assert_eq!(body["outputs"][0]["output_index"], "1");
        assert_eq!(body["outputs"][0]["value_sats"], "75000");
        assert_eq!(body["outputs"][0]["created_height"], "40");
        assert_eq!(body["outputs"][0]["confirmations"], "3");
        assert_eq!(body["outputs"][0]["address"], address);
        assert!(body["next"].is_null());
    }

    #[tokio::test]
    async fn bitcoin_utxo_route_requires_operational_readiness() {
        let address = regtest_address();
        let response = bitcoin_app_with_health(false)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/scopes/bitcoin/regtest/addresses/{address}/utxos"
                    ))
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response_json(response).await;
        assert_eq!(body["code"], "bitcoin_utxo_snapshot_unavailable");
        assert_eq!(body["retryable"], true);
    }

    #[tokio::test]
    async fn bitcoin_watch_rejects_unsupported_legacy_address_before_persistence() {
        let response = bitcoin_app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scopes/bitcoin/regtest/watches")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"selector":{{"type":"address","value":"{}"}},"start_height":"42","idempotency_key":"unsupported-address"}}"#,
                        regtest_legacy_address()
                    )))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response_json(response).await["code"], "unsupported_address");
    }

    #[tokio::test]
    async fn bitcoin_block_hashes_use_core_display_order_without_ethereum_prefix() {
        let response = bitcoin_app()
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/bitcoin/regtest/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["checkpoint"]["hash"], "11".repeat(32));
        assert_eq!(body["checkpoint"]["parent_hash"], "10".repeat(32));
    }

    #[test]
    fn projection_cursor_round_trips_full_snapshot_and_relative_key() {
        let state = ApiState::new_bitcoin(
            bitcoin_scope(),
            BitcoinNetwork::Regtest,
            Arc::new(FakeRepository {
                status: bitcoin_status(),
            }),
            Arc::new(FakeBitcoinUtxos),
            http::HealthState::new(true),
            BlockHeight(0),
            http::RequestLimits::default(),
        );
        let cursor = ProjectionCursor {
            snapshot: ProjectionSnapshot {
                generation: indexing::RebuildGeneration(7),
                revision: 9,
                checkpoint: Some(indexing::BlockRef {
                    height: BlockHeight(42),
                    hash: BlockHash(vec![0x11; 32]),
                    parent_hash: Some(BlockHash(vec![0x10; 32])),
                    timestamp: Some(1_000),
                }),
            },
            key: vec![0x00, 0xab, 0xff],
        };
        let encoded = encode_projection_cursor(&cursor);

        let decoded = match decode_projection_cursor(&encoded, &state) {
            Ok(decoded) => decoded,
            Err(_) => panic!("encoded cursor must decode"),
        };
        assert_eq!(decoded, cursor);
    }
}
