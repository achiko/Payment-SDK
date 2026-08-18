use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{BatchError, Error, ErrorKind};

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub message: String,
    /// Transactions accepted before a sequential batch failed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    pub transaction_ids: Vec<String>,
    /// Zero-based transfer index whose validation or submission failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_index: Option<usize>,
}

pub struct ApiError {
    error: Error,
    transaction_ids: Vec<String>,
    failed_index: Option<usize>,
}

impl ApiError {
    pub fn encoding() -> Self {
        Error::new(
            ErrorKind::InvalidResponse,
            "wallet result could not be encoded",
        )
        .into()
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        Self {
            error,
            transaction_ids: Vec::new(),
            failed_index: None,
        }
    }
}

impl From<BatchError> for ApiError {
    fn from(error: BatchError) -> Self {
        Self {
            error: error.error,
            transaction_ids: error.transaction_ids,
            failed_index: Some(error.failed_index),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.error.kind {
            ErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::Conflict => StatusCode::CONFLICT,
            ErrorKind::Transaction => StatusCode::UNPROCESSABLE_ENTITY,
            ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::InvalidResponse => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(ErrorBody {
                message: self.error.message,
                transaction_ids: self.transaction_ids,
                failed_index: self.failed_index,
            }),
        )
            .into_response()
    }
}
