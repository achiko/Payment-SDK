use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    error::{ApiError, ErrorBody},
    wallet::WalletPath,
};
use crate::{
    Gateway, HistoryQuery, SendFunds, Submission, TransactionPage, TransferRequest,
    TransferResponse, WalletSend,
};

pub fn routes() -> OpenApiRouter<Gateway> {
    OpenApiRouter::new()
        .routes(routes!(read))
        .routes(routes!(send))
        .routes(routes!(send_all))
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/transactions",
    params(WalletPath, HistoryQuery),
    responses(
        (status = 200, body = TransactionPage),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 500, body = ErrorBody),
        (status = 503, body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn read(
    State(state): State<Gateway>,
    Path(path): Path<WalletPath>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<TransactionPage>, ApiError> {
    let request = query.try_into()?;
    Ok(Json(state.history(&path.id, request).await?.try_into()?))
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/transactions",
    params(WalletPath),
    request_body = SendFunds,
    responses(
        (status = 202, description = "Transaction submitted", body = Submission),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 422, body = ErrorBody),
        (status = 503, body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn send(
    State(state): State<Gateway>,
    Path(path): Path<WalletPath>,
    Json(request): Json<SendFunds>,
) -> Result<(StatusCode, Json<Submission>), ApiError> {
    let (destination, amount) = request.try_into()?;
    let id = state.send(&path.id, destination, amount).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Submission {
            transaction_id: id.to_string(),
        }),
    ))
}

#[utoipa::path(
    post,
    path = "/v1/transactions",
    description = "Submits one same-chain batch. Mixed-chain requests are rejected before any transaction is submitted. Bitcoin may group transfers into one transaction; Ethereum submits nonce-ordered transactions. A failure response preserves accepted transaction IDs and the failed request index.",
    request_body = TransferRequest,
    responses(
        (status = 202, description = "Native batch submitted", body = TransferResponse),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 422, description = "Batch failed; accepted transaction IDs identify partial submission", body = ErrorBody),
        (status = 503, body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn send_all(
    State(state): State<Gateway>,
    Json(request): Json<TransferRequest>,
) -> Result<(StatusCode, Json<TransferResponse>), ApiError> {
    let transfers: Vec<WalletSend> = request.try_into()?;
    let transaction_ids = state
        .send_all(transfers)
        .await?
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    Ok((
        StatusCode::ACCEPTED,
        Json(TransferResponse { transaction_ids }),
    ))
}
