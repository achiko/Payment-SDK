use std::sync::Arc;

use http::client::{Client, ErrorKind as HttpErrorKind, Request, Response};
use indexing::{
    BlockRef, BoxFuture, Checkpoint, EventPage, EventQuery, History, HistoryQuery, IndexError,
    IndexErrorKind, IndexScope, ObservedTransaction, Observer, OutputPage, OutputQuery,
    OutputRequest, TransactionPage, TransactionQuery, UnwatchOutcome, UnwatchRequest, WatchReceipt,
    WatchRequest, WatchSelector, Watcher,
};
use serde::{Serialize, de::DeserializeOwned};
use url::Url;

use crate::{
    Config, ConfigError, ConfigErrorKind,
    checkpoint::BlockDto,
    output::{OutputsDto, encode_cursor},
    wire::{
        ErrorDto, EventsDto, SelectorBody, TransactionDto, TransactionsDto, UnwatchDto, WatchBody,
        WatchDto, invalid_response,
    },
};

impl<C> Checkpoint for Remote<C>
where
    C: Client + 'static,
{
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            let response: BlockDto = self
                .json(RequestSpec {
                    method: "GET".to_owned(),
                    url: self.scoped_url(scope, &["checkpoint"])?,
                    body: Vec::new(),
                })
                .await?;
            Ok(Some(response.convert()?))
        })
    }
}

/// Chain-neutral remote implementation of the indexing consumer traits.
#[derive(Clone)]
pub struct Remote<C> {
    client: Arc<C>,
    endpoints: Arc<[Url]>,
    bearer_token: Option<Arc<str>>,
}

impl<C> Remote<C>
where
    C: Client,
{
    pub fn new(client: Arc<C>, config: &Config) -> Result<Self, ConfigError> {
        Ok(Self {
            client,
            endpoints: config.validate()?.into(),
            bearer_token: config.bearer_token.as_deref().map(Arc::from),
        })
    }

    async fn send(&self, method: &str, url: Url, body: Vec<u8>) -> Result<Response, IndexError> {
        let suffix = relative_suffix(&self.endpoints[0], &url)?;
        let mut last = None;
        for endpoint in self.endpoints.iter() {
            let target = endpoint.join(&suffix).map_err(|_| {
                IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "Indexer request URL could not be constructed",
                    false,
                )
            })?;
            let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
            if !body.is_empty() {
                headers.push(("content-type".to_owned(), "application/json".to_owned()));
            }
            if let Some(token) = &self.bearer_token {
                headers.push(("authorization".to_owned(), format!("Bearer {token}")));
            }
            let request = Request {
                method: method.to_owned(),
                endpoint: target.to_string(),
                headers,
                body: body.clone(),
            };
            match self.client.execute(request).await {
                Ok(response) if !retryable_status(response.status) => return Ok(response),
                Ok(response) => last = Some(Ok(response)),
                Err(error) if retryable_transport(error.kind) => last = Some(Err(error)),
                Err(error) => return Err(transport_error(error.kind)),
            }
        }
        match last {
            Some(Ok(response)) => Ok(response),
            Some(Err(error)) => Err(transport_error(error.kind)),
            None => Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "no Indexer endpoint is configured",
                false,
            )),
        }
    }

    async fn json<T>(&self, request: RequestSpec) -> Result<T, IndexError>
    where
        T: DeserializeOwned,
    {
        let response = self
            .send(&request.method, request.url, request.body)
            .await?;
        decode(response)
    }

    fn scoped_url(&self, scope: &IndexScope, segments: &[&str]) -> Result<Url, IndexError> {
        let mut url = self.endpoints[0].clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "Indexer endpoint cannot contain URL paths",
                    false,
                )
            })?;
            path.pop_if_empty()
                .push("v1")
                .push("scopes")
                .push(&scope.chain.0)
                .push(&scope.network);
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }
}

impl Remote<http::client::Reqwest> {
    pub fn connect(config: Config) -> Result<Self, ConfigError> {
        let endpoints = config.validate()?;
        let mut http_config =
            http::client::Config::new(endpoints[0].as_str(), config.request_timeout);
        http_config.max_response_bytes = config.max_response_bytes;
        http_config.retry_policy = config.retry_policy;
        let client = http::client::Reqwest::new(http_config).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::HttpClient,
                "failed to construct Indexer HTTP client",
            )
        })?;
        Self::new(Arc::new(client), &config)
    }
}

