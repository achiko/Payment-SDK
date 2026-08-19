//! Deterministic Bitcoin Core RPC double for API system tests.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

#[path = "bitcoin_acceptance.rs"]
mod acceptance;

use axum::{Json, Router, extract::State, routing::post};
use bitcoin::{
    Amount, CompressedPublicKey, Network, OutPoint, PublicKey, ScriptBuf, Sequence, Transaction,
    TxIn, TxOut, Witness, absolute, consensus, consensus::encode::deserialize, hex::DisplayHex,
    transaction::Version,
};
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

/// Stable chain facts produced by [`BitcoinNode`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinFixture {
    pub genesis_hash: String,
    pub block_hash: String,
    pub tip_hash: String,
    pub transaction_id: String,
}

/// Running loopback Bitcoin Core double.
pub struct BitcoinNode {
    pub fixture: BitcoinFixture,
    pub rpc_url: String,
    state: Arc<Mutex<RpcFixture>>,
    phase: Arc<AtomicU8>,
    reorged: Arc<AtomicBool>,
    rpc: TestServer,
}

impl BitcoinNode {
    /// Starts a deterministic regtest chain whose blocks are initially hidden.
    pub async fn start() -> Self {
        let fixture_state = RpcFixture::new(Vec::new());
        let fixture = fixture_state.fixture.clone();
        let phase = Arc::clone(&fixture_state.phase);
        let reorged = Arc::clone(&fixture_state.reorged);
        let state = Arc::new(Mutex::new(fixture_state));
        let rpc = start_rpc(Arc::clone(&state)).await;
        Self {
            fixture,
            rpc_url: format!("http://{}", rpc.address),
            state,
            phase,
            reorged,
            rpc,
        }
    }

    /// Configures the funding transaction after the API has generated an
    /// address, but before [`Self::mine`] makes the block visible.
    #[must_use]
    pub fn fund(&self, outputs: Vec<FundingOutput>) -> String {
        assert_eq!(self.phase.load(Ordering::Acquire), 0, "fund before mining");
        assert!(!outputs.is_empty(), "funding needs at least one output");
        let replacement = RpcFixture::new(outputs);
        let transaction_id = replacement.fixture.transaction_id.clone();
        let mut state = self.state.lock().expect("RPC fixture lock must be healthy");
        state.fixture = replacement.fixture;
        state.block = replacement.block;
        state.funding = replacement.funding;
        state.previous = replacement.previous;
        transaction_id
    }

    /// Makes the deterministic funding block visible after the address is registered.
    pub fn mine(&self) {
        self.phase.store(1, Ordering::Release);
    }

    /// Includes the transaction submitted through the wallet API and mines
    /// one confirmation block above it.
    pub fn confirm(&self) {
        assert!(
            self.state
                .lock()
                .expect("RPC fixture lock must be healthy")
                .submitted
                .len()
                >= usize::from(self.phase.load(Ordering::Acquire)),
            "a transaction must be submitted before confirmation"
        );
        self.phase.fetch_add(1, Ordering::AcqRel);
    }

    /// Replaces both visible blocks with an alternate empty branch.
    pub fn reorg(&self) {
        assert_eq!(
            self.phase.load(Ordering::Acquire),
            1,
            "reorg funding branch"
        );
        self.reorged.store(true, Ordering::Release);
    }

    pub async fn stop(self) {
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
    submitted: Vec<Transaction>,
    phase: Arc<AtomicU8>,
    reorged: Arc<AtomicBool>,
}

impl RpcFixture {
    fn new(outputs: Vec<FundingOutput>) -> Self {
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
            submitted: Vec::new(),
            phase: Arc::new(AtomicU8::new(0)),
            reorged: Arc::new(AtomicBool::new(false)),
        }
    }

