use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use deposits::{
    Collection, CollectionId, CollectionLeg, CollectionLegKind, CollectionLegState, CollectionState,
};
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{PlanRequest, Planner, Sweeps, deposit_http::HttpError};

/// Supplies durable event timestamps to collection execution.
pub trait Clock: Send + Sync {
    fn now(&self) -> u64;
}

#[derive(Clone, Copy, Debug)]
pub struct SystemClock {
    epoch: SystemTime,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self { epoch: UNIX_EPOCH }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(self.epoch)
            .unwrap_or_default()
            .as_secs()
    }
}

#[derive(Clone)]
struct SweepState {
    sweeps: Arc<Sweeps>,
    clock: Arc<dyn Clock>,
}

/// UTXO sweep execution and status routes.
pub fn sweep_routes(sweeps: Arc<Sweeps>, clock: Arc<dyn Clock>) -> Router {
    Router::new()
        .route("/v1/collections/{id}", get(status))
        .route("/v1/collections/{id}/execute", post(execute))
        .with_state(SweepState { sweeps, clock })
}

#[derive(Clone)]
struct PlanState {
    planner: Arc<Planner>,
}

/// Creates durable collections from server-loaded chain evidence.
pub fn plan_routes(planner: Arc<Planner>) -> Router {
    Router::new()
        .route("/v1/collections", post(plan))
        .with_state(PlanState { planner })
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PlanBody {
    id: String,
    job_id: String,
    deposit_ids: Vec<String>,
    created_at: u64,
}

async fn plan(
    State(state): State<PlanState>,
    headers: HeaderMap,
    Json(body): Json<PlanBody>,
) -> Result<Json<SweepResponse>, HttpError> {
    require_identity(&headers, &body.id)?;
    let encoded = serde_json::to_vec(&body)
        .map_err(|_| HttpError::bad_request("collection request could not be encoded"))?;
    let hash: [u8; 32] = Sha256::digest(encoded).into();
    state
        .planner
        .plan(PlanRequest {
            collection_id: deposits::CollectionId(body.id.clone()),
            job_id: deposits::JobId(body.job_id),
            principal: deposits::CommandPrincipal("payment-api".to_owned()),
            idempotency_key: deposits::IdempotencyKey(body.id),
            request_hash: deposits::RequestHash(hash),
            deposit_ids: body
                .deposit_ids
                .into_iter()
                .map(deposits::DepositId)
                .collect(),
            created_at: body.created_at,
        })
        .await
        .map(SweepResponse::from)
        .map(Json)
        .map_err(Into::into)
}

async fn execute(
    State(state): State<SweepState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SweepResponse>, HttpError> {
    require_identity(&headers, &id)?;
    state
        .sweeps
        .execute(&CollectionId(id), state.clock.now())
        .await
        .map(SweepResponse::from)
        .map(Json)
        .map_err(Into::into)
}

async fn status(
    State(state): State<SweepState>,
    Path(id): Path<String>,
) -> Result<Json<SweepResponse>, HttpError> {
    state
        .sweeps
        .get(&CollectionId(id))
        .await?
        .map(SweepResponse::from)
        .map(Json)
        .ok_or_else(|| HttpError::not_found("collection does not exist"))
}

fn require_identity(headers: &HeaderMap, id: &str) -> Result<(), HttpError> {
    let supplied = headers
        .get("idempotency-key")
        .ok_or_else(|| HttpError::bad_request("idempotency-key header is required"))?
        .to_str()
        .map_err(|_| HttpError::bad_request("idempotency-key header is invalid"))?;
    if supplied != id {
        return Err(HttpError::bad_request(
            "idempotency-key must equal the durable collection ID",
        ));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct SweepResponse {
    pub id: String,
    pub mode: &'static str,
    pub state: &'static str,
    pub asset: SweepAsset,
    pub destination: SweepAddress,
    pub legs: Vec<SweepLeg>,
}

#[derive(Serialize)]
pub struct SweepAsset {
    pub chain: String,
    pub asset: String,
}

#[derive(Serialize)]
pub struct SweepAddress {
    pub chain: String,
    pub network: String,
    pub value: String,
}

#[derive(Serialize)]
pub struct SweepLeg {
    pub id: String,
    pub kind: &'static str,
    pub state: &'static str,
    pub transaction_id: Option<String>,
    pub watch_id: Option<String>,
    pub attempts: u32,
    pub allocations: Vec<SweepAllocation>,
}

/// Safe factual value attribution without signing material or chain evidence.
#[derive(Serialize)]
pub struct SweepAllocation {
    pub deposit_id: String,
    pub gross_debit: String,
    pub master_credit: String,
    pub allocated_fee: String,
}

impl From<Collection> for SweepResponse {
    fn from(value: Collection) -> Self {
        Self {
            id: value.id.0,
            mode: match value.mode {
                deposits::CollectionMode::AccountTransfer => "account_transfer",
                deposits::CollectionMode::UtxoBatch => "utxo_batch",
                deposits::CollectionMode::TokenWithGas => "token_with_gas",
            },
            state: collection_state(value.state),
            asset: SweepAsset {
                chain: value.asset.chain.0,
                asset: value.asset.asset,
            },
            destination: SweepAddress {
                chain: value.destination.scope.chain.0,
                network: value.destination.scope.network,
                value: value.destination.value,
            },
            legs: value.legs.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<CollectionLeg> for SweepLeg {
    fn from(value: CollectionLeg) -> Self {
        Self {
            id: value.id.0,
            kind: match value.kind {
                CollectionLegKind::GasFunding => "gas_funding",
                CollectionLegKind::Sweep => "sweep",
            },
            state: leg_state(&value.state),
            transaction_id: value
                .state
                .transaction_id()
                .map(|transaction| transaction.value.clone()),
            watch_id: value.watch_id.map(|watch| watch.0),
            attempts: value.attempt_count,
            allocations: value
                .allocations
                .into_iter()
                .map(|allocation| SweepAllocation {
                    deposit_id: allocation.deposit_id.0,
                    gross_debit: allocation.gross_debit.to_string(),
                    master_credit: allocation.master_credit.to_string(),
                    allocated_fee: allocation.allocated_fee.to_string(),
                })
                .collect(),
        }
    }
}

const fn collection_state(state: CollectionState) -> &'static str {
    match state {
        CollectionState::Required => "required",
        CollectionState::InProgress => "in_progress",
        CollectionState::Completed => "completed",
        CollectionState::Failed => "failed",
        CollectionState::Reorged => "reorged",
    }
}

const fn leg_state(state: &CollectionLegState) -> &'static str {
    match state {
        CollectionLegState::Required => "required",
        CollectionLegState::Signed { .. } => "signed",
        CollectionLegState::Broadcast { .. } => "broadcast",
        CollectionLegState::Confirmed { .. } => "confirmed",
        CollectionLegState::Failed { .. } => "failed",
        CollectionLegState::Reorged { .. } => "reorged",
    }
}
