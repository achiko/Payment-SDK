use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
};
use http::client::{BoxFuture as HttpFuture, Client as HttpClient, Error, Request, Response};
use indexing::{
    BlockHash, BlockHeight, BlockRef, CanonicalAddress, ChainId, EventCursor, EventQuery, History,
    HistoryQuery, IndexScope, Indexer, OutputCursor, OutputQuery, OutputRequest, OutputSnapshot,
    RebuildGeneration, TransactionQuery, TransactionRef, UnwatchOutcome, UnwatchRequest, WatchId,
    WatchRequest, WatchSelector, Watcher,
};
use indexing_http::{Config, Remote};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt;

#[derive(Clone)]
struct RouterClient(Router);

impl HttpClient for RouterClient {
    fn execute<'a>(&'a self, request: Request) -> HttpFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let uri = request
                .endpoint
                .strip_prefix("http://indexer.test")
                .unwrap_or(&request.endpoint);
            let mut builder = axum::http::Request::builder()
                .method(request.method.as_str())
                .uri(uri);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            let response = self
                .0
                .clone()
                .oneshot(
                    builder
                        .body(Body::from(request.body))
                        .expect("valid request"),
                )
                .await
                .expect("in-process router is infallible");
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().expect("test header is text").to_owned(),
                    )
                })
                .collect();
            let body = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("test response body")
                .to_vec();
            Ok(Response {
                status,
                headers,
                body,
            })
        })
    }
}

#[derive(Default)]
struct Fixture {
    watch: Mutex<Option<(String, Value)>>,
}

fn router() -> Router {
    Router::new()
        .route("/v1/scopes/{chain}/{network}/checkpoint", get(checkpoint))
        .route("/v1/scopes/{chain}/{network}/watches", post(watch))
        .route(
            "/v1/scopes/{chain}/{network}/watches/{watch}",
            delete(unwatch),
        )
        .route(
            "/v1/scopes/{chain}/{network}/transactions/{transaction}",
            get(transaction),
        )
        .route(
            "/v1/scopes/{chain}/{network}/addresses/{address}/transactions",
            get(history),
        )
        .route(
            "/v1/scopes/{chain}/{network}/addresses/{address}/outputs",
            get(outputs),
        )
        .route("/v1/scopes/{chain}/{network}/events", get(events))
        .with_state(Arc::new(Fixture::default()))
}

async fn checkpoint() -> Json<Value> {
    Json(block())
}

async fn watch(
    State(state): State<Arc<Fixture>>,
    Path((chain, network)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    assert_eq!(headers["authorization"], "Bearer secret-token");
    let key = body["idempotency_key"].as_str().expect("idempotency key");
    let selector = body["selector"].clone();
    let mut saved = state.watch.lock().await;
    match saved.as_ref() {
        None => *saved = Some((key.to_owned(), selector.clone())),
        Some((existing_key, existing_selector))
            if existing_key == key && existing_selector != &selector =>
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "code": "conflict",
                    "message": "idempotency key was reused with a different request",
                    "retryable": false,
                    "request_id": "request-conflict"
                })),
            )
                .into_response();
        }
        Some((existing_key, _)) => assert_eq!(existing_key, key),
    }
    Json(json!({
        "id": "watch-1",
        "scope": { "chain": chain, "network": network },
        "selector": selector,
        "start_height": body["start_height"],
        "registered_at": block(),
        "inactive_from": null,
        "confirmation_depth": "3",
        "require_chain_finality": true
    }))
    .into_response()
}

async fn unwatch(Path((_chain, _network, watch)): Path<(String, String, String)>) -> Json<Value> {
    assert_eq!(watch, "watch-1");
    Json(json!({ "outcome": "deactivated" }))
}

async fn transaction(
    Path((chain, network, transaction)): Path<(String, String, String)>,
) -> impl IntoResponse {
    if transaction == "missing" {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "code": "transaction_not_found",
                "message": "indexed transaction does not exist",
                "retryable": false,
                "request_id": "request-2"
            })),
        );
    }
    (
        StatusCode::OK,
        Json(transaction_json(&chain, &network, &transaction)),
    )
}

