use std::{fmt, num::NonZeroU32, time::Duration};

use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use deposits::{BoxFuture, DepositIndexerClient};
use indexing::{
    BlockHash, BlockHeight, BlockRef, ConfirmationPolicy, ConfirmationProof, EventCursor,
    IndexError, IndexErrorKind, IndexScope, MovementId, MovementKind, NetworkFee, ObservationEvent,
    ObservationEventId, ObservationRevision, ObservedTransaction, SyncPhase, SyncStatus,
    TransactionStatus, ValueMovement, WatchId, WatchReceipt, WatchRequest, WatchSelector,
};
use reqwest::{Method, StatusCode, Url, redirect::Policy};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::config::{BearerSecret, IndexerEndpoint, IndexerOptions};

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct IndexerClient {
    endpoint: IndexerEndpoint,
    bearer_token: Option<BearerSecret>,
    request_timeout: Duration,
    retry_attempts: NonZeroU32,
    retry_initial_backoff: Duration,
    retry_max_backoff: Duration,
    client: reqwest::Client,
}

impl IndexerClient {
    pub fn new(options: &IndexerOptions) -> Result<Self, IndexError> {
        options
            .validate()
            .map_err(|error| invalid_request(error.to_string()))?;
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(options.request_timeout())
            .timeout(options.request_timeout())
            .build()
            .map_err(|_| cannot_connect("failed to construct the Indexer HTTP client", false))?;
        Ok(Self {
            endpoint: options.indexer_url.clone(),
            bearer_token: options.bearer_token.clone(),
            request_timeout: options.request_timeout(),
            retry_attempts: options
                .retry_attempts()
                .map_err(|error| invalid_request(error.to_string()))?,
            retry_initial_backoff: options.retry_initial_backoff(),
            retry_max_backoff: options.retry_max_backoff(),
            client,
        })
    }

