use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use base::{
    Address, Addresser, BlockHash, BlockHeight, BlockPosition, BlockRef, Broadcaster, Decimal,
    SignRequest, SignedTransaction, Signer, Submission, TransactionBuilder, TransactionEnvelope,
    TransactionError, TransactionId, TransactionSnapshot,
};
use http_body_util::BodyExt;
use indexing::{
    AssetId, BoxFuture, CanonicalAddress, ChainId, Checkpoint, IndexError, IndexScope, MovementId,
    MovementKind, TransactionRef,
};
use payment_api::{State, WalletAsset, router};
use serde_json::{Value, json};
use tokio::sync::watch;
use tower::ServiceExt;
use wallets::{
    AddressEncoding, AddressFormat, AddressText, BalanceReader, FutureResult, HistoryReader,
    Provider, SecretBytes, SendFuture, Sender, TransactionFactory, Transfer, Wallets,
};

const TOKEN: &str = "route-contract-secret";

#[derive(Default)]
struct Calls {
    transfers: Mutex<Vec<Decimal>>,
    broadcasts: Mutex<usize>,
    batches: Mutex<Vec<usize>>,
}

enum FixtureCheckpoint {
    Value,
}

impl Checkpoint for FixtureCheckpoint {
    fn checkpoint<'a>(
        &'a self,
        _scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async { Ok(Some(block(8))) })
    }
}

struct FixtureProvider {
    calls: Arc<Calls>,
}

impl Provider for FixtureProvider {
    fn create<'a>(&'a self, _secret: SecretBytes) -> FutureResult<'a, Arc<dyn wallets::Wallet>> {
        let wallet = FixtureWallet {
            calls: Arc::clone(&self.calls),
        };
        Box::pin(async move { Ok(Arc::new(wallet) as Arc<dyn wallets::Wallet>) })
    }

    fn generate(&self) -> FutureResult<'_, Arc<dyn wallets::Wallet>> {
        self.create(SecretBytes::new([7_u8; 32]))
    }
}

struct FixtureSender {
    calls: Arc<Calls>,
}

impl Sender for FixtureSender {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls
                .batches
                .lock()
                .expect("fixture batch lock")
                .push(transfers.len());
            Ok(vec![TransactionId::new("fixture-batch")])
        })
    }
}

struct FixtureWallet {
    calls: Arc<Calls>,
}

impl Addresser for FixtureWallet {
    fn address(&self) -> Address {
        Address::from([7_u8; 20])
    }
}

impl Signer for FixtureWallet {
    fn sign<'a>(&'a self, _request: SignRequest) -> base::SignFuture<'a> {
        Box::pin(async { unreachable!("the fixture builder prepares its own envelope") })
    }
}

impl AddressFormat for FixtureWallet {
    fn address_text(&self, address: &Address) -> Result<AddressText, wallets::Error> {
        if address != &self.address() {
            return Err(wallets::Error::new(
                wallets::ErrorKind::AddressMismatch,
                "fixture address does not belong to this wallet",
            ));
        }
        Ok(AddressText::new(AddressEncoding::Hex, "fixture-address"))
    }

    fn parse_address(&self, address: &AddressText) -> Result<Address, wallets::Error> {
        if address.encoding != AddressEncoding::Hex || address.text != "fixture-destination" {
            return Err(wallets::Error::new(
                wallets::ErrorKind::InvalidAddress,
                "fixture destination is invalid",
            ));
        }
        Ok(Address::from([9_u8; 20]))
    }
}

impl BalanceReader for FixtureWallet {
    fn balance<'a>(&'a self) -> FutureResult<'a, wallets::Balance> {
        Box::pin(async {
            Ok(wallets::Balance {
                amount: "12.5".parse().expect("fixed decimal"),
                observed_at: Some(block(8)),
            })
        })
    }
}

