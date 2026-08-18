use std::{
    error::Error as StdError,
    fmt,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    Router,
    extract::{DefaultBodyLimit, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use tokio::net::TcpListener;

use super::{AuthenticationMode, BearerToken, Config, ConfigError};

pub const LIVENESS_PATH: &str = "/health/live";
pub const READINESS_PATH: &str = "/health/ready";

#[derive(Clone)]
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

#[derive(Clone)]
struct ReadinessState {
    health: HealthState,
}

async fn liveness() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

async fn readiness(State(state): State<ReadinessState>) -> Response {
    if state.health.is_ready() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    }
}

async fn require_bearer(
    State(token): State<BearerToken>,
    request: Request,
    next: Next,
) -> Response {
    if token.matches_authorization_header(request.headers().get(axum::http::header::AUTHORIZATION))
    {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response()
    }
}

async fn inject_authentication_mode(
    State(mode): State<AuthenticationMode>,
    mut request: Request,
    next: Next,
) -> Response {
    request.extensions_mut().insert(mode);
    next.run(request).await
}

/// Applies request limits and authentication to application routes, then adds
/// unauthenticated, detail-free health endpoints.
pub fn service_router(
    protected: Router,
    config: &Config,
    health: HealthState,
) -> Result<Router, ConfigError> {
    let protected = protected_router(protected, config)?;

    let health_routes = Router::new()
        .route(LIVENESS_PATH, get(liveness))
        .route(READINESS_PATH, get(readiness))
        .with_state(ReadinessState { health });

    Ok(protected.merge(health_routes))
}

/// Applies configured authentication and request limits without adding routes.
/// Applications that own their health resources use this to keep middleware
/// outside handlers while generating one complete transport contract.
pub fn protected_router(protected: Router, config: &Config) -> Result<Router, ConfigError> {
    config.validate()?;

    let mut protected = protected
        .layer(middleware::from_fn_with_state(
            config.authentication_mode,
            inject_authentication_mode,
        ))
        .layer(DefaultBodyLimit::max(config.limits.max_body_bytes()));
    if config.authentication_mode.is_strict() {
        if let Some(token) = config.bearer_token.clone() {
            protected = protected.layer(middleware::from_fn_with_state(token, require_bearer));
        }
    }

    Ok(protected)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Configuration,
    Bind,
    Serve,
}

#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl Error {
    fn with_source(
        kind: ErrorKind,
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

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

/// Binds and serves a configured router until `shutdown` resolves.
pub async fn serve<S>(router: Router, config: &Config, shutdown: S) -> Result<(), Error>
where
    S: Future<Output = ()> + Send + 'static,
{
    config.validate().map_err(|error| {
        Error::with_source(
            ErrorKind::Configuration,
            "invalid HTTP server configuration",
            error,
        )
    })?;
    let listener = TcpListener::bind(config.bind_addr).await.map_err(|error| {
        Error::with_source(ErrorKind::Bind, "failed to bind HTTP listener", error)
    })?;

    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| {
            Error::with_source(ErrorKind::Serve, "HTTP server stopped with an error", error)
        })
}
