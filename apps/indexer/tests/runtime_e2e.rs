use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::{Json, Router, extract::State, routing::post};
use bitcoin::{
    Amount, CompressedPublicKey, OutPoint, PublicKey, ScriptBuf, Sequence, Transaction, TxIn,
    TxOut, Witness, absolute, consensus, hex::DisplayHex, transaction::Version,
};
use indexer_worker::{
    AuthenticationMode, BitcoinConfig, BitcoinService, EthereumConfig, EthereumService,
};
use indexing::{
    BlockHeight, CanonicalAddress, ChainId, EventQuery, History, HistoryQuery, IndexScope,
    Observer, OutputQuery, OutputRequest, TransactionQuery, TransactionRef, TransactionStatus,
    WatchRequest, WatchSelector, Watcher,
};
use indexing_http::{Config as RemoteConfig, Remote};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{sync::oneshot, task::JoinHandle};

const ETH_GENESIS: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
const ETH_BLOCK: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ETH_TX: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const ETH_FROM: &str = "0x1111111111111111111111111111111111111111";
const ETH_TO: &str = "0x2222222222222222222222222222222222222222";

struct TestServer {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ignored = shutdown.send(());
        }
        self.task.await.expect("test server task must not panic");
    }
}

#[derive(Clone)]
enum RpcState {
    Ethereum,
    Bitcoin(Arc<BitcoinFixture>),
}

