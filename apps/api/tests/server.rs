use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use base::{
    Address, Addresser, Broadcaster, BuilderCast, Decimal, SignedTransaction, Submission,
    TransactionBuilder, TransactionEnvelope, TransactionError, TransactionErrorKind,
    TransactionFuture, TransactionId, TransactionRestore, TransactionSnapshot,
};
use indexing::{
    BlockHash, BlockHeight, BlockRef, BoxFuture, ChainId, Checkpoint, ConfirmationPolicy,
    ConfirmationProof, EventCursor, EventId, EventPage, EventQuery, History as IndexHistory,
    HistoryQuery, IndexError, IndexScope, ObservationEvent, ObservationRevision,
    ObservedTransaction, Observer, TransactionPage, TransactionQuery, TransactionRef,
    TransactionStatus, UnwatchOutcome, UnwatchRequest, WatchId, WatchReceipt, WatchRequest,
    Watcher,
};
use payment_api::{Config, Payments, Service, StorageRepository, serve};
use reqwest::StatusCode;
use storage_rocksdb::RocksDb;
use wallets::{
    AddressEncoding, AddressFormat, AddressText, AmountFormat, Balance, BalanceReader,
    Error as WalletError, FutureResult, History, HistoryReader, HistoryRequest, TransactionFactory,
};

struct FixtureWallet {
    encoding: AddressEncoding,
    preparations: Arc<AtomicUsize>,
    broadcasts: Arc<AtomicUsize>,
    failures: AtomicUsize,
    envelopes: Mutex<Vec<Vec<u8>>>,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl Addresser for FixtureWallet {
    fn address(&self) -> Address {
        Address::from([1_u8; 20])
    }
}

impl base::Signer for FixtureWallet {
    fn sign<'a>(&'a self, _request: base::SignRequest) -> base::SignFuture<'a> {
        Box::pin(async { unreachable!("server fixture never signs directly") })
    }
}

impl AddressFormat for FixtureWallet {
    fn address_text(&self, address: &Address) -> Result<AddressText, WalletError> {
        let text = match self.encoding {
            AddressEncoding::Hex => format!("0x{}", hex::encode(address.as_bytes())),
            AddressEncoding::Bech32 => "bc1qfixture000000000000000000000000000000000".to_owned(),
            _ => unreachable!("fixture only supports hexadecimal and Bech32 addresses"),
        };
        Ok(AddressText::new(self.encoding, text))
    }

    fn parse_address(&self, address: &AddressText) -> Result<Address, WalletError> {
        if address.encoding != self.encoding {
            return Err(WalletError::new(
                wallets::ErrorKind::InvalidAddress,
                "fixture address uses the wrong encoding",
            ));
        }
        match self.encoding {
            AddressEncoding::Hex => address
                .text
                .strip_prefix("0x")
                .and_then(|text| hex::decode(text).ok())
                .filter(|bytes| bytes.len() == 20)
                .map(Address::new)
                .ok_or_else(|| {
                    WalletError::new(
                        wallets::ErrorKind::InvalidAddress,
                        "fixture address is not a 20-byte 0x-prefixed hexadecimal address",
                    )
                }),
            AddressEncoding::Bech32
                if address.text == "bc1qfixture000000000000000000000000000000000" =>
            {
                Ok(Address::from([2_u8; 20]))
            }
            AddressEncoding::Bech32 => Err(WalletError::new(
                wallets::ErrorKind::InvalidAddress,
                "fixture address is not canonical Bech32 text",
            )),
            _ => unreachable!("fixture only supports hexadecimal and Bech32 addresses"),
        }
    }
}

impl BalanceReader for FixtureWallet {
    fn balance<'a>(&'a self) -> FutureResult<'a, Balance> {
        Box::pin(async {
            Ok(Balance {
                amount: Decimal::from(10_u64),
                observed_at: None,
            })
        })
    }
}

impl AmountFormat for FixtureWallet {
    fn display_amount(&self, atomic: &Decimal) -> Result<Decimal, WalletError> {
        Ok(atomic.clone())
    }
}

