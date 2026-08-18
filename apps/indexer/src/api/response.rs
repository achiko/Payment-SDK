use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chain_bitcoin::format_bitcoin_block_hash;
use http::server::AuthenticationMode;
use indexing::{
    ChainId, IndexError, IndexErrorKind, IndexScope, IndexedOutput, ObservationEvent,
    ObservedTransaction, SyncPhase, SyncStatus, TransactionStatus, WatchReceipt, WatchSelector,
};
use serde::Serialize;

use super::*;

#[derive(Serialize)]
pub(crate) struct StatusDto {
    scope: ScopeDto,
    authentication_mode: &'static str,
    phase: &'static str,
    checkpoint: Option<BlockDto>,
    observed_tip: Option<BlockDto>,
    confirmation_depth: String,
    require_chain_finality: bool,
    rebuild_reason: Option<String>,
    halted_reason: Option<String>,
}

impl StatusDto {
    pub(crate) fn try_from_status(
        status: SyncStatus,
        authentication_mode: AuthenticationMode,
    ) -> Result<Self, IndexError> {
        let chain = status.scope.chain.clone();
        Ok(Self {
            scope: ScopeDto::from(&status.scope),
            authentication_mode: authentication_mode.as_str(),
            phase: phase_name(status.phase),
            checkpoint: status
                .checkpoint
                .map(|block| BlockDto::try_from_block(block, &chain))
                .transpose()?,
            observed_tip: status
                .observed_tip
                .map(|block| BlockDto::try_from_block(block, &chain))
                .transpose()?,
            confirmation_depth: status.confirmation_policy.minimum_confirmations.to_string(),
            require_chain_finality: status.confirmation_policy.require_chain_finality,
            rebuild_reason: status.rebuild_reason.map(|reason| reason.message),
            halted_reason: status.halted_reason,
        })
    }
}

#[derive(Serialize)]
pub(crate) struct ScopeDto {
    chain: String,
    network: String,
}

