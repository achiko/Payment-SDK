use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::keccak256;
use axum::{Json, Router, extract::State, routing::post};
use chain_ethereum::{AssetKind, HttpConfig, Limits, WalletConfig, WalletProvider, Wei};
use http::client::Retry;
use indexer_worker::{AuthenticationMode, EthereumConfig, EthereumService};
use indexing::{
    ChainId, History, IndexScope, Indexer, TransactionQuery, TransactionRef, TransactionStatus,
};
use indexing_http::{Config as IndexerConfig, Remote};
use payment_api::{EvidenceStatus, Payments, Stage, StorageRepository, serve};
use serde_json::{Value, json};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;
use tokio::{sync::oneshot, task::JoinHandle};
use wallets::{SecretBytes, Wallets};

const GENESIS_HASH: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CONFIRMATION_HASH: &str =
    "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const REORG_BLOCK_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const REORG_TIP_HASH: &str = "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const DESTINATION: [u8; 20] = [0x22; 20];
const VALUE: &str = "0xde0b6b3a7640000";

#[derive(Clone)]
struct NodeState(Arc<Mutex<Node>>);

struct Node {
    sender: Option<String>,
    transaction: Option<String>,
    broadcasts: usize,
    reorged: bool,
}

struct TestServer {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send(()).ok();
        }
        self.task.await.expect("test HTTP server must not panic");
    }
}

struct RunningIndexer {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), indexer_worker::ServiceError>>,
}