impl HistoryReader for FixtureWallet {
    fn history<'a>(&'a self, _request: HistoryRequest) -> FutureResult<'a, History> {
        Box::pin(async {
            Ok(History {
                transactions: Vec::new(),
                next: None,
            })
        })
    }
}

impl TransactionFactory for FixtureWallet {
    fn transaction(&self) -> Box<dyn TransactionBuilder> {
        Box::new(FixtureBuilder {
            configured: false,
            preparations: self.preparations.clone(),
        })
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

impl wallets::CollectionFactory for FixtureWallet {}

impl wallets::Sweeper for FixtureWallet {}

impl TransactionRestore for FixtureWallet {
    fn restore(
        &self,
        _snapshot: &TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
        Ok(Box::new(FixtureBuilder {
            configured: true,
            preparations: self.preparations.clone(),
        }))
    }
}

struct FixtureBuilder {
    configured: bool,
    preparations: Arc<AtomicUsize>,
}

impl BuilderCast for FixtureBuilder {
    fn utxo(&mut self) -> Option<&mut dyn base::UtxoBuilder> {
        None
    }
}

impl TransactionBuilder for FixtureBuilder {
    fn transfer(&mut self, destination: Address, amount: Decimal) -> Result<(), TransactionError> {
        if destination.as_bytes().len() != 20 || amount != Decimal::from(2_u64) {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "unexpected fixture transfer",
            ));
        }
        self.configured = true;
        Ok(())
    }

    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
        if !self.configured {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "fixture transfer is not configured",
            ));
        }
        Ok(TransactionSnapshot::new(
            "fixture.transfer.v1",
            serde_json::json!({ "amount": "2" }),
        ))
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<SignedTransaction, TransactionError>> {
        let configured = self.configured;
        let preparations = self.preparations.clone();
        Box::pin(async move {
            if !configured {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidTransaction,
                    "fixture transfer is not configured",
                ));
            }
            preparations.fetch_add(1, Ordering::SeqCst);
            Ok(SignedTransaction::new(
                "fixture.signed.v1",
                TransactionId::new("fixture-transaction"),
                TransactionEnvelope::new([9_u8; 4]),
            ))
        })
    }
}

impl Broadcaster for FixtureWallet {
    fn broadcast<'a>(
        &'a self,
        prepared: &'a SignedTransaction,
    ) -> TransactionFuture<'a, Result<Submission, TransactionError>> {
        self.broadcasts.fetch_add(1, Ordering::SeqCst);
        self.order.lock().expect("order mutex").push("broadcast");
        self.envelopes
            .lock()
            .expect("envelope capture mutex")
            .push(prepared.envelope().as_bytes().to_vec());
        let fail = self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        Box::pin(async move {
            if fail {
                Err(TransactionError::new(
                    TransactionErrorKind::Unavailable,
                    "simulated process interruption after durable preparation",
                ))
            } else {
                Ok(Submission {
                    id: prepared.id().clone(),
                })
            }
        })
    }
}

struct FixtureIndexer {
    watches: AtomicUsize,
    birthdays: Mutex<Vec<BlockHeight>>,
    events: Mutex<Vec<ObservationEvent>>,
    queries: Mutex<Vec<Option<EventCursor>>>,
    order: Arc<Mutex<Vec<&'static str>>>,
    fail_events: AtomicBool,
}

impl FixtureIndexer {
    fn new(order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            watches: AtomicUsize::new(0),
            birthdays: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            queries: Mutex::new(Vec::new()),
            order,
            fail_events: AtomicBool::new(false),
        }
    }
}

impl Checkpoint for FixtureIndexer {
    fn checkpoint<'a>(
        &'a self,
        _scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.order.lock().expect("order mutex").push("checkpoint");
        Box::pin(async {
            Ok(Some(BlockRef {
                height: BlockHeight(42),
                hash: BlockHash(vec![0x42; 32]),
                parent_hash: Some(BlockHash(vec![0x41; 32])),
                timestamp: Some(1_000),
            }))
        })
    }
}

