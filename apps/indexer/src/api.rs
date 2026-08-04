use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

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
use chain_ethereum::{EthereumAddress, EthereumTransactionId, EthereumWatchTarget};
use chain_identity::{CanonicalAddress, CanonicalTransactionId};
use indexing::{
    BlockHeight, BoxFuture, EventCursor, IndexError, IndexErrorKind, IndexRepository, IndexScope,
    ObservationEvent, ObservationEventPage, ObservationEventRequest, ObservedTransaction,
    RegisterWatchCommand, RegisterWatchOutcome, SyncPhase, SyncStatus, TransactionPage,
    TransactionPageRequest, TransactionRequest, TransactionStatus, UnwatchCommand, UnwatchOutcome,
    WatchReceipt, WatchRequest, WatchSelector,
};
use serde::{Deserialize, Serialize};

pub trait ApiRepository: Send + Sync {
    fn status<'a>(&'a self, scope: &'a IndexScope)
    -> BoxFuture<'a, Result<SyncStatus, IndexError>>;

    fn register<'a>(
        &'a self,
        request: WatchRequest,
        target: EthereumWatchTarget,
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

impl<R> ApiRepository for R
where
    R: IndexRepository<Target = EthereumWatchTarget>,
{
    fn status<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
        IndexRepository::status(self, scope)
    }

    fn register<'a>(
        &'a self,
        request: WatchRequest,
        target: EthereumWatchTarget,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            let registered_at = self.checkpoint(&request.scope).await?;
            let outcome = self
                .register_watch(RegisterWatchCommand {
                    request,
                    target,
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
        IndexRepository::unwatch(self, command)
    }

    fn transaction<'a>(
        &'a self,
        request: TransactionRequest,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        IndexRepository::transaction(self, request)
    }

    fn transactions_by_address<'a>(
        &'a self,
        request: TransactionPageRequest,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        IndexRepository::transactions_by_address(self, request)
    }

    fn events<'a>(
        &'a self,
        request: ObservationEventRequest,
    ) -> BoxFuture<'a, Result<ObservationEventPage, IndexError>> {
        IndexRepository::events(self, request)
    }
}

pub struct ApiState {
    scope: IndexScope,
    repository: Arc<dyn ApiRepository>,
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
            repository,
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
    Ok(Json(StatusDto::from(status)))
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
    Ok((StatusCode::OK, Json(WatchDto::from(receipt))))
}