impl<C> Watcher for Remote<C>
where
    C: Client + 'static,
{
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            ensure_selector_scope(&request.scope, &request.selector)?;
            let selector = match &request.selector {
                WatchSelector::Address(value) => SelectorBody::Address(&value.value),
                WatchSelector::Transaction(value) => SelectorBody::Transaction(&value.value),
            };
            let body = encode(&WatchBody {
                selector,
                start_height: request.start_height.0.to_string(),
                idempotency_key: &request.idempotency_key,
            })?;
            let response: WatchDto = self
                .json(RequestSpec {
                    method: "POST".to_owned(),
                    url: self.scoped_url(&request.scope, &["watches"])?,
                    body,
                })
                .await?;
            let receipt = response.convert()?;
            ensure_scope(&request.scope, &receipt.scope)?;
            Ok(receipt)
        })
    }

    fn unwatch<'a>(
        &'a self,
        request: UnwatchRequest,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Box::pin(async move {
            let response: UnwatchDto = self
                .json(RequestSpec {
                    method: "DELETE".to_owned(),
                    url: self.scoped_url(&request.scope, &["watches", &request.watch_id.0])?,
                    body: Vec::new(),
                })
                .await?;
            response.convert()
        })
    }
}

impl<C> History for Remote<C>
where
    C: Client + 'static,
{
    fn transaction<'a>(
        &'a self,
        request: TransactionQuery,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        Box::pin(async move {
            ensure_transaction_scope(&request.scope, &request.transaction_id)?;
            let url = self.scoped_url(
                &request.scope,
                &["transactions", &request.transaction_id.value],
            )?;
            let response = self.send("GET", url, Vec::new()).await?;
            if response.status == 404 && has_error_code(&response, "transaction_not_found") {
                return Ok(None);
            }
            let transaction = decode::<TransactionDto>(response)?.convert()?;
            ensure_scope(&request.scope, &transaction.scope)?;
            Ok(Some(transaction))
        })
    }

    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        Box::pin(async move {
            ensure_address_scope(&request.scope, &request.address)?;
            if let Some(after) = &request.after {
                ensure_transaction_scope(&request.scope, after)?;
            }
            let mut url = self.scoped_url(
                &request.scope,
                &["addresses", &request.address.value, "transactions"],
            )?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", &request.limit.to_string());
                if let Some(after) = &request.after {
                    query.append_pair("after", &after.value);
                }
            }
            let response: TransactionsDto = self
                .json(RequestSpec {
                    method: "GET".to_owned(),
                    url,
                    body: Vec::new(),
                })
                .await?;
            let page = response.convert(&request.scope)?;
            for transaction in &page.transactions {
                ensure_scope(&request.scope, &transaction.scope)?;
            }
            Ok(page)
        })
    }
}

impl<C> Observer for Remote<C>
where
    C: Client + 'static,
{
    fn events<'a>(&'a self, request: EventQuery) -> BoxFuture<'a, Result<EventPage, IndexError>> {
        Box::pin(async move {
            let mut url = self.scoped_url(&request.scope, &["events"])?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", &request.limit.to_string());
                if let Some(after) = request.after {
                    query.append_pair("after_cursor", &after.0.to_string());
                }
            }
            let response: EventsDto = self
                .json(RequestSpec {
                    method: "GET".to_owned(),
                    url,
                    body: Vec::new(),
                })
                .await?;
            let page = response.convert()?;
            for event in &page.events {
                ensure_scope(&request.scope, &event.transaction.scope)?;
            }
            Ok(page)
        })
    }
}

impl<C> OutputQuery for Remote<C>
where
    C: Client + 'static,
{
    fn outputs<'a>(
        &'a self,
        request: OutputRequest,
    ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move {
            ensure_address_scope(&request.scope, &request.address)?;
            let mut url = self.scoped_url(
                &request.scope,
                &["addresses", &request.address.value, "outputs"],
            )?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", &request.limit.to_string());
                if let Some(after) = &request.after {
                    query.append_pair("after", &encode_cursor(after));
                }
            }
            let response: OutputsDto = self
                .json(RequestSpec {
                    method: "GET".to_owned(),
                    url,
                    body: Vec::new(),
                })
                .await?;
            let page = response.convert(&request.scope, &request.address)?;
            if page
                .next
                .as_ref()
                .is_some_and(|next| next.snapshot != page.snapshot)
            {
                return Err(invalid_response(
                    "output continuation belongs to a different snapshot",
                ));
            }
            Ok(page)
        })
    }
}

struct RequestSpec {
    method: String,
    url: Url,
    body: Vec<u8>,
}

fn encode<T>(value: &T) -> Result<Vec<u8>, IndexError>
where
    T: Serialize,
{
    serde_json::to_vec(value).map_err(|_| invalid_response("request could not be encoded"))
}

