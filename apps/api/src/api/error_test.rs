use axum::{http::StatusCode, response::IntoResponse};
use base::Id;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use wallets::{Error, ErrorKind, SendError};

use super::ApiError;

fn ambiguous_error(message: &str, id: &str) -> Error {
    Error {
        kind: ErrorKind::Transaction,
        message: message.to_owned(),
        ambiguous_transaction_id: Some(Id::new(id)),
    }
}

async fn projection(error: ApiError) -> (StatusCode, Value) {
    let response = error.into_response();
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("error response body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).expect("error response JSON");
    (status, body)
}

#[tokio::test]
async fn ambiguity_and_definite_failures_project_only_truthful_metadata() {
    let (status, body) =
        projection(ambiguous_error("single ambiguous", "single-local").into()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        json!({
            "message": "single ambiguous",
            "ambiguous_transaction_id": "single-local"
        })
    );

    let item = SendError::item(
        1,
        vec![Id::new("accepted-0")],
        ambiguous_error("item ambiguous", "item-local"),
    );
    let (status, body) = projection(item.into()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        json!({
            "message": "item ambiguous",
            "transaction_ids": ["accepted-0"],
            "failed_index": 1,
            "ambiguous_transaction_id": "item-local"
        })
    );

    let grouped = SendError::grouped(
        vec![Id::new("accepted-group")],
        ambiguous_error("grouped ambiguous", "grouped-local"),
    );
    let (status, body) = projection(grouped.into()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        json!({
            "message": "grouped ambiguous",
            "transaction_ids": ["accepted-group"],
            "ambiguous_transaction_id": "grouped-local"
        })
    );

    for (error, status, body) in [
        (
            Error::new(ErrorKind::Transaction, "definite single").into(),
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"message": "definite single"}),
        ),
        (
            SendError::collection(ErrorKind::InvalidBatch, "collection failure").into(),
            StatusCode::BAD_REQUEST,
            json!({"message": "collection failure"}),
        ),
        (
            SendError::operation(ErrorKind::Unavailable, "operation failure").into(),
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"message": "operation failure"}),
        ),
        (
            SendError::item(
                3,
                Vec::new(),
                Error::new(ErrorKind::SourceBusy, "source is busy"),
            )
            .into(),
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"message": "source is busy", "failed_index": 3}),
        ),
        (
            SendError::operation(ErrorKind::SourceBusy, "single source is busy").into(),
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"message": "single source is busy"}),
        ),
        (
            SendError::item(
                2,
                Vec::new(),
                Error::new(ErrorKind::Transaction, "item failure"),
            )
            .into(),
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"message": "item failure", "failed_index": 2}),
        ),
        (
            SendError::grouped(
                vec![Id::new("accepted-definite")],
                Error::new(ErrorKind::Transaction, "grouped failure"),
            )
            .into(),
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "message": "grouped failure",
                "transaction_ids": ["accepted-definite"]
            }),
        ),
    ] {
        assert_eq!(projection(error).await, (status, body));
    }
}