async fn unwatch(
    State(state): State<Arc<ApiState>>,
    Path((network, watch_id)): Path<(String, String)>,
) -> Result<Json<UnwatchDto>, ApiError> {
    state.validate_network(&network)?;
    let status = state.semantic_status().await?;
    let inactive_from = match status.checkpoint {
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
    Ok(Json(TransactionDto::from(transaction)))
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
    Ok(Json(TransactionPageDto {
        transactions: page
            .transactions
            .into_iter()
            .map(TransactionDto::from)
            .collect(),
        next: page.next.map(|next| next.value),
    }))
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
    Ok(Json(EventPageDto {
        events: page.events.into_iter().map(EventDto::from).collect(),
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
) -> Result<(WatchSelector, EthereumWatchTarget), ApiError> {
    match selector {
        SelectorDto::Address(value) => {
            let (native, canonical) = parse_address(state, &value)?;
            Ok((
                WatchSelector::Address(canonical),
                EthereumWatchTarget::Address(native),
            ))
        }
        SelectorDto::Transaction(value) => {
            let (native, canonical) = parse_transaction(state, &value)?;
            Ok((
                WatchSelector::Transaction(canonical),
                EthereumWatchTarget::Transaction(native),
            ))
        }
    }
}

fn parse_address(
    state: &ApiState,
    input: &str,
) -> Result<(EthereumAddress, CanonicalAddress), ApiError> {
    let bytes = decode_fixed::<20>(input)
        .map_err(|message| ApiError::bad_request("invalid_address", message, state.request_id()))?;
    let value = encode_hex(&bytes);
    Ok((
        EthereumAddress(bytes),
        CanonicalAddress {
            chain: state.scope.chain.clone(),
            value,
        },
    ))
}

fn parse_transaction(
    state: &ApiState,
    input: &str,
) -> Result<(EthereumTransactionId, CanonicalTransactionId), ApiError> {
    let bytes = decode_fixed::<32>(input).map_err(|message| {
        ApiError::bad_request("invalid_transaction_hash", message, state.request_id())
    })?;
    let value = encode_hex(&bytes);
    Ok((
        EthereumTransactionId(bytes),
        CanonicalTransactionId {
            chain: state.scope.chain.clone(),
            value,
        },
    ))
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

impl From<SyncStatus> for StatusDto {
    fn from(status: SyncStatus) -> Self {
        Self {
            scope: ScopeDto::from(&status.scope),
            phase: phase_name(status.phase),
            checkpoint: status.checkpoint.map(BlockDto::from),
            observed_tip: status.observed_tip.map(BlockDto::from),
            confirmation_depth: status.confirmation_policy.minimum_confirmations.to_string(),
            rebuild_reason: status.rebuild_reason.map(|reason| reason.message),
            halted_reason: status.halted_reason,
        }
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

impl From<indexing::BlockRef> for BlockDto {
    fn from(block: indexing::BlockRef) -> Self {
        Self {
            height: block.height.0.to_string(),
            hash: encode_hex(&block.hash.0),
            parent_hash: block.parent_hash.map(|hash| encode_hex(&hash.0)),
            timestamp: block.timestamp.map(|value| value.to_string()),
        }
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

impl From<WatchReceipt> for WatchDto {
    fn from(receipt: WatchReceipt) -> Self {
        Self {
            id: receipt.id.0,
            scope: ScopeDto::from(&receipt.scope),
            selector: SelectorResponseDto::from(receipt.selector),
            start_height: receipt.start_height.0.to_string(),
            registered_at: receipt.registered_at.map(BlockDto::from),
            inactive_from: receipt.inactive_from.map(|height| height.0.to_string()),
            confirmation_depth: receipt
                .confirmation_policy
                .minimum_confirmations
                .to_string(),
        }
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

impl From<ObservedTransaction> for TransactionDto {
    fn from(transaction: ObservedTransaction) -> Self {
        Self {
            scope: ScopeDto::from(&transaction.scope),
            transaction_id: transaction.transaction_id.value,
            revision: transaction.revision.0.to_string(),
            status: TransactionStatusDto::from(transaction.status),
            movements: transaction
                .movements
                .into_iter()
                .map(MovementDto::from)
                .collect(),
            fee: transaction.fee.map(FeeDto::from),
            first_seen_at: transaction.first_seen_at.to_string(),
            observed_at: transaction.observed_at.to_string(),
        }
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

impl From<TransactionStatus> for TransactionStatusDto {
    fn from(status: TransactionStatus) -> Self {
        match status {
            TransactionStatus::Pending => Self::Pending,
            TransactionStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: block.into(),
                confirmations: confirmations.to_string(),
            },
            TransactionStatus::Confirmed { block, proof } => Self::Confirmed {
                block: block.into(),
                proof: proof.into(),
            },
            TransactionStatus::Failed { block, reason } => Self::Failed {
                block: block.map(BlockDto::from),
                reason,
            },
            TransactionStatus::Replaced { by } => Self::Replaced { by: by.value },
            TransactionStatus::Dropped => Self::Dropped,
            TransactionStatus::Reorged { previous_block } => Self::Reorged {
                previous_block: previous_block.into(),
            },
        }
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

impl From<ObservationEvent> for EventDto {
    fn from(event: ObservationEvent) -> Self {
        Self {
            id: event.id.0,
            cursor: event.cursor.0.to_string(),
            watch_ids: event.watch_ids.into_iter().map(|id| id.0).collect(),
            previous_status: event.previous_status.map(TransactionStatusDto::from),
            transaction: event.transaction.into(),
        }
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
    use chain_identity::ChainId;
    use indexing::{BlockHash, ConfirmationPolicy, RebuildReason};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct FakeRepository {
        status: SyncStatus,
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
            _target: EthereumWatchTarget,
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
}