impl RunningIndexer {
    async fn stop(mut self) {
        self.shutdown
            .take()
            .expect("indexer shutdown sender must exist")
            .send(())
            .ok();
        self.task
            .await
            .expect("indexer task must not panic")
            .expect("indexer must shut down cleanly");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payment_is_confirmed_recovered_and_reorg_corrected() {
    let node_state = NodeState(Arc::new(Mutex::new(Node {
        sender: None,
        transaction: None,
        broadcasts: 0,
        reorged: false,
    })));
    let node = start_node(node_state.clone()).await;
    let files = TempDir::new().expect("temporary test directory must be created");
    let indexer_address = unused_address();
    let indexer_endpoint = format!("http://{indexer_address}");
    let scope = IndexScope {
        chain: ChainId("ethereum".to_owned()),
        network: "mainnet".to_owned(),
    };

    let first_indexer =
        start_indexer(files.path().join("indexer"), node.address, indexer_address).await;
    wait_until_ready(&format!("{indexer_endpoint}/health/ready")).await;

    let remote = Arc::new(
        Remote::connect(IndexerConfig::new(&indexer_endpoint))
            .expect("indexer HTTP client must construct"),
    );
    let (accounts, transactions) = wallet_rpc_config(node.address)
        .connect()
        .expect("wallet RPC clients must construct");
    let provider = WalletProvider::new(
        WalletConfig {
            scope: scope.clone(),
            asset: AssetKind::Native,
            decimals: 18,
        },
        Arc::new(accounts),
        Arc::new(transactions),
        remote.clone(),
    );
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum WalletKey {
        Ethereum,
    }
    let mut wallets = Wallets::new();
    wallets
        .register(WalletKey::Ethereum, provider)
        .expect("wallet key must be unique");
    let wallet = wallets
        .new_wallet(&WalletKey::Ethereum, SecretBytes::new([1_u8; 32]))
        .await
        .expect("concrete Ethereum wallet must be created");
    node_state
        .0
        .lock()
        .expect("node lock must be healthy")
        .sender = Some(format!("0x{}", hex::encode(wallet.address().as_bytes())));

    let payment_db = Arc::new(
        RocksDb::open(files.path().join("payments"))
            .expect("payment RocksDB must open for the test"),
    );
    let payment_store = Arc::new(StorageRepository::new(payment_db));
    let indexer: Arc<dyn Indexer> = remote.clone();
    let payments =
        Arc::new(Payments::new(payment_store, indexer).with("hot", scope.clone(), wallet));
    let payment_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("payment listener must bind");
    let payment_address = payment_listener
        .local_addr()
        .expect("payment listener address must exist");
    let payment_task = tokio::spawn(serve(payment_listener, payments.clone()));

    let response = reqwest::Client::new()
        .post(format!("http://{payment_address}/v1/payments"))
        .json(&json!({
            "id": "system-payment",
            "wallet": "hot",
            "destination": {
                "encoding": "hex",
                "text": format!("0x{}", hex::encode(DESTINATION))
            },
            "amount": "1",
            "confirmations": 1
        }))
        .send()
        .await
        .expect("payment request must reach the service");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let submitted: Value = response
        .json()
        .await
        .expect("payment response must be valid JSON");
    let transaction_id = submitted["stage"]["Submitted"]["transaction_id"]
        .as_str()
        .expect("payment must expose its submitted transaction ID")
        .to_owned();

    wait_for_transaction(&remote, &scope, &transaction_id).await;
    first_indexer.stop().await;

    let second_indexer =
        start_indexer(files.path().join("indexer"), node.address, indexer_address).await;
    wait_until_ready(&format!("{indexer_endpoint}/health/ready")).await;
    let applied = payments
        .reconcile(scope.clone(), 100)
        .await
        .expect("payment reconciliation must consume the persisted index event");
    assert!(applied > 0, "reconciliation must consume an index event");

    let confirmed = payments
        .get("system-payment")
        .await
        .expect("payment lookup must succeed")
        .expect("payment must remain durable");
    assert!(matches!(
        confirmed.stage,
        Stage::Confirmed {
            confirmations: 1,
            ..
        }
    ));
    assert_eq!(confirmed.evidence.len(), 2);
    assert!(matches!(
        confirmed.evidence[0].status,
        EvidenceStatus::Included { confirmations: 1 }
    ));
    assert!(matches!(
        confirmed.evidence[1].status,
        EvidenceStatus::Confirmed
    ));
    let public: Value = reqwest::get(format!(
        "http://{payment_address}/v1/payments/system-payment"
    ))
    .await
    .expect("confirmed payment must remain available over HTTP")
    .json()
    .await
    .expect("confirmed payment HTTP response must be valid JSON");
    assert_eq!(
        public["stage"]["Confirmed"]["transaction_id"],
        transaction_id
    );
    assert_eq!(public["stage"]["Confirmed"]["confirmations"], 1);
    assert_eq!(
        node_state
            .0
            .lock()
            .expect("node lock must be healthy")
            .broadcasts,
        1,
        "restart and reconciliation must not rebroadcast"
    );

    let history = remote
        .history(indexing::HistoryQuery {
            scope: scope.clone(),
            address: indexing::CanonicalAddress {
                scope: scope.clone(),
                value: format!("0x{}", hex::encode(DESTINATION)),
            },
            after: None,
            limit: 10,
        })
        .await
        .expect("indexed address history must be queryable");
    assert_eq!(history.transactions.len(), 1);
    assert_eq!(history.transactions[0].transaction_id.value, transaction_id);

    node_state
        .0
        .lock()
        .expect("node lock must be healthy")
        .reorged = true;
    wait_for_reorg(&remote, &scope, &transaction_id).await;
    let corrected = payments
        .reconcile(scope, 100)
        .await
        .expect("payment reconciliation must consume the reorg correction");
    assert!(corrected > 0, "reorg must append a correction event");
    let corrected = payments
        .get("system-payment")
        .await
        .expect("corrected payment lookup must succeed")
        .expect("corrected payment must remain durable");
    assert!(matches!(corrected.stage, Stage::Submitted { .. }));
    assert!(matches!(
        corrected
            .evidence
            .last()
            .expect("reorg evidence must be retained")
            .status,
        EvidenceStatus::Reorged
    ));

    payment_task.abort();
    let _ignored = payment_task.await;
    second_indexer.stop().await;
    node.stop().await;
}

fn wallet_rpc_config(address: SocketAddr) -> HttpConfig {
    let limits = Limits::new(
        1024,
        2_000,
        1_000_000,
        Wei::from_u128(1_000_000_000_000),
        Wei::from_u128(100_000_000_000),
        Wei::from_u128(1_000_000_000_000_000_000),
    )
    .expect("wallet RPC limits must be valid");
    HttpConfig::new(
        format!("http://{address}"),
        1,
        Duration::from_secs(5),
        1024 * 1024,
        Retry::default(),
        limits,
    )
    .expect("wallet RPC config must be valid")
}

async fn start_indexer(
    database: impl Into<std::path::PathBuf>,
    rpc: SocketAddr,
    api: SocketAddr,
) -> RunningIndexer {
    let mut config = EthereumConfig::new(
        database,
        "mainnet",
        0,
        1,
        GENESIS_HASH,
        format!("http://{rpc}"),
        AuthenticationMode::GlobalTrusted,
    );
    config.confirmation_depth = 1;
    config.reorg_retention = 10;
    config.http_bind = api;
    config.poll_seconds = 1;
    let service = EthereumService::new(config).expect("indexer config must validate");
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async move {
        let _ignored = receiver.await;
    }));
    RunningIndexer {
        shutdown: Some(shutdown),
        task,
    }
}