    pub async fn events(
        &self,
        scope: &IndexScope,
        after: Option<EventCursor>,
        limit: usize,
    ) -> Result<EventPage, IndexError> {
        ensure_ethereum_scope(scope)?;
        if limit == 0 || limit > 1_000 {
            return Err(invalid_request(
                "Indexer event page size must be between 1 and 1000",
            ));
        }
        let mut url = self.route(&["v1", "events"])?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(cursor) = after {
                query.append_pair("after_cursor", &cursor.0.to_string());
            }
            query.append_pair("limit", &limit.to_string());
        }
        let dto: EventPageDto = self.send_json(Method::GET, url, None::<&()>).await?;
        let events: Vec<ObservationEvent> = dto
            .events
            .into_iter()
            .map(EventDto::try_into)
            .collect::<Result<Vec<_>, _>>()?;
        for event in &events {
            if event.transaction.scope != *scope {
                return Err(protocol_error(
                    "Indexer event feed returned an event from another scope",
                ));
            }
        }
        let next_cursor = dto
            .next_cursor
            .map(|value| parse_decimal(&value, "next_cursor").map(EventCursor))
            .transpose()?;
        if let Some(next) = next_cursor
            && events.last().map(|event| event.cursor) != Some(next)
        {
            return Err(protocol_error(
                "Indexer event continuation cursor does not match the last event",
            ));
        }
        Ok(EventPage {
            events,
            next_cursor,
        })
    }

    async fn status_request(&self, scope: &IndexScope) -> Result<SyncStatus, IndexError> {
        ensure_ethereum_scope(scope)?;
        let url = self.route(&["v1", "scopes", "ethereum", &scope.network, "status"])?;
        let dto: StatusDto = self.send_json(Method::GET, url, None::<&()>).await?;
        let result: SyncStatus = dto.try_into()?;
        if result.scope != *scope {
            return Err(protocol_error(
                "Indexer status response belongs to another scope",
            ));
        }
        Ok(result)
    }

    async fn watch_request(&self, request: WatchRequest) -> Result<WatchReceipt, IndexError> {
        ensure_ethereum_scope(&request.scope)?;
        let selector_chain = match &request.selector {
            WatchSelector::Address(address) => &address.chain,
            WatchSelector::Transaction(transaction) => &transaction.chain,
        };
        if selector_chain != &request.scope.chain {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Indexer watch selector does not belong to its requested scope",
                false,
            ));
        }
        let url = self.route(&[
            "v1",
            "scopes",
            "ethereum",
            &request.scope.network,
            "watches",
        ])?;
        let selector = match &request.selector {
            WatchSelector::Address(address) => SelectorDto::Address(address.value.clone()),
            WatchSelector::Transaction(transaction) => {
                SelectorDto::Transaction(transaction.value.clone())
            }
        };
        let body = CreateWatchDto {
            selector,
            start_height: request.start_height.0.to_string(),
            idempotency_key: request.idempotency_key,
        };
        let dto: WatchDto = self.send_json(Method::POST, url, Some(&body)).await?;
        let result: WatchReceipt = dto.try_into()?;
        if result.scope != request.scope {
            return Err(protocol_error(
                "Indexer watch response belongs to another scope",
            ));
        }
        Ok(result)
    }

    fn route(&self, segments: &[&str]) -> Result<Url, IndexError> {
        let mut url = self.endpoint.url().clone();
        url.path_segments_mut()
            .map_err(|_| invalid_request("Indexer endpoint cannot be used as a base URL"))?
            .clear()
            .extend(segments);
        Ok(url)
    }

    async fn send_json<T, B>(
        &self,
        method: Method,
        url: Url,
        body: Option<&B>,
    ) -> Result<T, IndexError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| invalid_request("failed to encode Indexer request"))?;
        let mut attempt = 1_u32;
        loop {
            let response = self
                .send_once(method.clone(), url.clone(), body.as_deref())
                .await;
            match response {
                Ok(response)
                    if retryable_status(response.status())
                        && attempt < self.retry_attempts.get() =>
                {
                    drop(response);
                    tokio::time::sleep(self.backoff_after(attempt)).await;
                    attempt += 1;
                }
                Err(error) if error.retryable && attempt < self.retry_attempts.get() => {
                    tokio::time::sleep(self.backoff_after(attempt)).await;
                    attempt += 1;
                }
                Ok(response) => return decode_response(response).await,
                Err(error) => return Err(error),
            }
        }
    }

    async fn send_once(
        &self,
        method: Method,
        url: Url,
        body: Option<&[u8]>,
    ) -> Result<reqwest::Response, IndexError> {
        let mut request = self
            .client
            .request(method, url)
            .timeout(self.request_timeout);
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token.expose());
        }
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_vec());
        }
        request.send().await.map_err(|error| {
            if error.is_timeout() {
                cannot_connect("Indexer HTTP request timed out", true)
            } else if error.is_connect() || error.is_request() {
                cannot_connect("Indexer HTTP endpoint is unavailable", true)
            } else {
                cannot_connect("Indexer HTTP request failed", false)
            }
        })
    }

    fn backoff_after(&self, failed_attempt: u32) -> Duration {
        let exponent = failed_attempt.saturating_sub(1).min(31);
        let multiplier = 1_u32 << exponent;
        self.retry_initial_backoff
            .checked_mul(multiplier)
            .unwrap_or(self.retry_max_backoff)
            .min(self.retry_max_backoff)
    }
}

impl fmt::Debug for IndexerClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IndexerClient")
            .field("endpoint", &self.endpoint)
            .field("bearer_token", &self.bearer_token)
            .field("request_timeout", &self.request_timeout)
            .field("retry_attempts", &self.retry_attempts)
            .field("retry_initial_backoff", &self.retry_initial_backoff)
            .field("retry_max_backoff", &self.retry_max_backoff)
            .finish_non_exhaustive()
    }
}