fn decode<T>(response: Response) -> Result<T, IndexError>
where
    T: DeserializeOwned,
{
    if !(200..300).contains(&response.status) {
        return Err(response_error(response));
    }
    serde_json::from_slice(&response.body)
        .map_err(|_| invalid_response("Indexer response is not valid for this operation"))
}

fn response_error(response: Response) -> IndexError {
    let body = serde_json::from_slice::<ErrorDto>(&response.body).ok();
    let kind = body.as_ref().map_or_else(
        || status_kind(response.status),
        |error| code_kind(&error.code),
    );
    let retryable = body
        .as_ref()
        .map_or(matches!(response.status, 429 | 502 | 503 | 504), |error| {
            error.retryable
        });
    let message = body.map_or_else(
        || {
            format!(
                "Indexer request failed with HTTP status {}",
                response.status
            )
        },
        |error| error.message,
    );
    IndexError::new(kind, message, retryable)
}

fn has_error_code(response: &Response, expected: &str) -> bool {
    serde_json::from_slice::<ErrorDto>(&response.body).is_ok_and(|error| error.code == expected)
}

fn code_kind(code: &str) -> IndexErrorKind {
    match code {
        "conflict" => IndexErrorKind::Conflict,
        "policy_mismatch" => IndexErrorKind::PolicyMismatch,
        "scope_not_found" => IndexErrorKind::ScopeMismatch,
        "invalid_watch" => IndexErrorKind::InvalidWatch,
        "invalid_json"
        | "invalid_query"
        | "invalid_request"
        | "invalid_start_height"
        | "invalid_idempotency_key"
        | "invalid_page_size"
        | "invalid_address"
        | "invalid_transaction_hash"
        | "unsupported_address" => IndexErrorKind::InvalidRequest,
        "rebuild_required" => IndexErrorKind::RebuildRequired,
        "indexer_halted" => IndexErrorKind::Halted,
        "source_unavailable" => IndexErrorKind::Source,
        "storage_unavailable" => IndexErrorKind::Store,
        _ => IndexErrorKind::Other,
    }
}

fn status_kind(status: u16) -> IndexErrorKind {
    match status {
        400 | 401 | 403 | 405 | 422 => IndexErrorKind::InvalidRequest,
        404 => IndexErrorKind::ScopeMismatch,
        409 => IndexErrorKind::Conflict,
        429 | 502..=504 => IndexErrorKind::CannotConnect,
        _ => IndexErrorKind::Other,
    }
}

fn transport_error(kind: HttpErrorKind) -> IndexError {
    IndexError::new(
        IndexErrorKind::CannotConnect,
        match kind {
            HttpErrorKind::Timeout => "Indexer request timed out",
            _ => "Indexer endpoint is unavailable",
        },
        retryable_transport(kind),
    )
}

fn retryable_transport(kind: HttpErrorKind) -> bool {
    matches!(kind, HttpErrorKind::Timeout | HttpErrorKind::Unavailable)
}

fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

fn ensure_scope(expected: &IndexScope, actual: &IndexScope) -> Result<(), IndexError> {
    if expected == actual {
        Ok(())
    } else {
        Err(invalid_response(
            "Indexer response belongs to a different chain or network scope",
        ))
    }
}

fn ensure_address_scope(
    scope: &IndexScope,
    address: &indexing::CanonicalAddress,
) -> Result<(), IndexError> {
    ensure_identity_scope(scope, &address.scope, "address")
}

fn ensure_transaction_scope(
    scope: &IndexScope,
    transaction: &indexing::TransactionRef,
) -> Result<(), IndexError> {
    ensure_identity_scope(scope, &transaction.scope, "transaction")
}

fn ensure_selector_scope(scope: &IndexScope, selector: &WatchSelector) -> Result<(), IndexError> {
    match selector {
        WatchSelector::Address(address) => ensure_address_scope(scope, address),
        WatchSelector::Transaction(transaction) => ensure_transaction_scope(scope, transaction),
    }
}

fn ensure_identity_scope(
    scope: &IndexScope,
    identity_scope: &IndexScope,
    identity: &str,
) -> Result<(), IndexError> {
    if identity_scope == scope {
        return Ok(());
    }
    Err(IndexError::new(
        IndexErrorKind::ScopeMismatch,
        format!("{identity} belongs to a different chain or network scope"),
        false,
    ))
}

fn relative_suffix(base: &Url, target: &Url) -> Result<String, IndexError> {
    let base_path = base.path();
    let path = target.path().strip_prefix(base_path).ok_or_else(|| {
        IndexError::new(
            IndexErrorKind::InvalidRequest,
            "Indexer request path is outside the configured endpoint",
            false,
        )
    })?;
    Ok(match target.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    })
}
