//! Runtime proof for the complete native-UTXO deposit path.
//!
//! The fixture deliberately reuses the collection wallet from `collections.rs`:
//! protocol/RPC effects are deterministic while persistence and HTTP are real.

include!("collections.rs");

use std::time::Duration;

use deposits::{
    AddressRequest, DepositAddressSource, DepositError, LedgerReader, ProvisionedAddress,
};
use indexing::{
    BlockHash, BlockRef, Checkpoint, EventCursor, EventId, EventPage, EventQuery,
    History as IndexHistory, HistoryQuery, MovementId, ObservationEvent, ObservationRevision,
    ObservedTransaction, Observer, TransactionPage, TransactionQuery, TransactionStatus,
    ValueMovement,
};
use payment_api::{Config, DepositObserver, Deposits, Payments, Service, StorageRepository};

struct RuntimeIndex {
    watches: Mutex<Vec<WatchRequest>>,
    events: Mutex<Vec<ObservationEvent>>,
}

impl Checkpoint for RuntimeIndex {
    fn checkpoint<'a>(
        &'a self,
        requested: &'a IndexScope,
    ) -> indexing::BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            assert_eq!(requested, &scope());
            Ok(Some(BlockRef {
                height: BlockHeight(10),
                hash: BlockHash(vec![10; 32]),
                parent_hash: Some(BlockHash(vec![9; 32])),
                timestamp: Some(1_000),
            }))
        })
    }
}

impl Watcher for RuntimeIndex {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> indexing::BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            self.watches
                .lock()
                .expect("watch mutex")
                .push(request.clone());
            Ok(WatchReceipt {
                id: WatchId(format!(
                    "watch-{}",
                    self.watches.lock().expect("watch mutex").len()
                )),
                scope: request.scope,
                selector: request.selector,
                start_height: request.start_height,
                registered_at: None,
                inactive_from: None,
                confirmation_policy: ConfirmationPolicy {
                    minimum_confirmations: 2,
                    require_chain_finality: false,
                },
            })
        })
    }

    fn unwatch<'a>(
        &'a self,
        _: UnwatchRequest,
    ) -> indexing::BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Box::pin(async { Ok(UnwatchOutcome::Deactivated) })
    }
}

impl IndexHistory for RuntimeIndex {
    fn transaction<'a>(
        &'a self,
        _: TransactionQuery,
    ) -> indexing::BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        Box::pin(async { Ok(None) })
    }

    fn history<'a>(
        &'a self,
        _: HistoryQuery,
    ) -> indexing::BoxFuture<'a, Result<TransactionPage, IndexError>> {
        Box::pin(async {
            Ok(TransactionPage {
                transactions: Vec::new(),
                next: None,
            })
        })
    }
}

impl Observer for RuntimeIndex {
    fn events<'a>(
        &'a self,
        query: EventQuery,
    ) -> indexing::BoxFuture<'a, Result<EventPage, IndexError>> {
        Box::pin(async move {
            let events = self
                .events
                .lock()
                .expect("event mutex")
                .iter()
                .filter(|event| event.transaction.scope == query.scope)
                .filter(|event| query.after.is_none_or(|cursor| event.cursor > cursor))
                .take(query.limit)
                .cloned()
                .collect::<Vec<_>>();
            let next = events.last().map(|event| event.cursor).or(query.after);
            Ok(EventPage { events, next })
        })
    }
}

struct Addresses(AtomicUsize);

impl DepositAddressSource for Addresses {
    fn address<'a>(
        &'a self,
        request: AddressRequest,
    ) -> deposits::BoxFuture<'a, Result<ProvisionedAddress, DepositError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            let position = request.candidate;
            Ok(ProvisionedAddress {
                address: CanonicalAddress {
                    scope: request.scope,
                    value: format!("source-{position}"),
                },
                key: KeyId::Identifier(format!("key-{position}")),
                key_purpose: format!("key-{position}"),
            })
        })
    }
}

