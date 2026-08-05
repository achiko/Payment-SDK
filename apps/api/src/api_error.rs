use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use deposits::{DepositError, DepositErrorKind};
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    body: ErrorEnvelope,
}

impl ApiError {
    #[must_use]
    pub fn new(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            body: ErrorEnvelope {
                code: code.into(),
                message: message.into(),
                retryable,
                request_id: request_id(),
            },
        }
    }

    #[must_use]
    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, false)
    }

    #[must_use]
    pub fn not_found(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message, false)
    }

    #[must_use]
    pub fn conflict(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message, false)
    }

    #[must_use]
    pub fn from_deposit(error: DepositError) -> Self {
        match error.kind {
            DepositErrorKind::NotFound => Self::not_found(
                "resource_not_found",
                "Payment Service resource does not exist",
            ),
            DepositErrorKind::Conflict => Self::conflict("conflict", error.message),
            DepositErrorKind::InvalidState => Self::conflict("invalid_state", error.message),
            DepositErrorKind::InvariantViolation => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_command",
                error.message,
                false,
            ),
            DepositErrorKind::Storage => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                "Payment Service storage is unavailable",
                true,
            ),
            DepositErrorKind::Other => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Payment Service could not complete the request",
                false,
            ),
        }
    }

    #[cfg(test)]
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    code: String,
    message: String,
    retryable: bool,
    request_id: String,
}

fn request_id() -> String {
    format!("ps-request-{}", Uuid::now_v7())
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use serde_json::Value;

    use super::*;

    #[tokio::test]
    async fn storage_errors_are_retryable_and_do_not_expose_internal_text() {
        let error = ApiError::from_deposit(DepositError {
            kind: DepositErrorKind::Storage,
            message: "database at /secret/path failed with credential=secret".to_owned(),
        });
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        let response = error.into_response();
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("error response must be readable");
        let decoded: Value = serde_json::from_slice(&body).expect("error body must be JSON");
        assert_eq!(decoded["code"], "storage_unavailable");
        assert_eq!(decoded["retryable"], true);
        assert!(!String::from_utf8_lossy(&body).contains("secret"));
    }

    #[tokio::test]
    async fn conflicts_keep_safe_domain_context_and_have_uuid_request_ids() {
        let response = ApiError::from_deposit(DepositError {
            kind: DepositErrorKind::Conflict,
            message: "idempotency key was reused with different content".to_owned(),
        })
        .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("error response must be readable");
        let decoded: Value = serde_json::from_slice(&body).expect("error body must be JSON");
        assert_eq!(
            decoded["message"],
            "idempotency key was reused with different content"
        );
        assert!(
            decoded["request_id"]
                .as_str()
                .expect("request ID must be a string")
                .starts_with("ps-request-")
        );
    }
}
