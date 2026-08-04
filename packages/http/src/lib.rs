//! Reusable HTTP client and server primitives.
//!
//! The server intentionally terminates no TLS itself. Bind it to loopback for
//! plaintext development, or place it behind a trusted TLS-terminating proxy.

use std::{
    error::Error,
    fmt,
    future::Future,
    net::SocketAddr,
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use tokio::net::TcpListener;
use transport::{
    BoxFuture, Transport, TransportError, TransportErrorKind, TransportRequest, TransportResponse,
};

pub const LIVENESS_PATH: &str = "/health/live";
pub const READINESS_PATH: &str = "/health/ready";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportSecurity {
    /// Plain HTTP is permitted only when the listener is bound to loopback.
    PlaintextLoopback,
    /// A trusted upstream proxy is responsible for TLS termination.
    TlsTerminatedUpstream,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestLimits {
    max_body_bytes: usize,
    default_page_size: usize,
    max_page_size: usize,
}

impl RequestLimits {
    pub fn new(
        max_body_bytes: usize,
        default_page_size: usize,
        max_page_size: usize,
    ) -> Result<Self, HttpServerConfigError> {
        if max_body_bytes == 0 {
            return Err(HttpServerConfigError::new(
                HttpServerConfigErrorKind::InvalidLimits,
                "maximum request body size must be greater than zero",
            ));
        }
        if default_page_size == 0 || max_page_size == 0 || default_page_size > max_page_size {
            return Err(HttpServerConfigError::new(
                HttpServerConfigErrorKind::InvalidLimits,
                "page sizes must be non-zero and the default must not exceed the maximum",
            ));
        }

        Ok(Self {
            max_body_bytes,
            default_page_size,
            max_page_size,
        })
    }

    #[must_use]
    pub const fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    #[must_use]
    pub const fn max_page_size(&self) -> usize {
        self.max_page_size
    }

    pub fn page_size(&self, requested: Option<usize>) -> Result<usize, PageLimitError> {
        let size = requested.unwrap_or(self.default_page_size);
        if size == 0 {
            return Err(PageLimitError::Zero);
        }
        if size > self.max_page_size {
            return Err(PageLimitError::ExceedsMaximum {
                requested: size,
                maximum: self.max_page_size,
            });
        }
        Ok(size)
    }
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
            default_page_size: 100,
            max_page_size: 1_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageLimitError {
    Zero,
    ExceedsMaximum { requested: usize, maximum: usize },
}

impl fmt::Display for PageLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => formatter.write_str("page size must be greater than zero"),
            Self::ExceedsMaximum { maximum, .. } => {
                write!(
                    formatter,
                    "page size exceeds the configured maximum of {maximum}"
                )
            }
        }
    }
}

impl Error for PageLimitError {}

#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(Arc<[u8]>);

