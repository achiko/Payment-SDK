use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State as AxumState, rejection::QueryRejection},
    http::StatusCode,
};
use indexing::{
    BlockHeight, EventCursor, EventQuery, HistoryQuery, OutputCursor, OutputSnapshot, SyncPhase,
};
use serde::Deserialize;

use super::*;

#[derive(Deserialize)]
pub(crate) struct PageQuery {
    after: Option<String>,
    limit: Option<usize>,
}

pub(crate) fn page_size(requested: Option<usize>, state: &State) -> Result<usize, ResponseError> {
    const DEFAULT: usize = 100;
    const MAXIMUM: usize = 1_000;
    let size = requested.unwrap_or(DEFAULT);
    if size == 0 || size > MAXIMUM {
        return Err(ResponseError::bad_request(
            "invalid_page_size",
            format!("page size must be between 1 and {MAXIMUM}"),
            state.request_id(),
        ));
    }
    Ok(size)
}

pub(crate) async fn transactions_by_address(
    AxumState(state): AxumState<Arc<State>>,
    Path((chain, network, address)): Path<(String, String, String)>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<TransactionsBody>, ResponseError> {
    state.validate_scope(&chain, &network)?;
    state.semantic_status().await?;
    let Query(query) = query.map_err(|_| {
        ResponseError::bad_request(
            "invalid_query",
            "transaction page query is invalid",
            state.request_id(),
        )
    })?;
    let address = parse_address(&state, &address)?;
    let after = query
        .after
        .as_deref()
        .map(|value| parse_transaction(&state, value))
        .transpose()?;
    let limit = page_size(query.limit, &state)?;
    let page = state
        .indexer
        .history(HistoryQuery {
            scope: state.scope.clone(),
            address,
            after,
            limit,
        })
        .await
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?;
    let transactions = page
        .transactions
        .into_iter()
        .map(TransactionDto::try_from_transaction)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?;
    Ok(Json(TransactionsBody {
        transactions,
        next: page.next.map(|next| next.value),
    }))
}

pub(crate) async fn outputs(
    AxumState(state): AxumState<Arc<State>>,
    Path((chain, network, address)): Path<(String, String, String)>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<OutputsBody>, ResponseError> {
    state.validate_scope(&chain, &network)?;
    if !state
        .operational_health
        .as_ref()
        .is_some_and(http::server::HealthState::is_ready)
    {
        return Err(ResponseError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "output_snapshot_unavailable",
            "indexed outputs are unavailable until the Indexer is operationally ready",
            true,
            state.request_id(),
        ));
    }
    let status = state.semantic_status().await?;
    if status.phase != SyncPhase::Ready {
        return Err(ResponseError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "output_snapshot_unavailable",
            "indexed outputs are available only while the Indexer is ready",
            true,
            state.request_id(),
        ));
    }
    let Query(query) = query.map_err(|_| {
        ResponseError::bad_request(
            "invalid_query",
            "output page query is invalid",
            state.request_id(),
        )
    })?;
    let canonical = parse_address(&state, &address)?;
    let after = query
        .after
        .as_deref()
        .map(|value| decode_projection_cursor(value, &state))
        .transpose()?;
    let limit = page_size(query.limit, &state)?;
    let repository = state.outputs.as_ref().ok_or_else(|| {
        ResponseError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "indexed outputs are not configured",
            false,
            state.request_id(),
        )
    })?;
    let page = repository
        .outputs(OutputRequest {
            scope: state.scope.clone(),
            address: canonical,
            after,
            limit,
        })
        .await
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?;
    let snapshot = page.snapshot.clone();
    let outputs = page.outputs.into_iter().map(OutputBody::from).collect();
    let after = state.semantic_status().await?;
    if after.phase != SyncPhase::Ready || after.checkpoint != snapshot.checkpoint {
        return Err(ResponseError::new(
            StatusCode::CONFLICT,
            "output_snapshot_changed",
            "canonical state changed during the output read",
            true,
            state.request_id(),
        ));
    }
    let checkpoint = snapshot.checkpoint.clone().map(BlockDto::from_block);
    Ok(Json(OutputsBody {
        generation: snapshot.generation.0.to_string(),
        revision: snapshot.revision.to_string(),
        checkpoint,
        outputs,
        next: page.next.as_ref().map(encode_projection_cursor),
    }))
}