impl Watcher for FixtureIndexer {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        self.watches.fetch_add(1, Ordering::SeqCst);
        self.birthdays
            .lock()
            .expect("birthday mutex")
            .push(request.start_height);
        self.order.lock().expect("order mutex").push("watch");
        Box::pin(async move {
            Ok(WatchReceipt {
                id: WatchId(format!("watch:{}", request.idempotency_key)),
                scope: request.scope,
                selector: request.selector,
                start_height: request.start_height,
                registered_at: None,
                inactive_from: None,
                confirmation_policy: ConfirmationPolicy {
                    minimum_confirmations: 3,
                    require_chain_finality: false,
                },
            })
        })
    }

    fn unwatch<'a>(
        &'a self,
        _command: UnwatchRequest,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Box::pin(async { Ok(UnwatchOutcome::Deactivated) })
    }
}

impl IndexHistory for FixtureIndexer {
    fn transaction<'a>(
        &'a self,
        _request: TransactionQuery,
    ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        Box::pin(async { Ok(None) })
    }

    fn history<'a>(
        &'a self,
        _request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        Box::pin(async {
            Ok(TransactionPage {
                transactions: Vec::new(),
                next: None,
            })
        })
    }
}

impl Observer for FixtureIndexer {
    fn events<'a>(&'a self, request: EventQuery) -> BoxFuture<'a, Result<EventPage, IndexError>> {
        if self.fail_events.load(Ordering::SeqCst) {
            return Box::pin(async {
                Err(IndexError::new(
                    indexing::IndexErrorKind::CannotConnect,
                    "fixture indexer is unavailable",
                    true,
                ))
            });
        }
        self.queries
            .lock()
            .expect("query mutex")
            .push(request.after);
        let events = self
            .events
            .lock()
            .expect("event mutex")
            .iter()
            .filter(|event| request.after.is_none_or(|after| event.cursor > after))
            .take(request.limit)
            .cloned()
            .collect::<Vec<_>>();
        let next = events.last().map(|event| event.cursor).or(request.after);
        Box::pin(async move { Ok(EventPage { events, next }) })
    }
}

#[tokio::test]
async fn service_readiness_tracks_reconciliation_and_shutdown_is_graceful() {
    let directory = tempfile::tempdir().expect("temporary RocksDB directory");
    let storage = Arc::new(RocksDb::open(directory.path()).expect("RocksDB must open"));
    let store = Arc::new(StorageRepository::new(storage));
    let order = Arc::new(Mutex::new(Vec::new()));
    let indexer = Arc::new(FixtureIndexer::new(order.clone()));
    indexer.fail_events.store(true, Ordering::SeqCst);
    let wallet = Arc::new(FixtureWallet {
        encoding: AddressEncoding::Hex,
        preparations: Arc::new(AtomicUsize::new(0)),
        broadcasts: Arc::new(AtomicUsize::new(0)),
        failures: AtomicUsize::new(0),
        envelopes: Mutex::new(Vec::new()),
        order,
    });
    let payments = Arc::new(Payments::new(store, indexer.clone()).with("primary", scope(), wallet));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let address = listener.local_addr().expect("listener address");
    let mut config = Config::new(address, vec![scope()]);
    config.reconcile_interval = Duration::from_millis(10);
    config.reconcile_limit = 10;
    let token = http_kit::server::BearerToken::new("gateway-secret")
        .expect("test bearer token must be valid");
    let server = http_kit::server::Config::new(
        address,
        http_kit::server::TransportSecurity::PlaintextLoopback,
        Some(token),
        http_kit::server::RequestLimits::new(128).expect("test body limit must be valid"),
    );
    let service = Service::new(config, payments, server).expect("valid service");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(service.run_on(listener, async move {
        stopped
            .await
            .expect("test stop sender must remain available");
    }));
    let client = reqwest::Client::new();
    let ready_url = format!("http://{address}/health/ready");

    let payment_url = format!("http://{address}/v1/payments/missing");
    wait_for_status(&client, &ready_url, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_eq!(
        client
            .get(&payment_url)
            .send()
            .await
            .expect("gateway request")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&payment_url)
            .bearer_auth("wrong-secret")
            .send()
            .await
            .expect("gateway request")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&payment_url)
            .bearer_auth("gateway-secret")
            .send()
            .await
            .expect("gateway request")
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .post(format!("http://{address}/v1/payments"))
            .bearer_auth("gateway-secret")
            .header("content-type", "application/json")
            .body("x".repeat(129))
            .send()
            .await
            .expect("oversized gateway request")
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    indexer.fail_events.store(false, Ordering::SeqCst);
    wait_for_status(&client, &ready_url, StatusCode::NO_CONTENT).await;
    indexer.fail_events.store(true, Ordering::SeqCst);
    wait_for_status(&client, &ready_url, StatusCode::SERVICE_UNAVAILABLE).await;

    stop.send(()).expect("service stop receiver");
    task.await
        .expect("service task must join")
        .expect("service must stop cleanly");
}