struct BitcoinFixture {
    genesis_hash: String,
    block_hash: String,
    genesis: Value,
    block: Value,
    address: String,
    transaction_id: String,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ethereum_runtime_persists_history_and_cursor() {
    let rpc = start_rpc(RpcState::Ethereum).await;
    let database = TempDir::new().expect("temporary database directory must be created");
    let api_address = unused_address();
    let endpoint = format!("http://{api_address}");
    let scope = IndexScope {
        chain: ChainId(chain_ethereum::CHAIN.to_owned()),
        network: "e2e".to_owned(),
    };
    let remote = Remote::connect(RemoteConfig::new(&endpoint))
        .expect("remote Indexer client must construct");

    let first = start_ethereum(database.path(), rpc.address, api_address).await;
    let watch = wait_for_watch(
        &remote,
        WatchRequest {
            scope: scope.clone(),
            selector: WatchSelector::Address(CanonicalAddress {
                scope: scope.clone(),
                value: ETH_TO.to_owned(),
            }),
            start_height: BlockHeight(1),
            idempotency_key: "eth-e2e-watch".to_owned(),
        },
    )
    .await;
    let transaction = wait_for_transaction(&remote, &scope, ETH_TX).await;
    assert_confirmed(&transaction.status);
    let history = remote
        .history(HistoryQuery {
            scope: scope.clone(),
            address: CanonicalAddress {
                scope: scope.clone(),
                value: ETH_TO.to_owned(),
            },
            after: None,
            limit: 10,
        })
        .await
        .expect("Ethereum address history must be readable");
    assert_eq!(history.transactions.len(), 1);
    let events = wait_for_events(&remote, &scope).await;
    let cursor = events.events.last().expect("an event must exist").cursor;
    assert_confirmed(
        &events
            .events
            .last()
            .expect("an event must exist")
            .transaction
            .status,
    );
    first.stop().await;

    let second = start_ethereum(database.path(), rpc.address, api_address).await;
    wait_for_transaction(&remote, &scope, ETH_TX).await;
    let existing = wait_for_watch(
        &remote,
        WatchRequest {
            scope: scope.clone(),
            selector: WatchSelector::Address(CanonicalAddress {
                scope: scope.clone(),
                value: ETH_TO.to_owned(),
            }),
            start_height: BlockHeight(1),
            idempotency_key: "eth-e2e-watch".to_owned(),
        },
    )
    .await;
    assert_eq!(existing.id, watch.id);
    let persisted_history = remote
        .history(HistoryQuery {
            scope: scope.clone(),
            address: CanonicalAddress {
                scope: scope.clone(),
                value: ETH_TO.to_owned(),
            },
            after: None,
            limit: 10,
        })
        .await
        .expect("persisted Ethereum history must be readable");
    assert_eq!(persisted_history.transactions.len(), 1);
    let persisted = remote
        .events(EventQuery {
            scope: scope.clone(),
            after: None,
            limit: 10,
        })
        .await
        .expect("persisted event feed must be readable");
    assert_eq!(
        persisted.events.last().map(|event| event.cursor),
        Some(cursor)
    );
    assert!(
        remote
            .events(EventQuery {
                scope,
                after: Some(cursor),
                limit: 10,
            })
            .await
            .expect("persisted Ethereum cursor must be accepted")
            .events
            .is_empty()
    );
    second.stop().await;
    rpc.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bitcoin_runtime_persists_outputs_and_history() {
    let fixture = Arc::new(bitcoin_fixture());
    let rpc = start_rpc(RpcState::Bitcoin(Arc::clone(&fixture))).await;
    let database = TempDir::new().expect("temporary database directory must be created");
    let api_address = unused_address();
    let endpoint = format!("http://{api_address}");
    let scope = IndexScope {
        chain: ChainId(chain_bitcoin::CHAIN.to_owned()),
        network: "regtest".to_owned(),
    };
    let remote = Remote::connect(RemoteConfig::new(&endpoint))
        .expect("remote Indexer client must construct");

    let first = start_bitcoin(database.path(), rpc.address, api_address, &fixture).await;
    let request = WatchRequest {
        scope: scope.clone(),
        selector: WatchSelector::Address(CanonicalAddress {
            scope: scope.clone(),
            value: fixture.address.clone(),
        }),
        start_height: BlockHeight(1),
        idempotency_key: "btc-e2e-watch".to_owned(),
    };
    let watch = wait_for_watch(&remote, request.clone()).await;
    let transaction = wait_for_transaction(&remote, &scope, &fixture.transaction_id).await;
    assert_confirmed(&transaction.status);
    let events = wait_for_events(&remote, &scope).await;
    let cursor = events.events.last().expect("an event must exist").cursor;
    assert_confirmed(
        &events
            .events
            .last()
            .expect("an event must exist")
            .transaction
            .status,
    );
    let outputs = wait_for_outputs(&remote, &scope, &fixture.address).await;
    assert_eq!(outputs.outputs.len(), 1);
    assert_eq!(
        outputs.outputs[0].id.transaction.value,
        fixture.transaction_id
    );
    assert_eq!(
        outputs
            .snapshot
            .checkpoint
            .as_ref()
            .map(|block| block.height),
        Some(BlockHeight(1))
    );
    first.stop().await;

    let second = start_bitcoin(database.path(), rpc.address, api_address, &fixture).await;
    wait_for_transaction(&remote, &scope, &fixture.transaction_id).await;
    let existing = wait_for_watch(&remote, request).await;
    assert_eq!(existing.id, watch.id);
    let history = remote
        .history(HistoryQuery {
            scope: scope.clone(),
            address: CanonicalAddress {
                scope: scope.clone(),
                value: fixture.address.clone(),
            },
            after: None,
            limit: 10,
        })
        .await
        .expect("persisted Bitcoin history must be readable");
    assert_eq!(history.transactions.len(), 1);
    let persisted = remote
        .events(EventQuery {
            scope: scope.clone(),
            after: None,
            limit: 10,
        })
        .await
        .expect("persisted event feed must be readable");
    assert_eq!(
        persisted.events.last().map(|event| event.cursor),
        Some(cursor)
    );
    assert!(
        remote
            .events(EventQuery {
                scope: scope.clone(),
                after: Some(cursor),
                limit: 10,
            })
            .await
            .expect("persisted Bitcoin cursor must be accepted")
            .events
            .is_empty()
    );
    assert_eq!(
        wait_for_outputs(
            &remote,
            &IndexScope {
                chain: ChainId(chain_bitcoin::CHAIN.to_owned()),
                network: "regtest".to_owned()
            },
            &fixture.address
        )
        .await
        .outputs
        .len(),
        1
    );
    second.stop().await;
    rpc.stop().await;
}

struct RunningService {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), indexer_worker::ServiceError>>,
}

impl RunningService {
    async fn stop(mut self) {
        self.shutdown
            .take()
            .expect("shutdown sender must exist")
            .send(())
            .ok();
        self.task
            .await
            .expect("Indexer task must not panic")
            .expect("Indexer must shut down cleanly");
    }
}

async fn start_ethereum(
    path: &std::path::Path,
    rpc: SocketAddr,
    api: SocketAddr,
) -> RunningService {
    let mut config = EthereumConfig::new(
        path,
        "e2e",
        0,
        31_337,
        ETH_GENESIS,
        format!("http://{rpc}"),
        AuthenticationMode::GlobalTrusted,
    );
    config.confirmation_depth = 1;
    config.reorg_retention = 10;
    config.http_bind = api;
    config.poll_seconds = 1;
    let service = EthereumService::new(config).expect("Ethereum service config must validate");
    spawn_service(move |shutdown| {
        service.run_until(async move {
            let _ignored = shutdown.await;
        })
    })
    .await
}

async fn start_bitcoin(
    path: &std::path::Path,
    rpc: SocketAddr,
    api: SocketAddr,
    fixture: &BitcoinFixture,
) -> RunningService {
    let mut config = BitcoinConfig::new(
        path,
        chain_bitcoin::Network::Regtest,
        0,
        1,
        10,
        &fixture.genesis_hash,
        format!("http://{rpc}"),
        AuthenticationMode::GlobalTrusted,
    );
    config.rpc_headers = vec!["authorization=Basic test".to_owned()];
    config.http_bind = api;
    config.poll_seconds = 1;
    let service = BitcoinService::new(config).expect("Bitcoin service config must validate");
    spawn_service(move |shutdown| {
        service.run_until(async move {
            let _ignored = shutdown.await;
        })
    })
    .await
}

async fn spawn_service<F, Fut>(start: F) -> RunningService
where
    F: FnOnce(oneshot::Receiver<()>) -> Fut + Send + 'static,
    Fut: FutureOutput + 'static,
{
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(start(receiver));
    RunningService {
        shutdown: Some(shutdown),
        task,
    }
}

trait FutureOutput:
    std::future::Future<Output = Result<(), indexer_worker::ServiceError>> + Send
{
}
impl<T> FutureOutput for T where
    T: std::future::Future<Output = Result<(), indexer_worker::ServiceError>> + Send
{
}

async fn wait_for_watch(
    remote: &Remote<http::client::Reqwest>,
    request: WatchRequest,
) -> indexing::WatchReceipt {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match remote.watch(request.clone()).await {
            Ok(receipt) => return receipt,
            Err(error) if tokio::time::Instant::now() < deadline => {
                let _ignored = error;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => panic!("Indexer watch did not become available: {error}"),
        }
    }
}

async fn wait_for_transaction(
    remote: &Remote<http::client::Reqwest>,
    scope: &IndexScope,
    id: &str,
) -> indexing::ObservedTransaction {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let result = remote
            .transaction(TransactionQuery {
                scope: scope.clone(),
                transaction_id: TransactionRef {
                    scope: scope.clone(),
                    value: id.to_owned(),
                },
            })
            .await;
        match result {
            Ok(Some(transaction)) => return transaction,
            Ok(None) | Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(None) => panic!("indexed transaction did not appear"),
            Err(error) => panic!("indexed transaction lookup failed: {error}"),
        }
    }
}

