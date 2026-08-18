use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{Json, Router, extract::State, routing::post};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute,
    consensus, hashes::Hash, hex::DisplayHex, transaction::Version,
};
use chain_bitcoin::{
    AddressType, CoreConfig, FeeRate, IndexUtxos, Network, RpcClient, WalletConfig, WalletProvider,
    parse_bitcoin_block_hash,
};
use http::client::{Config as HttpConfig, Reqwest};
use indexer_worker::{AuthenticationMode, BitcoinConfig, BitcoinService};
use indexing::{
    BlockHeight, CanonicalAddress, ChainId, History, HistoryQuery, IndexScope, Indexer,
    OutputQuery, TransactionQuery, TransactionRef, TransactionStatus, WatchRequest, WatchSelector,
    Watcher,
};
use indexing_http::{Config as IndexerConfig, Remote};
use json_rpc::TransportClient;
use payment_api::{Payments, Stage, StorageRepository, serve};
use serde_json::{Value, json};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;
use tokio::{sync::oneshot, task::JoinHandle};
use wallets::{SecretBytes, Wallets};

const SECRET: [u8; 32] = [1; 32];
const FUNDING_VALUE: u64 = 70_000;

#[derive(Clone)]
struct NodeState(Arc<Mutex<Node>>);

struct Node {
    fixture: Fixture,
    submitted: Option<Transaction>,
    broadcasts: usize,
}

struct Fixture {
    genesis_hash: String,
    funding_hash: String,
    spend_hash: String,
    confirmation_hash: String,
    genesis: Transaction,
    parent: Transaction,
    funding: Transaction,
    wallet_address: String,
    destination: String,
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
        self.task.await.expect("test server must not panic");
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
            .expect("indexer must stop cleanly");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_input_payment_is_signed_broadcast_indexed_and_confirmed() {
    let fixture = fixture();
    let state = NodeState(Arc::new(Mutex::new(Node {
        fixture,
        submitted: None,
        broadcasts: 0,
    })));
    let node = start_node(state.clone()).await;
    let files = TempDir::new().expect("temporary directory must be created");
    let indexer_address = unused_address();
    let endpoint = format!("http://{indexer_address}");
    let scope = IndexScope {
        chain: ChainId("bitcoin".to_owned()),
        network: "regtest".to_owned(),
    };
    let genesis_hash = state
        .0
        .lock()
        .expect("node lock must be healthy")
        .fixture
        .genesis_hash
        .clone();
    let indexer = start_indexer(
        files.path().join("indexer"),
        node.address,
        indexer_address,
        &genesis_hash,
    )
    .await;
    wait_ready(&format!("{endpoint}/health/ready")).await;

    let remote = Arc::new(
        Remote::connect(IndexerConfig::new(&endpoint)).expect("indexer client must construct"),
    );
    let wallet_address = state
        .0
        .lock()
        .expect("node lock must be healthy")
        .fixture
        .wallet_address
        .clone();
    remote
        .watch(WatchRequest {
            scope: scope.clone(),
            selector: WatchSelector::Address(CanonicalAddress {
                scope: scope.clone(),
                value: wallet_address.clone(),
            }),
            start_height: BlockHeight(1),
            idempotency_key: "system-bitcoin-wallet".to_owned(),
        })
        .await
        .expect("wallet address watch must register");
    wait_outputs(remote.as_ref(), &scope, &wallet_address, 2).await;

    let mut http = HttpConfig::new(format!("http://{}", node.address), Duration::from_secs(5));
    http.default_headers = vec![("authorization".to_owned(), "Basic test".to_owned())];
    let endpoint = format!("http://{}", node.address);
    let rpc = RpcClient::connect(
        TransportClient::new(
            Reqwest::new(http).expect("HTTP transport must construct"),
            endpoint,
        ),
        CoreConfig {
            expected_network: Network::Regtest,
            expected_genesis_hash: parse_bitcoin_block_hash(&genesis_hash)
                .expect("genesis hash must parse"),
        },
    )
    .await
    .expect("wallet RPC must connect");
    let outputs: Arc<dyn OutputQuery> = remote.clone();
    let utxos = Arc::new(
        IndexUtxos::new(scope.clone(), Network::Regtest, outputs)
            .expect("indexed UTXO adapter must construct"),
    );
    let provider = WalletProvider::new(
        WalletConfig {
            scope: scope.clone(),
            network: Network::Regtest,
            address_type: AddressType::SegwitV0,
            fee_target_blocks: 2,
            max_fee_rate: FeeRate::new(10_000),
        },
        utxos,
        Arc::new(rpc.fees()),
        Arc::new(rpc.transactions()),
        remote.clone(),
    );
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum WalletKey {
        Bitcoin,
    }
    let mut wallets = Wallets::new();
    wallets
        .register(WalletKey::Bitcoin, provider)
        .expect("wallet key must be unique");
    let wallet = wallets
        .new_wallet(&WalletKey::Bitcoin, SecretBytes::new(SECRET))
        .await
        .expect("concrete Bitcoin wallet must be created");
    assert_eq!(
        wallet
            .address_text(&wallet.address())
            .expect("address must encode")
            .text,
        wallet_address
    );

    let payments_db =
        Arc::new(RocksDb::open(files.path().join("payments")).expect("payment database must open"));
    let store = Arc::new(StorageRepository::new(payments_db));
    let index: Arc<dyn Indexer> = remote.clone();
    let payments = Arc::new(Payments::new(store, index).with("hot", scope.clone(), wallet));
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("payment listener must bind");
    let payment_address = listener.local_addr().expect("payment address must exist");
    let payment_task = tokio::spawn(serve(listener, payments.clone()));
    let destination = state
        .0
        .lock()
        .expect("node lock must be healthy")
        .fixture
        .destination
        .clone();
    let response = reqwest::Client::new()
        .post(format!("http://{payment_address}/v1/payments"))
        .json(&json!({
            "id": "bitcoin-system-payment",
            "wallet": "hot",
            "destination": {"encoding": "bech32", "text": destination},
            "amount": "0.001",
            "confirmations": 1
        }))
        .send()
        .await
        .expect("payment request must reach the service");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "{}",
        response.text().await.expect("error body must read")
    );

