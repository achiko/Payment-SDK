use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use deposits::{
    DepositBalances, DepositId, LedgerEntry, LedgerEntryCause, LedgerObservationKind, LedgerQuery,
};
use indexing::{BlockRef, ConfirmationProof, TransactionStatus};
use serde::{Deserialize, Serialize};

use crate::{AssetResponse, Deposits, deposit_http::HttpError};

pub(super) fn ledger_routes(deposits: Arc<Deposits>) -> Router {
    Router::new()
        .route("/v1/deposits/{id}/balance", get(balance))
        .route("/v1/deposits/{id}/history", get(history))
        .with_state(deposits)
}

async fn balance(
    State(deposits): State<Arc<Deposits>>,
    Path(id): Path<String>,
) -> Result<Json<BalanceResponse>, HttpError> {
    let id = DepositId(id);
    let deposit = deposits
        .get(&id)
        .await?
        .ok_or_else(|| HttpError::not_found("deposit does not exist"))?;
    let entry = deposits
        .head(&id)
        .await?
        .ok_or_else(|| HttpError::unavailable("deposit has no ledger head"))?;
    Ok(Json(BalanceResponse {
        deposit_id: id.0,
        asset: AssetResponse {
            chain: deposit.asset.chain.0,
            asset: deposit.asset.asset,
        },
        entry: entry.into(),
    }))
}

#[derive(Deserialize)]
pub struct HistoryFilter {
    pub after: Option<String>,
    pub limit: Option<usize>,
}

async fn history(
    State(deposits): State<Arc<Deposits>>,
    Path(id): Path<String>,
    Query(filter): Query<HistoryFilter>,
) -> Result<Json<HistoryResponse>, HttpError> {
    let id = DepositId(id);
    let deposit = deposits
        .get(&id)
        .await?
        .ok_or_else(|| HttpError::not_found("deposit does not exist"))?;
    let limit = filter.limit.unwrap_or(100);
    if limit == 0 || limit > 1_000 {
        return Err(HttpError::bad_request("limit must be between 1 and 1000"));
    }
    let page = deposits
        .history(LedgerQuery {
            deposit_id: id.clone(),
            after: filter.after.map(deposits::EntryId),
            limit,
        })
        .await?;
    Ok(Json(HistoryResponse {
        deposit_id: id.0,
        asset: AssetResponse {
            chain: deposit.asset.chain.0,
            asset: deposit.asset.asset,
        },
        entries: page.entries.into_iter().map(Into::into).collect(),
        next: page.next.map(|entry| entry.0),
    }))
}

#[derive(Serialize)]
pub struct BalanceResponse {
    pub deposit_id: String,
    pub asset: AssetResponse,
    pub entry: EntryResponse,
}

#[derive(Serialize)]
pub struct HistoryResponse {
    pub deposit_id: String,
    pub asset: AssetResponse,
    pub entries: Vec<EntryResponse>,
    pub next: Option<String>,
}

#[derive(Serialize)]
pub struct EntryResponse {
    pub id: String,
    pub previous: Option<String>,
    pub cause: CauseResponse,
    pub balances: BalancesResponse,
    pub recorded_at: u64,
}

#[derive(Serialize)]
pub struct BalancesResponse {
    pub received: String,
    pub confirmed: String,
    pub balance: String,
    pub collected: String,
    pub accounted: String,
}

impl From<DepositBalances> for BalancesResponse {
    fn from(value: DepositBalances) -> Self {
        Self {
            received: value.received.to_string(),
            confirmed: value.confirmed.to_string(),
            balance: value.balance.to_string(),
            collected: value.collected.to_string(),
            accounted: value.accounted.to_string(),
        }
    }
}

