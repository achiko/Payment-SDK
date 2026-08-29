use std::{fmt, num::NonZeroU32, time::Duration};

use jsonrpsee::{
    core::{
        client::{ClientT, Error as RpcError},
        http_helpers::HttpError,
        params::BatchRequestBuilder,
        traits::ToRpcParams,
    },
    http_client::{HeaderMap, HeaderValue, HttpClient, HttpClientBuilder, transport},
    types::ErrorObjectOwned,
};
use serde_json::{Value, value::RawValue};

use crate::{BoxFuture, Call, CallResult, Client, Error, ErrorKind, Failure, RawJson};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    max_attempts: NonZeroU32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl Retry {
    pub fn new(
        max_attempts: NonZeroU32,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> std::result::Result<Self, Error> {
        if initial_backoff > max_backoff {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "initial retry backoff must not exceed its maximum",
            ));
        }
        Ok(Self {
            max_attempts,
            initial_backoff,
            max_backoff,
        })
    }

    #[must_use]
    pub const fn no_retry() -> Self {
        Self {
            max_attempts: NonZeroU32::MIN,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
        }
    }

    fn backoff(self, attempt: u32) -> Duration {
        let multiplier = 1_u32 << attempt.saturating_sub(1).min(31);
        self.initial_backoff
            .checked_mul(multiplier)
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff)
    }
}

impl Default for Retry {
    fn default() -> Self {
        Self::no_retry()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub endpoints: Vec<String>,
    pub request_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub headers: Vec<(String, String)>,
    pub retry: Retry,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names = self
            .headers
            .iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        formatter
            .debug_struct("Config")
            .field("endpoint_count", &self.endpoints.len())
            .field("request_timeout", &self.request_timeout)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("header_names", &header_names)
            .field("retry", &self.retry)
            .finish()
    }
}

impl Config {
    #[must_use]
    pub fn new(endpoint: impl Into<String>, request_timeout: Duration) -> Self {
        Self {
            endpoints: vec![endpoint.into()],
            request_timeout,
            max_request_bytes: 10 * 1024 * 1024,
            max_response_bytes: 64 * 1024 * 1024,
            headers: Vec::new(),
            retry: Retry::default(),
        }
    }
}

#[derive(Clone)]
pub struct Http {
    clients: Vec<HttpClient>,
    retry: Retry,
    header_names: Vec<String>,
}

impl fmt::Debug for Http {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Http")
            .field("endpoint_count", &self.clients.len())
            .field("header_names", &self.header_names)
            .field("retry", &self.retry)
            .finish()
    }
}

impl Http {
    pub fn new(config: Config) -> std::result::Result<Self, Error> {
        validate(&config)?;
        let headers = headers(&config.headers)?;
        let max_request = u32::try_from(config.max_request_bytes).map_err(|_| invalid_limit())?;
        let max_response = u32::try_from(config.max_response_bytes).map_err(|_| invalid_limit())?;
        let clients = config
            .endpoints
            .iter()
            .map(|endpoint| {
                HttpClientBuilder::default()
                    .request_timeout(config.request_timeout)
                    .max_request_size(max_request)
                    .max_response_size(max_response)
                    .set_headers(headers.clone())
                    .build(endpoint)
                    .map_err(map_error)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self {
            clients,
            retry: config.retry,
            header_names: config.headers.into_iter().map(|(name, _)| name).collect(),
        })
    }
}

impl Client for Http {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, std::result::Result<CallResult, Error>> {
        Box::pin(async move {
            let params = Params::new(params)?;
            let mut last = None;
            for attempt in 1..=self.retry.max_attempts.get() {
                for client in &self.clients {
                    match client
                        .request::<Box<RawValue>, _>(method, params.clone())
                        .await
                    {
                        Ok(value) => return Ok(Ok(RawJson(value.get().as_bytes().to_vec()))),
                        Err(RpcError::Call(error)) => return Ok(Err(failure(error))),
                        Err(source) => {
                            let error = map_error(source);
                            if !error.is_retryable() {
                                return Err(error);
                            }
                            last = Some(error);
                        }
                    }
                }
                if attempt < self.retry.max_attempts.get() {
                    tokio::time::sleep(self.retry.backoff(attempt)).await;
                }
            }
            Err(last.unwrap_or_else(|| {
                Error::new(ErrorKind::Unavailable, "JSON-RPC endpoints are unavailable")
            }))
        })
    }

    fn request_once<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, std::result::Result<CallResult, Error>> {
        Box::pin(async move {
            let params = Params::new(params)?;
            let client = self.clients.first().ok_or_else(|| {
                Error::new(ErrorKind::Unavailable, "JSON-RPC endpoint is unavailable")
            })?;
            match client.request::<Box<RawValue>, _>(method, params).await {
                Ok(value) => Ok(Ok(RawJson(value.get().as_bytes().to_vec()))),
                Err(RpcError::Call(error)) => Ok(Err(failure(error))),
                Err(source) => Err(map_error(source)),
            }
        })
    }

