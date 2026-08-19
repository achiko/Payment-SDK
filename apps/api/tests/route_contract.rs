use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use base::{
    Address, Addresser, BlockHash, BlockHeight, BlockRef, Broadcaster, Decimal, SignRequest,
    SignedTransaction, Signer, Submission, TransactionBuilder, TransactionEnvelope,
    TransactionError, TransactionId, TransactionSnapshot,
};
use http_body_util::BodyExt;
use indexing::{
    AssetId, BoxFuture, CanonicalAddress, ChainId, Checkpoint, IndexError, IndexScope, MovementId,
    MovementKind, TransactionRef,
};
use payment_api::{Chain, State, router};
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
    let calls = Arc::new(Calls::default());
    let checkpoint: Arc<dyn Checkpoint> = Arc::new(FixtureCheckpoint::Value);
    let mut wallets = Wallets::<String, Chain>::new(checkpoint);
    wallets
        .register(
            Chain::Bitcoin,
            scope(),
            FixtureProvider {
                calls: Arc::clone(&calls),
            },
            Arc::new(FixtureSender {
                calls: Arc::clone(&calls),
            }),
        )
        .expect("fixture family must register");
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
        Some(json!({"chain": "bitcoin"})),
        false,
    )
    .await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);

    let created = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"chain": "bitcoin"})),
        true,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let wallet: Value = serde_json::from_slice(&created.body).expect("wallet JSON");
    let id = wallet["id"].as_str().expect("wallet ID");
    assert_eq!(wallet["chain"], "bitcoin");
    assert_eq!(wallet["network"], "regtest");
    assert_eq!(wallet["address"], "fixture-address");

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

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("bitcoin".to_owned()),
        network: "regtest".to_owned(),
    }
}

fn block(height: u64) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(vec![height as u8; 32]),
        parent_hash: None,
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
    let mut request = Request::builder().method(method).uri(path);
    if authorized {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(
                    body.map_or_else(String::new, |value| value.to_string()),
                ))
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