impl HistoryReader for FixtureWallet {
    fn history<'a>(
        &'a self,
        _request: wallets::HistoryRequest,
    ) -> FutureResult<'a, wallets::History> {
        Box::pin(async {
            let scope = scope();
            let address = CanonicalAddress {
                scope: scope.clone(),
                value: "fixture-address".to_owned(),
            };
            Ok(wallets::History {
                checkpoint: Some(block(8)),
                transactions: vec![wallets::HistoryEntry {
                    scope: scope.clone(),
                    transaction_id: TransactionRef {
                        scope: scope.clone(),
                        value: "fixture-observed".to_owned(),
                    },
                    status: wallets::HistoryStatus::Confirmed {
                        block: block(7),
                        confirmations: 2,
                    },
                    movements: vec![wallets::HistoryMovement {
                        id: MovementId("output-0".to_owned()),
                        kind: MovementKind::Output,
                        asset: wallets::HistoryAsset {
                            id: AssetId {
                                chain: scope.chain.clone(),
                                asset: "native".to_owned(),
                            },
                            name: Some("Fixture Coin".to_owned()),
                            ticker: Some("FIX".to_owned()),
                            decimals: 8,
                        },
                        amount: "1.5".parse().expect("fixed decimal"),
                        from: None,
                        to: Some(address),
                    }],
                    fee: None,
                }],
                next: None,
            })
        })
    }
}

impl TransactionFactory for FixtureWallet {
    fn transaction(&self) -> Box<dyn TransactionBuilder> {
        Box::new(FixtureTransaction {
            calls: Arc::clone(&self.calls),
        })
    }

    fn restore(
        &self,
        _snapshot: &TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
        Ok(self.transaction())
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

impl Broadcaster for FixtureWallet {
    fn broadcast<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
    ) -> base::TransactionFuture<'a, Result<Submission, TransactionError>> {
        Box::pin(async move {
            *self
                .calls
                .broadcasts
                .lock()
                .expect("fixture broadcast lock") += 1;
            Ok(Submission {
                id: transaction.id().clone(),
            })
        })
    }
}

struct FixtureTransaction {
    calls: Arc<Calls>,
}

impl TransactionBuilder for FixtureTransaction {
    fn transfer(&mut self, _destination: Address, amount: Decimal) -> Result<(), TransactionError> {
        self.calls
            .transfers
            .lock()
            .expect("fixture transfer lock")
            .push(amount);
        Ok(())
    }

    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
        Ok(TransactionSnapshot::new("fixture", json!({})))
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> base::TransactionFuture<'a, Result<SignedTransaction, TransactionError>> {
        Box::pin(async {
            Ok(SignedTransaction::new(
                "fixture",
                TransactionId::new("fixture-single"),
                TransactionEnvelope::new([1_u8]),
            ))
        })
    }
}

struct Fixture {
    app: Router,
    ready: watch::Sender<bool>,
    calls: Arc<Calls>,
}

fn fixture(initially_ready: bool) -> Fixture {
    fixture_with_usdc(initially_ready, true)
}

fn fixture_with_usdc(initially_ready: bool, usdc: bool) -> Fixture {
    let calls = Arc::new(Calls::default());
    let checkpoint: Arc<dyn Checkpoint> = Arc::new(FixtureCheckpoint::Value);
    let mut wallets = Wallets::<String, WalletAsset>::new(checkpoint);
    wallets
        .register(
            WalletAsset::Btc,
            scope(),
            FixtureProvider {
                calls: Arc::clone(&calls),
            },
            Arc::new(FixtureSender {
                calls: Arc::clone(&calls),
            }),
            None,
        )
        .expect("fixture family must register");
    if usdc {
        wallets
            .register(
                WalletAsset::Usdc,
                ethereum_scope(),
                FixtureProvider {
                    calls: Arc::clone(&calls),
                },
                Arc::new(FixtureSender {
                    calls: Arc::clone(&calls),
                }),
                None,
            )
            .expect("USDC fixture family must register");
    }
    let (ready, receiver) = watch::channel(initially_ready);
    let state = State::new(Arc::new(wallets), receiver);
    let token = http_support::server::BearerToken::new(TOKEN).expect("fixture bearer token");
    let config = http_support::server::Config::new(
        "127.0.0.1:0".parse().expect("fixture bind address"),
        http_support::server::TransportSecurity::PlaintextLoopback,
        Some(token),
        http_support::server::RequestLimits::default(),
    );
    Fixture {
        app: router(state, &config).expect("fixture router must build"),
        ready,
        calls,
    }
}

#[tokio::test]
async fn unconfigured_wallet_asset_is_not_found() {
    let fixture = fixture_with_usdc(true, false);
    let response = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "usdc"})),
        true,
    )
    .await;

    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&response)["message"],
        "wallet asset is not configured"
    );
}

#[tokio::test]
async fn invalid_wallet_asset_uses_the_json_error_contract() {
    let fixture = fixture(true);
    let response = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "usd"})),
        true,
    )
    .await;

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&response)["message"],
        "request body must match the documented JSON schema"
    );
}

