use std::{str::FromStr, sync::Arc};

use axum::{
    Json, Router,
    extract::{Extension, Path, State as AxumState, rejection::JsonRejection},
    http::StatusCode,
    routing::{delete, get, post},
};
use chain_bitcoin::{BitcoinAddress, TransactionId as BitcoinTransactionId};
use http::server::AuthenticationMode;
use indexing::{
    CanonicalAddress, TransactionQuery, TransactionRef, UnwatchOutcome, UnwatchRequest,
    WatchRequest, WatchSelector,
};
use serde::Deserialize;

use super::*;

pub fn router(state: Arc<State>) -> Router {
    let mut router = Router::new()
        .route("/v1/scopes/{chain}/{network}/status", get(status))
        .route("/v1/scopes/{chain}/{network}/checkpoint", get(checkpoint))
        .route("/v1/scopes/{chain}/{network}/watches", post(register_watch))
        .route(
            "/v1/scopes/{chain}/{network}/watches/{watch_id}",
            delete(unwatch),
        )
        .route(
            "/v1/scopes/{chain}/{network}/transactions/{tx_hash}",
            get(transaction),
        )
        .route(
            "/v1/scopes/{chain}/{network}/addresses/{address}/transactions",
            get(transactions_by_address),
        )
        .route("/v1/scopes/{chain}/{network}/events", get(events));
    if state.outputs.is_some() {
        router = router.route(
            "/v1/scopes/{chain}/{network}/addresses/{address}/outputs",
            get(outputs),
        );
    }
    router
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}

pub(crate) async fn checkpoint(
    AxumState(state): AxumState<Arc<State>>,
    Path((chain, network)): Path<(String, String)>,
) -> Result<Json<BlockDto>, ResponseError> {
    state.validate_scope(&chain, &network)?;
    state.semantic_status().await?;
    let checkpoint = state
        .indexer
        .checkpoint(&state.scope)
        .await
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?
        .ok_or_else(|| {
            ResponseError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "checkpoint_unavailable",
                "canonical checkpoint is not available",
                true,
                state.request_id(),
            )
        })?;
    Ok(Json(BlockDto::from_block(checkpoint)))
}

pub(crate) async fn route_not_found(AxumState(state): AxumState<Arc<State>>) -> ResponseError {
    ResponseError::new(
        StatusCode::NOT_FOUND,
        "route_not_found",
        "requested Indexer route does not exist",
        false,
        state.request_id(),
    )
}

pub(crate) async fn method_not_allowed(AxumState(state): AxumState<Arc<State>>) -> ResponseError {
    ResponseError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed for this Indexer route",
        false,
        state.request_id(),
    )
}

pub(crate) async fn status(
    AxumState(state): AxumState<Arc<State>>,
    Extension(authentication_mode): Extension<AuthenticationMode>,
    Path((chain, network)): Path<(String, String)>,
) -> Result<Json<StatusDto>, ResponseError> {
    state.validate_scope(&chain, &network)?;
    let status = state
        .status
        .status(&state.scope)
        .await
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?;
    Ok(Json(
        StatusDto::try_from_status(status, authentication_mode)
            .map_err(|error| ResponseError::from_index(error, state.request_id()))?,
    ))
}

pub(crate) async fn register_watch(
    AxumState(state): AxumState<Arc<State>>,
    Path((chain, network)): Path<(String, String)>,
    body: Result<Json<WatchBody>, JsonRejection>,
) -> Result<Json<WatchDto>, ResponseError> {
    state.validate_scope(&chain, &network)?;
    state.semantic_status().await?;
    let Json(body) = body.map_err(|_| {
        ResponseError::bad_request(
            "invalid_json",
            "watch request body is not valid JSON",
            state.request_id(),
        )
    })?;
    let start_height = parse_decimal(&body.start_height, "start_height", &state)?;
    if start_height < state.bootstrap_height.0 {
        return Err(ResponseError::bad_request(
            "invalid_start_height",
            "watch start height precedes the configured bootstrap height",
            state.request_id(),
        ));
    }
    let idempotency_key = match body.idempotency_key {
        Some(value) if value.trim().is_empty() => {
            return Err(ResponseError::bad_request(
                "invalid_idempotency_key",
                "watch idempotency key must not be empty",
                state.request_id(),
            ));
        }
        Some(value) => value,
        None => {
            return Err(ResponseError::bad_request(
                "invalid_idempotency_key",
                "watch idempotency key is required",
                state.request_id(),
            ));
        }
    };
    let selector = parse_selector(&state, body.selector)?;
    let receipt = state
        .indexer
        .watch(WatchRequest {
            scope: state.scope.clone(),
            selector,
            start_height: indexing::BlockHeight(start_height),
            idempotency_key,
        })
        .await
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?;
    let watch = WatchDto::try_from_receipt(receipt)
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?;
    Ok(Json(watch))
}

pub(crate) async fn unwatch(
    AxumState(state): AxumState<Arc<State>>,
    Path((chain, network, watch_id)): Path<(String, String, String)>,
) -> Result<Json<UnwatchDto>, ResponseError> {
    state.validate_scope(&chain, &network)?;
    state.semantic_status().await?;
    let outcome = state
        .indexer
        .unwatch(UnwatchRequest {
            scope: state.scope.clone(),
            watch_id: indexing::WatchId(watch_id),
        })
        .await
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?;
    Ok(Json(UnwatchDto {
        outcome: match outcome {
            UnwatchOutcome::Deactivated => "deactivated",
            UnwatchOutcome::AlreadyInactive => "already_inactive",
        },
    }))
}

