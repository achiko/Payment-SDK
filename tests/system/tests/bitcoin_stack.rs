//! Deterministic Bitcoin Core and Indexer stack for Payment Service tests.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
};

use axum::{Json, Router, extract::State, routing::post};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness, absolute, consensus,
    consensus::encode::deserialize, hex::DisplayHex, transaction::Version,
};
use indexer_worker::{AuthenticationMode, BitcoinConfig, BitcoinService};
use serde_json::{Value, json};
use tokio::{sync::oneshot, task::JoinHandle};

/// One output placed in the fixture's height-one funding transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FundingOutput {
    pub address: String,
    pub satoshis: u64,
}

impl FundingOutput {
    #[must_use]
    pub fn new(address: impl Into<String>, satoshis: u64) -> Self {
        Self {
            address: address.into(),
            satoshis,
        }
    }
}

/// Stable chain facts produced by [`BitcoinStack`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinFixture {
    pub genesis_hash: String,
    pub block_hash: String,
    pub tip_hash: String,
    pub transaction_id: String,
}

/// Running loopback Bitcoin Core double plus a real Bitcoin Indexer Service.
pub struct BitcoinStack {
    pub fixture: BitcoinFixture,
    pub rpc_url: String,
    pub indexer_url: String,
    broadcasts: Arc<Mutex<Vec<Vec<u8>>>>,
    phase: Arc<AtomicU8>,
    rpc: TestServer,
    indexer: RunningIndexer,
}

impl BitcoinStack {
    /// Starts a deterministic regtest chain at height one and its real indexer.
    pub async fn start(database: &Path, outputs: Vec<FundingOutput>) -> Self {
        let state = Arc::new(RpcFixture::new(outputs));
        let fixture = state.fixture.clone();
        let broadcasts = Arc::clone(&state.broadcasts);
        let phase = Arc::clone(&state.phase);
        let rpc = start_rpc(state).await;
        let api = unused_address();
        let indexer = start_indexer(database, rpc.address, api, &fixture).await;
        Self {
            fixture,
            rpc_url: format!("http://{}", rpc.address),
            indexer_url: format!("http://{api}"),
            broadcasts,
            phase,
            rpc,
            indexer,
        }
    }

    /// Returns exact consensus envelopes submitted through `sendrawtransaction`.
    #[must_use]
    pub fn broadcasts(&self) -> Vec<Vec<u8>> {
        self.broadcasts
            .lock()
            .expect("broadcast capture mutex must not be poisoned")
            .clone()
    }

    /// Makes the deterministic funding block visible after watches exist.
    pub fn mine(&self) {
        self.phase.store(1, Ordering::Release);
    }

    /// Includes the captured sweep and one confirmation block.
    pub fn confirm(&self) {
        assert_eq!(self.broadcasts().len(), 1, "one sweep must be broadcast");
        self.phase.store(2, Ordering::Release);
    }

    /// Replaces both post-funding blocks with a branch omitting the sweep.
    pub fn reorg(&self) {
        self.phase.store(3, Ordering::Release);
    }

    /// Re-includes the identical captured sweep on the replacement branch.
    pub fn reinclude(&self) {
        assert_eq!(self.broadcasts().len(), 1, "sweep must not be rebuilt");
        self.phase.store(4, Ordering::Release);
    }

    pub async fn stop(self) {
        self.indexer.stop().await;
        self.rpc.stop().await;
    }
}

struct RpcFixture {
    fixture: BitcoinFixture,
    genesis: Value,
    block: Value,
    tip: Value,
    funding: Transaction,
    previous: Transaction,
    broadcasts: Arc<Mutex<Vec<Vec<u8>>>>,
    phase: Arc<AtomicU8>,
}

impl RpcFixture {
    fn new(outputs: Vec<FundingOutput>) -> Self {
        assert!(!outputs.is_empty(), "Bitcoin fixture needs an output");
        let outputs: Vec<TxOut> = outputs
            .into_iter()
            .map(|output| {
                let address = output
                    .address
                    .parse::<bitcoin::Address<_>>()
                    .expect("fixture address must parse")
                    .require_network(bitcoin::Network::Regtest)
                    .expect("fixture address must be regtest");
                TxOut {
                    value: Amount::from_sat(output.satoshis),
                    script_pubkey: address.script_pubkey(),
                }
            })
            .collect();
        let genesis = coinbase();
        let previous = coinbase_value(
            outputs
                .iter()
                .map(|output| output.value.to_sat())
                .sum::<u64>()
                + 10_000,
        );
        let funding = regular_transaction(previous.compute_txid(), outputs);
        let genesis_hash = format!("{:064x}", 1);
        let block_hash = format!("{:064x}", 2);
        let tip_hash = format!("{:064x}", 4);
        Self {
            fixture: BitcoinFixture {
                genesis_hash: genesis_hash.clone(),
                block_hash: block_hash.clone(),
                tip_hash: tip_hash.clone(),
                transaction_id: funding.compute_txid().to_string(),
            },
            genesis: bitcoin_block(0, &genesis_hash, None, &genesis, true),
            block: bitcoin_block(1, &block_hash, Some(&genesis_hash), &funding, false),
            tip: bitcoin_block(2, &tip_hash, Some(&block_hash), &coinbase(), true),
            funding,
            previous,
            broadcasts: Arc::new(Mutex::new(Vec::new())),
            phase: Arc::new(AtomicU8::new(0)),
        }
    }

