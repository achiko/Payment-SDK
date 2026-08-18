use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use axum::http::StatusCode;
use chain_bitcoin::Network;
use indexing::{
    BlockHeight, BoxFuture, IndexError, IndexScope, Indexer, OutputQuery, OutputRequest,
    StatusStore, SyncPhase, SyncStatus,
};

pub trait StatusReader: Send + Sync {
    fn status<'a>(&'a self, scope: &'a IndexScope)
    -> BoxFuture<'a, Result<SyncStatus, IndexError>>;
}

impl StatusReader for indexing_rocksdb::RocksRepository {
    fn status<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
        StatusStore::status(self, scope)
    }
}

impl StatusReader for indexing_rocksdb::Handle {
    fn status<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> indexing::BoxFuture<'a, Result<SyncStatus, IndexError>> {
        Box::pin(async move { self.status(scope).await })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChainKind {
    Ethereum,
    Bitcoin(Network),
}

impl ChainKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Ethereum => "ethereum",
            Self::Bitcoin(_) => "bitcoin",
        }
    }
}

pub struct State {
    scope: IndexScope,
    chain: ChainKind,
    indexer: Arc<dyn Indexer>,
    status: Arc<dyn StatusReader>,
    outputs: Option<Arc<dyn OutputQuery>>,
    operational_health: Option<http::server::HealthState>,
    bootstrap_height: BlockHeight,
    request_counter: AtomicU64,
}