async fn wait_for_events(
    remote: &Remote<http::client::Reqwest>,
    scope: &IndexScope,
) -> indexing::EventPage {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match remote
            .events(EventQuery {
                scope: scope.clone(),
                after: None,
                limit: 10,
            })
            .await
        {
            Ok(page) if !page.events.is_empty() => return page,
            Ok(_) | Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(_) => panic!("event feed remained empty"),
            Err(error) => panic!("event feed failed: {error}"),
        }
    }
}

fn assert_confirmed(status: &TransactionStatus) {
    assert!(matches!(status, TransactionStatus::Confirmed { .. }));
}

async fn wait_for_outputs(
    remote: &Remote<http::client::Reqwest>,
    scope: &IndexScope,
    address: &str,
) -> indexing::OutputPage {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(page) = remote
            .outputs(OutputRequest {
                scope: scope.clone(),
                address: CanonicalAddress {
                    scope: scope.clone(),
                    value: address.to_owned(),
                },
                after: None,
                limit: 10,
            })
            .await
            && !page.outputs.is_empty()
        {
            return page;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "output endpoint did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn start_rpc(state: RpcState) -> TestServer {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("mock RPC listener must bind");
    let address = listener.local_addr().expect("listener address must exist");
    let (shutdown, receiver) = oneshot::channel();
    let app = Router::new()
        .route("/", post(rpc_request))
        .with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ignored = receiver.await;
            })
            .await
            .expect("mock RPC server must run");
    });
    TestServer {
        address,
        shutdown: Some(shutdown),
        task,
    }
}

async fn rpc_request(State(state): State<RpcState>, Json(request): Json<Value>) -> Json<Value> {
    if let Some(batch) = request.as_array() {
        return Json(Value::Array(
            batch
                .iter()
                .map(|item| rpc_response(&state, item))
                .collect(),
        ));
    }
    Json(rpc_response(&state, &request))
}

fn rpc_response(state: &RpcState, request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"]
        .as_str()
        .expect("RPC method must be a string");
    let params = &request["params"];
    let result = match state {
        RpcState::Ethereum => ethereum_result(method, params),
        RpcState::Bitcoin(fixture) => bitcoin_result(fixture, method, params),
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn ethereum_result(method: &str, params: &Value) -> Value {
    match method {
        "eth_chainId" => json!("0x7a69"),
        "eth_getBlockByNumber" => {
            let tag = params[0].as_str().expect("block tag must be a string");
            let full = params[1].as_bool().expect("full flag must be a boolean");
            if tag == "0x0" {
                eth_block(
                    0,
                    ETH_GENESIS,
                    &format!("0x{}", "00".repeat(32)),
                    Vec::new(),
                )
            } else if full {
                eth_block(1, ETH_BLOCK, ETH_GENESIS, vec![eth_transaction()])
            } else {
                eth_block(1, ETH_BLOCK, ETH_GENESIS, vec![json!(ETH_TX)])
            }
        }
        "eth_getBlockReceipts" => json!([eth_receipt()]),
        _ => panic!("unexpected Ethereum RPC method {method}"),
    }
}

fn eth_block(number: u64, hash: &str, parent: &str, transactions: Vec<Value>) -> Value {
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
        "uncles": [],
        "transactions": transactions
    })
}