    fn result(&self, method: &str, params: &Value) -> Value {
        match method {
            "getnetworkinfo" => json!({"version": 310000}),
            "getblockchaininfo" => json!({
                "chain": "regtest", "blocks": self.height(), "headers": self.height(),
                "bestblockhash": self.tip_hash(),
                "initialblockdownload": false, "pruned": false
            }),
            "getindexinfo" => {
                json!({"txindex": {"synced": true, "best_block_height": self.height()}})
            }
            "getblockcount" => json!(self.height()),
            "getblockhash" => json!(self.hash_at(params[0].as_u64().expect("height"))),
            "getblockheader" => self.header(params[0].as_str().expect("block hash")),
            "getblock" => self.block(params[0].as_str().expect("block hash")),
            "getrawtransaction" => {
                self.raw_transaction(params[0].as_str().expect("transaction ID"))
            }
            "estimatesmartfee" => json!({"feerate": 0.00001000, "blocks": 2}),
            "testmempoolaccept" => {
                let raw = decode_envelope(&params[0][0]);
                let transaction: Transaction =
                    deserialize(&raw).expect("submitted fixture transaction must decode");
                json!([{
                    "txid": transaction.compute_txid().to_string(),
                    "allowed": true,
                    "vsize": transaction.vsize(),
                    "fees": {"base": "0.00000100".parse::<serde_json::Number>().expect("fixed fee")}
                }])
            }
            "sendrawtransaction" => {
                let raw = decode_envelope(&params[0]);
                let transaction: Transaction =
                    deserialize(&raw).expect("broadcast fixture transaction must decode");
                self.broadcasts
                    .lock()
                    .expect("broadcast capture mutex must not be poisoned")
                    .push(raw);
                json!(transaction.compute_txid().to_string())
            }
            other => panic!("unexpected Bitcoin RPC method {other}"),
        }
    }

    fn height(&self) -> u64 {
        match self.phase.load(Ordering::Acquire) {
            0 => 0,
            1 => 2,
            2 | 3 => 4,
            _ => 6,
        }
    }

    fn tip_hash(&self) -> String {
        self.hash_at(self.height())
    }

    fn hash_at(&self, height: u64) -> String {
        let value = match (self.phase.load(Ordering::Acquire), height) {
            (_, 0) => 1,
            (_, 1) => 2,
            (_, 2) => 4,
            (2, 3) => 5,
            (2, 4) => 6,
            (3 | 4, 3) => 7,
            (3 | 4, 4) => 8,
            (4, 5) => 9,
            (4, 6) => 10,
            _ => panic!("height {height} is unavailable in the active fixture phase"),
        };
        format!("{value:064x}")
    }

    fn header(&self, hash: &str) -> Value {
        let (height, parent) = block_identity(hash);
        let mut header = json!({"hash": hash, "height": height, "time": 99 + height});
        if let Some(parent) = parent {
            header["previousblockhash"] = json!(parent);
        }
        header
    }

    fn block(&self, hash: &str) -> Value {
        if hash == self.fixture.genesis_hash {
            return self.genesis.clone();
        }
        if hash == self.fixture.block_hash {
            return self.block.clone();
        }
        if hash == self.fixture.tip_hash {
            return self.tip.clone();
        }
        let (height, parent) = block_identity(hash);
        let sweep = matches!(hash_value(hash), 5 | 9);
        let transaction = if sweep { self.sweep() } else { coinbase() };
        bitcoin_block(height, hash, parent.as_deref(), &transaction, !sweep)
    }