impl From<LedgerEntry> for EntryResponse {
    fn from(value: LedgerEntry) -> Self {
        Self {
            id: value.id.0,
            previous: value.previous.map(|entry| entry.0),
            cause: value.cause.into(),
            balances: value.balances.into(),
            recorded_at: value.recorded_at,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CauseResponse {
    Opened {
        idempotency_key: String,
    },
    Observation {
        projection_id: String,
        event_id: String,
        revision: u64,
        status: StatusResponse,
        effect: &'static str,
        movement_ids: Vec<String>,
        network_fee: Option<String>,
    },
    Accounting {
        idempotency_key: String,
        reason: String,
    },
    Reconciliation {
        case_id: String,
        idempotency_key: String,
        reason: String,
    },
}

impl From<LedgerEntryCause> for CauseResponse {
    fn from(value: LedgerEntryCause) -> Self {
        match value {
            LedgerEntryCause::Opened { idempotency_key } => Self::Opened {
                idempotency_key: idempotency_key.0,
            },
            LedgerEntryCause::Observation {
                projection_id,
                event_id,
                observation_revision,
                status,
                kind,
                movement_ids,
                network_fee,
            } => Self::Observation {
                projection_id: projection_id.0,
                event_id: event_id.0,
                revision: observation_revision.0,
                status: status.into(),
                effect: effect_name(kind),
                movement_ids: movement_ids.into_iter().map(|id| id.0).collect(),
                network_fee: network_fee.map(|amount| amount.to_string()),
            },
            LedgerEntryCause::Accounting {
                idempotency_key,
                reason,
            } => Self::Accounting {
                idempotency_key: idempotency_key.0,
                reason,
            },
            LedgerEntryCause::ReconciliationResolution {
                case_id,
                idempotency_key,
                reason,
            } => Self::Reconciliation {
                case_id: case_id.0,
                idempotency_key: idempotency_key.0,
                reason,
            },
        }
    }
}

const fn effect_name(kind: LedgerObservationKind) -> &'static str {
    match kind {
        LedgerObservationKind::Incoming => "incoming",
        LedgerObservationKind::Collection => "collection",
        LedgerObservationKind::GasFunding => "gas_funding",
        LedgerObservationKind::OtherBalanceChange => "other_balance_change",
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StatusResponse {
    Pending,
    Included {
        block: BlockResponse,
        confirmations: u64,
    },
    Confirmed {
        block: BlockResponse,
        proof: ProofResponse,
    },
    Failed {
        block: Option<BlockResponse>,
        reason: Option<String>,
    },
    Replaced {
        chain: String,
        network: String,
        transaction_id: String,
    },
    Dropped,
    Reorged {
        previous_block: BlockResponse,
    },
}

impl From<TransactionStatus> for StatusResponse {
    fn from(value: TransactionStatus) -> Self {
        match value {
            TransactionStatus::Pending => Self::Pending,
            TransactionStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: block.into(),
                confirmations,
            },
            TransactionStatus::Confirmed { block, proof } => Self::Confirmed {
                block: block.into(),
                proof: proof.into(),
            },
            TransactionStatus::Failed { block, reason } => Self::Failed {
                block: block.map(Into::into),
                reason,
            },
            TransactionStatus::Replaced { by } => Self::Replaced {
                chain: by.scope.chain.0,
                network: by.scope.network,
                transaction_id: by.value,
            },
            TransactionStatus::Dropped => Self::Dropped,
            TransactionStatus::Reorged { previous_block } => Self::Reorged {
                previous_block: previous_block.into(),
            },
        }
    }
}

#[derive(Serialize)]
pub struct BlockResponse {
    pub height: u64,
    pub hash: String,
    pub parent_hash: Option<String>,
    pub timestamp: Option<u64>,
}

impl From<BlockRef> for BlockResponse {
    fn from(value: BlockRef) -> Self {
        Self {
            height: value.height.0,
            hash: hex::encode(value.hash.0),
            parent_hash: value.parent_hash.map(|hash| hex::encode(hash.0)),
            timestamp: value.timestamp,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProofResponse {
    Depth { required: u64, observed: u64 },
    ChainFinalized,
    DepthAndChainFinalized { required: u64, observed: u64 },
}

impl From<ConfirmationProof> for ProofResponse {
    fn from(value: ConfirmationProof) -> Self {
        match value {
            ConfirmationProof::Depth { required, observed } => Self::Depth { required, observed },
            ConfirmationProof::ChainFinalized => Self::ChainFinalized,
            ConfirmationProof::DepthAndChainFinalized { required, observed } => {
                Self::DepthAndChainFinalized { required, observed }
            }
        }
    }
}
