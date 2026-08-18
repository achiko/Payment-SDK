use std::{error::Error as StdError, fmt, time::Duration};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::{BoxFuture, Client, Error, ErrorKind, Request, Response, Retry};

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub endpoint: String,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
    pub default_headers: Vec<(String, String)>,
    pub retry_policy: Retry,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self
            .default_headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        formatter
            .debug_struct("Config")
            .field("endpoint", &"[REDACTED]")
            .field("request_timeout", &self.request_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("default_header_names", &header_names)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

impl Config {
    #[must_use]
    pub fn new(endpoint: impl Into<String>, request_timeout: Duration) -> Self {
        Self {
            endpoint: endpoint.into(),
            request_timeout,
            max_response_bytes: 64 * 1024 * 1024,
            default_headers: Vec::new(),
            retry_policy: Retry::default(),
        }
    }
}

#[derive(Clone)]
pub struct Reqwest {
    config: Config,
    client: reqwest::Client,
}

impl fmt::Debug for Reqwest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Reqwest")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Reqwest {
    pub fn new(config: Config) -> Result<Self, BuildError> {
        if config.endpoint.trim().is_empty() {
            return Err(BuildError::new(
                BuildErrorKind::InvalidEndpoint,
                "HTTP endpoint must not be empty",
            ));
        }
        reqwest::Url::parse(&config.endpoint).map_err(|error| {
            BuildError::with_source(
                BuildErrorKind::InvalidEndpoint,
                "HTTP endpoint is invalid",
                error,
            )
        })?;
        if config.request_timeout.is_zero() {
            return Err(BuildError::new(
                BuildErrorKind::InvalidTimeout,
                "HTTP request timeout must be greater than zero",
            ));
        }
        if config.max_response_bytes == 0 {
            return Err(BuildError::new(
                BuildErrorKind::InvalidResponseLimit,
                "HTTP maximum response size must be greater than zero",
            ));
        }

        let client = reqwest::Client::builder()
            // RPC POST bodies and credentials must never be replayed to a
            // provider-selected redirect target.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                BuildError::with_source(
                    BuildErrorKind::Client,
                    "failed to construct HTTP client",
                    error,
                )
            })?;
        Ok(Self { config, client })
    }

    async fn send_once(&self, request: &Request) -> Result<Response, Error> {
        let endpoint = if request.endpoint.is_empty() {
            &self.config.endpoint
        } else {
            &request.endpoint
        };
        let mut headers = HeaderMap::new();
        for (name, value) in self
            .config
            .default_headers
            .iter()
            .chain(request.headers.iter())
        {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error {
                kind: ErrorKind::Rejected,
                message: "HTTP request contains an invalid header name".to_owned(),
            })?;
            let value = HeaderValue::from_str(value).map_err(|_| Error {
                kind: ErrorKind::Rejected,
                message: "HTTP request contains an invalid header value".to_owned(),
            })?;
            headers.insert(name, value);
        }

        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| Error {
            kind: ErrorKind::Rejected,
            message: "HTTP request contains an invalid method".to_owned(),
        })?;
        let mut response = self
            .client
            .request(method, endpoint)
            .headers(headers)
            .timeout(self.config.request_timeout)
            .body(request.body.clone())
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status().as_u16();
        let mut body =
            ResponseBody::new(self.config.max_response_bytes, response.content_length())?;
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
            body.push_chunk(&chunk)?;
        }

        Ok(Response {
            status,
            headers,
            body: body.into_bytes(),
        })
    }
}

/// Incremental response collector shared by declared-length and chunked HTTP
/// responses. Errors deliberately contain no response bytes.
pub(crate) struct ResponseBody {
    bytes: Vec<u8>,
    maximum: usize,
}

impl ResponseBody {
    pub(crate) fn new(maximum: usize, declared_length: Option<u64>) -> Result<Self, Error> {
        if declared_length
            .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > maximum))
        {
            return Err(response_size_limit_error());
        }
        Ok(Self {
            bytes: Vec::new(),
            maximum,
        })
    }

    pub(crate) fn push_chunk(&mut self, chunk: &[u8]) -> Result<(), Error> {
        let next_length = self
            .bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(response_size_overflow_error)?;
        if next_length > self.maximum {
            return Err(response_size_limit_error());
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

fn response_size_limit_error() -> Error {
    Error {
        kind: ErrorKind::InvalidResponse,
        message: "HTTP response exceeds the configured size limit".to_owned(),
    }
}

fn response_size_overflow_error() -> Error {
    Error {
        kind: ErrorKind::InvalidResponse,
        message: "HTTP response size overflowed".to_owned(),
    }
}

impl Client for Reqwest {
    fn execute<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(self.execute_retry(request))
    }
}

impl Reqwest {
    async fn execute_retry(&self, request: Request) -> Result<Response, Error> {
        let mut attempt = 1_u32;
        loop {
            let result = self.send_once(&request).await;
            if attempt >= self.config.retry_policy.max_attempts.get() || !retryable(&result) {
                return result;
            }
            tokio::time::sleep(self.config.retry_policy.backoff_after(attempt)).await;
            attempt += 1;
        }
    }
}

fn retryable(result: &Result<Response, Error>) -> bool {
    match result {
        Ok(response) => is_retryable_status(response.status),
        Err(error) => is_retryable_transport_error(error.kind),
    }
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

fn is_retryable_transport_error(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::Timeout | ErrorKind::Unavailable)
}

fn map_reqwest_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        ErrorKind::Timeout
    } else if error.is_connect() || error.is_request() {
        ErrorKind::Unavailable
    } else if error.is_decode() {
        ErrorKind::InvalidResponse
    } else {
        ErrorKind::Other
    };
    Error {
        kind,
        // Do not propagate reqwest's display text: it may contain a URL with credentials.
        message: match kind {
            ErrorKind::Timeout => "HTTP request timed out",
            ErrorKind::Unavailable => "HTTP endpoint is unavailable",
            ErrorKind::InvalidResponse => "HTTP response could not be decoded",
            _ => "HTTP request failed",
        }
        .to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildErrorKind {
    InvalidEndpoint,
    InvalidTimeout,
    InvalidResponseLimit,
    InvalidRetry,
    Client,
}

#[derive(Debug)]
pub struct BuildError {
    pub kind: BuildErrorKind,
    pub message: String,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl BuildError {
    pub(super) fn new(kind: BuildErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: BuildErrorKind,
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for BuildError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}
