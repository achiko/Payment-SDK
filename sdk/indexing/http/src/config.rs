use std::{error::Error, fmt, num::NonZeroU32, time::Duration};

use http::client::Retry;
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigErrorKind {
    MissingEndpoint,
    InvalidEndpoint,
    InvalidToken,
    InvalidTimeout,
    InvalidResponseLimit,
    InvalidRetry,
    HttpClient,
}

#[derive(Debug)]
pub struct ConfigError {
    pub kind: ConfigErrorKind,
    pub message: String,
}

impl ConfigError {
    pub(crate) fn new(kind: ConfigErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ConfigError {}

/// Connection policy for a remote Indexer Service.
///
/// Endpoints are tried in declaration order. The generic HTTP client applies
/// its retry policy to each endpoint before the adapter fails over to the next.
/// Credentials are deliberately omitted from `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub endpoints: Vec<String>,
    pub bearer_token: Option<String>,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
    pub retry_policy: Retry,
}

impl Config {
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoints: vec![endpoint.into()],
            bearer_token: None,
            request_timeout: Duration::from_secs(15),
            max_response_bytes: 16 * 1024 * 1024,
            retry_policy: Retry::default(),
        }
    }

    #[must_use]
    pub fn with_endpoints(endpoints: impl IntoIterator<Item = String>) -> Self {
        Self {
            endpoints: endpoints.into_iter().collect(),
            bearer_token: None,
            request_timeout: Duration::from_secs(15),
            max_response_bytes: 16 * 1024 * 1024,
            retry_policy: Retry::default(),
        }
    }

    pub fn validate(&self) -> Result<Vec<Url>, ConfigError> {
        if self.endpoints.is_empty() {
            return Err(ConfigError::new(
                ConfigErrorKind::MissingEndpoint,
                "at least one Indexer endpoint is required",
            ));
        }
        if self.request_timeout.is_zero() {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidTimeout,
                "Indexer request timeout must be greater than zero",
            ));
        }
        if self.max_response_bytes == 0 {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidResponseLimit,
                "Indexer response limit must be greater than zero",
            ));
        }
        validate_token(self.bearer_token.as_deref())?;
        self.endpoints
            .iter()
            .map(|endpoint| normalize(endpoint))
            .collect()
    }

    pub fn retry(
        &mut self,
        attempts: NonZeroU32,
        initial: Duration,
        maximum: Duration,
    ) -> Result<(), ConfigError> {
        self.retry_policy = Retry::new(attempts, initial, maximum).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::InvalidRetry,
                "Indexer retry policy is invalid",
            )
        })?;
        Ok(())
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("endpoint_count", &self.endpoints.len())
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("request_timeout", &self.request_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

fn normalize(endpoint: &str) -> Result<Url, ConfigError> {
    let mut url = Url::parse(endpoint).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::InvalidEndpoint,
            "Indexer endpoint must be an absolute HTTP URL",
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
        return Err(ConfigError::new(
            ConfigErrorKind::InvalidEndpoint,
            "Indexer endpoint must use HTTP or HTTPS",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigError::new(
            ConfigErrorKind::InvalidEndpoint,
            "Indexer endpoint must not contain a query or fragment",
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn validate_token(token: Option<&str>) -> Result<(), ConfigError> {
    if token.is_some_and(|value| {
        value.is_empty()
            || value
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    }) {
        return Err(ConfigError::new(
            ConfigErrorKind::InvalidToken,
            "bearer token must be non-empty and contain no whitespace or control characters",
        ));
    }
    Ok(())
}