#[tokio::test]
async fn readiness_reflects_runtime_state_while_liveness_stays_available() {
    let fixture = fixture(false);

    assert_eq!(
        request(&fixture.app, "GET", "/health/live", None, false)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&fixture.app, "GET", "/health/ready", None, false)
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );

    fixture.ready.send(true).expect("readiness receiver exists");

    assert_eq!(
        request(&fixture.app, "GET", "/health/ready", None, false)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn wallet_routes_delegate_to_the_wallet_collection() {
    let fixture = fixture(true);
    let unauthorized = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "btc"})),
        false,
    )
    .await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);

    let created = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "btc"})),
        true,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let wallet: Value = serde_json::from_slice(&created.body).expect("wallet JSON");
    let id = wallet["id"].as_str().expect("wallet ID");
    assert_eq!(wallet["asset"], "btc");
    assert_eq!(wallet["chain"], "bitcoin");
    assert_eq!(wallet["network"], "regtest");
    assert_eq!(wallet["address"], "fixture-address");

    let usdc = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "usdc"})),
        true,
    )
    .await;
    assert_eq!(usdc.status, StatusCode::CREATED);
    let usdc = json_body(&usdc);
    assert_eq!(usdc["asset"], "usdc");
    assert_eq!(usdc["chain"], "ethereum");
    assert_eq!(usdc["network"], "mainnet");

    let read = request(
        &fixture.app,
        "GET",
        &format!("/v1/wallets/{id}"),
        None,
        true,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&read.body).expect("wallet JSON"),
        wallet
    );

    let balance = request(
        &fixture.app,
        "GET",
        &format!("/v1/wallets/{id}/balance"),
        None,
        true,
    )
    .await;
    assert_eq!(balance.status, StatusCode::OK);
    assert_eq!(
        json_body(&balance),
        json!({"amount": "12.5", "observed_height": 8})
    );

    let history = request(
        &fixture.app,
        "GET",
        &format!("/v1/wallets/{id}/transactions?limit=10"),
        None,
        true,
    )
    .await;
    assert_eq!(history.status, StatusCode::OK);
    let history = json_body(&history);
    assert_eq!(history["checkpoint"]["height"], 8);
    assert_eq!(
        history["transactions"][0]["transaction_id"],
        "fixture-observed"
    );
    assert_eq!(history["transactions"][0]["status"]["kind"], "confirmed");
    assert_eq!(history["transactions"][0]["status"]["confirmations"], 2);
    assert_eq!(history["transactions"][0]["movements"][0]["amount"], "1.5");

    let submitted = request(
        &fixture.app,
        "POST",
        &format!("/v1/wallets/{id}/transactions"),
        Some(json!({
            "destination": {"encoding": "hex", "text": "fixture-destination"},
            "amount": "2.25"
        })),
        true,
    )
    .await;
    assert_eq!(submitted.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&submitted),
        json!({"transaction_id": "fixture-single"})
    );

    let batch = request(
        &fixture.app,
        "POST",
        "/v1/transactions",
        Some(json!({"transfers": [{
            "wallet_id": id,
            "destination": {"encoding": "hex", "text": "fixture-destination"},
            "amount": "3"
        }]})),
        true,
    )
    .await;
    assert_eq!(batch.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&batch),
        json!({"transaction_ids": ["fixture-batch"]})
    );

    assert_eq!(
        fixture
            .calls
            .transfers
            .lock()
            .expect("transfer calls")
            .as_slice(),
        &["2.25".parse::<Decimal>().expect("fixed decimal")]
    );
    assert_eq!(
        *fixture.calls.broadcasts.lock().expect("broadcast calls"),
        1
    );
    assert_eq!(
        fixture
            .calls
            .batches
            .lock()
            .expect("batch calls")
            .as_slice(),
        &[1]
    );
}