    fn raw_transaction(&self, id: &str) -> Value {
        let (transaction, blockhash) = if id == self.previous.compute_txid().to_string() {
            (&self.previous, self.fixture.genesis_hash.clone())
        } else if id == self.funding.compute_txid().to_string() {
            (&self.funding, self.fixture.block_hash.clone())
        } else {
            let sweep = self.sweep();
            return json!({
                "txid": sweep.compute_txid().to_string(),
                "hex": consensus::serialize(&sweep).to_lower_hex_string(),
                "blockhash": self.sweep_hash(),
            });
        };
        json!({
            "txid": transaction.compute_txid().to_string(),
            "hex": consensus::serialize(transaction).to_lower_hex_string(),
            "blockhash": blockhash,
        })
    }

    fn sweep(&self) -> Transaction {
        self.broadcasts
            .lock()
            .expect("broadcast capture mutex must not be poisoned")
            .last()
            .map(|raw| deserialize(raw).expect("captured sweep must decode"))
            .expect("sweep block requires a captured broadcast")
    }

    fn sweep_hash(&self) -> String {
        if self.phase.load(Ordering::Acquire) == 2 {
            format!("{:064x}", 5)
        } else {
            format!("{:064x}", 9)
        }
    }
}

fn hash_value(hash: &str) -> u64 {
    u64::from_str_radix(hash.trim_start_matches('0'), 16).unwrap_or(0)
}

fn block_identity(hash: &str) -> (u64, Option<String>) {
    let (height, parent) = match hash_value(hash) {
        1 => (0, None),
        2 => (1, Some(1)),
        4 => (2, Some(2)),
        5 | 7 => (3, Some(4)),
        6 => (4, Some(5)),
        8 => (4, Some(7)),
        9 => (5, Some(8)),
        10 => (6, Some(9)),
        value => panic!("unknown fixture block hash {value}"),
    };
    (height, parent.map(|value| format!("{value:064x}")))
}

fn decode_envelope(value: &Value) -> Vec<u8> {
    hex::decode(value.as_str().expect("transaction envelope must be hex"))
        .expect("transaction envelope must contain valid hex")
}

fn regular_transaction(previous: bitcoin::Txid, output: Vec<TxOut>) -> Transaction {
    Transaction {
        version: Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(previous, 0),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output,
    }
}

fn coinbase() -> Transaction {
    coinbase_value(50_000)
}

fn coinbase_value(value: u64) -> Transaction {
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
            script_pubkey: ScriptBuf::new(),
        }],
    }
}

fn bitcoin_block(
    height: u64,
    hash: &str,
    parent: Option<&str>,
    transaction: &Transaction,
    coinbase: bool,
) -> Value {
    let inputs = transaction
        .input
        .iter()
        .map(|input| {
            if coinbase {
                json!({"coinbase": "01"})
            } else {
                json!({
                    "txid": input.previous_output.txid.to_string(),
                    "vout": input.previous_output.vout
                })
            }
        })
        .collect::<Vec<_>>();
    let mut block = json!({
        "hash": hash, "height": height, "time": 100, "nTx": 1,
        "tx": [{
            "txid": transaction.compute_txid().to_string(),
            "hex": consensus::serialize(transaction).to_lower_hex_string(),
            "vin": inputs,
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

#[derive(Clone)]
struct RpcState(Arc<RpcFixture>);

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
    json!({"jsonrpc": "2.0", "id": id, "result": state.0.result(method, &request["params"])})
}

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
        self.task.await.expect("RPC task must not panic");
    }
}

async fn start_rpc(fixture: Arc<RpcFixture>) -> TestServer {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("mock RPC listener must bind");
    let address = listener.local_addr().expect("RPC address must exist");
    let (shutdown, receiver) = oneshot::channel();
    let app = Router::new()
        .route("/", post(rpc_request))
        .with_state(RpcState(fixture));
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

struct RunningIndexer {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), indexer_worker::ServiceError>>,
}

impl RunningIndexer {
    async fn stop(mut self) {
        self.shutdown
            .take()
            .expect("Indexer shutdown sender must exist")
            .send(())
            .ok();
        self.task
            .await
            .expect("Indexer task must not panic")
            .expect("Indexer must stop cleanly");
    }
}

async fn start_indexer(
    database: &Path,
    rpc: SocketAddr,
    api: SocketAddr,
    fixture: &BitcoinFixture,
) -> RunningIndexer {
    let mut config = BitcoinConfig::new(
        database,
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
    let service = BitcoinService::new(config).expect("Bitcoin Indexer config must validate");
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async move {
        let _ignored = receiver.await;
    }));
    RunningIndexer {
        shutdown: Some(shutdown),
        task,
    }
}

fn unused_address() -> SocketAddr {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("temporary listener must bind");
    listener.local_addr().expect("temporary address must exist")
}