fn credit_event() -> ObservationEvent {
    ObservationEvent {
        id: EventId("funding-event".to_owned()),
        cursor: EventCursor(1),
        watch_ids: vec![WatchId("watch-1".to_owned()), WatchId("watch-2".to_owned())],
        previous_status: None,
        transaction: ObservedTransaction {
            scope: scope(),
            transaction_id: TransactionRef {
                scope: scope(),
                value: "funding".to_owned(),
            },
            revision: ObservationRevision(1),
            status: TransactionStatus::Confirmed {
                block: BlockRef {
                    height: BlockHeight(11),
                    hash: BlockHash(vec![11; 32]),
                    parent_hash: Some(BlockHash(vec![10; 32])),
                    timestamp: Some(1_100),
                },
                proof: indexing::ConfirmationProof::Depth {
                    required: 2,
                    observed: 2,
                },
            },
            movements: vec![
                ValueMovement::Transfer {
                    id: MovementId("output-0".to_owned()),
                    asset: asset(),
                    amount: Decimal::from(100_u64),
                    from: CanonicalAddress {
                        scope: scope(),
                        value: "sender".to_owned(),
                    },
                    to: CanonicalAddress {
                        scope: scope(),
                        value: "source-0".to_owned(),
                    },
                },
                ValueMovement::Transfer {
                    id: MovementId("output-1".to_owned()),
                    asset: asset(),
                    amount: Decimal::from(300_u64),
                    from: CanonicalAddress {
                        scope: scope(),
                        value: "sender".to_owned(),
                    },
                    to: CanonicalAddress {
                        scope: scope(),
                        value: "source-1".to_owned(),
                    },
                },
            ],
            fee: None,
            first_seen_at: 1_050,
            observed_at: 1_100,
        },
    }
}