async fn wait_until_ready(url: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if reqwest::get(url)
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "service did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_transaction(
    remote: &Remote<http::client::Reqwest>,
    scope: &IndexScope,
    id: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut last = String::from("no response");
    loop {
        match remote
            .transaction(TransactionQuery {
                scope: scope.clone(),
                transaction_id: TransactionRef {
                    scope: scope.clone(),
                    value: id.to_owned(),
                },
            })
            .await
        {
            Ok(Some(transaction))
                if matches!(transaction.status, TransactionStatus::Confirmed { .. }) =>
            {
                return;
            }
            Ok(value) if tokio::time::Instant::now() < deadline => {
                last = format!("transaction response: {value:?}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) if tokio::time::Instant::now() < deadline => {
                last = format!("transaction error: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(value) => {
                panic!("submitted transaction did not become confirmed: {value:?}; {last}")
            }
            Err(error) => panic!("submitted transaction lookup failed: {error}"),
        }
    }
}

async fn wait_for_reorg(remote: &Remote<http::client::Reqwest>, scope: &IndexScope, id: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match remote
            .transaction(TransactionQuery {
                scope: scope.clone(),
                transaction_id: TransactionRef {
                    scope: scope.clone(),
                    value: id.to_owned(),
                },
            })
            .await
        {
            Ok(Some(transaction))
                if matches!(transaction.status, TransactionStatus::Reorged { .. }) =>
            {
                return;
            }
            Ok(_) | Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(value) => panic!("transaction did not receive a reorg correction: {value:?}"),
            Err(error) => panic!("reorged transaction lookup failed: {error}"),
        }
    }
}

async fn start_node(state: NodeState) -> TestServer {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("mock node listener must bind");
    let address = listener.local_addr().expect("mock node address must exist");
    let (shutdown, receiver) = oneshot::channel();
    let app = Router::new()
        .route("/", post(node_request))
        .with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ignored = receiver.await;
            })
            .await
            .expect("mock node must run");
    });
    TestServer {
        address,
        shutdown: Some(shutdown),
        task,
    }
}

async fn node_request(State(state): State<NodeState>, Json(request): Json<Value>) -> Json<Value> {
    if let Some(batch) = request.as_array() {
        return Json(Value::Array(
            batch
                .iter()
                .map(|request| node_response(&state, request))
                .collect(),
        ));
    }
    Json(node_response(&state, &request))
}

