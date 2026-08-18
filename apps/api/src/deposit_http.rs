use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base::Decimal;
use deposits::{
    Deposit, DepositError, DepositErrorKind, DepositId, DepositQuery, DepositRegistration,
    DepositState, DepositStateKind, IdempotencyKey, UserId,
};
use indexing::{AssetId, ChainId};
use serde::{Deserialize, Serialize};

use crate::Deposits;
use crate::deposit_ledger::ledger_routes;

const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// Deposit routes bound to one application-configured chain/network scope.
pub fn deposit_routes(deposits: Arc<Deposits>) -> Router {
    Router::new()
        .route("/v1/deposits", post(open).get(list))
        .route("/v1/deposits/resume", post(resume))
        .route("/v1/deposits/{id}", get(get_deposit))
        .with_state(deposits.clone())
        .merge(ledger_routes(deposits))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepositRequest {
    pub id: String,
    pub user_id: String,
    pub asset: AssetRequest,
    pub expected: String,
    pub expires_at: u64,
    pub created_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetRequest {
    pub chain: String,
    pub asset: String,
}

async fn open(
    State(deposits): State<Arc<Deposits>>,
    headers: HeaderMap,
    Json(body): Json<DepositRequest>,
) -> Result<(StatusCode, Json<DepositResponse>), HttpError> {
    let idempotency_key = required_header(&headers, IDEMPOTENCY_KEY)?;
    let expected = body
        .expected
        .parse::<Decimal>()
        .map_err(|_| HttpError::bad_request("expected must be an exact decimal string"))?;
    let deposit = deposits
        .open(DepositRegistration {
            scope: deposits.scope().clone(),
            id: DepositId(body.id),
            idempotency_key: IdempotencyKey(idempotency_key),
            user_id: UserId(body.user_id),
            asset: AssetId {
                chain: ChainId(body.asset.chain),
                asset: body.asset.asset,
            },
            expected,
            expires_at: body.expires_at,
            created_at: body.created_at,
        })
        .await?;
    Ok((StatusCode::OK, Json(deposit.into())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeRequest {
    pub limit: usize,
}

#[derive(Serialize)]
pub struct ResumeResponse {
    pub resumed: usize,
}

async fn resume(
    State(deposits): State<Arc<Deposits>>,
    Json(body): Json<ResumeRequest>,
) -> Result<Json<ResumeResponse>, HttpError> {
    if body.limit == 0 {
        return Err(HttpError::bad_request("limit must be positive"));
    }
    let resumed = deposits.resume(body.limit).await?;
    Ok(Json(ResumeResponse { resumed }))
}

async fn get_deposit(
    State(deposits): State<Arc<Deposits>>,
    Path(id): Path<String>,
) -> Result<Json<DepositResponse>, HttpError> {
    deposits
        .get(&DepositId(id))
        .await?
        .map(DepositResponse::from)
        .map(Json)
        .ok_or_else(|| HttpError::not_found("deposit does not exist"))
}

#[derive(Deserialize)]
pub struct DepositFilter {
    pub after: Option<String>,
    pub limit: Option<usize>,
    pub user_id: Option<String>,
    pub state: Option<StateQuery>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateQuery {
    AwaitingWatch,
    Active,
    Expired,
    Closed,
}

impl From<StateQuery> for DepositStateKind {
    fn from(value: StateQuery) -> Self {
        match value {
            StateQuery::AwaitingWatch => Self::AwaitingWatch,
            StateQuery::Active => Self::Active,
            StateQuery::Expired => Self::Expired,
            StateQuery::Closed => Self::Closed,
        }
    }
}

#[derive(Serialize)]
pub struct DepositList {
    pub deposits: Vec<DepositResponse>,
    pub next: Option<String>,
}

async fn list(
    State(deposits): State<Arc<Deposits>>,
    Query(query): Query<DepositFilter>,
) -> Result<Json<DepositList>, HttpError> {
    let limit = query.limit.unwrap_or(100);
    if limit == 0 || limit > 1_000 {
        return Err(HttpError::bad_request("limit must be between 1 and 1000"));
    }
    let page = deposits
        .list(DepositQuery {
            after: query.after.map(DepositId),
            limit,
            user_id: query.user_id.map(UserId),
            state: query.state.map(Into::into),
        })
        .await?;
    Ok(Json(DepositList {
        deposits: page.deposits.into_iter().map(Into::into).collect(),
        next: page.next.map(|id| id.0),
    }))
}

#[derive(Serialize)]
pub struct DepositResponse {
    pub id: String,
    pub user_id: String,
    pub asset: AssetResponse,
    pub address: AddressResponse,
    pub expected: String,
    pub birthday: u64,
    pub expires_at: u64,
    pub state: StateResponse,
    pub created_at: u64,
}

#[derive(Serialize)]
pub struct AssetResponse {
    pub chain: String,
    pub asset: String,
}

#[derive(Serialize)]
pub struct AddressResponse {
    pub chain: String,
    pub network: String,
    pub value: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StateResponse {
    AwaitingWatch,
    Active { watch_id: String },
    Expired { watch_id: String },
    Closed,
}

impl From<Deposit> for DepositResponse {
    fn from(value: Deposit) -> Self {
        let state = match value.state {
            DepositState::AwaitingWatch => StateResponse::AwaitingWatch,
            DepositState::Active { watch_id } => StateResponse::Active {
                watch_id: watch_id.0,
            },
            DepositState::Expired { watch_id } => StateResponse::Expired {
                watch_id: watch_id.0,
            },
            DepositState::Closed => StateResponse::Closed,
        };
        Self {
            id: value.id.0,
            user_id: value.user_id.0,
            asset: AssetResponse {
                chain: value.asset.chain.0,
                asset: value.asset.asset,
            },
            address: AddressResponse {
                chain: value.address.scope.chain.0,
                network: value.address.scope.network,
                value: value.address.value,
            },
            expected: value.expected.to_string(),
            birthday: value.birthday.0,
            expires_at: value.expires_at,
            state,
            created_at: value.created_at,
        }
    }
}

pub(super) struct HttpError {
    status: StatusCode,
    message: String,
}

impl HttpError {
    pub(super) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(super) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub(super) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }
}

impl From<DepositError> for HttpError {
    fn from(error: DepositError) -> Self {
        let status = match error.kind {
            DepositErrorKind::NotFound => StatusCode::NOT_FOUND,
            DepositErrorKind::Conflict | DepositErrorKind::InvalidState => StatusCode::CONFLICT,
            DepositErrorKind::InvariantViolation => StatusCode::UNPROCESSABLE_ENTITY,
            DepositErrorKind::Store | DepositErrorKind::Other => StatusCode::SERVICE_UNAVAILABLE,
        };
        Self {
            status,
            message: error.message,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Result<String, HttpError> {
    let value = headers
        .get(name)
        .ok_or_else(|| HttpError::bad_request(format!("{name} header is required")))?
        .to_str()
        .map_err(|_| HttpError::bad_request(format!("{name} header is invalid")))?
        .trim();
    if value.is_empty() || value.len() > 256 {
        return Err(HttpError::bad_request(format!(
            "{name} header must contain 1 to 256 characters"
        )));
    }
    Ok(value.to_owned())
}