impl From<&IndexScope> for ScopeDto {
    fn from(scope: &IndexScope) -> Self {
        Self {
            chain: scope.chain.0.clone(),
            network: scope.network.clone(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct BlockDto {
    height: String,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<String>,
}

impl BlockDto {
    pub(crate) fn from_block(block: indexing::BlockRef) -> Self {
        Self {
            height: block.height.0.to_string(),
            hash: encode_hex(&block.hash.0),
            parent_hash: block.parent_hash.map(|hash| encode_hex(&hash.0)),
            timestamp: block.timestamp.map(|value| value.to_string()),
        }
    }

    pub(crate) fn try_from_block(
        block: indexing::BlockRef,
        chain: &ChainId,
    ) -> Result<Self, IndexError> {
        Ok(Self {
            height: block.height.0.to_string(),
            hash: encode_chain_block_hash(chain, &block.hash)?,
            parent_hash: block
                .parent_hash
                .as_ref()
                .map(|hash| encode_chain_block_hash(chain, hash))
                .transpose()?,
            timestamp: block.timestamp.map(|value| value.to_string()),
        })
    }
}

pub(crate) fn encode_chain_block_hash(
    chain: &ChainId,
    hash: &indexing::BlockHash,
) -> Result<String, IndexError> {
    if chain.0 == "bitcoin" {
        format_bitcoin_block_hash(hash).map_err(|error| {
            IndexError::new(
                IndexErrorKind::Other,
                format!("Bitcoin block hash could not be encoded: {}", error.message),
                false,
            )
        })
    } else {
        Ok(encode_hex(&hash.0))
    }
}

#[derive(Serialize)]
pub(crate) struct WatchDto {
    id: String,
    scope: ScopeDto,
    selector: SelectorResponseDto,
    start_height: String,
    registered_at: Option<BlockDto>,
    inactive_from: Option<String>,
    confirmation_depth: String,
    require_chain_finality: bool,
}

impl WatchDto {
    pub(crate) fn try_from_receipt(receipt: WatchReceipt) -> Result<Self, IndexError> {
        let chain = receipt.scope.chain.clone();
        Ok(Self {
            id: receipt.id.0,
            scope: ScopeDto::from(&receipt.scope),
            selector: SelectorResponseDto::from(receipt.selector),
            start_height: receipt.start_height.0.to_string(),
            registered_at: receipt
                .registered_at
                .map(|block| BlockDto::try_from_block(block, &chain))
                .transpose()?,
            inactive_from: receipt.inactive_from.map(|height| height.0.to_string()),
            confirmation_depth: receipt
                .confirmation_policy
                .minimum_confirmations
                .to_string(),
            require_chain_finality: receipt.confirmation_policy.require_chain_finality,
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum SelectorResponseDto {
    Address(String),
    Transaction(String),
}

impl From<WatchSelector> for SelectorResponseDto {
    fn from(selector: WatchSelector) -> Self {
        match selector {
            WatchSelector::Address(address) => Self::Address(address.value),
            WatchSelector::Transaction(transaction) => Self::Transaction(transaction.value),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct UnwatchDto {
    pub(crate) outcome: &'static str,
}

#[derive(Serialize)]
pub(crate) struct TransactionsBody {
    pub(crate) transactions: Vec<TransactionDto>,
    pub(crate) next: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct OutputsBody {
    pub(crate) generation: String,
    pub(crate) revision: String,
    pub(crate) checkpoint: Option<BlockDto>,
    pub(crate) outputs: Vec<OutputBody>,
    pub(crate) next: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct OutputBody {
    transaction_id: String,
    output_index: String,
    asset: String,
    amount: String,
    evidence: String,
    address: String,
    created_height: String,
    coinbase: bool,
}

impl From<IndexedOutput> for OutputBody {
    fn from(output: IndexedOutput) -> Self {
        Self {
            transaction_id: output.id.transaction.value,
            output_index: output.id.index.to_string(),
            asset: output.asset.asset,
            amount: output.amount.to_string(),
            evidence: encode_hex(&output.evidence),
            address: output.address.value,
            created_height: output.created_at.0.to_string(),
            coinbase: output.coinbase,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct TransactionDto {
    scope: ScopeDto,
    transaction_id: String,
    revision: String,
    status: TransactionStatusDto,
    movements: Vec<MovementDto>,
    fee: Option<FeeDto>,
    first_seen_at: String,
    observed_at: String,
}

impl TransactionDto {
    pub(crate) fn try_from_transaction(
        transaction: ObservedTransaction,
    ) -> Result<Self, IndexError> {
        let chain = transaction.scope.chain.clone();
        Ok(Self {
            scope: ScopeDto::from(&transaction.scope),
            transaction_id: transaction.transaction_id.value,
            revision: transaction.revision.0.to_string(),
            status: TransactionStatusDto::try_from_status(transaction.status, &chain)?,
            movements: transaction
                .movements
                .into_iter()
                .map(MovementDto::from)
                .collect(),
            fee: transaction.fee.map(FeeDto::from),
            first_seen_at: transaction.first_seen_at.to_string(),
            observed_at: transaction.observed_at.to_string(),
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TransactionStatusDto {
    Pending,
    Included {
        block: BlockDto,
        confirmations: String,
    },
    Confirmed {
        block: BlockDto,
        proof: ConfirmationProofDto,
    },
    Failed {
        block: Option<BlockDto>,
        reason: Option<String>,
    },
    Replaced {
        by: String,
    },
    Dropped,
    Reorged {
        previous_block: BlockDto,
    },
}

impl TransactionStatusDto {
    pub(crate) fn try_from_status(
        status: TransactionStatus,
        chain: &ChainId,
    ) -> Result<Self, IndexError> {
        Ok(match status {
            TransactionStatus::Pending => Self::Pending,
            TransactionStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: BlockDto::try_from_block(block, chain)?,
                confirmations: confirmations.to_string(),
            },
            TransactionStatus::Confirmed { block, proof } => Self::Confirmed {
                block: BlockDto::try_from_block(block, chain)?,
                proof: proof.into(),
            },
            TransactionStatus::Failed { block, reason } => Self::Failed {
                block: block
                    .map(|block| BlockDto::try_from_block(block, chain))
                    .transpose()?,
                reason,
            },
            TransactionStatus::Replaced { by } => Self::Replaced { by: by.value },
            TransactionStatus::Dropped => Self::Dropped,
            TransactionStatus::Reorged { previous_block } => Self::Reorged {
                previous_block: BlockDto::try_from_block(previous_block, chain)?,
            },
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ConfirmationProofDto {
    Depth { required: String, observed: String },
    ChainFinalized,
    DepthAndChainFinalized { required: String, observed: String },
}

impl From<indexing::ConfirmationProof> for ConfirmationProofDto {
    fn from(proof: indexing::ConfirmationProof) -> Self {
        match proof {
            indexing::ConfirmationProof::Depth { required, observed } => Self::Depth {
                required: required.to_string(),
                observed: observed.to_string(),
            },
            indexing::ConfirmationProof::ChainFinalized => Self::ChainFinalized,
            indexing::ConfirmationProof::DepthAndChainFinalized { required, observed } => {
                Self::DepthAndChainFinalized {
                    required: required.to_string(),
                    observed: observed.to_string(),
                }
            }
        }
    }
}

#[derive(Serialize)]
pub(crate) struct MovementDto {
    id: String,
    asset: String,
    amount: String,
    from: Option<String>,
    to: Option<String>,
    kind: &'static str,
}

impl From<indexing::ValueMovement> for MovementDto {
    fn from(movement: indexing::ValueMovement) -> Self {
        Self {
            id: movement.id().0.clone(),
            asset: movement.asset().asset.clone(),
            amount: movement.amount().to_string(),
            from: movement.from().map(|address| address.value.clone()),
            to: movement.to().map(|address| address.value.clone()),
            kind: match movement.kind() {
                indexing::MovementKind::Transfer => "transfer",
                indexing::MovementKind::Input => "input",
                indexing::MovementKind::Output => "output",
                indexing::MovementKind::InternalTransfer => "internal_transfer",
                indexing::MovementKind::Mint => "mint",
                indexing::MovementKind::Burn => "burn",
            },
        }
    }
}

#[derive(Serialize)]
pub(crate) struct FeeDto {
    asset: String,
    amount: String,
    payer: Option<String>,
}

impl From<indexing::NetworkFee> for FeeDto {
    fn from(fee: indexing::NetworkFee) -> Self {
        Self {
            asset: fee.asset.asset,
            amount: fee.amount.to_string(),
            payer: fee.payer.map(|address| address.value),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct EventsBody {
    pub(crate) events: Vec<EventDto>,
    pub(crate) next_cursor: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct EventDto {
    id: String,
    cursor: String,
    watch_ids: Vec<String>,
    previous_status: Option<TransactionStatusDto>,
    transaction: TransactionDto,
}

impl EventDto {
    pub(crate) fn try_from_event(event: ObservationEvent) -> Result<Self, IndexError> {
        let chain = event.transaction.scope.chain.clone();
        Ok(Self {
            id: event.id.0,
            cursor: event.cursor.0.to_string(),
            watch_ids: event.watch_ids.into_iter().map(|id| id.0).collect(),
            previous_status: event
                .previous_status
                .map(|status| TransactionStatusDto::try_from_status(status, &chain))
                .transpose()?,
            transaction: TransactionDto::try_from_transaction(event.transaction)?,
        })
    }
}

pub(crate) fn phase_name(phase: SyncPhase) -> &'static str {
    match phase {
        SyncPhase::Starting => "starting",
        SyncPhase::Reconciling => "reconciling",
        SyncPhase::CatchingUp => "catching_up",
        SyncPhase::Ready => "ready",
        SyncPhase::Reverting => "reverting",
        SyncPhase::Replaying => "replaying",
        SyncPhase::RebuildRequired => "rebuild_required",
        SyncPhase::Halted => "halted",
    }
}

#[derive(Serialize)]
pub(crate) struct ErrorDto {
    code: &'static str,
    message: String,
    retryable: bool,
    request_id: String,
}

pub(crate) struct ResponseError {
    status: StatusCode,
    body: ErrorDto,
}

impl ResponseError {
    pub(crate) fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        retryable: bool,
        request_id: String,
    ) -> Self {
        Self {
            status,
            body: ErrorDto {
                code,
                message: message.into(),
                retryable,
                request_id,
            },
        }
    }

    pub(crate) fn bad_request(
        code: &'static str,
        message: impl Into<String>,
        request_id: String,
    ) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, false, request_id)
    }

    pub(crate) fn from_index(error: IndexError, request_id: String) -> Self {
        let (status, code) = match error.kind {
            IndexErrorKind::Conflict => (StatusCode::CONFLICT, "conflict"),
            IndexErrorKind::ScopeMismatch => (StatusCode::NOT_FOUND, "scope_not_found"),
            IndexErrorKind::PolicyMismatch => (StatusCode::CONFLICT, "policy_mismatch"),
            IndexErrorKind::InvalidWatch => (StatusCode::BAD_REQUEST, "invalid_watch"),
            IndexErrorKind::InvalidRequest | IndexErrorKind::InvalidBlock => {
                (StatusCode::BAD_REQUEST, "invalid_request")
            }
            IndexErrorKind::ReorgBeyondRetention | IndexErrorKind::RebuildRequired => {
                (StatusCode::SERVICE_UNAVAILABLE, "rebuild_required")
            }
            IndexErrorKind::Halted => (StatusCode::SERVICE_UNAVAILABLE, "indexer_halted"),
            IndexErrorKind::Source | IndexErrorKind::CannotConnect if error.retryable => {
                (StatusCode::SERVICE_UNAVAILABLE, "source_unavailable")
            }
            IndexErrorKind::Store if error.retryable => {
                (StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable")
            }
            IndexErrorKind::Source
            | IndexErrorKind::Store
            | IndexErrorKind::CannotConnect
            | IndexErrorKind::Other => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            "Indexer operation failed".to_owned()
        } else {
            error.message
        };
        Self::new(status, code, message, error.retryable, request_id)
    }
}

impl IntoResponse for ResponseError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}