impl State {
    #[must_use]
    pub(crate) fn new<R>(
        scope: IndexScope,
        repository: Arc<R>,
        bootstrap_height: BlockHeight,
    ) -> Self
    where
        R: Indexer + StatusReader + 'static,
    {
        Self {
            scope,
            chain: ChainKind::Ethereum,
            indexer: repository.clone(),
            status: repository,
            outputs: None,
            operational_health: None,
            bootstrap_height,
            request_counter: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub(crate) fn new_bitcoin<R>(
        scope: IndexScope,
        network: Network,
        repository: Arc<R>,
        outputs: Arc<dyn OutputQuery>,
        operational_health: http::server::HealthState,
        bootstrap_height: BlockHeight,
    ) -> Self
    where
        R: Indexer + StatusReader + 'static,
    {
        Self {
            scope,
            chain: ChainKind::Bitcoin(network),
            indexer: repository.clone(),
            status: repository,
            outputs: Some(outputs),
            operational_health: Some(operational_health),
            bootstrap_height,
            request_counter: AtomicU64::new(0),
        }
    }

    fn request_id(&self) -> String {
        let next = self.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("ix-request-{next:020}")
    }

    fn validate_scope(&self, chain: &str, network: &str) -> Result<(), ResponseError> {
        if chain == self.chain.name() && network == self.scope.network {
            Ok(())
        } else {
            Err(ResponseError::new(
                StatusCode::NOT_FOUND,
                "scope_not_found",
                "requested Indexer scope does not exist",
                false,
                self.request_id(),
            ))
        }
    }

    async fn semantic_status(&self) -> Result<SyncStatus, ResponseError> {
        let status = self
            .status
            .status(&self.scope)
            .await
            .map_err(|error| ResponseError::from_index(error, self.request_id()))?;
        if matches!(status.phase, SyncPhase::RebuildRequired | SyncPhase::Halted) {
            return Err(ResponseError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "semantic_surface_unavailable",
                "semantic operations are unavailable until Indexer recovery completes",
                true,
                self.request_id(),
            ));
        }
        Ok(status)
    }
}

mod command;
mod query;
mod response;

pub use command::router;
use command::*;
use query::*;
use response::*;

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::Extension,
        http::Request,
        response::Response,
    };
    use base::Decimal;
    use http::server::AuthenticationMode;
    use indexing::{
        AssetId, BlockHash, ChainId, Checkpoint, ConfirmationPolicy, EventPage, EventQuery,
        History, HistoryQuery, IndexErrorKind, IndexedOutput, ObservedTransaction, Observer,
        OutputId, RebuildReason, TransactionPage, TransactionQuery, TransactionRef, UnwatchOutcome,
        UnwatchRequest, WatchReceipt, WatchRequest, Watcher,
    };
    use indexing::{OutputCursor, OutputPage, OutputSnapshot};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct FakeRepository {
        status: SyncStatus,
    }

    enum FakeBitcoinUtxos {
        Value,
    }

    impl OutputQuery for FakeBitcoinUtxos {
        fn outputs<'a>(
            &'a self,
            request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            Box::pin(async move {
                Ok(OutputPage {
                    snapshot: OutputSnapshot {
                        generation: indexing::RebuildGeneration(7),
                        revision: 9,
                        checkpoint: Some(indexing::BlockRef {
                            height: BlockHeight(42),
                            hash: BlockHash(vec![0x11; 32]),
                            parent_hash: Some(BlockHash(vec![0x10; 32])),
                            timestamp: Some(1_000),
                        }),
                    },
                    outputs: vec![IndexedOutput {
                        id: OutputId {
                            transaction: TransactionRef {
                                scope: request.address.scope.clone(),
                                value: "22".repeat(32),
                            },
                            index: 1,
                        },
                        asset: AssetId {
                            chain: request.address.scope.chain.clone(),
                            asset: "native".to_owned(),
                        },
                        amount: Decimal::from(75_000_u64),
                        evidence: vec![0x51, 0x20],
                        address: request.address,
                        created_at: BlockHeight(40),
                        coinbase: false,
                    }],
                    next: None,
                })
            })
        }
    }

    impl StatusReader for FakeRepository {
        fn status<'a>(
            &'a self,
            _scope: &'a IndexScope,
        ) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
            let status = self.status.clone();
            Box::pin(async move { Ok(status) })
        }
    }

    impl Checkpoint for FakeRepository {
        fn checkpoint<'a>(
            &'a self,
            _scope: &'a IndexScope,
        ) -> BoxFuture<'a, Result<Option<indexing::BlockRef>, IndexError>> {
            let checkpoint = self.status.checkpoint.clone();
            Box::pin(async move { Ok(checkpoint) })
        }
    }

    impl Watcher for FakeRepository {
        fn watch<'a>(
            &'a self,
            request: WatchRequest,
        ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
            let registered_at = self.status.checkpoint.clone();
            let confirmation_policy = self.status.confirmation_policy;
            Box::pin(async move {
                Ok(WatchReceipt {
                    id: indexing::WatchId("watch-1".to_owned()),
                    scope: request.scope,
                    selector: request.selector,
                    start_height: request.start_height,
                    registered_at,
                    inactive_from: None,
                    confirmation_policy,
                })
            })
        }

        fn unwatch<'a>(
            &'a self,
            _request: UnwatchRequest,
        ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
            unexpected_call()
        }
    }

    impl History for FakeRepository {
        fn transaction<'a>(
            &'a self,
            _request: TransactionQuery,
        ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
            unexpected_call()
        }

        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            unexpected_call()
        }
    }

    impl Observer for FakeRepository {
        fn events<'a>(
            &'a self,
            _request: EventQuery,
        ) -> BoxFuture<'a, Result<EventPage, IndexError>> {
            unexpected_call()
        }
    }

    fn unexpected_call<'a, T>() -> BoxFuture<'a, Result<T, IndexError>> {
        Box::pin(async {
            Err(IndexError::new(
                IndexErrorKind::Other,
                "unexpected fake repository call",
                false,
            ))
        })
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId(chain_ethereum::CHAIN.to_owned()),
            network: "test".to_owned(),
        }
    }

    fn status(phase: SyncPhase) -> SyncStatus {
        let checkpoint = indexing::BlockRef {
            height: BlockHeight(42),
            hash: BlockHash(vec![0x11; 32]),
            parent_hash: Some(BlockHash(vec![0x10; 32])),
            timestamp: Some(1_000),
        };
        SyncStatus {
            scope: scope(),
            checkpoint: Some(checkpoint.clone()),
            observed_tip: Some(checkpoint.clone()),
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: 12,
                require_chain_finality: false,
            },
            phase,
            rebuild_reason: (phase == SyncPhase::RebuildRequired).then_some(RebuildReason {
                checkpoint,
                oldest_retained: BlockHeight(1),
                message: "operator rebuild required".to_owned(),
            }),
            halted_reason: None,
        }
    }

    fn bitcoin_scope() -> IndexScope {
        IndexScope {
            chain: ChainId(chain_bitcoin::CHAIN.to_owned()),
            network: "regtest".to_owned(),
        }
    }

    fn bitcoin_status() -> SyncStatus {
        let mut value = status(SyncPhase::Ready);
        value.scope = bitcoin_scope();
        value
    }

    fn regtest_address() -> String {
        let public_key = bitcoin::PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        bitcoin::Address::p2wpkh(
            &bitcoin::CompressedPublicKey::try_from(public_key)
                .expect("test public key must be compressed"),
            bitcoin::Network::Regtest,
        )
        .to_string()
    }

    fn regtest_legacy_address() -> String {
        let public_key = bitcoin::PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        bitcoin::Address::p2pkh(public_key, bitcoin::Network::Regtest).to_string()
    }

    fn app(phase: SyncPhase) -> Router {
        app_with_mode(phase, AuthenticationMode::Strict)
    }

    fn app_with_mode(phase: SyncPhase, authentication_mode: AuthenticationMode) -> Router {
        raw_app(phase).layer(Extension(authentication_mode))
    }

    fn raw_app(phase: SyncPhase) -> Router {
        let state = Arc::new(State::new(
            scope(),
            Arc::new(FakeRepository {
                status: status(phase),
            }),
            BlockHeight(10),
        ));
        router(state)
    }

    fn served_app(authentication_mode: AuthenticationMode) -> Router {
        let config = http::server::Config::new(
            SocketAddr::from(([127, 0, 0, 1], 8080)),
            http::server::TransportSecurity::PlaintextLoopback,
            Some(
                http::server::BearerToken::new("indexer-test-token")
                    .expect("test bearer must be valid"),
            ),
            http::server::RequestLimits::default(),
        )
        .with_authentication_mode(authentication_mode);
        http::server::service_router(
            raw_app(SyncPhase::Ready),
            &config,
            http::server::HealthState::new(true),
        )
        .expect("test service router must be valid")
    }

    fn bitcoin_app() -> Router {
        bitcoin_app_with_health(true)
    }

    fn bitcoin_app_with_health(ready: bool) -> Router {
        let state = Arc::new(State::new_bitcoin(
            bitcoin_scope(),
            Network::Regtest,
            Arc::new(FakeRepository {
                status: bitcoin_status(),
            }),
            Arc::new(FakeBitcoinUtxos::Value),
            http::server::HealthState::new(ready),
            BlockHeight(0),
        ));
        router(state).layer(Extension(AuthenticationMode::Strict))
    }

    async fn response_json(response: Response) -> Value {
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("test response body must be readable");
        serde_json::from_slice(&body).expect("test response must be JSON")
    }

    #[test]
    fn fixed_hex_parser_is_strict() {
        assert_eq!(
            decode_fixed::<2>("0x00ff").expect("valid bytes must decode"),
            [0, 255]
        );
        assert!(decode_fixed::<2>("00ff").is_err());
        assert!(decode_fixed::<2>("0x0ff").is_err());
        assert!(decode_fixed::<2>("0x00fg").is_err());
    }

    #[tokio::test]
    async fn status_encodes_large_fields_as_strings() {
        let response = app(SyncPhase::Ready)
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["checkpoint"]["height"], "42");
        assert_eq!(body["confirmation_depth"], "12");
        assert_eq!(body["authentication_mode"], "strict");
    }

    #[tokio::test]
    async fn checkpoint_returns_the_canonical_watch_boundary() {
        let response = app(SyncPhase::Ready)
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/checkpoint")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["height"], "42");
        assert_eq!(body["hash"], format!("0x{}", "11".repeat(32)));
    }

    #[tokio::test]
    async fn one_router_rejects_a_chain_outside_its_composed_scope() {
        let response = app(SyncPhase::Ready)
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/bitcoin/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response_json(response).await["code"], "scope_not_found");
    }

    #[tokio::test]
    async fn status_reports_global_trusted_authentication_mode() {
        let response = app_with_mode(SyncPhase::Ready, AuthenticationMode::GlobalTrusted)
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response_json(response).await["authentication_mode"],
            "global_trusted"
        );
    }

    #[tokio::test]
    async fn strict_requires_bearer_on_loopback_while_global_trusted_ignores_it() {
        let strict = served_app(AuthenticationMode::Strict);
        let unauthorized = strict
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = strict
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/status")
                    .header(
                        axum::http::header::AUTHORIZATION,
                        "Bearer indexer-test-token",
                    )
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(authorized.status(), StatusCode::OK);

        let global = served_app(AuthenticationMode::GlobalTrusted)
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(global.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn strict_watch_registration_requires_idempotency_without_changing_its_response() {
        let application = app(SyncPhase::Ready);
        let response = application
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scopes/ethereum/test/watches")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"selector":{{"type":"address","value":"0x{}"}},"start_height":"42"}}"#,
                        "11".repeat(20)
                    )))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json(response).await["code"],
            "invalid_idempotency_key"
        );

        let response = application
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scopes/ethereum/test/watches")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"selector":{{"type":"address","value":"0x{}"}},"start_height":"42","idempotency_key":"deposit-1"}}"#,
                        "11".repeat(20)
                    )))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("idempotency-key").is_none());
        assert!(
            response_json(response)
                .await
                .get("idempotency_key")
                .is_none()
        );
    }

    #[tokio::test]
    async fn global_trusted_watch_also_requires_idempotency() {
        let response = app_with_mode(SyncPhase::Ready, AuthenticationMode::GlobalTrusted)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scopes/ethereum/test/watches")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"selector":{{"type":"address","value":"0x{}"}},"start_height":"42"}}"#,
                        "11".repeat(20)
                    )))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json(response).await["code"],
            "invalid_idempotency_key"
        );
    }

    #[tokio::test]
    async fn validation_errors_use_the_structured_contract() {
        let response = app(SyncPhase::Ready)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scopes/ethereum/test/watches")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"selector":{{"type":"address","value":"0x{}"}},"start_height":"9","idempotency_key":"deposit-1"}}"#,
                        "11".repeat(20)
                    )))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response_json(response).await;
        assert_eq!(body["code"], "invalid_start_height");
        assert_eq!(body["retryable"], false);
        assert!(
            body["request_id"]
                .as_str()
                .is_some_and(|value| value.starts_with("ix-request-"))
        );
    }

    #[tokio::test]
    async fn rebuild_required_keeps_status_available_and_blocks_semantic_queries() {
        let application = app(SyncPhase::RebuildRequired);
        let status_response = application
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(status_response.status(), StatusCode::OK);

        let semantic_response = application
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/scopes/ethereum/test/transactions/0x{}",
                        "22".repeat(32)
                    ))
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(semantic_response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response_json(semantic_response).await;
        assert_eq!(body["code"], "semantic_surface_unavailable");
        assert_eq!(body["retryable"], true);
    }

    #[tokio::test]
    async fn pagination_above_the_public_maximum_is_rejected() {
        let response = app(SyncPhase::Ready)
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/ethereum/test/events?limit=1001")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response_json(response).await;
        assert_eq!(body["code"], "invalid_page_size");
    }

    #[tokio::test]
    async fn routing_errors_use_the_structured_contract() {
        let application = app(SyncPhase::Ready);
        let not_found = application
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/unknown")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        assert_eq!(response_json(not_found).await["code"], "route_not_found");

        let method = application
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scopes/ethereum/test/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response_json(method).await["code"], "method_not_allowed");
    }

    #[tokio::test]
    async fn bitcoin_utxo_route_returns_decimal_values_and_confirmations() {
        let address = regtest_address();
        let response = bitcoin_app()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/scopes/bitcoin/regtest/addresses/{address}/outputs?limit=10"
                    ))
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["generation"], "7");
        assert_eq!(body["revision"], "9");
        assert_eq!(body["checkpoint"]["height"], "42");
        assert_eq!(body["outputs"][0]["output_index"], "1");
        assert_eq!(body["outputs"][0]["amount"], "75000");
        assert_eq!(body["outputs"][0]["created_height"], "40");
        assert_eq!(body["outputs"][0]["asset"], "native");
        assert_eq!(body["outputs"][0]["evidence"], "0x5120");
        assert_eq!(body["outputs"][0]["address"], address);
        assert!(body["next"].is_null());
    }

    #[tokio::test]
    async fn bitcoin_utxo_route_requires_operational_readiness() {
        let address = regtest_address();
        let response = bitcoin_app_with_health(false)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/scopes/bitcoin/regtest/addresses/{address}/outputs"
                    ))
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response_json(response).await;
        assert_eq!(body["code"], "output_snapshot_unavailable");
        assert_eq!(body["retryable"], true);
    }

    #[tokio::test]
    async fn bitcoin_watch_rejects_unsupported_legacy_address_before_persistence() {
        let response = bitcoin_app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scopes/bitcoin/regtest/watches")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"selector":{{"type":"address","value":"{}"}},"start_height":"42","idempotency_key":"unsupported-address"}}"#,
                        regtest_legacy_address()
                    )))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response_json(response).await["code"], "unsupported_address");
    }

    #[tokio::test]
    async fn bitcoin_block_hashes_use_core_display_order_without_ethereum_prefix() {
        let response = bitcoin_app()
            .oneshot(
                Request::builder()
                    .uri("/v1/scopes/bitcoin/regtest/status")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["checkpoint"]["hash"], "11".repeat(32));
        assert_eq!(body["checkpoint"]["parent_hash"], "10".repeat(32));
    }

    #[test]
    fn projection_cursor_round_trips_full_snapshot_and_relative_key() {
        let state = State::new_bitcoin(
            bitcoin_scope(),
            Network::Regtest,
            Arc::new(FakeRepository {
                status: bitcoin_status(),
            }),
            Arc::new(FakeBitcoinUtxos::Value),
            http::server::HealthState::new(true),
            BlockHeight(0),
        );
        let cursor = OutputCursor {
            snapshot: OutputSnapshot {
                generation: indexing::RebuildGeneration(7),
                revision: 9,
                checkpoint: Some(indexing::BlockRef {
                    height: BlockHeight(42),
                    hash: BlockHash(vec![0x11; 32]),
                    parent_hash: Some(BlockHash(vec![0x10; 32])),
                    timestamp: Some(1_000),
                }),
            },
            position: vec![0x00, 0xab, 0xff],
        };
        let encoded = encode_projection_cursor(&cursor);

        let decoded = match decode_projection_cursor(&encoded, &state) {
            Ok(decoded) => decoded,
            Err(_) => panic!("encoded cursor must decode"),
        };
        assert_eq!(decoded, cursor);
    }
}