#[tokio::test]
async fn protected_runtime_opens_observes_and_sweeps_utxo_deposits()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let engine = RocksDb::open(directory.path())?;
    let payment_store = Arc::new(PaymentStore::new(engine.clone()));
    let payments_store = Arc::new(StorageRepository::new(Arc::new(engine)));
    let indexer = Arc::new(RuntimeIndex {
        watches: Mutex::new(Vec::new()),
        events: Mutex::new(Vec::new()),
    });
    let prepared = base::SignedTransaction::new(
        "fixture.signed.v1",
        base::TransactionId::new("sweep-transaction"),
        base::TransactionEnvelope::new([1, 2, 3, 4]),
    );
    let prepares = Arc::new(AtomicUsize::new(0));
    let broadcasts = Arc::new(Mutex::new(Vec::new()));
    let wallet_source = Arc::new(Wallets {
        prepared: prepared.clone(),
        prepares: prepares.clone(),
        broadcasts: broadcasts.clone(),
        fail_first: Arc::new(AtomicUsize::new(1)),
        transfers: Arc::new(Mutex::new(Vec::new())),
        decimals: 0,
        sweep_amount: Decimal::zero(),
    });
    let payment_wallet = Arc::new(FixtureWallet {
        address: "treasury".to_owned(),
        prepared,
        prepares: Arc::new(AtomicUsize::new(0)),
        broadcasts: Arc::new(Mutex::new(Vec::new())),
        fail_first: Arc::new(AtomicUsize::new(1)),
        transfers: Arc::new(Mutex::new(Vec::new())),
        decimals: 0,
        sweep_amount: Decimal::zero(),
    });
    let payments = Arc::new(Payments::new(payments_store, indexer.clone()).with(
        "primary",
        scope(),
        payment_wallet,
    ));
    let deposits = Arc::new(Deposits::new(
        payment_store.clone(),
        indexer.clone(),
        Arc::new(Addresses(AtomicUsize::new(0))),
        scope(),
    ));
    let observer = Arc::new(DepositObserver::new(
        scope(),
        indexer.clone(),
        payment_store.clone(),
    ));
    let sweeps = Arc::new(Sweeps::new(
        payment_store.clone(),
        wallet_source,
        indexer.clone(),
        scope(),
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let mut config = Config::new(address, vec![scope()]);
    config.reconcile_interval = Duration::from_millis(10);
    config.reconcile_limit = 100;
    let token = http_kit::server::BearerToken::new("deposit-secret")?;
    let server = http_kit::server::Config::new(
        address,
        http_kit::server::TransportSecurity::PlaintextLoopback,
        Some(token),
        http_kit::server::RequestLimits::default(),
    );
    let service = Service::new(config, payments, server)?
        .with_observer(observer)
        .with_deposits(deposits)
        .with_sweeps(sweeps, Arc::new(FixedClock(2_000)));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(service.run_on(listener, async move {
        let _ = stopped.await;
    }));
    let client = reqwest::Client::new();
    let root = format!("http://{address}");
    wait_ready(&client, &root, "deposit-secret").await;

    assert_eq!(
        client
            .get(format!("{root}/v1/deposits"))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    for (position, amount) in [100_u64, 300].into_iter().enumerate() {
        let response = client
            .post(format!("{root}/v1/deposits"))
            .bearer_auth("deposit-secret")
            .header("idempotency-key", format!("open-{position}"))
            .json(&serde_json::json!({
                "id": format!("deposit-{position}"),
                "user_id": format!("user-{position}"),
                "asset": { "chain": "fixture", "asset": "native" },
                "expected": amount.to_string(),
                "expires_at": 9_999,
                "created_at": 1_000
            }))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    indexer
        .events
        .lock()
        .expect("event mutex")
        .push(credit_event());
    wait_balance(&client, &root, "deposit-secret", "deposit-0", "100").await;
    let history: serde_json::Value = client
        .get(format!("{root}/v1/deposits/deposit-0/history"))
        .bearer_auth("deposit-secret")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(history["entries"].as_array().map(Vec::len), Some(2));

    let owner = CommandPrincipal("exchange".to_owned());
    let mut participants = Vec::new();
    for (position, amount) in [100_u64, 300].into_iter().enumerate() {
        let user_id = UserId(format!("user-{position}"));
        payment_store
            .ensure_user(User {
                id: user_id.clone(),
                owner: owner.clone(),
                first_seen_at: 1,
            })
            .await?;
        let deposit_id = DepositId(format!("deposit-{position}"));
        let head = payment_store
            .current(&deposit_id)
            .await?
            .expect("observed ledger head");
        participants.push(BatchParticipant {
            user_id,
            deposit_id,
            expected_ledger_head: head.id,
            reservation_amount: Decimal::from(amount),
            spend_resources: vec![resource("funding", position as u32, amount)],
        });
    }
    let collection_id = CollectionId("runtime-sweep".to_owned());
    let job_id = JobId("runtime-job".to_owned());
    payment_store
        .create_or_replay(JobPlan {
            id: job_id.clone(),
            command: CommandIdentity {
                principal: owner.clone(),
                operation: CommandOperation::CollectionPlan,
                client_key: IdempotencyKey("runtime-collect".to_owned()),
                request_hash: RequestHash([7; 32]),
            },
            payload: JobPayload::CreateBatch(BatchJob {
                collection_id: collection_id.clone(),
                deposit_ids: participants
                    .iter()
                    .map(|value| value.deposit_id.clone())
                    .collect(),
            }),
            user_owner: owner,
            policy: PolicyIdentity {
                version: "v1".to_owned(),
                digest: [9; 32],
            },
            created_at: 1_200,
        })
        .await?;
    payment_store
        .create_or_replay_utxo_batch(CreateBatch {
            id: collection_id.clone(),
            job_id,
            asset: asset(),
            destination: CanonicalAddress {
                scope: scope(),
                value: "master".to_owned(),
            },
            policy: PolicyIdentity {
                version: "v1".to_owned(),
                digest: [9; 32],
            },
            participants,
            leg: CreateLeg {
                id: deposits::LegId("sweep".to_owned()),
                kind: CollectionLegKind::Sweep,
                planned_amount: None,
            },
            created_at: 1_200,
        })
        .await?;

    let execute = format!("{root}/v1/collections/{}/execute", collection_id.0);
    let swept: serde_json::Value = client
        .post(execute)
        .bearer_auth("deposit-secret")
        .header("idempotency-key", &collection_id.0)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(swept["mode"], "utxo_batch");
    assert_eq!(swept["legs"][0]["state"], "broadcast");
    assert_eq!(prepares.load(Ordering::SeqCst), 1);
    assert_eq!(
        broadcasts.lock().expect("broadcast mutex").as_slice(),
        &[vec![1, 2, 3, 4]]
    );

    stop.send(()).expect("service stop receiver");
    task.await??;
    Ok(())
}

async fn wait_ready(client: &reqwest::Client, root: &str, token: &str) {
    for _ in 0..200 {
        if client
            .get(format!("{root}/health/ready"))
            .bearer_auth(token)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("payment runtime did not become ready");
}

async fn wait_balance(client: &reqwest::Client, root: &str, token: &str, id: &str, expected: &str) {
    for _ in 0..200 {
        if let Ok(response) = client
            .get(format!("{root}/v1/deposits/{id}/balance"))
            .bearer_auth(token)
            .send()
            .await
            && let Ok(body) = response.json::<serde_json::Value>().await
            && body["entry"]["balances"]["confirmed"] == expected
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("deposit balance was not observed");
}