#[tokio::test]
async fn transaction_query_rejection_follows_auth_and_precedes_body_semantics() {
    let fixture = fixture(true);
    let unauthorized = raw_request(
        &fixture.app,
        "POST",
        "/v1/transactions?commitment=finalized",
        Some("{"),
        false,
        &[],
    )
    .await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);

    let wallet_id = generated_wallet_id(&fixture).await;
    let malformed = raw_request(
        &fixture.app,
        "POST",
        &format!("/v1/wallets/{wallet_id}/transactions?commitment=finalized"),
        Some("{"),
        true,
        &[],
    )
    .await;
    assert_transaction_query_rejected(&malformed);

    let empty = request(
        &fixture.app,
        "POST",
        "/v1/transactions?min_context_slot=9",
        Some(json!({"transfers": []})),
        true,
    )
    .await;
    assert_transaction_query_rejected(&empty);

    let transfers = (0..51)
        .map(|_| {
            json!({
                "wallet_id": wallet_id,
                "destination": {
                    "encoding": "hex",
                    "text": "fixture-destination"
                },
                "amount": "1"
            })
        })
        .collect::<Vec<_>>();
    let oversized = request(
        &fixture.app,
        "POST",
        "/v1/transactions?priority_fee=1",
        Some(json!({"transfers": transfers})),
        true,
    )
    .await;
    assert_transaction_query_rejected(&oversized);
    assert_no_transaction_calls(&fixture.calls);
}

#[tokio::test]
async fn empty_query_and_unrecognized_headers_do_not_change_transaction_semantics() {
    let fixture = fixture(true);
    let wallet_id = generated_wallet_id(&fixture).await;
    let single_body = json!({
        "destination": {"encoding": "hex", "text": "fixture-destination"},
        "amount": "2.25"
    })
    .to_string();
    let single = raw_request(
        &fixture.app,
        "POST",
        &format!("/v1/wallets/{wallet_id}/transactions?"),
        Some(&single_body),
        true,
        &[
            ("x-min-context-slot", "9"),
            ("traceparent", "fixture-trace"),
        ],
    )
    .await;
    assert_eq!(single.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&single),
        json!({"transaction_id": "fixture-single"})
    );

    let batch_body = json!({"transfers": [{
        "wallet_id": wallet_id,
        "destination": {"encoding": "hex", "text": "fixture-destination"},
        "amount": "3"
    }]})
    .to_string();
    let batch = raw_request(
        &fixture.app,
        "POST",
        "/v1/transactions?",
        Some(&batch_body),
        true,
        &[("x-retry-transaction", "true")],
    )
    .await;
    assert_eq!(batch.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&batch),
        json!({"transaction_ids": ["fixture-batch"]})
    );

    assert_eq!(
        fixture
            .calls
            .transfers
            .lock()
            .expect("transfer calls")
            .as_slice(),
        &["2.25".parse::<Decimal>().expect("fixed decimal")]
    );
    assert_eq!(
        *fixture.calls.broadcasts.lock().expect("broadcast calls"),
        1
    );
    assert_eq!(
        fixture
            .calls
            .batches
            .lock()
            .expect("batch calls")
            .as_slice(),
        &[1]
    );
}

#[tokio::test]
async fn batch_wire_maximum_precedes_conversion_and_leaves_minimum_to_the_sdk() {
    let fixture = fixture(true);

    let empty = request(
        &fixture.app,
        "POST",
        "/v1/transactions",
        Some(json!({"transfers": []})),
        true,
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&empty),
        json!({"message": "at least one transfer is required"})
    );

    let oversized = request(
        &fixture.app,
        "POST",
        "/v1/transactions",
        Some(json!({
            "transfers": (0..=wallets::MAX_TRANSFERS)
                .map(|_| json!({
                    "wallet_id": "missing-wallet",
                    "destination": {
                        "encoding": "hex",
                        "text": "fixture-destination"
                    },
                    "amount": "not-a-decimal"
                }))
                .collect::<Vec<_>>()
        })),
        true,
    )
    .await;
    assert_eq!(oversized.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&oversized),
        json!({"message": "at most 50 transfers are allowed"})
    );
    assert_no_transaction_calls(&fixture.calls);

    let wallet_id = generated_wallet_id(&fixture).await;
    for admitted_count in [1, wallets::MAX_TRANSFERS] {
        let transfers = (0..admitted_count)
            .map(|_| {
                json!({
                    "wallet_id": wallet_id,
                    "destination": {
                        "encoding": "hex",
                        "text": "fixture-destination"
                    },
                    "amount": "1"
                })
            })
            .collect::<Vec<_>>();
        let admitted = request(
            &fixture.app,
            "POST",
            "/v1/transactions",
            Some(json!({"transfers": transfers})),
            true,
        )
        .await;
        assert_eq!(admitted.status, StatusCode::ACCEPTED);
        assert_eq!(
            json_body(&admitted),
            json!({"transaction_ids": ["fixture-batch"]})
        );
    }

    assert_eq!(
        fixture
            .calls
            .batches
            .lock()
            .expect("batch calls")
            .as_slice(),
        &[1, wallets::MAX_TRANSFERS]
    );
    assert!(
        fixture
            .calls
            .transfers
            .lock()
            .expect("transfer calls")
            .is_empty()
    );
    assert_eq!(
        *fixture.calls.broadcasts.lock().expect("broadcast calls"),
        0
    );
}

