use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::error::{ApiError, ErrorBody};
use crate::{Balance, CreateWallet, Gateway, Wallet};

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
pub struct WalletPath {
    pub id: String,
}

pub fn routes() -> OpenApiRouter<Gateway> {
    OpenApiRouter::new()
        .routes(routes!(create))
        .routes(routes!(read))
        .routes(routes!(balance))
}

#[utoipa::path(
    post,
    path = "/v1/wallets",
    request_body = CreateWallet,
    responses(
        (status = 201, description = "Wallet generated and watched", body = Wallet),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 409, body = ErrorBody),
        (status = 503, body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn create(
    State(state): State<Gateway>,
    Json(request): Json<CreateWallet>,
) -> Result<(StatusCode, Json<Wallet>), ApiError> {
    let wallet = state.generate(request.chain).await?;
    Ok((StatusCode::CREATED, Json(wallet)))
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
    State(state): State<Gateway>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, ApiError> {
    Ok(Json(state.wallet(&path.id).await?))
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
    State(state): State<Gateway>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Balance>, ApiError> {
    Ok(Json(state.balance(&path.id).await?))
}