    let submitted_id = loop {
        if let Some(id) = state
            .0
            .lock()
            .expect("node lock must be healthy")
            .submitted
            .as_ref()
            .map(|transaction| transaction.compute_txid().to_string())
        {
            break id;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let submitted = state
        .0
        .lock()
        .expect("node lock must be healthy")
        .submitted
        .clone()
        .expect("transaction must have been broadcast");
    assert_eq!(
        submitted.input.len(),
        2,
        "the payment must spend both indexed UTXOs"
    );
    wait_confirmed(remote.as_ref(), &scope, &submitted_id).await;
    payments
        .reconcile(scope.clone(), 100)
        .await
        .expect("payment reconciliation must consume index events");
    let payment = payments
        .get("bitcoin-system-payment")
        .await
        .expect("payment lookup must succeed")
        .expect("payment must remain durable");
    assert!(matches!(payment.stage, Stage::Confirmed { .. }));
    let history = remote
        .history(HistoryQuery {
            scope: scope.clone(),
            address: CanonicalAddress {
                scope: scope.clone(),
                value: wallet_address,
            },
            after: None,
            limit: 10,
        })
        .await
        .expect("wallet history must be readable");
    assert!(
        history
            .transactions
            .iter()
            .any(|item| item.transaction_id.value == submitted_id)
    );
    assert_eq!(
        state
            .0
            .lock()
            .expect("node lock must be healthy")
            .broadcasts,
        1
    );

    payment_task.abort();
    let _ignored = payment_task.await;
    indexer.stop().await;
    node.stop().await;
}

async fn start_indexer(
    database: impl Into<std::path::PathBuf>,
    rpc: SocketAddr,
    api: SocketAddr,
    genesis: &str,
) -> RunningIndexer {
    let mut config = BitcoinConfig::new(
        database,
        Network::Regtest,
        0,
        1,
        10,
        genesis,
        format!("http://{rpc}"),
        AuthenticationMode::GlobalTrusted,
    );
    config.rpc_headers = vec!["authorization=Basic test".to_owned()];
    config.http_bind = api;
    config.poll_seconds = 1;
    let service = BitcoinService::new(config).expect("indexer config must validate");
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async move {
        let _ignored = receiver.await;
    }));
    RunningIndexer {
        shutdown: Some(shutdown),
        task,
    }
}