fn node_response(state: &NodeState, request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"]
        .as_str()
        .expect("JSON-RPC method must be a string");
    let params = &request["params"];
    let result = node_result(state, method, params);
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn node_result(state: &NodeState, method: &str, params: &Value) -> Value {
    match method {
        "eth_chainId" => json!("0x1"),
        "eth_getTransactionCount" => json!("0x0"),
        "eth_estimateGas" => json!("0x5208"),
        "eth_maxPriorityFeePerGas" => json!("0x1"),
        "eth_getBalance" => json!("0x8ac7230489e80000"),
        "eth_sendRawTransaction" => {
            let envelope = params[0]
                .as_str()
                .expect("raw transaction must be a string")
                .strip_prefix("0x")
                .expect("raw transaction must have a hexadecimal prefix");
            let bytes = hex::decode(envelope).expect("raw transaction must be hexadecimal");
            let hash = format!("0x{}", hex::encode(keccak256(bytes)));
            let mut node = state.0.lock().expect("node lock must be healthy");
            node.transaction = Some(hash.clone());
            node.broadcasts += 1;
            json!(hash)
        }
        "eth_getBlockByNumber" => {
            let tag = params[0].as_str().expect("block tag must be a string");
            let full = params[1].as_bool().expect("full flag must be a boolean");
            let node = state.0.lock().expect("node lock must be healthy");
            if tag == "0x0" || tag == "latest" && node.transaction.is_none() {
                genesis()
            } else if node.reorged && tag == "0x1" {
                reorg_block()
            } else if node.reorged && (tag == "0x2" || tag == "latest") {
                reorg_tip()
            } else if tag == "0x1" && node.transaction.is_some() {
                block(&node, full)
            } else if (tag == "0x2" || tag == "latest") && node.transaction.is_some() {
                confirmation_block()
            } else {
                Value::Null
            }
        }
        "eth_getBlockReceipts" => {
            let node = state.0.lock().expect("node lock must be healthy");
            if params[0] == BLOCK_HASH {
                json!([receipt(&node)])
            } else {
                json!([])
            }
        }
        "eth_getTransactionReceipt" => {
            let node = state.0.lock().expect("node lock must be healthy");
            node.transaction
                .as_ref()
                .map(|_| receipt(&node))
                .unwrap_or(Value::Null)
        }
        "eth_blockNumber" => {
            let node = state.0.lock().expect("node lock must be healthy");
            json!(if node.transaction.is_some() {
                "0x2"
            } else {
                "0x0"
            })
        }
        other => panic!("unexpected Ethereum JSON-RPC method {other}"),
    }
}

fn genesis() -> Value {
    block_value(
        0,
        GENESIS_HASH,
        &format!("0x{}", "00".repeat(32)),
        Vec::new(),
    )
}

fn block(node: &Node, full: bool) -> Value {
    let transaction = node
        .transaction
        .as_ref()
        .expect("broadcast transaction must exist");
    let transactions = if full {
        vec![json!({
            "hash": transaction,
            "from": node.sender.as_ref().expect("wallet sender must be configured"),
            "to": format!("0x{}", hex::encode(DESTINATION)),
            "value": VALUE,
            "transactionIndex": "0x0",
            "blockHash": BLOCK_HASH,
            "blockNumber": "0x1"
        })]
    } else {
        vec![json!(transaction)]
    };
    block_value(1, BLOCK_HASH, GENESIS_HASH, transactions)
}

fn confirmation_block() -> Value {
    block_value(2, CONFIRMATION_HASH, BLOCK_HASH, Vec::new())
}

fn reorg_block() -> Value {
    block_value(1, REORG_BLOCK_HASH, GENESIS_HASH, Vec::new())
}

fn reorg_tip() -> Value {
    block_value(2, REORG_TIP_HASH, REORG_BLOCK_HASH, Vec::new())
}

fn block_value(number: u64, hash: &str, parent: &str, transactions: Vec<Value>) -> Value {
    json!({
        "hash": hash,
        "parentHash": parent,
        "sha3Uncles": format!("0x{}", "dd".repeat(32)),
        "miner": format!("0x{}", "00".repeat(20)),
        "stateRoot": format!("0x{}", "ee".repeat(32)),
        "transactionsRoot": format!("0x{}", "ff".repeat(32)),
        "receiptsRoot": format!("0x{}", "ab".repeat(32)),
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "difficulty": "0x0",
        "number": format!("0x{number:x}"),
        "gasLimit": "0x1c9c380",
        "gasUsed": if number == 0 { "0x0" } else { "0x5208" },
        "timestamp": "0x64",
        "extraData": "0x",
        "mixHash": format!("0x{}", "cd".repeat(32)),
        "nonce": "0x0000000000000000",
        "baseFeePerGas": "0x1",
        "uncles": [],
        "transactions": transactions
    })
}

fn receipt(node: &Node) -> Value {
    json!({
        "transactionHash": node.transaction.as_ref().expect("transaction must exist"),
        "transactionIndex": "0x0",
        "blockHash": BLOCK_HASH,
        "blockNumber": "0x1",
        "from": node.sender.as_ref().expect("wallet sender must be configured"),
        "to": format!("0x{}", hex::encode(DESTINATION)),
        "contractAddress": null,
        "status": "0x1",
        "gasUsed": "0x5208",
        "effectiveGasPrice": "0x2",
        "logs": []
    })
}

fn unused_address() -> SocketAddr {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("temporary listener must bind");
    listener
        .local_addr()
        .expect("temporary listener address must exist")
}
