use axum::http::StatusCode;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::Gateway;

pub fn routes() -> OpenApiRouter<Gateway> {
    OpenApiRouter::new()
        .routes(routes!(live))
        .routes(routes!(ready))
}

#[utoipa::path(
    get,
    path = "/health/live",
    responses((status = 204, description = "Process is alive")),
    tag = "health"
)]
async fn live() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[utoipa::path(
    get,
    path = "/health/ready",
    responses((status = 204, description = "Wallets and indexes are ready")),
    tag = "health"
)]
async fn ready() -> StatusCode {
    // Runtime construction waits for every configured index before binding.
    StatusCode::NO_CONTENT
}
