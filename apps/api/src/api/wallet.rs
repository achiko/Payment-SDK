use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    State as HttpState,
    contract::{Wallet, WalletAsset, WalletPath},
    error::{ApiError, ErrorBody},
};

pub fn routes() -> OpenApiRouter<HttpState> {
    OpenApiRouter::new()
        .routes(routes!(create))
        .routes(routes!(read))
        .routes(routes!(balance))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateWallet {
    pub asset: WalletAsset,
}

#[utoipa::path(
    post,
    path = "/v1/wallets",
    request_body = CreateWallet,
    responses(
        (status = 201, description = "Wallet generated and registered for indexing", body = Wallet),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 409, body = ErrorBody),
        (status = 503, body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn create(
    State(state): State<HttpState>,
    request: Result<Json<CreateWallet>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), ApiError> {
    let Json(request) = request.map_err(ApiError::invalid_json)?;
    let id = uuid::Uuid::now_v7().to_string();
    let wallet = state
        .wallets
        .generate(id, &request.asset)
        .await
        .map_err(|error| {
            if error.kind == wallets::ErrorKind::Unsupported {
                return ApiError::not_found("wallet asset is not configured");
            }
            error.into()
        })?;
    Ok((StatusCode::CREATED, Json(wallet.into())))
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}",
    params(WalletPath),
    responses(
        (status = 200, body = Wallet),
        (status = 404, body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn read(
    State(state): State<HttpState>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, ApiError> {
    Ok(Json(state.wallets.get(&path.id)?.into()))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Balance {
    pub amount: String,
    pub observed_height: Option<u64>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/balance",
    params(WalletPath),
    responses(
        (status = 200, body = Balance),
        (status = 404, body = ErrorBody),
        (status = 503, body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn balance(
    State(state): State<HttpState>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Balance>, ApiError> {
    let balance = state.wallets.balance(&path.id).await?;
    Ok(Json(Balance {
        amount: balance.amount.to_string(),
        observed_height: balance.observed_at.map(|block| block.height.0),
    }))
}
