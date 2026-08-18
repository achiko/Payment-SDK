use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response as HttpResponse},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use wallets::AddressText;

use crate::{
    Clock, Deposits, Error, ErrorKind, Payment, Payments, Planner, Request, Stage, Sweeps,
    deposit_routes, plan_routes, sweep_routes,
};

pub const LIVE_PATH: &str = "/health/live";
pub const READY_PATH: &str = "/health/ready";

/// Builds an HTTP-only router.
///
/// This unsupervised surface deliberately remains unready because it does not
/// run reconciliation. Operational hosts should use [`crate::Service`].
pub fn router(payments: Arc<Payments>) -> Router {
    service_router(payments, HealthState::new())
}

/// Builds the complete Payment Service HTTP surface from explicitly composed
/// capabilities. Deposit and collection routes exist only when their backing
/// facades are supplied by the application composition root.
pub fn gateway_router(
    payments: Arc<Payments>,
    deposits: Option<Arc<Deposits>>,
    planner: Option<Arc<Planner>>,
    sweeps: Option<(Arc<Sweeps>, Arc<dyn Clock>)>,
) -> Router {
    let mut app = router(payments);
    if let Some(deposits) = deposits {
        app = app.merge(deposit_routes(deposits));
    }
    if let Some(planner) = planner {
        app = app.merge(plan_routes(planner));
    }
    if let Some((sweeps, clock)) = sweeps {
        app = app.merge(sweep_routes(sweeps, clock));
    }
    app
}

/// Applies the shared HTTP server's bearer authentication, request-size
/// limits, and detail-free health endpoints to the complete gateway surface.
/// Strict mode fails construction unless a bearer token (or an explicitly
/// declared application authorizer) is configured.
pub fn authenticated_gateway(
    payments: Arc<Payments>,
    deposits: Option<Arc<Deposits>>,
    planner: Option<Arc<Planner>>,
    sweeps: Option<(Arc<Sweeps>, Arc<dyn Clock>)>,
    config: &http_kit::server::Config,
    health: http_kit::server::HealthState,
) -> Result<Router, http_kit::server::ConfigError> {
    let protected = payment_routes(payments)
        .merge(deposits.map(deposit_routes).unwrap_or_default())
        .merge(planner.map(plan_routes).unwrap_or_default())
        .merge(
            sweeps
                .map(|(sweeps, clock)| sweep_routes(sweeps, clock))
                .unwrap_or_default(),
        );
    http_kit::server::service_router(protected, config, health)
}

pub(crate) fn service_router(payments: Arc<Payments>, health: HealthState) -> Router {
    payment_routes(payments).merge(
        Router::new()
            .route(LIVE_PATH, get(live))
            .route(READY_PATH, get(ready))
            .with_state(ReadyState { health }),
    )
}

fn payment_routes(payments: Arc<Payments>) -> Router {
    Router::new()
        .route("/v1/payments", post(pay))
        .route("/v1/payments/{id}", get(get_payment))
        .with_state(StateData { payments })
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    payments: Arc<Payments>,
) -> std::io::Result<()> {
    axum::serve(listener, router(payments)).await
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn live() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn ready(State(state): State<ReadyState>) -> HttpResponse {
    if state.health.is_ready() {
        (StatusCode::OK, Json(Health { status: "ok" })).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Health {
                status: "not_ready",
            }),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PayBody {
    id: String,
    wallet: String,
    destination: AddressText,
    amount: String,
    confirmations: u64,
    #[serde(default)]
    require_finality: bool,
}

async fn pay(
    State(state): State<StateData>,
    Json(request): Json<PayBody>,
) -> Result<Json<Response>, ResponseError> {
    state
        .payments
        .pay(Request {
            id: request.id,
            wallet: request.wallet,
            destination: request.destination,
            amount: request.amount,
            confirmations: request.confirmations,
            require_finality: request.require_finality,
        })
        .await
        .map(Response::from)
        .map(Json)
        .map_err(ResponseError::from)
}

async fn get_payment(
    State(state): State<StateData>,
    Path(id): Path<String>,
) -> Result<Json<Response>, ResponseError> {
    state
        .payments
        .get(&id)
        .await
        .map_err(ResponseError::from)?
        .map(Response::from)
        .map(Json)
        .ok_or_else(|| ResponseError::not_found("payment does not exist"))
}

#[derive(Clone)]
struct StateData {
    payments: Arc<Payments>,
}

#[derive(Clone)]
struct ReadyState {
    health: HealthState,
}

#[derive(Clone)]
pub(crate) struct HealthState {
    ready: Arc<AtomicBool>,
}

impl HealthState {
    pub(crate) fn new() -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

struct ResponseError {
    status: StatusCode,
    message: String,
}

impl ResponseError {
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

impl From<Error> for ResponseError {
    fn from(error: Error) -> Self {
        let status = match error.kind {
            ErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
            ErrorKind::UnknownWallet => StatusCode::NOT_FOUND,
            ErrorKind::Conflict => StatusCode::CONFLICT,
            ErrorKind::Transaction => StatusCode::UNPROCESSABLE_ENTITY,
            ErrorKind::Indexer | ErrorKind::Store => StatusCode::SERVICE_UNAVAILABLE,
        };
        Self {
            status,
            message: error.message,
        }
    }
}

/// Public payment state deliberately excludes signed transaction envelopes.
#[derive(Serialize)]
struct Response {
    id: String,
    request: Request,
    stage: StageResponse,
}

#[derive(Serialize)]
enum StageResponse {
    Requested,
    Prepared {
        transaction_id: String,
    },
    Watched {
        transaction_id: String,
    },
    Submitted {
        transaction_id: String,
    },
    Confirmed {
        transaction_id: String,
        confirmations: u64,
    },
}

impl From<Payment> for Response {
    fn from(payment: Payment) -> Self {
        let stage = match payment.stage {
            Stage::Requested => StageResponse::Requested,
            Stage::Prepared { prepared, .. } => StageResponse::Prepared {
                transaction_id: prepared.id().to_string(),
            },
            Stage::Watched { prepared, .. } => StageResponse::Watched {
                transaction_id: prepared.id().to_string(),
            },
            Stage::Submitted { transaction_id, .. } => StageResponse::Submitted {
                transaction_id: transaction_id.to_string(),
            },
            Stage::Confirmed {
                transaction_id,
                confirmations,
                ..
            } => StageResponse::Confirmed {
                transaction_id: transaction_id.to_string(),
                confirmations,
            },
        };
        Self {
            id: payment.id,
            request: payment.request,
            stage,
        }
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

impl IntoResponse for ResponseError {
    fn into_response(self) -> HttpResponse {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}