async fn wait_for_status(client: &reqwest::Client, url: &str, expected: StatusCode) {
    for _ in 0..100 {
        if let Ok(response) = client.get(url).send().await
            && response.status() == expected
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("endpoint {url} did not report {expected}");
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("fixture".to_owned()),
        network: "local".to_owned(),
    }
}

fn event(cursor: u64, revision: u64, status: TransactionStatus) -> ObservationEvent {
    ObservationEvent {
        id: EventId(format!("event-{cursor}")),
        cursor: EventCursor(cursor),
        watch_ids: vec![WatchId("payment-watch".to_owned())],
        previous_status: None,
        transaction: ObservedTransaction {
            scope: scope(),
            transaction_id: TransactionRef {
                scope: scope(),
                value: "fixture-transaction".to_owned(),
            },
            revision: ObservationRevision(revision),
            status,
            movements: Vec::new(),
            fee: None,
            first_seen_at: 1,
            observed_at: revision,
        },
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

fn request(amount: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "payment-1",
        "wallet": "primary",
        "destination": {
            "encoding": "hex",
            "text": format!("0x{}", hex::encode([2_u8; 20]))
        },
        "amount": amount,
        "confirmations": 3
    })
}

async fn start(
    store: Arc<StorageRepository>,
    wallet: Arc<FixtureWallet>,
    indexer: Arc<FixtureIndexer>,
) -> (String, tokio::task::JoinHandle<std::io::Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let address = listener.local_addr().expect("listener address");
    let payments = Arc::new(Payments::new(store, indexer).with("primary", scope(), wallet));
    let task = tokio::spawn(serve(listener, payments));
    (format!("http://{address}"), task)
}

#[tokio::test]
async fn persists_submitted_state_and_recovers_after_service_restart() {
    let directory = tempfile::tempdir().expect("temporary RocksDB directory");
    let storage = Arc::new(RocksDb::open(directory.path()).expect("RocksDB must open"));
    let store = Arc::new(StorageRepository::new(storage));
    let broadcasts = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));
    let indexer = Arc::new(FixtureIndexer::new(order.clone()));
    let wallet = Arc::new(FixtureWallet {
        encoding: AddressEncoding::Hex,
        preparations: Arc::new(AtomicUsize::new(0)),
        broadcasts: broadcasts.clone(),
        failures: AtomicUsize::new(0),
        envelopes: Mutex::new(Vec::new()),
        order,
    });

    let client = reqwest::Client::new();
    let (first_url, first_server) = start(store.clone(), wallet.clone(), indexer.clone()).await;
    let first = client
        .post(format!("{first_url}/v1/payments"))
        .json(&request("2"))
        .send()
        .await
        .expect("first response");
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: serde_json::Value = first.json().await.expect("payment JSON");
    assert_eq!(first_body["request"]["destination"]["encoding"], "hex");
    assert_eq!(
        first_body["request"]["destination"]["text"],
        format!("0x{}", hex::encode([2_u8; 20]))
    );
    assert_eq!(broadcasts.load(Ordering::SeqCst), 1);
    assert_eq!(
        wallet.order.lock().expect("order mutex").as_slice(),
        &["checkpoint", "watch", "broadcast"]
    );
    assert_eq!(
        indexer.birthdays.lock().expect("birthday mutex").as_slice(),
        &[BlockHeight(42)]
    );
    first_server.abort();

    let (second_url, second_server) = start(store.clone(), wallet.clone(), indexer.clone()).await;
    let second = client
        .post(format!("{second_url}/v1/payments"))
        .json(&request("2"))
        .send()
        .await
        .expect("retry response");
    assert_eq!(second.status(), StatusCode::OK);
    let body: serde_json::Value = second.json().await.expect("payment JSON");
    assert!(body["stage"]["Submitted"].is_object());
    assert_eq!(broadcasts.load(Ordering::SeqCst), 1);

    let replay = client
        .post(format!("{second_url}/v1/payments"))
        .json(&request("2"))
        .send()
        .await
        .expect("idempotent response");
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(broadcasts.load(Ordering::SeqCst), 1);

    let conflict = client
        .post(format!("{second_url}/v1/payments"))
        .json(&request("3"))
        .send()
        .await
        .expect("conflict response");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    second_server.abort();
}