#[tokio::test]
async fn transaction_bodies_reject_unknown_controls_before_sdk_delegation() {
    let fixture = fixture(true);
    let wallet_id = generated_wallet_id(&fixture).await;
    let single_path = format!("/v1/wallets/{wallet_id}/transactions");
    let cases = [
        (
            "single destination lag control",
            single_path.as_str(),
            json!({
                "destination": {
                    "encoding": "hex",
                    "text": "fixture-destination",
                    "max_lag": 4
                },
                "amount": "1"
            }),
        ),
        (
            "batch destination reference control",
            "/v1/transactions",
            json!({"transfers": [{
                "wallet_id": wallet_id,
                "destination": {
                    "encoding": "hex",
                    "text": "fixture-destination",
                    "reference_slot": 9
                },
                "amount": "1"
            }]}),
        ),
        (
            "single commitment control",
            single_path.as_str(),
            json!({
                "destination": {
                    "encoding": "hex",
                    "text": "fixture-destination"
                },
                "amount": "1",
                "commitment": "finalized"
            }),
        ),
        (
            "single retry control",
            single_path.as_str(),
            json!({
                "destination": {
                    "encoding": "hex",
                    "text": "fixture-destination"
                },
                "amount": "1",
                "retry": true
            }),
        ),
        (
            "batch item Memo override",
            "/v1/transactions",
            json!({"transfers": [{
                "wallet_id": wallet_id,
                "destination": {
                    "encoding": "hex",
                    "text": "fixture-destination"
                },
                "amount": "1",
                "memo": "caller-selected"
            }]}),
        ),
        (
            "batch priority control",
            "/v1/transactions",
            json!({
                "transfers": [{
                    "wallet_id": wallet_id,
                    "destination": {
                        "encoding": "hex",
                        "text": "fixture-destination"
                    },
                    "amount": "1"
                }],
                "priority_fee": "1"
            }),
        ),
    ];

    for (case, path, body) in cases {
        let response = request(&fixture.app, "POST", path, Some(body), true).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{case}");
        assert_eq!(
            json_body(&response),
            json!({"message": "request body must match the documented JSON schema"}),
            "{case}"
        );
        assert_no_transaction_calls(&fixture.calls);
    }
}

async fn generated_wallet_id(fixture: &Fixture) -> String {
    let response = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "btc"})),
        true,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);
    json_body(&response)["id"]
        .as_str()
        .expect("generated wallet ID")
        .to_owned()
}

fn assert_transaction_query_rejected(response: &Response) {
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(response),
        json!({"message": "transaction query parameters are not supported"})
    );
}

fn assert_no_transaction_calls(calls: &Calls) {
    assert!(calls.transfers.lock().expect("transfer calls").is_empty());
    assert_eq!(*calls.broadcasts.lock().expect("broadcast calls"), 0);
    assert!(calls.batches.lock().expect("batch calls").is_empty());
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("bitcoin".to_owned()),
        network: "regtest".to_owned(),
    }
}

fn ethereum_scope() -> IndexScope {
    IndexScope {
        chain: ChainId("ethereum".to_owned()),
        network: "mainnet".to_owned(),
    }
}

fn block(height: u64) -> BlockRef {
    BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(vec![height as u8; 32]),
        parent: None,
        timestamp: Some(height),
    }
}

struct Response {
    status: StatusCode,
    body: Bytes,
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    authorized: bool,
) -> Response {
    let body = body.map(|value| value.to_string());
    raw_request(app, method, path, body.as_deref(), authorized, &[]).await
}

async fn raw_request(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<&str>,
    authorized: bool,
    headers: &[(&str, &str)],
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    if authorized {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(body.unwrap_or_default().to_owned()))
                .expect("fixture request"),
        )
        .await
        .expect("route response");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    Response { status, body }
}

fn json_body(response: &Response) -> Value {
    serde_json::from_slice(&response.body).expect("response JSON")
}