    fn batch<'a>(
        &'a self,
        calls: Vec<Call>,
    ) -> BoxFuture<'a, std::result::Result<Vec<CallResult>, Error>> {
        Box::pin(async move {
            if calls.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidRequest,
                    "JSON-RPC batch must not be empty",
                ));
            }
            let build = || {
                let mut batch = BatchRequestBuilder::new();
                for call in &calls {
                    batch
                        .insert(call.method.as_str(), Params::new(call.params.clone())?)
                        .map_err(|_| {
                            Error::new(
                                ErrorKind::InvalidRequest,
                                "JSON-RPC parameters could not be serialized",
                            )
                        })?;
                }
                Ok(batch)
            };
            let mut last = None;
            for attempt in 1..=self.retry.max_attempts.get() {
                for client in &self.clients {
                    let batch = build()?;
                    match client.batch_request::<Box<RawValue>>(batch).await {
                        Ok(responses) => {
                            return Ok(responses
                                .into_iter()
                                .map(|entry| match entry {
                                    Ok(value) => Ok(RawJson(value.get().as_bytes().to_vec())),
                                    Err(error) => Err(failure(error.into_owned())),
                                })
                                .collect());
                        }
                        Err(source) => {
                            let error = map_error(source);
                            if !error.is_retryable() {
                                return Err(error);
                            }
                            last = Some(error);
                        }
                    }
                }
                if attempt < self.retry.max_attempts.get() {
                    tokio::time::sleep(self.retry.backoff(attempt)).await;
                }
            }
            Err(last.unwrap_or_else(|| {
                Error::new(ErrorKind::Unavailable, "JSON-RPC endpoints are unavailable")
            }))
        })
    }
}

#[derive(Clone)]
struct Params(Option<Box<RawValue>>);

impl Params {
    fn new(value: Value) -> std::result::Result<Self, Error> {
        if !matches!(value, Value::Array(_) | Value::Object(_)) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC parameters must be an array or object",
            ));
        }
        serde_json::value::to_raw_value(&value)
            .map(Some)
            .map(Self)
            .map_err(|_| {
                Error::new(
                    ErrorKind::InvalidRequest,
                    "JSON-RPC parameters could not be serialized",
                )
            })
    }
}

impl ToRpcParams for Params {
    fn to_rpc_params(self) -> std::result::Result<Option<Box<RawValue>>, serde_json::Error> {
        Ok(self.0)
    }
}

fn failure(error: ErrorObjectOwned) -> Failure {
    Failure {
        code: error.code() as i64,
        message: error.message().to_owned(),
        data: error
            .data()
            .map(|data| RawJson(data.get().as_bytes().to_vec())),
    }
}

fn map_error(error: RpcError) -> Error {
    match error {
        RpcError::Call(error) => Error::new(
            ErrorKind::InvalidResponse,
            format!("JSON-RPC call failed with code {}", error.code()),
        ),
        RpcError::RequestTimeout => Error::new(ErrorKind::Timeout, "JSON-RPC request timed out"),
        RpcError::Transport(source) => {
            if let Some(error) = source.downcast_ref::<transport::Error>() {
                return match error {
                    transport::Error::Rejected { status_code } => Error::new(
                        ErrorKind::HttpStatus(*status_code),
                        "JSON-RPC endpoint rejected the request",
                    ),
                    transport::Error::Http(HttpError::TooLarge | HttpError::Malformed) => {
                        Error::new(
                            ErrorKind::InvalidResponse,
                            "JSON-RPC endpoint returned an invalid response",
                        )
                    }
                    _ => Error::new(ErrorKind::Unavailable, "JSON-RPC transport is unavailable"),
                };
            }
            Error::new(ErrorKind::Unavailable, "JSON-RPC transport is unavailable")
        }
        RpcError::ParseError(_) | RpcError::InvalidRequestId(_) => Error::new(
            ErrorKind::InvalidResponse,
            "JSON-RPC endpoint returned an invalid response",
        ),
        _ => Error::new(ErrorKind::InvalidResponse, "JSON-RPC request failed"),
    }
}

fn headers(values: &[(String, String)]) -> std::result::Result<HeaderMap, Error> {
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        let name = name.parse::<http_types::HeaderName>().map_err(|_| {
            Error::new(
                ErrorKind::InvalidConfiguration,
                "JSON-RPC header name is invalid",
            )
        })?;
        let value = HeaderValue::from_str(value).map_err(|_| {
            Error::new(
                ErrorKind::InvalidConfiguration,
                "JSON-RPC header value is invalid",
            )
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn validate(config: &Config) -> std::result::Result<(), Error> {
    if config.endpoints.is_empty() || config.endpoints.iter().any(|value| value.trim().is_empty()) {
        return Err(Error::new(
            ErrorKind::InvalidConfiguration,
            "JSON-RPC requires at least one endpoint",
        ));
    }
    if config.request_timeout.is_zero()
        || config.max_request_bytes == 0
        || config.max_response_bytes == 0
    {
        return Err(Error::new(
            ErrorKind::InvalidConfiguration,
            "JSON-RPC bounds must be greater than zero",
        ));
    }
    Ok(())
}

fn invalid_limit() -> Error {
    Error::new(
        ErrorKind::InvalidConfiguration,
        "JSON-RPC size limit exceeds the supported range",
    )
}