impl BearerToken {
    pub fn new(token: impl AsRef<str>) -> Result<Self, HttpServerConfigError> {
        let bytes = token.as_ref().as_bytes();
        if bytes.is_empty()
            || bytes
                .iter()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(HttpServerConfigError::new(
                HttpServerConfigErrorKind::InvalidBearerToken,
                "bearer token must be non-empty and contain no whitespace or control characters",
            ));
        }
        Ok(Self(Arc::from(bytes)))
    }

    fn matches_header(&self, value: Option<&HeaderValue>) -> bool {
        const PREFIX: &[u8] = b"Bearer ";
        let Some(value) = value else {
            return false;
        };
        let bytes = value.as_bytes();
        if bytes.len() < PREFIX.len() || !bytes.starts_with(PREFIX) {
            return false;
        }
        constant_time_eq(&bytes[PREFIX.len()..], &self.0)
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([REDACTED])")
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let maximum = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..maximum {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

#[derive(Clone, Debug)]
pub struct HttpServerConfig {
    bind_addr: SocketAddr,
    transport_security: TransportSecurity,
    bearer_token: Option<BearerToken>,
    limits: RequestLimits,
}

impl HttpServerConfig {
    #[must_use]
    pub fn new(
        bind_addr: SocketAddr,
        transport_security: TransportSecurity,
        bearer_token: Option<BearerToken>,
        limits: RequestLimits,
    ) -> Self {
        Self {
            bind_addr,
            transport_security,
            bearer_token,
            limits,
        }
    }

    pub fn validate(&self) -> Result<(), HttpServerConfigError> {
        if !self.bind_addr.ip().is_loopback()
            && self.transport_security != TransportSecurity::TlsTerminatedUpstream
        {
            return Err(HttpServerConfigError::new(
                HttpServerConfigErrorKind::InsecureNonLoopbackBind,
                "a non-loopback listener requires upstream TLS termination",
            ));
        }
        if !self.bind_addr.ip().is_loopback() && self.bearer_token.is_none() {
            return Err(HttpServerConfigError::new(
                HttpServerConfigErrorKind::MissingBearerToken,
                "a non-loopback listener requires bearer authentication",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    #[must_use]
    pub const fn limits(&self) -> &RequestLimits {
        &self.limits
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpServerConfigErrorKind {
    InvalidLimits,
    InvalidBearerToken,
    InsecureNonLoopbackBind,
    MissingBearerToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpServerConfigError {
    pub kind: HttpServerConfigErrorKind,
    pub message: String,
}

impl HttpServerConfigError {
    fn new(kind: HttpServerConfigErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for HttpServerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HttpServerConfigError {}

#[derive(Clone, Default)]
pub struct HealthState {
    ready: Arc<AtomicBool>,
}

impl HealthState {
    #[must_use]
    pub fn new(ready: bool) -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(ready)),
        }
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

impl fmt::Debug for HealthState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HealthState")
            .field("ready", &self.is_ready())
            .finish()
    }
}

async fn liveness() -> Response {
    json_status(StatusCode::OK, r#"{"status":"live"}"#)
}

async fn readiness(State(health): State<HealthState>) -> Response {
    if health.is_ready() {
        json_status(StatusCode::OK, r#"{"status":"ready"}"#)
    } else {
        json_status(StatusCode::SERVICE_UNAVAILABLE, r#"{"status":"not_ready"}"#)
    }
}

fn json_status(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

async fn require_bearer(
    State(token): State<BearerToken>,
    request: Request,
    next: Next,
) -> Response {
    if token.matches_header(request.headers().get(axum::http::header::AUTHORIZATION)) {
        next.run(request).await
    } else {
        static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);
        let request_id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        let body = format!(
            "{{\"code\":\"unauthorized\",\"message\":\"valid bearer authentication is required\",\"retryable\":false,\"request_id\":\"http-request-{request_id:020}\"}}"
        );
        (
            StatusCode::UNAUTHORIZED,
            [
                (axum::http::header::WWW_AUTHENTICATE, "Bearer"),
                (axum::http::header::CONTENT_TYPE, "application/json"),
            ],
            body,
        )
            .into_response()
    }
}

/// Applies request limits and authentication to application routes, then adds
/// unauthenticated, detail-free health endpoints.
pub fn service_router(
    protected: Router,
    config: &HttpServerConfig,
    health: HealthState,
) -> Result<Router, HttpServerConfigError> {
    config.validate()?;

    let mut protected = protected.layer(DefaultBodyLimit::max(config.limits.max_body_bytes));
    if let Some(token) = config.bearer_token.clone() {
        protected = protected.layer(middleware::from_fn_with_state(token, require_bearer));
    }

    let health_routes = Router::new()
        .route(LIVENESS_PATH, get(liveness))
        .route(READINESS_PATH, get(readiness))
        .with_state(health);

    Ok(protected.merge(health_routes))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpServerErrorKind {
    Configuration,
    Bind,
    Serve,
}

#[derive(Debug)]
pub struct HttpServerError {
    pub kind: HttpServerErrorKind,
    pub message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl HttpServerError {
    fn with_source(
        kind: HttpServerErrorKind,
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for HttpServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HttpServerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Binds and serves a configured router until `shutdown` resolves.
pub async fn serve<S>(
    router: Router,
    config: &HttpServerConfig,
    shutdown: S,
) -> Result<(), HttpServerError>
where
    S: Future<Output = ()> + Send + 'static,
{
    config.validate().map_err(|error| {
        HttpServerError::with_source(
            HttpServerErrorKind::Configuration,
            "invalid HTTP server configuration",
            error,
        )
    })?;
    let listener = TcpListener::bind(config.bind_addr).await.map_err(|error| {
        HttpServerError::with_source(
            HttpServerErrorKind::Bind,
            "failed to bind HTTP listener",
            error,
        )
    })?;

    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| {
            HttpServerError::with_source(
                HttpServerErrorKind::Serve,
                "HTTP server stopped with an error",
                error,
            )
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    max_attempts: NonZeroU32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl RetryPolicy {
    pub fn new(
        max_attempts: NonZeroU32,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Result<Self, HttpTransportBuildError> {
        if initial_backoff > max_backoff {
            return Err(HttpTransportBuildError::new(
                HttpTransportBuildErrorKind::InvalidRetryPolicy,
                "initial retry backoff must not exceed the maximum",
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

    fn backoff_after(&self, failed_attempt: u32) -> Duration {
        let exponent = failed_attempt.saturating_sub(1).min(31);
        let multiplier = 1_u32 << exponent;
        self.initial_backoff
            .checked_mul(multiplier)
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::no_retry()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HttpTransportConfig {
    pub endpoint: String,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
    pub default_headers: Vec<(String, String)>,
    pub retry_policy: RetryPolicy,
}

impl fmt::Debug for HttpTransportConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self
            .default_headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        formatter
            .debug_struct("HttpTransportConfig")
            .field("endpoint", &"[REDACTED]")
            .field("request_timeout", &self.request_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("default_header_names", &header_names)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

impl HttpTransportConfig {
    #[must_use]
    pub fn new(endpoint: impl Into<String>, request_timeout: Duration) -> Self {
        Self {
            endpoint: endpoint.into(),
            request_timeout,
            max_response_bytes: 64 * 1024 * 1024,
            default_headers: Vec::new(),
            retry_policy: RetryPolicy::default(),
        }
    }
}

#[derive(Clone)]
pub struct HttpTransport {
    config: HttpTransportConfig,
    client: reqwest::Client,
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpTransport")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    pub fn new(config: HttpTransportConfig) -> Result<Self, HttpTransportBuildError> {
        if config.endpoint.trim().is_empty() {
            return Err(HttpTransportBuildError::new(
                HttpTransportBuildErrorKind::InvalidEndpoint,
                "HTTP endpoint must not be empty",
            ));
        }
        reqwest::Url::parse(&config.endpoint).map_err(|error| {
            HttpTransportBuildError::with_source(
                HttpTransportBuildErrorKind::InvalidEndpoint,
                "HTTP endpoint is invalid",
                error,
            )
        })?;
        if config.request_timeout.is_zero() {
            return Err(HttpTransportBuildError::new(
                HttpTransportBuildErrorKind::InvalidTimeout,
                "HTTP request timeout must be greater than zero",
            ));
        }
        if config.max_response_bytes == 0 {
            return Err(HttpTransportBuildError::new(
                HttpTransportBuildErrorKind::InvalidResponseLimit,
                "HTTP maximum response size must be greater than zero",
            ));
        }

        let client = reqwest::Client::builder().build().map_err(|error| {
            HttpTransportBuildError::with_source(
                HttpTransportBuildErrorKind::Client,
                "failed to construct HTTP client",
                error,
            )
        })?;
        Ok(Self { config, client })
    }

    async fn send_once(
        &self,
        request: &TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
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
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| TransportError {
                kind: TransportErrorKind::Rejected,
                message: "HTTP request contains an invalid header name".to_owned(),
            })?;
            let value = HeaderValue::from_str(value).map_err(|_| TransportError {
                kind: TransportErrorKind::Rejected,
                message: "HTTP request contains an invalid header value".to_owned(),
            })?;
            headers.insert(name, value);
        }

        let mut response = self
            .client
            .post(endpoint)
            .headers(headers)
            .timeout(self.config.request_timeout)
            .body(request.body.clone())
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status().as_u16();
        let mut body =
            BoundedResponseBody::new(self.config.max_response_bytes, response.content_length())?;
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

        Ok(TransportResponse {
            status,
            headers,
            body: body.into_bytes(),
        })
    }
}

/// Incremental response collector shared by declared-length and chunked HTTP
/// responses. Errors deliberately contain no response bytes.
struct BoundedResponseBody {
    bytes: Vec<u8>,
    maximum: usize,
}

impl BoundedResponseBody {
    fn new(maximum: usize, declared_length: Option<u64>) -> Result<Self, TransportError> {
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

    fn push_chunk(&mut self, chunk: &[u8]) -> Result<(), TransportError> {
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

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

fn response_size_limit_error() -> TransportError {
    TransportError {
        kind: TransportErrorKind::InvalidResponse,
        message: "HTTP response exceeds the configured size limit".to_owned(),
    }
}

fn response_size_overflow_error() -> TransportError {
    TransportError {
        kind: TransportErrorKind::InvalidResponse,
        message: "HTTP response size overflowed".to_owned(),
    }
}

impl Transport for HttpTransport {
    fn send<'a>(
        &'a self,
        request: TransportRequest,
    ) -> BoxFuture<'a, Result<TransportResponse, TransportError>> {
        Box::pin(async move {
            let mut attempt = 1_u32;
            loop {
                match self.send_once(&request).await {
                    Ok(response)
                        if is_retryable_status(response.status)
                            && attempt < self.config.retry_policy.max_attempts.get() =>
                    {
                        tokio::time::sleep(self.config.retry_policy.backoff_after(attempt)).await;
                        attempt += 1;
                    }
                    Err(error)
                        if is_retryable_transport_error(error.kind)
                            && attempt < self.config.retry_policy.max_attempts.get() =>
                    {
                        tokio::time::sleep(self.config.retry_policy.backoff_after(attempt)).await;
                        attempt += 1;
                    }
                    result => return result,
                }
            }
        })
    }
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

fn is_retryable_transport_error(kind: TransportErrorKind) -> bool {
    matches!(
        kind,
        TransportErrorKind::Timeout | TransportErrorKind::Unavailable
    )
}

fn map_reqwest_error(error: reqwest::Error) -> TransportError {
    let kind = if error.is_timeout() {
        TransportErrorKind::Timeout
    } else if error.is_connect() || error.is_request() {
        TransportErrorKind::Unavailable
    } else if error.is_decode() {
        TransportErrorKind::InvalidResponse
    } else {
        TransportErrorKind::Other
    };
    TransportError {
        kind,
        // Do not propagate reqwest's display text: it may contain a URL with credentials.
        message: match kind {
            TransportErrorKind::Timeout => "HTTP request timed out",
            TransportErrorKind::Unavailable => "HTTP endpoint is unavailable",
            TransportErrorKind::InvalidResponse => "HTTP response could not be decoded",
            _ => "HTTP request failed",
        }
        .to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpTransportBuildErrorKind {
    InvalidEndpoint,
    InvalidTimeout,
    InvalidResponseLimit,
    InvalidRetryPolicy,
    Client,
}

#[derive(Debug)]
pub struct HttpTransportBuildError {
    pub kind: HttpTransportBuildErrorKind,
    pub message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl HttpTransportBuildError {
    fn new(kind: HttpTransportBuildErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: HttpTransportBuildErrorKind,
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for HttpTransportBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HttpTransportBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        routing::post,
    };
    use tower::ServiceExt;

    fn loopback_config(token: Option<BearerToken>, limits: RequestLimits) -> HttpServerConfig {
        HttpServerConfig::new(
            "127.0.0.1:0"
                .parse()
                .expect("test loopback socket address must parse"),
            TransportSecurity::PlaintextLoopback,
            token,
            limits,
        )
    }

    #[tokio::test]
    async fn protected_routes_require_the_exact_bearer_token() {
        let token = BearerToken::new("correct-secret").expect("test token must be valid");
        let router = service_router(
            Router::new().route("/private", get(|| async { "ok" })),
            &loopback_config(Some(token), RequestLimits::default()),
            HealthState::new(true),
        )
        .expect("test router configuration must be valid");

        let missing = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        let missing_body = to_bytes(missing.into_body(), 1024)
            .await
            .expect("authentication body must be readable");
        let missing_text =
            std::str::from_utf8(&missing_body).expect("authentication body must be valid UTF-8");
        assert!(missing_text.contains("\"code\":\"unauthorized\""));
        assert!(missing_text.contains("\"retryable\":false"));
        assert!(missing_text.contains("\"request_id\":\"http-request-"));

        let wrong = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .header(axum::http::header::AUTHORIZATION, "Bearer wrong-secret")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let wrong_body = to_bytes(wrong.into_body(), 1024)
            .await
            .expect("authentication body must be readable");
        let wrong_text =
            std::str::from_utf8(&wrong_body).expect("authentication body must be valid UTF-8");
        assert!(!wrong_text.contains("wrong-secret"));
        assert!(!wrong_text.contains("correct-secret"));

        let accepted = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .header(axum::http::header::AUTHORIZATION, "Bearer correct-secret")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(accepted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_routes_are_unauthenticated_and_sanitized() {
        let token = BearerToken::new("secret").expect("test token must be valid");
        let health = HealthState::new(false);
        let router = service_router(
            Router::new().route("/private", get(|| async { "private" })),
            &loopback_config(Some(token), RequestLimits::default()),
            health.clone(),
        )
        .expect("test router configuration must be valid");

        let live = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(LIVENESS_PATH)
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(live.status(), StatusCode::OK);
        let live_body = to_bytes(live.into_body(), 1024)
            .await
            .expect("health body must be readable");
        assert_eq!(live_body.as_ref(), br#"{"status":"live"}"#);

        let not_ready = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(READINESS_PATH)
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(not_ready.into_body(), 1024)
            .await
            .expect("health body must be readable");
        assert_eq!(body.as_ref(), br#"{"status":"not_ready"}"#);

        health.set_ready(true);
        let ready = router
            .oneshot(
                axum::http::Request::builder()
                    .uri(READINESS_PATH)
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(ready.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn configured_body_limit_rejects_large_requests() {
        let limits = RequestLimits::new(4, 10, 20).expect("test limits must be valid");
        let router = service_router(
            Router::new().route("/echo", post(|body: String| async move { body })),
            &loopback_config(None, limits),
            HealthState::new(true),
        )
        .expect("test router configuration must be valid");

        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/echo")
                    .header(axum::http::header::CONTENT_TYPE, "text/plain")
                    .body(Body::from("12345"))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn non_loopback_listener_requires_tls_termination_and_authentication() {
        let address = "0.0.0.0:8080"
            .parse()
            .expect("test non-loopback socket address must parse");
        let insecure = HttpServerConfig::new(
            address,
            TransportSecurity::PlaintextLoopback,
            None,
            RequestLimits::default(),
        );
        assert_eq!(
            insecure
                .validate()
                .expect_err("insecure bind must fail")
                .kind,
            HttpServerConfigErrorKind::InsecureNonLoopbackBind
        );

        let unauthenticated = HttpServerConfig::new(
            address,
            TransportSecurity::TlsTerminatedUpstream,
            None,
            RequestLimits::default(),
        );
        assert_eq!(
            unauthenticated
                .validate()
                .expect_err("unauthenticated bind must fail")
                .kind,
            HttpServerConfigErrorKind::MissingBearerToken
        );
    }

    #[test]
    fn page_limits_are_enforced() {
        let limits = RequestLimits::new(128, 25, 100).expect("test limits must be valid");
        assert_eq!(limits.page_size(None), Ok(25));
        assert_eq!(limits.page_size(Some(100)), Ok(100));
        assert_eq!(
            limits.page_size(Some(101)),
            Err(PageLimitError::ExceedsMaximum {
                requested: 101,
                maximum: 100
            })
        );
    }

    #[test]
    fn debug_output_never_contains_credentials() {
        let token = BearerToken::new("top-secret").expect("test token must be valid");
        assert!(!format!("{token:?}").contains("top-secret"));

        let config = HttpTransportConfig {
            endpoint: "https://user:password@example.invalid".to_owned(),
            request_timeout: Duration::from_secs(1),
            max_response_bytes: 1024,
            default_headers: vec![("authorization".to_owned(), "Bearer hidden".to_owned())],
            retry_policy: RetryPolicy::default(),
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("password"));
        assert!(!debug.contains("Bearer hidden"));
        assert!(debug.contains("authorization"));

        let transport = HttpTransport::new(config).expect("test transport must build");
        let debug = format!("{transport:?}");
        assert!(!debug.contains("password"));
        assert!(!debug.contains("Bearer hidden"));
        assert!(debug.contains("authorization"));
    }

    #[tokio::test]
    async fn rejected_request_metadata_does_not_leak_credentials_or_body() {
        let transport = HttpTransport::new(HttpTransportConfig::new(
            "https://user:password@example.invalid",
            Duration::from_secs(1),
        ))
        .expect("test transport must build");
        let error = transport
            .send(TransportRequest {
                endpoint: String::new(),
                headers: vec![(
                    "authorization".to_owned(),
                    "Bearer hidden\ninvalid".to_owned(),
                )],
                body: b"sensitive-request-body".to_vec(),
            })
            .await
            .expect_err("an invalid header value must be rejected before sending");

        assert_eq!(error.kind, TransportErrorKind::Rejected);
        for secret in ["password", "Bearer hidden", "sensitive-request-body"] {
            assert!(!error.message.contains(secret));
        }
    }

    #[test]
    fn bounded_response_rejects_declared_and_streamed_overflow_without_leaking_body() {
        let declared_error = match BoundedResponseBody::new(4, Some(5)) {
            Ok(_) => panic!("an oversized declared response must fail"),
            Err(error) => error,
        };
        assert_eq!(declared_error.kind, TransportErrorKind::InvalidResponse);
        assert_eq!(
            declared_error.message,
            "HTTP response exceeds the configured size limit"
        );

        let mut streamed =
            BoundedResponseBody::new(6, None).expect("an unknown response length is allowed");
        streamed
            .push_chunk(b"secret")
            .expect("a chunk at the limit must be accepted");
        let streamed_error = streamed
            .push_chunk(b"-response-body")
            .expect_err("a chunk crossing the response limit must fail");

        assert_eq!(streamed_error.kind, TransportErrorKind::InvalidResponse);
        assert!(!streamed_error.message.contains("secret"));
        assert!(!streamed_error.message.contains("response-body"));
    }

    #[test]
    fn bounded_response_accepts_multiple_chunks_at_the_exact_limit() {
        let mut response =
            BoundedResponseBody::new(5, Some(5)).expect("the declared length is within limit");
        response
            .push_chunk(b"12")
            .expect("the first response chunk must fit");
        response
            .push_chunk(b"345")
            .expect("the final response chunk must reach the exact limit");

        assert_eq!(response.into_bytes(), b"12345");
    }
}