pub(crate) async fn transaction(
    AxumState(state): AxumState<Arc<State>>,
    Path((chain, network, tx_hash)): Path<(String, String, String)>,
) -> Result<Json<TransactionDto>, ResponseError> {
    state.validate_scope(&chain, &network)?;
    state.semantic_status().await?;
    let transaction_id = parse_transaction(&state, &tx_hash)?;
    let transaction = state
        .indexer
        .transaction(TransactionQuery {
            scope: state.scope.clone(),
            transaction_id,
        })
        .await
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?
        .ok_or_else(|| {
            ResponseError::new(
                StatusCode::NOT_FOUND,
                "transaction_not_found",
                "indexed transaction does not exist",
                false,
                state.request_id(),
            )
        })?;
    Ok(Json(
        TransactionDto::try_from_transaction(transaction)
            .map_err(|error| ResponseError::from_index(error, state.request_id()))?,
    ))
}

#[derive(Deserialize)]

pub(crate) struct WatchBody {
    selector: SelectorDto,
    start_height: String,
    idempotency_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum SelectorDto {
    Address(String),
    Transaction(String),
}

pub(crate) fn parse_selector(
    state: &State,
    selector: SelectorDto,
) -> Result<WatchSelector, ResponseError> {
    match selector {
        SelectorDto::Address(value) => Ok(WatchSelector::Address(parse_address(state, &value)?)),
        SelectorDto::Transaction(value) => Ok(WatchSelector::Transaction(parse_transaction(
            state, &value,
        )?)),
    }
}

pub(crate) fn parse_address(state: &State, input: &str) -> Result<CanonicalAddress, ResponseError> {
    match state.chain {
        ChainKind::Ethereum => {
            let bytes = decode_fixed::<20>(input).map_err(|message| {
                ResponseError::bad_request("invalid_address", message, state.request_id())
            })?;
            let value = encode_hex(&bytes);
            Ok(CanonicalAddress {
                scope: state.scope.clone(),
                value,
            })
        }
        ChainKind::Bitcoin(network) => {
            let native = BitcoinAddress::parse_for_network(input, network).map_err(|error| {
                ResponseError::bad_request("invalid_address", error.message, state.request_id())
            })?;
            let script = native.script_pubkey_for_network(network).map_err(|error| {
                ResponseError::bad_request("invalid_address", error.message, state.request_id())
            })?;
            if !script.is_p2wpkh() && !script.is_p2tr() {
                return Err(ResponseError::bad_request(
                    "unsupported_address",
                    "Bitcoin watches support P2WPKH and P2TR addresses only",
                    state.request_id(),
                ));
            }
            let canonical = CanonicalAddress {
                scope: state.scope.clone(),
                value: native.to_string(),
            };
            Ok(canonical)
        }
    }
}

pub(crate) fn parse_transaction(
    state: &State,
    input: &str,
) -> Result<TransactionRef, ResponseError> {
    match state.chain {
        ChainKind::Ethereum => {
            let bytes = decode_fixed::<32>(input).map_err(|message| {
                ResponseError::bad_request("invalid_transaction_hash", message, state.request_id())
            })?;
            let value = encode_hex(&bytes);
            Ok(TransactionRef {
                scope: state.scope.clone(),
                value,
            })
        }
        ChainKind::Bitcoin(_) => {
            let native = BitcoinTransactionId::from_str(input).map_err(|error| {
                ResponseError::bad_request(
                    "invalid_transaction_hash",
                    error.to_string(),
                    state.request_id(),
                )
            })?;
            let canonical = TransactionRef {
                scope: state.scope.clone(),
                value: native.to_string(),
            };
            Ok(canonical)
        }
    }
}

pub(crate) fn parse_decimal(input: &str, field: &str, state: &State) -> Result<u64, ResponseError> {
    input.parse::<u64>().map_err(|_| {
        ResponseError::bad_request(
            "invalid_decimal",
            format!("{field} must be an unsigned decimal string"),
            state.request_id(),
        )
    })
}

pub(crate) fn decode_fixed<const N: usize>(input: &str) -> Result<[u8; N], &'static str> {
    let hex = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .ok_or("Ethereum value must have a 0x prefix")?;
    if hex.len() != N * 2 {
        return Err("Ethereum value has an invalid byte length");
    }
    let mut output = [0_u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| "Ethereum value contains non-hex characters")?;
    }
    Ok(output)
}

pub(crate) fn decode_hex(input: &str) -> Result<Vec<u8>, &'static str> {
    let hex = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .ok_or("hexadecimal value must have a 0x prefix")?;
    if hex.is_empty() || hex.len() % 2 != 0 {
        return Err("hexadecimal value must contain a non-empty whole number of bytes");
    }
    let mut output = Vec::with_capacity(hex.len() / 2);
    for index in (0..hex.len()).step_by(2) {
        output.push(
            u8::from_str_radix(&hex[index..index + 2], 16)
                .map_err(|_| "hexadecimal value contains non-hex characters")?,
        );
    }
    Ok(output)
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(2 + bytes.len() * 2);
    output.push_str("0x");
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