async fn wait_ready(url: &str) {
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
            "indexer did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_outputs(remote: &Remote<Reqwest>, scope: &IndexScope, address: &str, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let result = remote
            .outputs(indexing::OutputRequest {
                scope: scope.clone(),
                address: CanonicalAddress {
                    scope: scope.clone(),
                    value: address.to_owned(),
                },
                after: None,
                limit: 10,
            })
            .await;
        if result.is_ok_and(|page| page.outputs.len() == count) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "indexed outputs did not appear"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_confirmed(remote: &Remote<Reqwest>, scope: &IndexScope, id: &str) {
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
        if result.as_ref().is_ok_and(|value| {
            value.as_ref().is_some_and(|transaction| {
                matches!(transaction.status, TransactionStatus::Confirmed { .. })
            })
        }) {
            return;
        }
        let last = format!("{result:?}");
        assert!(
            tokio::time::Instant::now() < deadline,
            "transaction did not become confirmed: {last}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn start_node(state: NodeState) -> TestServer {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("node listener must bind");
    let address = listener.local_addr().expect("node address must exist");
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/", post(node_request))
                .with_state(state),
        )
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
            batch.iter().map(|item| response(&state, item)).collect(),
        ));
    }
    Json(response(&state, &request))
}

fn response(state: &NodeState, request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"].as_str().expect("method must be text");
    let result = node_result(state, method, &request["params"]);
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn node_result(state: &NodeState, method: &str, params: &Value) -> Value {
    let mut node = state.0.lock().expect("node lock must be healthy");
    let tip = if node.submitted.is_some() { 3 } else { 1 };
    match method {
        "getnetworkinfo" => json!({"version": 310000}),
        "getblockchaininfo" => json!({
            "chain": "regtest", "blocks": tip, "headers": tip,
            "bestblockhash": if tip == 3 { &node.fixture.confirmation_hash } else { &node.fixture.funding_hash },
            "initialblockdownload": false, "pruned": false
        }),
        "getindexinfo" => json!({"txindex": {"synced": true, "best_block_height": tip}}),
        "getblockcount" => json!(tip),
        "getblockhash" => match params[0].as_u64().expect("height must be integer") {
            0 => json!(node.fixture.genesis_hash),
            1 => json!(node.fixture.funding_hash),
            2 => json!(node.fixture.spend_hash),
            3 => json!(node.fixture.confirmation_hash),
            height => panic!("unexpected block height {height}"),
        },
        "getblockheader" => {
            let hash = params[0].as_str().expect("hash must be text");
            if hash == node.fixture.genesis_hash {
                json!({"hash": hash, "height": 0, "time": 99})
            } else if hash == node.fixture.funding_hash {
                json!({"hash": hash, "height": 1, "previousblockhash": node.fixture.genesis_hash, "time": 100})
            } else if hash == node.fixture.spend_hash {
                json!({"hash": hash, "height": 2, "previousblockhash": node.fixture.funding_hash, "time": 101})
            } else {
                json!({"hash": hash, "height": 3, "previousblockhash": node.fixture.spend_hash, "time": 102})
            }
        }
        "getblock" => {
            let hash = params[0].as_str().expect("hash must be text");
            if hash == node.fixture.genesis_hash {
                block_json(0, hash, None, &[node.fixture.genesis.clone()])
            } else if hash == node.fixture.funding_hash {
                block_json(
                    1,
                    hash,
                    Some(&node.fixture.genesis_hash),
                    &[node.fixture.funding.clone()],
                )
            } else if hash == node.fixture.spend_hash {
                block_json(
                    2,
                    hash,
                    Some(&node.fixture.funding_hash),
                    &[node
                        .submitted
                        .clone()
                        .expect("submitted transaction must exist")],
                )
            } else {
                block_json(3, hash, Some(&node.fixture.spend_hash), &[])
            }
        }
        "getrawtransaction" => {
            let id = params[0].as_str().expect("transaction ID must be text");
            if id == node.fixture.parent.compute_txid().to_string() {
                raw_transaction(&node.fixture.parent, &node.fixture.genesis_hash)
            } else if id == node.fixture.funding.compute_txid().to_string() {
                raw_transaction(&node.fixture.funding, &node.fixture.funding_hash)
            } else {
                panic!("unexpected previous transaction {id}")
            }
        }
        "estimatesmartfee" => json!({"feerate": 0.00001000, "blocks": 2}),
        "testmempoolaccept" => {
            let transaction = decode_submitted(&params[0][0]);
            json!([{"txid": transaction.compute_txid().to_string(), "allowed": true, "vsize": transaction.vsize()}])
        }
        "sendrawtransaction" => {
            let transaction = decode_submitted(&params[0]);
            let id = transaction.compute_txid().to_string();
            node.submitted = Some(transaction);
            node.broadcasts += 1;
            json!(id)
        }
        other => panic!("unexpected Bitcoin RPC method {other}"),
    }
}

fn decode_submitted(value: &Value) -> Transaction {
    let bytes = hex::decode(value.as_str().expect("transaction must be text"))
        .expect("transaction must be hexadecimal");
    consensus::deserialize(&bytes).expect("signed transaction must decode")
}

fn raw_transaction(transaction: &Transaction, block_hash: &str) -> Value {
    json!({
        "txid": transaction.compute_txid().to_string(),
        "hex": consensus::serialize(transaction).to_lower_hex_string(),
        "blockhash": block_hash,
        "confirmations": 1
    })
}

fn block_json(
    height: u64,
    hash: &str,
    parent: Option<&str>,
    transactions: &[Transaction],
) -> Value {
    let mut block = json!({
        "hash": hash, "height": height, "time": 100 + height, "nTx": transactions.len(),
        "tx": transactions.iter().map(transaction_json).collect::<Vec<_>>()
    });
    if let Some(parent) = parent {
        block["previousblockhash"] = json!(parent);
    }
    block
}

fn transaction_json(transaction: &Transaction) -> Value {
    json!({
        "txid": transaction.compute_txid().to_string(),
        "hex": consensus::serialize(transaction).to_lower_hex_string(),
        "vin": transaction.input.iter().map(|input| {
            if input.previous_output.is_null() {
                json!({"coinbase": "01"})
            } else {
                json!({"txid": input.previous_output.txid.to_string(), "vout": input.previous_output.vout})
            }
        }).collect::<Vec<_>>(),
        "vout": transaction.output.iter().enumerate().map(|(index, output)| json!({
            "value": output.value.to_btc(), "n": index,
            "scriptPubKey": {"hex": output.script_pubkey.as_bytes().to_lower_hex_string()}
        })).collect::<Vec<_>>()
    })
}

fn fixture() -> Fixture {
    use bitcoin::{
        CompressedPublicKey, PrivateKey, PublicKey,
        secp256k1::{Secp256k1, SecretKey},
    };
    let secp = Secp256k1::new();
    let secret = SecretKey::from_slice(&SECRET).expect("secret must be valid");
    let public =
        PublicKey::from_private_key(&secp, &PrivateKey::new(secret, bitcoin::Network::Regtest));
    let wallet = bitcoin::Address::p2wpkh(
        &CompressedPublicKey::try_from(public).expect("key must compress"),
        bitcoin::Network::Regtest,
    );
    let destination_secret =
        SecretKey::from_slice(&[2; 32]).expect("destination secret must be valid");
    let destination_public = PublicKey::from_private_key(
        &secp,
        &PrivateKey::new(destination_secret, bitcoin::Network::Regtest),
    );
    let destination = bitcoin::Address::p2wpkh(
        &CompressedPublicKey::try_from(destination_public).expect("key must compress"),
        bitcoin::Network::Regtest,
    );
    let genesis = coinbase(ScriptBuf::new(), 200_000);
    let parent = coinbase(ScriptBuf::new(), 200_000);
    let funding = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(parent.compute_txid(), 0),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(FUNDING_VALUE),
                script_pubkey: wallet.script_pubkey(),
            },
            TxOut {
                value: Amount::from_sat(FUNDING_VALUE),
                script_pubkey: wallet.script_pubkey(),
            },
        ],
    };
    Fixture {
        genesis_hash: Txid::from_byte_array([1; 32]).to_string(),
        funding_hash: Txid::from_byte_array([2; 32]).to_string(),
        spend_hash: Txid::from_byte_array([3; 32]).to_string(),
        confirmation_hash: Txid::from_byte_array([4; 32]).to_string(),
        genesis,
        parent,
        funding,
        wallet_address: wallet.to_string(),
        destination: destination.to_string(),
    }
}

fn coinbase(script: ScriptBuf, value: u64) -> Transaction {
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
            script_pubkey: script,
        }],
    }
}

fn unused_address() -> SocketAddr {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("temporary listener must bind");
    listener.local_addr().expect("temporary address must exist")
}
