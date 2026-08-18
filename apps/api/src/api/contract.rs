use std::sync::Arc;

use axum::{Extension, Json};
use utoipa::openapi::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::error::{ApiError, ErrorBody};
use crate::Gateway;

pub fn routes() -> OpenApiRouter<Gateway> {
    OpenApiRouter::new().routes(routes!(read))
}

#[utoipa::path(
    get,
    path = "/openapi.json",
    responses(
        (status = 200, description = "OpenAPI 3 contract", body = Object),
        (status = 500, body = ErrorBody)
    ),
    tag = "openapi"
)]
async fn read(
    Extension(contract): Extension<Arc<OpenApi>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    serde_json::to_value(contract.as_ref())
        .map(Json)
        .map_err(|_| ApiError::encoding())
}