impl DepositIndexerClient for IndexerClient {
    fn status<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
        Box::pin(async move { self.status_request(scope).await })
    }

    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move { self.watch_request(request).await })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventPage {
    pub events: Vec<ObservationEvent>,
    pub next_cursor: Option<EventCursor>,
}

fn ensure_ethereum_scope(scope: &IndexScope) -> Result<(), IndexError> {
    if scope.chain.0 != "ethereum" || scope.network.trim().is_empty() {
        Err(IndexError::new(
            IndexErrorKind::ScopeMismatch,
            "PS Indexer client is configured only for one Ethereum network",
            false,
        ))
    } else {
        Ok(())
    }
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

async fn decode_response<T>(response: reqwest::Response) -> Result<T, IndexError>
where
    T: DeserializeOwned,
{
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        return Err(remote_error(status, &body));
    }
    serde_json::from_slice(&body)
        .map_err(|_| protocol_error("Indexer returned an invalid JSON response"))
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, IndexError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(protocol_error("Indexer response exceeds the size limit"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| cannot_connect("failed to read the Indexer HTTP response", true))?
    {
        let next_len = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| protocol_error("Indexer response size overflowed"))?;
        if next_len > MAX_RESPONSE_BYTES {
            return Err(protocol_error("Indexer response exceeds the size limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn remote_error(status: StatusCode, body: &[u8]) -> IndexError {
    let decoded = serde_json::from_slice::<ErrorDto>(body).ok();
    let code = decoded.as_ref().map(|error| error.code.as_str());
    let retryable = decoded
        .as_ref()
        .map_or_else(|| retryable_status(status), |error| error.retryable);
    let kind = match code {
        Some("scope_not_found") => IndexErrorKind::ScopeMismatch,
        Some("policy_mismatch") => IndexErrorKind::PolicyMismatch,
        Some("invalid_watch") => IndexErrorKind::InvalidWatch,
        Some("invalid_request" | "invalid_json" | "invalid_page_size") => {
            IndexErrorKind::InvalidRequest
        }
        Some("conflict") => IndexErrorKind::Conflict,
        Some("rebuild_required") => IndexErrorKind::RebuildRequired,
        Some("indexer_halted") => IndexErrorKind::Halted,
        Some("source_unavailable" | "storage_unavailable") => IndexErrorKind::CannotConnect,
        _ if status == StatusCode::CONFLICT => IndexErrorKind::Conflict,
        _ if status == StatusCode::NOT_FOUND => IndexErrorKind::ScopeMismatch,
        _ if status == StatusCode::BAD_REQUEST => IndexErrorKind::InvalidRequest,
        _ if retryable_status(status) => IndexErrorKind::CannotConnect,
        _ => IndexErrorKind::Other,
    };
    let message = match (code, decoded.as_ref()) {
        (Some("source_unavailable"), _) => "Indexer source is unavailable".to_owned(),
        (Some("storage_unavailable"), _) => "Indexer storage is unavailable".to_owned(),
        (Some("internal_error"), _) => "Indexer operation failed".to_owned(),
        (_, Some(error)) => error.message.clone(),
        _ => format!(
            "Indexer HTTP request failed with status {}",
            status.as_u16()
        ),
    };
    IndexError::new(kind, message, retryable)
}

fn cannot_connect(message: impl Into<String>, retryable: bool) -> IndexError {
    IndexError::new(IndexErrorKind::CannotConnect, message, retryable)
}

fn invalid_request(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidRequest, message, false)
}

fn protocol_error(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Other, message, false)
}

#[derive(Deserialize)]
struct ErrorDto {
    code: String,
    message: String,
    retryable: bool,
    #[allow(dead_code)]
    request_id: String,
}

#[derive(Serialize)]
struct CreateWatchDto {
    selector: SelectorDto,
    start_height: String,
    idempotency_key: String,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum SelectorDto {
    Address(String),
    Transaction(String),
}

#[derive(Deserialize)]
struct StatusDto {
    scope: ScopeDto,
    phase: String,
    checkpoint: Option<BlockDto>,
    observed_tip: Option<BlockDto>,
    confirmation_depth: String,
    #[allow(dead_code)]
    rebuild_reason: Option<String>,
    halted_reason: Option<String>,
}

impl TryFrom<StatusDto> for SyncStatus {
    type Error = IndexError;

    fn try_from(value: StatusDto) -> Result<Self, Self::Error> {
        Ok(Self {
            scope: value.scope.try_into()?,
            checkpoint: value.checkpoint.map(BlockDto::try_into).transpose()?,
            observed_tip: value.observed_tip.map(BlockDto::try_into).transpose()?,
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: parse_decimal(
                    &value.confirmation_depth,
                    "confirmation_depth",
                )?,
                require_chain_finality: false,
            },
            phase: parse_phase(&value.phase)?,
            // The v1 wire API exposes only the operator-facing reason string,
            // not the typed retained-height fields required by RebuildReason.
            rebuild_reason: None,
            halted_reason: value.halted_reason,
        })
    }
}

#[derive(Clone, Deserialize)]
struct ScopeDto {
    chain: String,
    network: String,
}

impl TryFrom<ScopeDto> for IndexScope {
    type Error = IndexError;

    fn try_from(value: ScopeDto) -> Result<Self, Self::Error> {
        if value.chain.trim().is_empty() || value.network.trim().is_empty() {
            return Err(protocol_error("Indexer response contains an empty scope"));
        }
        Ok(Self {
            chain: ChainId(value.chain),
            network: value.network,
        })
    }
}

#[derive(Clone, Deserialize)]
struct BlockDto {
    height: String,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<String>,
}

impl TryFrom<BlockDto> for BlockRef {
    type Error = IndexError;

    fn try_from(value: BlockDto) -> Result<Self, Self::Error> {
        Ok(Self {
            height: BlockHeight(parse_decimal(&value.height, "block.height")?),
            hash: BlockHash(parse_fixed::<32>(&value.hash, "block.hash")?.to_vec()),
            parent_hash: value
                .parent_hash
                .map(|hash| {
                    parse_fixed::<32>(&hash, "block.parent_hash")
                        .map(|bytes| BlockHash(bytes.to_vec()))
                })
                .transpose()?,
            timestamp: value
                .timestamp
                .map(|timestamp| parse_decimal(&timestamp, "block.timestamp"))
                .transpose()?,
        })
    }
}

#[derive(Deserialize)]
struct WatchDto {
    id: String,
    scope: ScopeDto,
    selector: SelectorResponseDto,
    start_height: String,
    registered_at: Option<BlockDto>,
    inactive_from: Option<String>,
    confirmation_depth: String,
}

impl TryFrom<WatchDto> for WatchReceipt {
    type Error = IndexError;

    fn try_from(value: WatchDto) -> Result<Self, Self::Error> {
        if value.id.trim().is_empty() {
            return Err(protocol_error("Indexer returned an empty watch ID"));
        }
        Ok(Self {
            id: WatchId(value.id),
            scope: value.scope.try_into()?,
            selector: value.selector.try_into()?,
            start_height: BlockHeight(parse_decimal(&value.start_height, "start_height")?),
            registered_at: value.registered_at.map(BlockDto::try_into).transpose()?,
            inactive_from: value
                .inactive_from
                .map(|height| parse_decimal(&height, "inactive_from").map(BlockHeight))
                .transpose()?,
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: parse_decimal(
                    &value.confirmation_depth,
                    "confirmation_depth",
                )?,
                require_chain_finality: false,
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum SelectorResponseDto {
    Address(String),
    Transaction(String),
}

impl TryFrom<SelectorResponseDto> for WatchSelector {
    type Error = IndexError;

    fn try_from(value: SelectorResponseDto) -> Result<Self, Self::Error> {
        let chain = ChainId("ethereum".to_owned());
        match value {
            SelectorResponseDto::Address(address) => {
                validate_address(&address, "watch.selector")?;
                Ok(Self::Address(CanonicalAddress {
                    chain,
                    value: address,
                }))
            }
            SelectorResponseDto::Transaction(transaction) => {
                validate_transaction(&transaction, "watch.selector")?;
                Ok(Self::Transaction(CanonicalTransactionId {
                    chain,
                    value: transaction,
                }))
            }
        }
    }
}

#[derive(Deserialize)]
struct EventPageDto {
    events: Vec<EventDto>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct EventDto {
    id: String,
    cursor: String,
    watch_ids: Vec<String>,
    previous_status: Option<TransactionStatusDto>,
    transaction: TransactionDto,
}

impl TryFrom<EventDto> for ObservationEvent {
    type Error = IndexError;

    fn try_from(value: EventDto) -> Result<Self, Self::Error> {
        if value.id.trim().is_empty() || value.watch_ids.iter().any(|id| id.trim().is_empty()) {
            return Err(protocol_error(
                "Indexer event contains an empty event or watch ID",
            ));
        }
        Ok(Self {
            id: ObservationEventId(value.id),
            cursor: EventCursor(parse_decimal(&value.cursor, "event.cursor")?),
            watch_ids: value.watch_ids.into_iter().map(WatchId).collect(),
            previous_status: value
                .previous_status
                .map(TransactionStatusDto::try_into)
                .transpose()?,
            transaction: value.transaction.try_into()?,
        })
    }
}

#[derive(Deserialize)]
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

impl TryFrom<TransactionDto> for ObservedTransaction {
    type Error = IndexError;

    fn try_from(value: TransactionDto) -> Result<Self, Self::Error> {
        let scope: IndexScope = value.scope.try_into()?;
        let chain = scope.chain.clone();
        validate_transaction(&value.transaction_id, "transaction_id")?;
        let movements = value
            .movements
            .into_iter()
            .map(|movement| movement.into_value(&scope.chain))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            transaction_id: CanonicalTransactionId {
                chain: scope.chain.clone(),
                value: value.transaction_id,
            },
            scope,
            revision: ObservationRevision(parse_decimal(&value.revision, "revision")?),
            status: value.status.try_into()?,
            movements,
            fee: value.fee.map(|fee| fee.into_value(chain)).transpose()?,
            first_seen_at: parse_decimal(&value.first_seen_at, "first_seen_at")?,
            observed_at: parse_decimal(&value.observed_at, "observed_at")?,
        })
    }
}

#[derive(Deserialize)]
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

impl TryFrom<TransactionStatusDto> for TransactionStatus {
    type Error = IndexError;

    fn try_from(value: TransactionStatusDto) -> Result<Self, Self::Error> {
        match value {
            TransactionStatusDto::Pending | TransactionStatusDto::Dropped => Err(protocol_error(
                "v1 PS ingestion rejects non-canonical mempool status",
            )),
            TransactionStatusDto::Replaced { by } => {
                validate_transaction(&by, "replacement transaction")?;
                Err(protocol_error(
                    "v1 PS ingestion rejects non-canonical mempool status",
                ))
            }
            TransactionStatusDto::Included {
                block,
                confirmations,
            } => Ok(Self::Included {
                block: block.try_into()?,
                confirmations: parse_decimal(&confirmations, "confirmations")?,
            }),
            TransactionStatusDto::Confirmed { block, proof } => Ok(Self::Confirmed {
                block: block.try_into()?,
                proof: proof.try_into()?,
            }),
            TransactionStatusDto::Failed { block, reason } => Ok(Self::Failed {
                block: block.map(BlockDto::try_into).transpose()?,
                reason,
            }),
            TransactionStatusDto::Reorged { previous_block } => Ok(Self::Reorged {
                previous_block: previous_block.try_into()?,
            }),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ConfirmationProofDto {
    Depth { required: String, observed: String },
    ChainFinalized,
    DepthAndChainFinalized { required: String, observed: String },
}

impl TryFrom<ConfirmationProofDto> for ConfirmationProof {
    type Error = IndexError;

    fn try_from(value: ConfirmationProofDto) -> Result<Self, Self::Error> {
        match value {
            ConfirmationProofDto::Depth { required, observed } => Ok(Self::Depth {
                required: parse_decimal(&required, "proof.required")?,
                observed: parse_decimal(&observed, "proof.observed")?,
            }),
            ConfirmationProofDto::ChainFinalized => Ok(Self::ChainFinalized),
            ConfirmationProofDto::DepthAndChainFinalized { required, observed } => {
                Ok(Self::DepthAndChainFinalized {
                    required: parse_decimal(&required, "proof.required")?,
                    observed: parse_decimal(&observed, "proof.observed")?,
                })
            }
        }
    }
}

#[derive(Deserialize)]
struct MovementDto {
    id: String,
    asset: String,
    amount: String,
    from: Option<String>,
    to: Option<String>,
    kind: String,
}

impl MovementDto {
    fn into_value(self, chain: &ChainId) -> Result<ValueMovement, IndexError> {
        if self.id.trim().is_empty() || self.asset.trim().is_empty() {
            return Err(protocol_error(
                "Indexer movement contains an empty ID or asset",
            ));
        }
        let kind = match self.kind.as_str() {
            "transfer" => MovementKind::Transfer,
            "input" => MovementKind::Input,
            "output" => MovementKind::Output,
            "mint" => MovementKind::Mint,
            "burn" => MovementKind::Burn,
            "internal_transfer" => {
                return Err(protocol_error(
                    "v1 PS ingestion rejects trace-derived internal transfers",
                ));
            }
            _ => return Err(protocol_error("Indexer movement kind is unknown")),
        };
        Ok(ValueMovement {
            id: MovementId(self.id),
            asset: AssetId {
                chain: chain.clone(),
                asset: self.asset,
            },
            amount: parse_amount(&self.amount, "movement.amount")?,
            from: self
                .from
                .map(|address| canonical_address(chain, address, "movement.from"))
                .transpose()?,
            to: self
                .to
                .map(|address| canonical_address(chain, address, "movement.to"))
                .transpose()?,
            kind,
        })
    }
}

#[derive(Deserialize)]
struct FeeDto {
    asset: String,
    amount: String,
    payer: Option<String>,
}

impl FeeDto {
    fn into_value(self, chain: ChainId) -> Result<NetworkFee, IndexError> {
        if self.asset.trim().is_empty() {
            return Err(protocol_error("Indexer fee contains an empty asset"));
        }
        Ok(NetworkFee {
            asset: AssetId {
                chain: chain.clone(),
                asset: self.asset,
            },
            amount: parse_amount(&self.amount, "fee.amount")?,
            payer: self
                .payer
                .map(|address| canonical_address(&chain, address, "fee.payer"))
                .transpose()?,
        })
    }
}

fn canonical_address(
    chain: &ChainId,
    value: String,
    field: &str,
) -> Result<CanonicalAddress, IndexError> {
    validate_address(&value, field)?;
    Ok(CanonicalAddress {
        chain: chain.clone(),
        value,
    })
}

fn parse_phase(value: &str) -> Result<SyncPhase, IndexError> {
    match value {
        "starting" => Ok(SyncPhase::Starting),
        "reconciling" => Ok(SyncPhase::Reconciling),
        "catching_up" => Ok(SyncPhase::CatchingUp),
        "ready" => Ok(SyncPhase::Ready),
        "reverting" => Ok(SyncPhase::Reverting),
        "replaying" => Ok(SyncPhase::Replaying),
        "rebuild_required" => Ok(SyncPhase::RebuildRequired),
        "halted" => Ok(SyncPhase::Halted),
        _ => Err(protocol_error("Indexer returned an unknown sync phase")),
    }
}

fn parse_decimal(input: &str, field: &str) -> Result<u64, IndexError> {
    input
        .parse::<u64>()
        .map_err(|_| protocol_error(format!("Indexer {field} is not an unsigned decimal string")))
}

fn parse_amount(input: &str, field: &str) -> Result<AtomicAmount, IndexError> {
    parse_fixed::<32>(input, field).map(AtomicAmount)
}

fn validate_address(input: &str, field: &str) -> Result<(), IndexError> {
    parse_fixed::<20>(input, field).map(|_| ())
}

fn validate_transaction(input: &str, field: &str) -> Result<(), IndexError> {
    parse_fixed::<32>(input, field).map(|_| ())
}

fn parse_fixed<const N: usize>(input: &str, field: &str) -> Result<[u8; N], IndexError> {
    let hex = input
        .strip_prefix("0x")
        .ok_or_else(|| protocol_error(format!("Indexer {field} is missing its 0x prefix")))?;
    if hex.len() != N * 2 {
        return Err(protocol_error(format!(
            "Indexer {field} has an invalid byte length"
        )));
    }
    let mut result = [0_u8; N];
    for (index, byte) in result.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| protocol_error(format!("Indexer {field} contains invalid hexadecimal")))?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode as AxumStatusCode},
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    use super::*;

    fn options(endpoint: String, token: Option<&str>) -> IndexerOptions {
        IndexerOptions {
            indexer_url: endpoint.parse().expect("test endpoint must parse"),
            network: "test".to_owned(),
            bearer_token: token.map(|value| value.parse().expect("token must parse")),
            request_timeout_seconds: 2,
            retry_attempts: 3,
            retry_initial_millis: 1,
            retry_max_millis: 2,
        }
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("ethereum".to_owned()),
            network: "test".to_owned(),
        }
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

    #[tokio::test]
    async fn status_watch_and_event_dtos_match_indexer_wire_contract() {
        async fn status(headers: HeaderMap) -> (AxumStatusCode, Json<Value>) {
            assert_eq!(
                headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer test-secret")
            );
            (
                AxumStatusCode::OK,
                Json(json!({
                    "scope": {"chain": "ethereum", "network": "test"},
                    "phase": "ready",
                    "checkpoint": block_json(42),
                    "observed_tip": block_json(43),
                    "confirmation_depth": "12",
                    "rebuild_reason": null,
                    "halted_reason": null
                })),
            )
        }

        async fn watch(Json(body): Json<Value>) -> (AxumStatusCode, Json<Value>) {
            assert_eq!(body["start_height"], "42");
            assert_eq!(body["idempotency_key"], "ps-deposit:create-1");
            assert_eq!(body["selector"]["type"], "address");
            (
                AxumStatusCode::OK,
                Json(json!({
                    "id": "watch-1",
                    "scope": {"chain": "ethereum", "network": "test"},
                    "selector": {
                        "type": "address",
                        "value": "0x1111111111111111111111111111111111111111"
                    },
                    "start_height": "42",
                    "registered_at": block_json(42),
                    "inactive_from": null,
                    "confirmation_depth": "12"
                })),
            )
        }

        async fn events() -> (AxumStatusCode, Json<Value>) {
            (
                AxumStatusCode::OK,
                Json(json!({
                    "events": [event_json()],
                    "next_cursor": null
                })),
            )
        }

        let endpoint = spawn(
            Router::new()
                .route("/v1/scopes/ethereum/test/status", get(status))
                .route("/v1/scopes/ethereum/test/watches", post(watch))
                .route("/v1/events", get(events)),
        )
        .await;
        let client =
            IndexerClient::new(&options(endpoint, Some("test-secret"))).expect("client must build");

        let status = client.status(&scope()).await.expect("status must decode");
        assert_eq!(status.phase, SyncPhase::Ready);
        assert_eq!(
            status.checkpoint.expect("checkpoint must exist").height,
            BlockHeight(42)
        );

        let receipt = client
            .watch(WatchRequest {
                scope: scope(),
                selector: WatchSelector::Address(CanonicalAddress {
                    chain: ChainId("ethereum".to_owned()),
                    value: "0x1111111111111111111111111111111111111111".to_owned(),
                }),
                start_height: BlockHeight(42),
                idempotency_key: "ps-deposit:create-1".to_owned(),
            })
            .await
            .expect("watch must decode");
        assert_eq!(receipt.id, WatchId("watch-1".to_owned()));

        let page = client
            .events(&scope(), None, 100)
            .await
            .expect("events must decode");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].cursor, EventCursor(1));
        assert_eq!(page.next_cursor, None);
    }

    #[tokio::test]
    async fn retry_is_bounded_and_client_debug_is_redacted() {
        async fn flaky(State(attempts): State<Arc<AtomicUsize>>) -> (AxumStatusCode, Json<Value>) {
            let attempt = attempts.fetch_add(1, Ordering::AcqRel);
            if attempt < 2 {
                return (
                    AxumStatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "code": "source_unavailable",
                        "message": "source unavailable",
                        "retryable": true,
                        "request_id": "test"
                    })),
                );
            }
            (
                AxumStatusCode::OK,
                Json(json!({
                    "scope": {"chain": "ethereum", "network": "test"},
                    "phase": "ready",
                    "checkpoint": block_json(42),
                    "observed_tip": block_json(42),
                    "confirmation_depth": "12",
                    "rebuild_reason": null,
                    "halted_reason": null
                })),
            )
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let endpoint = spawn(
            Router::new()
                .route("/v1/scopes/ethereum/test/status", get(flaky))
                .with_state(Arc::clone(&attempts)),
        )
        .await;
        let client = IndexerClient::new(&options(endpoint.clone(), Some("test-secret")))
            .expect("client must build");
        client
            .status(&scope())
            .await
            .expect("third attempt succeeds");
        assert_eq!(attempts.load(Ordering::Acquire), 3);

        let debug = format!("{client:?}");
        assert!(!debug.contains(&endpoint));
        assert!(!debug.contains("test-secret"));
    }

    #[tokio::test]
    async fn mempool_and_internal_trace_facts_are_rejected() {
        async fn pending() -> (AxumStatusCode, Json<Value>) {
            let mut event = event_json();
            event["transaction"]["status"] = json!({"kind": "pending"});
            event["transaction"]["movements"][0]["kind"] = json!("internal_transfer");
            (
                AxumStatusCode::OK,
                Json(json!({"events": [event], "next_cursor": null})),
            )
        }
        let endpoint = spawn(Router::new().route("/v1/events", get(pending))).await;
        let client = IndexerClient::new(&options(endpoint, None)).expect("client must build");
        let error = client
            .events(&scope(), None, 10)
            .await
            .expect_err("mempool facts must be rejected");
        assert!(!error.retryable);
    }

    fn block_json(height: u64) -> Value {
        json!({
            "height": height.to_string(),
            "hash": format!("0x{}", "11".repeat(32)),
            "parent_hash": format!("0x{}", "10".repeat(32)),
            "timestamp": "1000"
        })
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
                    "block": block_json(42),
                    "confirmations": "1"
                },
                "movements": [{
                    "id": "native:0",
                    "asset": "native",
                    "amount": format!("0x{}", "00".repeat(32)),
                    "from": "0x1111111111111111111111111111111111111111",
                    "to": "0x2222222222222222222222222222222222222222",
                    "kind": "transfer"
                }],
                "fee": {
                    "asset": "native",
                    "amount": format!("0x{}", "00".repeat(32)),
                    "payer": "0x1111111111111111111111111111111111111111"
                },
                "first_seen_at": "1000",
                "observed_at": "1001"
            }
        })
    }
}