#[tokio::test]
async fn parses_canonical_bitcoin_text_with_the_selected_wallet() {
    let directory = tempfile::tempdir().expect("temporary RocksDB directory");
    let storage = Arc::new(RocksDb::open(directory.path()).expect("RocksDB must open"));
    let store = Arc::new(StorageRepository::new(storage));
    let order = Arc::new(Mutex::new(Vec::new()));
    let indexer = Arc::new(FixtureIndexer::new(order.clone()));
    let wallet = Arc::new(FixtureWallet {
        encoding: AddressEncoding::Bech32,
        preparations: Arc::new(AtomicUsize::new(0)),
        broadcasts: Arc::new(AtomicUsize::new(0)),
        failures: AtomicUsize::new(0),
        envelopes: Mutex::new(Vec::new()),
        order,
    });
    let (url, server) = start(store, wallet, indexer).await;

    let response = reqwest::Client::new()
        .post(format!("{url}/v1/payments"))
        .json(&serde_json::json!({
            "id": "bitcoin-payment",
            "wallet": "primary",
            "destination": {
                "encoding": "bech32",
                "text": "bc1qfixture000000000000000000000000000000000"
            },
            "amount": "2",
            "confirmations": 3
        }))
        .send()
        .await
        .expect("Bitcoin payment response");

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("payment JSON");
    assert_eq!(body["request"]["destination"]["encoding"], "bech32");
    assert_eq!(
        body["request"]["destination"]["text"],
        "bc1qfixture000000000000000000000000000000000"
    );
    server.abort();
}

