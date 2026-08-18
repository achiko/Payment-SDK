use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use base::Decimal;
use deposits::{
    AddressRequest, BoxFuture, DepositAddressSource, DepositState, IdempotencyKey, KeyId,
    PaymentStore, ProvisionedAddress, UserId,
};
use indexing::{
    AssetId, BlockHash, BlockHeight, BlockRef, CanonicalAddress, ChainId, Checkpoint,
    ConfirmationPolicy, IndexError, IndexErrorKind, IndexScope, UnwatchOutcome, UnwatchRequest,
    WatchId, WatchReceipt, WatchRequest, Watcher,
};
use payment_api::{DepositId, DepositQuery, DepositRegistration, Deposits, deposit_routes};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;

struct Addresses {
    calls: AtomicUsize,
}

impl DepositAddressSource for Addresses {
    fn address<'a>(
        &'a self,
        request: AddressRequest,
    ) -> BoxFuture<'a, Result<ProvisionedAddress, deposits::DepositError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ProvisionedAddress {
                address: CanonicalAddress {
                    scope: request.scope,
                    value: "deposit-address".to_owned(),
                },
                key: KeyId::Identifier("key-1".to_owned()),
                key_purpose: "deposit-v1".to_owned(),
            })
        })
    }
}

struct Indexer {
    scope: IndexScope,
    attempts: AtomicUsize,
    requests: Mutex<Vec<WatchRequest>>,
}

impl Checkpoint for Indexer {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            assert_eq!(scope, &self.scope);
            Ok(Some(BlockRef {
                height: BlockHeight(42),
                hash: BlockHash(vec![42; 32]),
                parent_hash: None,
                timestamp: Some(1_000),
            }))
        })
    }
}

impl Watcher for Indexer {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("watch request mutex must not be poisoned")
                .push(request.clone());
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(IndexError::new(
                    IndexErrorKind::Source,
                    "simulated lost response",
                    true,
                ));
            }
            Ok(WatchReceipt {
                id: WatchId("watch-1".to_owned()),
                scope: request.scope,
                selector: request.selector,
                start_height: request.start_height,
                registered_at: None,
                inactive_from: None,
                confirmation_policy: policy(),
            })
        })
    }

    fn unwatch<'a>(
        &'a self,
        _: UnwatchRequest,
    ) -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Box::pin(async { unreachable!("deposit fixture never removes watches") })
    }
}

fn policy() -> ConfirmationPolicy {
    ConfirmationPolicy {
        minimum_confirmations: 2,
        require_chain_finality: false,
    }
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("fixture".to_owned()),
        network: "test".to_owned(),
    }
}

fn request(scope: IndexScope) -> DepositRegistration {
    DepositRegistration {
        scope,
        id: DepositId("deposit-1".to_owned()),
        idempotency_key: IdempotencyKey("command-1".to_owned()),
        user_id: UserId("user-1".to_owned()),
        asset: AssetId {
            chain: ChainId("fixture".to_owned()),
            asset: "native".to_owned(),
        },
        expected: "125".parse::<Decimal>().expect("valid atomic amount"),
        expires_at: 2_000,
        created_at: 1_000,
    }
}

#[tokio::test]
async fn api_facade_never_exposes_an_unwatched_address() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let configured_scope = scope();
    let store = Arc::new(PaymentStore::new(RocksDb::open(directory.path())?));
    let indexer = Arc::new(Indexer {
        scope: configured_scope.clone(),
        attempts: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let addresses = Arc::new(Addresses {
        calls: AtomicUsize::new(0),
    });
    let deposits = Deposits::new(
        store,
        indexer.clone(),
        addresses.clone(),
        configured_scope.clone(),
    );
    let command = request(configured_scope);

    deposits
        .open(command.clone())
        .await
        .expect_err("a lost IX acknowledgement must not return the address");
    let awaiting = deposits
        .get(&command.id)
        .await?
        .expect("the deposit must already be durable");
    assert_eq!(awaiting.state, DepositState::AwaitingWatch);
    assert_eq!(awaiting.birthday, BlockHeight(42));

    let active = deposits.open(command).await?;
    assert_eq!(
        active.state,
        DepositState::Active {
            watch_id: WatchId("watch-1".to_owned()),
        }
    );
    assert_eq!(addresses.calls.load(Ordering::SeqCst), 1);
    {
        let requests = indexer
            .requests
            .lock()
            .expect("watch request mutex must not be poisoned");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
    }

    let page = deposits
        .list(DepositQuery {
            after: None,
            limit: 10,
            user_id: Some(UserId("user-1".to_owned())),
            state: None,
        })
        .await?;
    assert_eq!(page.deposits, vec![active]);
    Ok(())
}

#[tokio::test]
async fn http_retries_the_durable_open_and_lists_the_active_deposit()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let configured_scope = scope();
    let store = Arc::new(PaymentStore::new(RocksDb::open(directory.path())?));
    let indexer = Arc::new(Indexer {
        scope: configured_scope.clone(),
        attempts: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let addresses = Arc::new(Addresses {
        calls: AtomicUsize::new(0),
    });
    let deposits = Arc::new(Deposits::new(
        store,
        indexer,
        addresses.clone(),
        configured_scope,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move { axum::serve(listener, deposit_routes(deposits)).await });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/v1/deposits");
    let body = serde_json::json!({
        "id": "deposit-http",
        "user_id": "user-http",
        "asset": { "chain": "fixture", "asset": "native" },
        "expected": "125",
        "expires_at": 2_000,
        "created_at": 1_000
    });

    let missing_key = client.post(&url).json(&body).send().await?;
    assert_eq!(missing_key.status(), reqwest::StatusCode::BAD_REQUEST);

    let mut caller_selected = body.clone();
    caller_selected["key_purpose"] = serde_json::json!("caller-controlled");
    let rejected = client
        .post(&url)
        .header("idempotency-key", "caller-controlled")
        .json(&caller_selected)
        .send()
        .await?;
    assert_eq!(rejected.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(addresses.calls.load(Ordering::SeqCst), 0);

    let interrupted = client
        .post(&url)
        .header("idempotency-key", "command-http")
        .json(&body)
        .send()
        .await?;
    assert_eq!(
        interrupted.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );

    let active = client
        .post(&url)
        .header("idempotency-key", "command-http")
        .json(&body)
        .send()
        .await?;
    assert_eq!(active.status(), reqwest::StatusCode::OK);
    let active: serde_json::Value = active.json().await?;
    assert_eq!(active["id"], "deposit-http");
    assert_eq!(active["state"]["kind"], "active");
    assert!(active.get("key").is_none());
    assert!(active.get("key_purpose").is_none());

    let listed: serde_json::Value = client
        .get(format!("{url}?user_id=user-http&state=active&limit=10"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(listed["deposits"].as_array().map(Vec::len), Some(1));

    let fetched: serde_json::Value = client
        .get(format!("{url}/deposit-http"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(fetched["address"]["value"], "deposit-address");
    assert_eq!(addresses.calls.load(Ordering::SeqCst), 1);

    task.abort();
    Ok(())
}