fn eth_transaction() -> Value {
    json!({
        "hash": ETH_TX, "from": ETH_FROM, "to": ETH_TO, "value": "0x2a",
        "transactionIndex": "0x0", "blockHash": ETH_BLOCK, "blockNumber": "0x1"
    })
}

fn eth_receipt() -> Value {
    json!({
        "transactionHash": ETH_TX, "transactionIndex": "0x0", "blockHash": ETH_BLOCK,
        "blockNumber": "0x1", "from": ETH_FROM, "to": ETH_TO, "contractAddress": null,
        "status": "0x1", "gasUsed": "0x5208", "effectiveGasPrice": "0x2", "logs": []
    })
}

fn bitcoin_result(fixture: &BitcoinFixture, method: &str, params: &Value) -> Value {
    match method {
        "getnetworkinfo" => json!({"version": 310000}),
        "getblockchaininfo" => json!({
            "chain": "regtest", "blocks": 1, "headers": 1,
            "bestblockhash": fixture.block_hash, "initialblockdownload": false, "pruned": false
        }),
        "getindexinfo" => json!({"txindex": {"synced": true, "best_block_height": 1}}),
        "getblockcount" => json!(1),
        "getblockhash" => {
            if params[0] == 0 {
                json!(fixture.genesis_hash)
            } else {
                json!(fixture.block_hash)
            }
        }
        "getblockheader" => {
            if params[0] == fixture.genesis_hash {
                json!({"hash": fixture.genesis_hash, "height": 0, "time": 99})
            } else {
                json!({"hash": fixture.block_hash, "height": 1, "previousblockhash": fixture.genesis_hash, "time": 100})
            }
        }
        "getblock" => {
            if params[0] == fixture.genesis_hash {
                fixture.genesis.clone()
            } else {
                fixture.block.clone()
            }
        }
        _ => panic!("unexpected Bitcoin RPC method {method}"),
    }
}

fn bitcoin_fixture() -> BitcoinFixture {
    let public_key = PublicKey::from_slice(&[
        0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
        0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16,
        0xf8, 0x17, 0x98,
    ])
    .expect("test public key must parse");
    let address = bitcoin::Address::p2wpkh(
        &CompressedPublicKey::try_from(public_key).expect("test key must be compressed"),
        bitcoin::Network::Regtest,
    );
    let transaction = coinbase(address.script_pubkey(), 50_000);
    let genesis_tx = coinbase(ScriptBuf::new(), 50_000);
    let genesis_hash = format!("{:064x}", 1);
    let block_hash = format!("{:064x}", 2);
    BitcoinFixture {
        genesis_hash: genesis_hash.clone(),
        block_hash: block_hash.clone(),
        genesis: bitcoin_block(0, &genesis_hash, None, &genesis_tx),
        block: bitcoin_block(1, &block_hash, Some(&genesis_hash), &transaction),
        address: address.to_string(),
        transaction_id: transaction.compute_txid().to_string(),
    }
}

fn coinbase(script_pubkey: ScriptBuf, value: u64) -> Transaction {
    Transaction {
        version: Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(vec![1, 1]),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey,
        }],
    }
}

fn bitcoin_block(
    height: u64,
    hash: &str,
    parent: Option<&str>,
    transaction: &Transaction,
) -> Value {
    let mut block = json!({
        "hash": hash, "height": height, "time": 100, "nTx": 1,
        "tx": [{
            "txid": transaction.compute_txid().to_string(),
            "hex": consensus::serialize(transaction).to_lower_hex_string(),
            "vin": [{"coinbase": "01"}],
            "vout": transaction.output.iter().enumerate().map(|(index, output)| json!({
                "value": output.value.to_btc(), "n": index,
                "scriptPubKey": {"hex": output.script_pubkey.as_bytes().to_lower_hex_string()}
            })).collect::<Vec<_>>()
        }]
    });
    if let Some(parent) = parent {
        block["previousblockhash"] = json!(parent);
    }
    block
}

fn unused_address() -> SocketAddr {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("temporary listener must bind");
    listener.local_addr().expect("temporary address must exist")
}
