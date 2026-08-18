//! Authenticated, stateless Wallet Service composition root.

mod compose;
mod config;
mod runtime;
mod service;

pub use compose::{ComposeError, compose};
pub use config::{BEARER_ENV, BIND_ENV, Config, ConfigError, DEFAULT_BIND, ENV_KEYS, TLS_ENV};
pub use runtime::{run, serve, serve_until};
pub use service::Service;

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use base::Decimal;
use serde::{Deserialize, Serialize};
use wallets::{AddressText, HistoryRequest, Wallet};

pub const LIVE_PATH: &str = "/health/live";
pub const READY_PATH: &str = "/health/ready";

#[derive(Default)]
pub(crate) struct Server {
    wallets: BTreeMap<String, Arc<dyn Wallet>>,
}

impl Server {
    pub(crate) const fn new() -> Self {
        Self {
            wallets: BTreeMap::new(),
        }
    }
    pub(crate) fn with(mut self, id: impl Into<String>, wallet: Arc<dyn Wallet>) -> Self {
        self.wallets.insert(id.into(), wallet);
        self
    }
    pub(crate) fn router(
        self,
        config: &http_support::server::Config,
    ) -> Result<Router, http_support::server::ConfigError> {
        let ready = !self.wallets.is_empty();
        let protected = Router::new()
            .route("/v1/wallets/{id}", get(summary))
            .route("/v1/wallets/{id}/balance", get(balance))
            .route("/v1/wallets/{id}/history", get(history))
            .route("/v1/wallets/{id}/transactions", post(prepare))
            .route(
                "/v1/wallets/{id}/transactions/{transaction_id}",
                put(broadcast),
            )
            .with_state(Arc::new(StateData {
                wallets: self.wallets,
            }));
        http_support::server::service_router(
            protected,
            config,
            http_support::server::HealthState::new(ready),
        )
    }
}

struct StateData {
    wallets: BTreeMap<String, Arc<dyn Wallet>>,
}

#[derive(Serialize)]
struct Summary {
    id: String,
    address: AddressText,
}

async fn summary(
    State(state): State<Arc<StateData>>,
    Path(id): Path<String>,
) -> Result<Json<Summary>, ApiError> {
    let wallet = find(&state, &id)?;
    let address = wallet
        .address_text(&wallet.address())
        .map_err(ApiError::address)?;
    Ok(Json(Summary { id, address }))
}

#[derive(Serialize)]
struct Balance {
    amount: String,
    observed_height: Option<u64>,
}

async fn balance(
    State(state): State<Arc<StateData>>,
    Path(id): Path<String>,
) -> Result<Json<Balance>, ApiError> {
    let balance = find(&state, &id)?
        .balance()
        .await
        .map_err(ApiError::wallet)?;
    Ok(Json(Balance {
        amount: balance.amount.to_string(),
        observed_height: balance.observed_at.map(|block| block.height.0),
    }))
}

#[derive(Deserialize)]
struct HistoryQuery {
    after: Option<String>,
    after_chain: Option<String>,
    after_network: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}
const fn default_limit() -> usize {
    100
}

async fn history(
    State(state): State<Arc<StateData>>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if query.limit == 0 || query.limit > 1_000 {
        return Err(ApiError::bad_request(
            "history limit must be between 1 and 1000",
        ));
    }
    let after = match (query.after, query.after_chain, query.after_network) {
        (None, None, None) => None,
        (Some(value), Some(chain), Some(network)) => Some(indexing::TransactionRef {
            scope: indexing::IndexScope {
                chain: indexing::ChainId(chain),
                network,
            },
            value,
        }),
        _ => {
            return Err(ApiError::bad_request(
                "history cursor requires after, after_chain, and after_network together",
            ));
        }
    };
    let page = find(&state, &id)?
        .history(HistoryRequest {
            after,
            limit: query.limit,
        })
        .await
        .map_err(ApiError::wallet)?;
    serde_json::to_value(page).map(Json).map_err(|_| {
        ApiError::wallet(wallets::Error::new(
            wallets::ErrorKind::History,
            "wallet history could not be serialized",
        ))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendRequest {
    destination: AddressText,
    amount: String,
}

async fn prepare(
    State(state): State<Arc<StateData>>,
    Path(id): Path<String>,
    Json(request): Json<SendRequest>,
) -> Result<Json<base::SignedTransaction>, ApiError> {
    let wallet = find(&state, &id)?;
    let destination = wallet
        .parse_address(&request.destination)
        .map_err(ApiError::address)?;
    let amount = request
        .amount
        .parse::<Decimal>()
        .map_err(|_| ApiError::bad_request("amount must be an exact base-10 decimal"))?;
    let mut builder = wallet.transaction();
    builder
        .transfer(destination, amount)
        .map_err(ApiError::transaction)?;
    builder
        .prepare()
        .await
        .map(Json)
        .map_err(ApiError::transaction)
}

#[derive(Serialize)]
struct Submission {
    transaction_id: String,
}

async fn broadcast(
    State(state): State<Arc<StateData>>,
    Path((id, transaction_id)): Path<(String, String)>,
    Json(transaction): Json<base::SignedTransaction>,
) -> Result<Json<Submission>, ApiError> {
    if transaction.id().as_str() != transaction_id {
        return Err(ApiError::bad_request(
            "transaction path ID must match the signed transaction ID",
        ));
    }
    let submission = find(&state, &id)?
        .broadcaster()
        .broadcast(&transaction)
        .await
        .map_err(ApiError::transaction)?;
    if submission.id != *transaction.id() {
        return Err(ApiError::transaction(base::TransactionError::new(
            base::TransactionErrorKind::Divergent,
            "broadcaster returned a different transaction ID",
        )));
    }
    Ok(Json(Submission {
        transaction_id: submission.id.to_string(),
    }))
}

fn find(state: &StateData, id: &str) -> Result<Arc<dyn Wallet>, ApiError> {
    state
        .wallets
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("wallet does not exist"))
}

struct ApiError {
    status: StatusCode,
    message: String,
}
impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
    fn wallet(error: wallets::Error) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: error.to_string(),
        }
    }
    fn address(error: wallets::Error) -> Self {
        Self::bad_request(error.to_string())
    }
    fn transaction(error: base::TransactionError) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: error.to_string(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}