#[tokio::test]
async fn crash_after_prepared_recovers_the_exact_envelope_without_resigning() {
    let directory = tempfile::tempdir().expect("temporary RocksDB directory");
    let storage = Arc::new(RocksDb::open(directory.path()).expect("RocksDB must open"));
    let store = Arc::new(StorageRepository::new(storage));
    let order = Arc::new(Mutex::new(Vec::new()));
    let indexer = Arc::new(FixtureIndexer::new(order.clone()));
    let wallet = Arc::new(FixtureWallet {
        encoding: AddressEncoding::Hex,
        preparations: Arc::new(AtomicUsize::new(0)),
        broadcasts: Arc::new(AtomicUsize::new(0)),
        failures: AtomicUsize::new(1),
        envelopes: Mutex::new(Vec::new()),
        order,
    });
    let payments =
        Payments::new(store.clone(), indexer.clone()).with("primary", scope(), wallet.clone());
    let request = payment_api::Request {
        id: "prepared-recovery".to_owned(),
        wallet: "primary".to_owned(),
        destination: AddressText::new(
            AddressEncoding::Hex,
            format!("0x{}", hex::encode([2_u8; 20])),
        ),
        amount: "2".to_owned(),
        confirmations: 3,
        require_finality: false,
    };

    let first = payments.pay(request.clone()).await;
    assert!(first.is_err(), "first external effect must be interrupted");
    let durable = payments
        .get(&request.id)
        .await
        .expect("durable payment must load")
        .expect("prepared payment must exist");
    let durable_envelope = match durable.stage {
        payment_api::Stage::Watched {
            prepared, watch, ..
        } => {
            assert!(watch.id.contains("prepared-recovery"));
            assert!(watch.id.contains("fixture-transaction"));
            prepared.envelope().as_bytes().to_vec()
        }
        stage => panic!("expected durable watched stage, got {stage:?}"),
    };

    drop(payments);
    let recovered_payments =
        Payments::new(store, indexer.clone()).with("primary", scope(), wallet.clone());
    let recovered = recovered_payments
        .pay(request)
        .await
        .expect("restarted service must submit the durable envelope");
    assert!(matches!(
        recovered.stage,
        payment_api::Stage::Submitted { .. }
    ));
    assert_eq!(wallet.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(indexer.watches.load(Ordering::SeqCst), 1);
    let envelopes = wallet.envelopes.lock().expect("envelope capture mutex");
    assert_eq!(
        envelopes.as_slice(),
        &[durable_envelope.clone(), durable_envelope]
    );
}

#[tokio::test]
async fn observer_confirms_reorgs_and_resumes_from_the_durable_cursor() {
    let directory = tempfile::tempdir().expect("temporary RocksDB directory");
    let storage = Arc::new(RocksDb::open(directory.path()).expect("RocksDB must open"));
    let store = Arc::new(StorageRepository::new(storage));
    let order = Arc::new(Mutex::new(Vec::new()));
    let indexer = Arc::new(FixtureIndexer::new(order.clone()));
    let wallet = Arc::new(FixtureWallet {
        encoding: AddressEncoding::Hex,
        preparations: Arc::new(AtomicUsize::new(0)),
        broadcasts: Arc::new(AtomicUsize::new(0)),
        failures: AtomicUsize::new(0),
        envelopes: Mutex::new(Vec::new()),
        order,
    });
    let request = payment_api::Request {
        id: "observed-payment".to_owned(),
        wallet: "primary".to_owned(),
        destination: AddressText::new(
            AddressEncoding::Hex,
            format!("0x{}", hex::encode([2_u8; 20])),
        ),
        amount: "2".to_owned(),
        confirmations: 3,
        require_finality: false,
    };
    let payments =
        Payments::new(store.clone(), indexer.clone()).with("primary", scope(), wallet.clone());
    let submitted = payments
        .pay(request.clone())
        .await
        .expect("payment submits");
    assert!(matches!(
        submitted.stage,
        payment_api::Stage::Submitted { .. }
    ));

    indexer.events.lock().expect("event mutex").push(event(
        1,
        1,
        TransactionStatus::Confirmed {
            block: block(10),
            proof: ConfirmationProof::Depth {
                required: 3,
                observed: 3,
            },
        },
    ));
    assert_eq!(payments.reconcile(scope(), 10).await.expect("reconcile"), 1);
    let confirmed = payments
        .get(&request.id)
        .await
        .expect("load")
        .expect("payment");
    assert!(matches!(
        confirmed.stage,
        payment_api::Stage::Confirmed { .. }
    ));
    assert_eq!(confirmed.evidence.len(), 1);

    indexer.events.lock().expect("event mutex").push(event(
        2,
        2,
        TransactionStatus::Reorged {
            previous_block: block(10),
        },
    ));
    assert_eq!(payments.reconcile(scope(), 10).await.expect("reconcile"), 1);
    let reorged = payments
        .get(&request.id)
        .await
        .expect("load")
        .expect("payment");
    assert!(matches!(
        reorged.stage,
        payment_api::Stage::Submitted { .. }
    ));
    assert_eq!(reorged.evidence.len(), 2);
    drop(payments);

    let restarted = Payments::new(store, indexer.clone()).with("primary", scope(), wallet);
    assert_eq!(
        restarted
            .reconcile(scope(), 10)
            .await
            .expect("restart reconcile"),
        0
    );
    let after_restart = restarted
        .get(&request.id)
        .await
        .expect("load")
        .expect("payment");
    assert_eq!(after_restart.evidence.len(), 2);
    assert_eq!(
        indexer.queries.lock().expect("query mutex").last(),
        Some(&Some(EventCursor(2)))
    );
}
