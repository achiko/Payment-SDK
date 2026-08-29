use axum::{
    Json,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub struct ErrorBody {
    pub message: String,
    /// Transactions accepted before an ordered batch failed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    pub transaction_ids: Vec<String>,
    /// Zero-based transfer index whose validation or submission failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_index: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
enum ErrorKind {
    InvalidRequest,
    NotFound,
    Conflict,
    Transaction,
    Unavailable,
    InvalidResponse,
}

#[derive(Debug)]
pub struct ApiError {
    kind: ErrorKind,
    message: String,
    transaction_ids: Vec<String>,
    failed_index: Option<usize>,
}

impl ApiError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest, message)
    }

    pub fn invalid_json(_error: JsonRejection) -> Self {
        Self::invalid_request("request body must match the documented JSON schema")
    }

    pub fn invalid_batch(failed_index: usize, message: impl Into<String>) -> Self {
        Self {
            failed_index: Some(failed_index),
            ..Self::invalid_request(message)
        }
    }

    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidResponse, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            transaction_ids: Vec::new(),
            failed_index: None,
        }
    }
}

impl From<wallets::Error> for ApiError {
    fn from(error: wallets::Error) -> Self {
        let kind = match error.kind {
            wallets::ErrorKind::Unsupported
            | wallets::ErrorKind::InvalidSecret
            | wallets::ErrorKind::InvalidAddress
            | wallets::ErrorKind::InvalidAmount
            | wallets::ErrorKind::InvalidBatch
            | wallets::ErrorKind::AddressMismatch => ErrorKind::InvalidRequest,
            wallets::ErrorKind::Duplicate | wallets::ErrorKind::Conflict => ErrorKind::Conflict,
            wallets::ErrorKind::NotFound => ErrorKind::NotFound,
            wallets::ErrorKind::Transaction => ErrorKind::Transaction,
            wallets::ErrorKind::Unavailable
            | wallets::ErrorKind::Generation
            | wallets::ErrorKind::Balance
            | wallets::ErrorKind::History => ErrorKind::Unavailable,
        };
        Self::new(kind, error.message)
    }
}

impl From<wallets::SendError> for ApiError {
    fn from(error: wallets::SendError) -> Self {
        let mut response = Self::from(error.source);
        response.transaction_ids = error
            .accepted
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        response.failed_index = error.failed_index;
        response
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.kind {
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
                message: self.message,
                transaction_ids: self.transaction_ids,
                failed_index: self.failed_index,
            }),
        )
            .into_response()
    }
}
