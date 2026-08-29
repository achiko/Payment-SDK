use axum::{
    Json,
    extract::{FromRequestParts, Path, Query, State, rejection::JsonRejection},
    http::{StatusCode, request::Parts},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    State as HttpState,
    contract::{AddressInput, WalletPath},
    cursor::HistoryCursor,
    error::{ApiError, ErrorBody},
};

pub fn routes() -> OpenApiRouter<HttpState> {
    OpenApiRouter::new()
        .routes(routes!(read))
        .routes(routes!(send))
        .routes(routes!(send_all))
}

enum TransactionQuery {
    AbsentOrEmpty,
}

impl<S> FromRequestParts<S> for TransactionQuery
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if parts.uri.query().is_some_and(|query| !query.is_empty()) {
            return Err(ApiError::invalid_request(
                "transaction query parameters are not supported",
            ));
        }
        Ok(Self::AbsentOrEmpty)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Scope {
    pub chain: String,
    pub network: String,
}

impl From<indexing::IndexScope> for Scope {
    fn from(value: indexing::IndexScope) -> Self {
        Self {
            chain: value.chain.0,
            network: value.network,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Address {
    pub scope: Scope,
    pub value: String,
}

impl From<indexing::CanonicalAddress> for Address {
    fn from(value: indexing::CanonicalAddress) -> Self {
        Self {
            scope: value.scope.into(),
            value: value.value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Asset {
    pub chain: String,
    pub id: String,
    pub name: Option<String>,
    pub ticker: Option<String>,
    pub decimals: u32,
}

impl From<wallets::HistoryAsset> for Asset {
    fn from(value: wallets::HistoryAsset) -> Self {
        Self {
            chain: value.id.chain.0,
            id: value.id.asset,
            name: value.name,
            ticker: value.ticker,
            decimals: value.decimals,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MovementKind {
    Transfer,
    Input,
    Output,
    Mint,
    Burn,
}

impl From<indexing::MovementKind> for MovementKind {
    fn from(value: indexing::MovementKind) -> Self {
        match value {
            indexing::MovementKind::Transfer => Self::Transfer,
            indexing::MovementKind::Input => Self::Input,
            indexing::MovementKind::Output => Self::Output,
            indexing::MovementKind::Mint => Self::Mint,
            indexing::MovementKind::Burn => Self::Burn,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Movement {
    pub id: String,
    pub kind: MovementKind,
    pub asset: Asset,
    pub amount: String,
    pub from: Option<Address>,
    pub to: Option<Address>,
}

impl From<wallets::HistoryMovement> for Movement {
    fn from(value: wallets::HistoryMovement) -> Self {
        Self {
            id: value.id.0,
            kind: value.kind.into(),
            asset: value.asset.into(),
            amount: value.amount.to_string(),
            from: value.from.map(Into::into),
            to: value.to.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Fee {
    pub asset: Asset,
    pub amount: String,
    pub payer: Option<Address>,
}

impl From<wallets::HistoryFee> for Fee {
    fn from(value: wallets::HistoryFee) -> Self {
        Self {
            asset: value.asset.into(),
            amount: value.amount.to_string(),
            payer: value.payer.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Block {
    pub position: u64,
    pub height: u64,
    pub hash: String,
    pub parent: Option<ParentBlock>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct ParentBlock {
    pub position: u64,
    pub hash: String,
}

impl From<base::BlockRef> for Block {
    fn from(value: base::BlockRef) -> Self {
        Self {
            position: value.position.0,
            height: value.height.0,
            hash: hex::encode(value.hash.0),
            parent: value.parent.map(|parent| ParentBlock {
                position: parent.position.0,
                hash: hex::encode(parent.hash.0),
            }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    Included {
        block: Block,
        confirmations: u64,
    },
    Confirmed {
        block: Block,
        confirmations: u64,
    },
    Failed {
        block: Block,
        reason: Option<String>,
    },
}

impl From<wallets::HistoryStatus> for Status {
    fn from(value: wallets::HistoryStatus) -> Self {
        match value {
            wallets::HistoryStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: block.into(),
                confirmations,
            },
            wallets::HistoryStatus::Confirmed {
                block,
                confirmations,
            } => Self::Confirmed {
                block: block.into(),
                confirmations,
            },
            wallets::HistoryStatus::Failed { block, reason } => Self::Failed {
                block: block.into(),
                reason,
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Transaction {
    pub scope: Scope,
    pub transaction_id: String,
    pub status: Status,
    pub movements: Vec<Movement>,
    pub fee: Option<Fee>,
}

impl From<wallets::HistoryEntry> for Transaction {
    fn from(value: wallets::HistoryEntry) -> Self {
        Self {
            scope: value.scope.into(),
            transaction_id: value.transaction_id.value,
            status: value.status.into(),
            movements: value.movements.into_iter().map(Into::into).collect(),
            fee: value.fee.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct TransactionPage {
    pub checkpoint: Option<Block>,
    pub transactions: Vec<Transaction>,
    pub next_cursor: Option<String>,
}

impl TryFrom<wallets::History> for TransactionPage {
    type Error = ApiError;

    fn try_from(history: wallets::History) -> Result<Self, Self::Error> {
        let next_cursor = history
            .next
            .as_ref()
            .map(HistoryCursor::encode)
            .transpose()?;
        Ok(Self {
            checkpoint: history.checkpoint.map(Into::into),
            transactions: history.transactions.into_iter().map(Into::into).collect(),
            next_cursor,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct HistoryQuery {
    pub cursor: Option<String>,
    #[serde(default = "HistoryQuery::default_limit")]
    #[param(minimum = 1, maximum = 1000, default = 100)]
    pub limit: usize,
}

impl TryFrom<HistoryQuery> for wallets::HistoryRequest {
    type Error = ApiError;

    fn try_from(query: HistoryQuery) -> Result<Self, Self::Error> {
        if query.limit == 0 || query.limit > 1_000 {
            return Err(ApiError::invalid_request(
                "history limit must be between 1 and 1000",
            ));
        }
        let after = query
            .cursor
            .as_deref()
            .map(HistoryCursor::decode)
            .transpose()?;
        Ok(Self {
            after,
            limit: query.limit,
        })
    }
}

impl HistoryQuery {
    const fn default_limit() -> usize {
        100
    }
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/transactions",
    params(WalletPath, HistoryQuery),
    responses(
        (status = 200, body = TransactionPage),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 409, description = "Cursor checkpoint is no longer canonical", body = ErrorBody),
        (status = 500, body = ErrorBody),
        (status = 503, body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn read(
    State(state): State<HttpState>,
    Path(path): Path<WalletPath>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<TransactionPage>, ApiError> {
    let request = query.try_into()?;
    Ok(Json(
        state.wallets.history(&path.id, request).await?.try_into()?,
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SendFunds {
    pub destination: AddressInput,
    pub amount: String,
}

impl TryFrom<SendFunds> for (wallets::AddressText, base::Decimal) {
    type Error = ApiError;

    fn try_from(request: SendFunds) -> Result<Self, Self::Error> {
        let amount = request
            .amount
            .parse()
            .map_err(|_| ApiError::invalid_request("amount must be an exact base-10 decimal"))?;
        Ok((request.destination.into(), amount))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Submission {
    pub transaction_id: String,
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/transactions",
    description = "Submits one transfer through the wallet's configured asset family. The native SOL integration is reserved for this shared route; no Solana-only transaction route is defined. A 202 response means submitted, not confirmed.",
    params(WalletPath),
    request_body = SendFunds,
    responses(
        (status = 202, description = "Transaction submitted", body = Submission),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 422, body = ErrorBody),
        (status = 503, description = "Transaction unavailable or exact-envelope submission outcome ambiguous; ambiguous_transaction_id is present only for an unknown outcome", body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn send(
    State(state): State<HttpState>,
    Path(path): Path<WalletPath>,
    _query: TransactionQuery,
    request: Result<Json<SendFunds>, JsonRejection>,
) -> Result<(StatusCode, Json<Submission>), ApiError> {
    let Json(request) = request.map_err(ApiError::invalid_json)?;
    let (destination, amount) = request.try_into()?;
    let id = state.wallets.send(&path.id, destination, amount).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Submission {
            transaction_id: id.to_string(),
        }),
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WalletTransfer {
    pub wallet_id: String,
    pub destination: AddressInput,
    pub amount: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferRequest {
    /// Transfers for one configured asset family, in execution order.
    #[schema(min_items = 1, max_items = 50)]
    pub transfers: Vec<WalletTransfer>,
}

impl TryFrom<TransferRequest> for Vec<wallets::WalletTransfer<String>> {
    type Error = ApiError;

    fn try_from(request: TransferRequest) -> Result<Self, Self::Error> {
        if request.transfers.len() > wallets::MAX_TRANSFERS {
            return Err(ApiError::invalid_request(format!(
                "at most {} transfers are allowed",
                wallets::MAX_TRANSFERS
            )));
        }
        request
            .transfers
            .into_iter()
            .enumerate()
            .map(|(failed_index, transfer)| {
                let amount = transfer.amount.parse().map_err(|_| {
                    ApiError::invalid_batch(failed_index, "amount must be an exact base-10 decimal")
                })?;
                Ok(wallets::WalletTransfer {
                    wallet: transfer.wallet_id,
                    to: transfer.destination.into(),
                    amount,
                })
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct TransferResponse {
    /// Accepted transaction IDs in chain-native submission order.
    pub transaction_ids: Vec<String>,
}

#[utoipa::path(
    post,
    path = "/v1/transactions",
    description = "Submits one ordered exact-asset batch through the configured wallet family. The native SOL integration is reserved for this shared route; no Solana-only transaction route is defined. Requests mixing wallet families are rejected before any transaction is submitted. Bitcoin may group occurrences into one transaction. Ethereum reserves consecutive nonces per sender and prepares the whole batch before submitting exact envelopes in request order. Definitely acknowledged transaction IDs are returned only when a non-empty prefix was accepted, and a failed index is returned only when one original occurrence truthfully failed.",
    request_body = TransferRequest,
    responses(
        (status = 202, description = "Asset batch submitted", body = TransferResponse),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 422, description = "Definite batch failure; transaction_ids is present only for a definitely acknowledged prefix and failed_index only for a truthful item-scoped failure", body = ErrorBody),
        (status = 503, description = "Batch unavailable or exact-envelope submission outcome ambiguous; transaction_ids is present only for a definitely acknowledged prefix, failed_index only for a truthful item-scoped failure, and ambiguous_transaction_id only for an unknown outcome", body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn send_all(
    State(state): State<HttpState>,
    _query: TransactionQuery,
    request: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<TransferResponse>), ApiError> {
    let Json(request) = request.map_err(ApiError::invalid_json)?;
    let transaction_ids = state
        .wallets
        .send_all(request.try_into()?)
        .await?
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    Ok((
        StatusCode::ACCEPTED,
        Json(TransferResponse { transaction_ids }),
    ))
}

#[cfg(test)]
#[path = "transaction_test.rs"]
mod tests;
