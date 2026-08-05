use reqwest::Url;
use std::{error::Error, fmt, net::IpAddr, num::NonZeroU32, str::FromStr, time::Duration};
use zeroize::Zeroize;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;
const DEFAULT_RETRY_ATTEMPTS: NonZeroU32 = NonZeroU32::new(3).expect("three is non-zero");
const DEFAULT_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const DEFAULT_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(1);

/// Validated base URL for the remote custody service.
///
/// Plain HTTP is accepted only for loopback hosts. Non-loopback endpoints must
/// use HTTPS so bearer credentials are not sent over plaintext transport.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteSignerEndpoint(Url);

impl RemoteSignerEndpoint {
    pub fn new(input: impl AsRef<str>) -> Result<Self, RemoteSignerConfigError> {
        let mut url = Url::parse(input.as_ref()).map_err(|_| {
            RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidEndpoint,
                "remote signer endpoint is not a valid URL",
            )
        })?;
        if url.cannot_be_a_base() || url.host_str().is_none() {
            return Err(RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidEndpoint,
                "remote signer endpoint must be an absolute hierarchical URL",
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidEndpoint,
                "remote signer endpoint must not contain credentials",
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidEndpoint,
                "remote signer endpoint must not contain a query or fragment",
            ));
        }
        match url.scheme() {
            "https" => {}
            "http" if is_loopback_host(&url) => {}
            "http" => {
                return Err(RemoteSignerConfigError::new(
                    RemoteSignerConfigErrorKind::InsecureEndpoint,
                    "non-loopback remote signer endpoints require HTTPS",
                ));
            }
            _ => {
                return Err(RemoteSignerConfigError::new(
                    RemoteSignerConfigErrorKind::InvalidEndpoint,
                    "remote signer endpoint must use HTTP or HTTPS",
                ));
            }
        }

        let normalized_path = format!("{}/", url.path().trim_end_matches('/'));
        url.set_path(&normalized_path);
        Ok(Self(url))
    }

    pub(crate) fn route(&self, relative_path: &str) -> Result<Url, RemoteSignerConfigError> {
        self.0.join(relative_path).map_err(|_| {
            RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidEndpoint,
                "remote signer route could not be constructed",
            )
        })
    }
}

impl FromStr for RemoteSignerEndpoint {
    type Err = RemoteSignerConfigError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::new(input)
    }
}

impl fmt::Debug for RemoteSignerEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteSignerEndpoint([REDACTED])")
    }
}

fn is_loopback_host(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

/// Bearer credential that clears its owned bytes on drop and never exposes
/// them through `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerSecret(String);

impl BearerSecret {
    pub fn new(input: impl Into<String>) -> Result<Self, RemoteSignerConfigError> {
        let value = input.into();
        if value.is_empty()
            || value
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidBearerSecret,
                "remote signer bearer credential must be non-empty and contain no whitespace",
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for BearerSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for BearerSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerSecret([REDACTED])")
    }
}

/// Retry bounds applied only to provision and sign requests carrying an
/// operation ID. Lookup, readiness, and capability requests are never retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRetryPolicy {
    pub(crate) max_attempts: NonZeroU32,
    pub(crate) initial_backoff: Duration,
    pub(crate) max_backoff: Duration,
}

impl RemoteRetryPolicy {
    pub fn new(
        max_attempts: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Result<Self, RemoteSignerConfigError> {
        let max_attempts = NonZeroU32::new(max_attempts).ok_or_else(|| {
            RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidRetryPolicy,
                "remote signer retry attempts must be greater than zero",
            )
        })?;
        if initial_backoff.is_zero() || max_backoff.is_zero() || initial_backoff > max_backoff {
            return Err(RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidRetryPolicy,
                "remote signer retry delays must be non-zero and ordered",
            ));
        }
        Ok(Self {
            max_attempts,
            initial_backoff,
            max_backoff,
        })
    }

    pub(crate) fn backoff_after(&self, failed_attempt: u32) -> Duration {
        let exponent = failed_attempt.saturating_sub(1).min(31);
        let multiplier = 1_u32 << exponent;
        self.initial_backoff
            .checked_mul(multiplier)
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff)
    }
}

impl Default for RemoteRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_RETRY_ATTEMPTS,
            initial_backoff: DEFAULT_RETRY_INITIAL_BACKOFF,
            max_backoff: DEFAULT_RETRY_MAX_BACKOFF,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RemoteSignerConfig {
    pub(crate) endpoint: RemoteSignerEndpoint,
    pub(crate) bearer_secret: BearerSecret,
    pub(crate) connect_timeout: Duration,
    pub(crate) request_timeout: Duration,
    pub(crate) max_response_bytes: usize,
    pub(crate) retry_policy: RemoteRetryPolicy,
}

impl RemoteSignerConfig {
    #[must_use]
    pub fn new(endpoint: RemoteSignerEndpoint, bearer_secret: BearerSecret) -> Self {
        Self {
            endpoint,
            bearer_secret,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            retry_policy: RemoteRetryPolicy::default(),
        }
    }

    pub fn with_timeouts(
        mut self,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, RemoteSignerConfigError> {
        if connect_timeout.is_zero() || request_timeout.is_zero() {
            return Err(RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidTimeout,
                "remote signer timeouts must be greater than zero",
            ));
        }
        self.connect_timeout = connect_timeout;
        self.request_timeout = request_timeout;
        Ok(self)
    }

    pub fn with_max_response_bytes(
        mut self,
        max_response_bytes: usize,
    ) -> Result<Self, RemoteSignerConfigError> {
        if max_response_bytes == 0 {
            return Err(RemoteSignerConfigError::new(
                RemoteSignerConfigErrorKind::InvalidResponseLimit,
                "remote signer response limit must be greater than zero",
            ));
        }
        self.max_response_bytes = max_response_bytes;
        Ok(self)
    }

    #[must_use]
    pub fn with_retry_policy(mut self, retry_policy: RemoteRetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }
}

impl fmt::Debug for RemoteSignerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteSignerConfig")
            .field("endpoint", &self.endpoint)
            .field("bearer_secret", &self.bearer_secret)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteSignerConfigErrorKind {
    InvalidEndpoint,
    InsecureEndpoint,
    InvalidBearerSecret,
    InvalidTimeout,
    InvalidResponseLimit,
    InvalidRetryPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSignerConfigError {
    pub kind: RemoteSignerConfigErrorKind,
    pub message: String,
}

impl RemoteSignerConfigError {
    fn new(kind: RemoteSignerConfigErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteSignerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RemoteSignerConfigError {}