async fn history(
    Path((chain, network, address)): Path<(String, String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Json<Value> {
    assert_eq!(address, "address/with space");
    assert_eq!(query.get("limit").map(String::as_str), Some("25"));
    assert_eq!(query.get("after").map(String::as_str), Some("before/1"));
    Json(json!({
        "transactions": [transaction_json(&chain, &network, "transaction-1")],
        "next": "transaction-2"
    }))
}

async fn events(
    Path((chain, network)): Path<(String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Json<Value> {
    assert_eq!(query.get("limit").map(String::as_str), Some("10"));
    assert_eq!(query.get("after_cursor").map(String::as_str), Some("7"));
    Json(json!({
        "events": [{
            "id": "event-8",
            "cursor": "8",
            "watch_ids": ["watch-1"],
            "previous_status": { "kind": "pending" },
            "transaction": transaction_json(&chain, &network, "transaction-1")
        }],
        "next_cursor": "8"
    }))
}

async fn outputs(
    Path((_chain, _network, address)): Path<(String, String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Json<Value> {
    assert_eq!(address, "address/with space");
    assert_eq!(query.get("limit").map(String::as_str), Some("5"));
    assert_eq!(
        query.get("after").map(String::as_str),
        Some(
            format!(
                "7:9:42:0x{}:0x{}:999:0xaabb",
                "11".repeat(32),
                "10".repeat(32)
            )
            .as_str()
        )
    );
    Json(json!({
        "generation": "7",
        "revision": "9",
        "checkpoint": block(),
        "outputs": [{
            "transaction_id": "transaction-output",
            "output_index": "2",
            "asset": "native",
            "amount": "75000",
            "evidence": "0x5120",
            "address": "address/with space",
            "created_height": "40",
            "coinbase": false
        }],
        "next": format!(
            "7:9:42:0x{}:0x{}:999:0xccdd",
            "11".repeat(32),
            "10".repeat(32)
        )
    }))
}

fn transaction_json(chain: &str, network: &str, transaction: &str) -> Value {
    json!({
        "scope": { "chain": chain, "network": network },
        "transaction_id": transaction,
        "revision": "4",
        "status": {
            "kind": "confirmed",
            "block": block(),
            "proof": { "kind": "depth", "required": "3", "observed": "4" }
        },
        "movements": [{
            "id": "movement-1",
            "asset": "native",
            "amount": "12.5",
            "from": "source",
            "to": "destination",
            "kind": "transfer"
        }],
        "fee": { "asset": "native", "amount": "0.1", "payer": "source" },
        "first_seen_at": "1000",
        "observed_at": "1001"
    })
}

fn block() -> Value {
    json!({
        "height": "42",
        "hash": format!("0x{}", "11".repeat(32)),
        "parent_hash": format!("0x{}", "10".repeat(32)),
        "timestamp": "999"
    })
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("fixture".to_owned()),
        network: "local".to_owned(),
    }
}

#[tokio::test]
async fn remote_satisfies_the_complete_indexer_contract() {
    let mut config = Config::new("http://indexer.test");
    config.bearer_token = Some("secret-token".to_owned());
    let remote =
        Arc::new(Remote::new(Arc::new(RouterClient(router())), &config).expect("valid client"));
    let indexer: Arc<dyn Indexer> = remote.clone();
    let scope = scope();
    assert_eq!(
        indexer
            .checkpoint(&scope)
            .await
            .expect("checkpoint request")
            .expect("canonical checkpoint")
            .height,
        BlockHeight(42)
    );
    let watch_request = WatchRequest {
        scope: scope.clone(),
        selector: WatchSelector::Address(CanonicalAddress {
            scope: scope.clone(),
            value: "address/with space".to_owned(),
        }),
        start_height: BlockHeight(1),
        idempotency_key: "create-deposit-1".to_owned(),
    };

    let first = indexer.watch(watch_request.clone()).await.expect("watch");
    let retried = indexer
        .watch(watch_request)
        .await
        .expect("idempotent retry");
    assert_eq!(first, retried);
    assert_eq!(first.id, WatchId("watch-1".to_owned()));
    assert!(first.confirmation_policy.require_chain_finality);

    let conflict = indexer
        .watch(WatchRequest {
            scope: scope.clone(),
            selector: WatchSelector::Transaction(TransactionRef {
                scope: scope.clone(),
                value: "different-transaction".to_owned(),
            }),
            start_height: BlockHeight(1),
            idempotency_key: "create-deposit-1".to_owned(),
        })
        .await
        .expect_err("structured conflict must be mapped");
    assert_eq!(conflict.kind, indexing::IndexErrorKind::Conflict);
    assert!(!conflict.retryable);

    let page = indexer
        .history(HistoryQuery {
            scope: scope.clone(),
            address: CanonicalAddress {
                scope: scope.clone(),
                value: "address/with space".to_owned(),
            },
            after: Some(TransactionRef {
                scope: scope.clone(),
                value: "before/1".to_owned(),
            }),
            limit: 25,
        })
        .await
        .expect("history");
    assert_eq!(page.transactions.len(), 1);
    assert_eq!(
        page.transactions[0].movements[0].amount().to_string(),
        "12.5"
    );
    assert_eq!(page.next.expect("continuation").value, "transaction-2");

    let found = indexer
        .transaction(TransactionQuery {
            scope: scope.clone(),
            transaction_id: TransactionRef {
                scope: scope.clone(),
                value: "transaction-1".to_owned(),
            },
        })
        .await
        .expect("transaction")
        .expect("present transaction");
    assert_eq!(found.transaction_id.value, "transaction-1");
    let missing = indexer
        .transaction(TransactionQuery {
            scope: scope.clone(),
            transaction_id: TransactionRef {
                scope: scope.clone(),
                value: "missing".to_owned(),
            },
        })
        .await
        .expect("missing lookup");
    assert!(missing.is_none());

    let events = indexer
        .events(EventQuery {
            scope: scope.clone(),
            after: Some(EventCursor(7)),
            limit: 10,
        })
        .await
        .expect("events");
    assert_eq!(events.events.len(), 1);
    assert_eq!(events.next, Some(EventCursor(8)));

    let snapshot = OutputSnapshot {
        generation: RebuildGeneration(7),
        revision: 9,
        checkpoint: Some(BlockRef {
            height: BlockHeight(42),
            hash: BlockHash(vec![0x11; 32]),
            parent_hash: Some(BlockHash(vec![0x10; 32])),
            timestamp: Some(999),
        }),
    };
    let outputs = remote
        .outputs(OutputRequest {
            scope: scope.clone(),
            address: CanonicalAddress {
                scope: scope.clone(),
                value: "address/with space".to_owned(),
            },
            after: Some(OutputCursor {
                snapshot: snapshot.clone(),
                position: vec![0xaa, 0xbb],
            }),
            limit: 5,
        })
        .await
        .expect("outputs");
    assert_eq!(outputs.snapshot, snapshot);
    assert_eq!(outputs.outputs.len(), 1);
    assert_eq!(outputs.outputs[0].amount.to_string(), "75000");
    assert_eq!(outputs.outputs[0].evidence, vec![0x51, 0x20]);
    assert_eq!(
        outputs.next.expect("output continuation").position,
        vec![0xcc, 0xdd]
    );

    let outcome = indexer
        .unwatch(UnwatchRequest {
            scope,
            watch_id: first.id,
        })
        .await
        .expect("unwatch");
    assert_eq!(outcome, UnwatchOutcome::Deactivated);
}

#[tokio::test]
async fn remote_rejects_identities_outside_the_route_scope() {
    let remote = Remote::new(
        Arc::new(RouterClient(router())),
        &Config::new("http://indexer.test"),
    )
    .expect("valid client");
    let route_scope = scope();
    let other_scope = IndexScope {
        chain: route_scope.chain.clone(),
        network: "other-network".to_owned(),
    };

    let watch_error = remote
        .watch(WatchRequest {
            scope: route_scope.clone(),
            selector: WatchSelector::Address(CanonicalAddress {
                scope: other_scope.clone(),
                value: "address".to_owned(),
            }),
            start_height: BlockHeight(0),
            idempotency_key: "wrong-network-watch".to_owned(),
        })
        .await
        .expect_err("watch identity must match its route scope");
    assert_eq!(watch_error.kind, indexing::IndexErrorKind::ScopeMismatch);

    let transaction_error = remote
        .transaction(TransactionQuery {
            scope: route_scope.clone(),
            transaction_id: TransactionRef {
                scope: other_scope.clone(),
                value: "transaction".to_owned(),
            },
        })
        .await
        .expect_err("transaction identity must match its route scope");
    assert_eq!(
        transaction_error.kind,
        indexing::IndexErrorKind::ScopeMismatch
    );

    let history_error = remote
        .history(HistoryQuery {
            scope: route_scope.clone(),
            address: CanonicalAddress {
                scope: other_scope.clone(),
                value: "address".to_owned(),
            },
            after: None,
            limit: 1,
        })
        .await
        .expect_err("history address must match its route scope");
    assert_eq!(history_error.kind, indexing::IndexErrorKind::ScopeMismatch);

    let output_error = remote
        .outputs(OutputRequest {
            scope: route_scope,
            address: CanonicalAddress {
                scope: other_scope,
                value: "address".to_owned(),
            },
            after: None,
            limit: 1,
        })
        .await
        .expect_err("output address must match its route scope");
    assert_eq!(output_error.kind, indexing::IndexErrorKind::ScopeMismatch);
}