pub(crate) fn decode_projection_cursor(
    input: &str,
    state: &State,
) -> Result<OutputCursor, ResponseError> {
    let parts = input.split(':').collect::<Vec<_>>();
    if parts.len() != 7 {
        return Err(ResponseError::bad_request(
            "invalid_cursor",
            "UTXO cursor does not contain a complete projection snapshot",
            state.request_id(),
        ));
    }
    let generation = parse_decimal(parts[0], "cursor generation", state)?;
    let revision = parse_decimal(parts[1], "cursor revision", state)?;
    let checkpoint = if parts[2] == "-" {
        if parts[3..6].iter().any(|part| *part != "-") {
            return Err(ResponseError::bad_request(
                "invalid_cursor",
                "UTXO cursor has an inconsistent empty checkpoint",
                state.request_id(),
            ));
        }
        None
    } else {
        let height = parse_decimal(parts[2], "cursor checkpoint height", state)?;
        let hash = decode_hex(parts[3]).map_err(|message| {
            ResponseError::bad_request("invalid_cursor", message, state.request_id())
        })?;
        if hash.len() != 32 {
            return Err(ResponseError::bad_request(
                "invalid_cursor",
                "UTXO cursor checkpoint hash must contain 32 bytes",
                state.request_id(),
            ));
        }
        let parent_hash = if parts[4] == "-" {
            None
        } else {
            let parent = decode_hex(parts[4]).map_err(|message| {
                ResponseError::bad_request("invalid_cursor", message, state.request_id())
            })?;
            if parent.len() != 32 {
                return Err(ResponseError::bad_request(
                    "invalid_cursor",
                    "UTXO cursor parent hash must contain 32 bytes",
                    state.request_id(),
                ));
            }
            Some(indexing::BlockHash(parent))
        };
        let timestamp = if parts[5] == "-" {
            None
        } else {
            Some(parse_decimal(
                parts[5],
                "cursor checkpoint timestamp",
                state,
            )?)
        };
        Some(indexing::BlockRef {
            height: BlockHeight(height),
            hash: indexing::BlockHash(hash),
            parent_hash,
            timestamp,
        })
    };
    let key = decode_hex(parts[6]).map_err(|message| {
        ResponseError::bad_request("invalid_cursor", message, state.request_id())
    })?;
    if key.is_empty() {
        return Err(ResponseError::bad_request(
            "invalid_cursor",
            "UTXO cursor key must not be empty",
            state.request_id(),
        ));
    }
    Ok(OutputCursor {
        snapshot: OutputSnapshot {
            generation: indexing::RebuildGeneration(generation),
            revision,
            checkpoint,
        },
        position: key,
    })
}

pub(crate) fn encode_projection_cursor(cursor: &OutputCursor) -> String {
    let (height, hash, parent, timestamp) = cursor.snapshot.checkpoint.as_ref().map_or_else(
        || {
            (
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
            )
        },
        |checkpoint| {
            (
                checkpoint.height.0.to_string(),
                encode_hex(&checkpoint.hash.0),
                checkpoint
                    .parent_hash
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |hash| encode_hex(&hash.0)),
                checkpoint
                    .timestamp
                    .map_or_else(|| "-".to_owned(), |timestamp| timestamp.to_string()),
            )
        },
    );
    format!(
        "{}:{}:{height}:{hash}:{parent}:{timestamp}:{}",
        cursor.snapshot.generation.0,
        cursor.snapshot.revision,
        encode_hex(&cursor.position)
    )
}

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    after_cursor: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn events(
    AxumState(state): AxumState<Arc<State>>,
    Path((chain, network)): Path<(String, String)>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Result<Json<EventsBody>, ResponseError> {
    state.validate_scope(&chain, &network)?;
    state.semantic_status().await?;
    let Query(query) = query.map_err(|_| {
        ResponseError::bad_request(
            "invalid_query",
            "event page query is invalid",
            state.request_id(),
        )
    })?;
    let after = query
        .after_cursor
        .as_deref()
        .map(|value| parse_decimal(value, "after_cursor", &state).map(EventCursor))
        .transpose()?;
    let limit = page_size(query.limit, &state)?;
    let page = state
        .indexer
        .events(EventQuery {
            scope: state.scope.clone(),
            after,
            limit,
        })
        .await
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?;
    let events = page
        .events
        .into_iter()
        .map(EventDto::try_from_event)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ResponseError::from_index(error, state.request_id()))?;
    Ok(Json(EventsBody {
        events,
        next_cursor: page.next.map(|cursor| cursor.0.to_string()),
    }))
}