    fn result(&mut self, method: &str, params: &Value) -> Value {
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
                let transaction = decode_transaction(&params[0][0]);
                json!([{
                    "txid": transaction.compute_txid().to_string(),
                    "allowed": true,
                    "vsize": transaction.vsize(),
                    "fees": {"base": "0.00000100".parse::<serde_json::Number>().expect("fixed fee")}
                }])
            }
            "sendrawtransaction" => {
                let transaction = decode_transaction(&params[0]);
                let id = transaction.compute_txid().to_string();
                self.submitted.push(transaction);
                json!(id)
            }
            other => panic!("unexpected Bitcoin RPC method {other}"),
        }
    }

    fn height(&self) -> u64 {
        match self.phase.load(Ordering::Acquire) {
            0 => 0,
            1 => 2,
            phase => u64::from(phase) * 2,
        }
    }

    fn tip_hash(&self) -> String {
        self.hash_at(self.height())
    }

    fn hash_at(&self, height: u64) -> String {
        if self.reorged.load(Ordering::Acquire) {
            return match height {
                0 => format!("{:064x}", 1),
                1 => format!("{:064x}", 9),
                2 => format!("{:064x}", 10),
                _ => panic!("height {height} is unavailable in the reorg fixture"),
            };
        }
        let value = match height {
            0 => 1,
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 6,
            5 => 7,
            6 => 8,
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
        if matches!(hash_value(hash), 9 | 10) {
            let (height, parent) = block_identity(hash);
            return bitcoin_block(height, hash, parent.as_deref(), &coinbase(), true);
        }
        if matches!(hash_value(hash), 5 | 7) {
            let position = usize::from(hash_value(hash) == 7);
            let height = 3 + u64::try_from(position).expect("position must fit") * 2;
            let parent = if position == 0 { 4 } else { 6 };
            return bitcoin_block(
                height,
                hash,
                Some(&format!("{parent:064x}")),
                self.submitted
                    .get(position)
                    .expect("submitted block requires a transaction"),
                false,
            );
        }
        if matches!(hash_value(hash), 6 | 8) {
            let height = if hash_value(hash) == 6 { 4 } else { 6 };
            return bitcoin_block(
                height,
                hash,
                Some(&format!("{:064x}", hash_value(hash) - 1)),
                &coinbase(),
                true,
            );
        }
        panic!("unknown fixture block hash {hash}")
    }

    fn raw_transaction(&self, id: &str) -> Value {
        let (transaction, blockhash) = if id == self.previous.compute_txid().to_string() {
            (&self.previous, self.fixture.genesis_hash.clone())
        } else if id == self.funding.compute_txid().to_string() {
            (&self.funding, self.fixture.block_hash.clone())
        } else if let Some((position, transaction)) = self
            .submitted
            .iter()
            .enumerate()
            .find(|(_, transaction)| transaction.compute_txid().to_string() == id)
        {
            (transaction, format!("{:064x}", 5 + position * 2))
        } else {
            panic!("unknown fixture transaction {id}");
        };
        json!({
            "txid": transaction.compute_txid().to_string(),
            "hex": consensus::serialize(transaction).to_lower_hex_string(),
            "blockhash": blockhash,
        })
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
        5 => (3, Some(4)),
        6 => (4, Some(5)),
        7 => (5, Some(6)),
        8 => (6, Some(7)),
        9 => (1, Some(1)),
        10 => (2, Some(9)),
        value => panic!("unknown fixture block hash {value}"),
    };
    (height, parent.map(|value| format!("{value:064x}")))
}

fn decode_transaction(value: &Value) -> Transaction {
    let bytes = hex::decode(value.as_str().expect("transaction envelope must be hex"))
        .expect("transaction envelope must contain valid hex");
    deserialize(&bytes).expect("transaction envelope must decode")
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
struct RpcState(Arc<Mutex<RpcFixture>>);

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
    let result = state
        .0
        .lock()
        .expect("RPC fixture lock must be healthy")
        .result(method, &request["params"]);
    json!({"jsonrpc": "2.0", "id": id, "result": result})
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

async fn start_rpc(fixture: Arc<Mutex<RpcFixture>>) -> TestServer {
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
