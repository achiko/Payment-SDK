use std::{
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
};

use alloy_primitives::keccak256;
use axum::{Json, Router, extract::State, routing::post};
use indexer_worker::{AuthenticationMode, EthereumConfig, EthereumService};
use serde_json::{Value, json};
use tokio::{sync::oneshot, task::JoinHandle};

pub const GENESIS_HASH: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

#[derive(Clone, Debug)]
pub struct Transaction {
    pub hash: String,
    pub from: String,
    pub to: String,
    pub value: String,
    pub logs: Vec<Value>,
}

impl Transaction {
    pub fn native(hash_byte: u8, from: String, to: String, value: u128) -> Self {
        Self {
            hash: format!("0x{}", hex::encode([hash_byte; 32])),
            from,
            to,
            value: format!("0x{value:x}"),
            logs: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct EthereumStack {
    state: StateHandle,
    pub rpc_url: String,
    pub indexer_url: String,
    node: Arc<Mutex<Option<Server>>>,
    indexer: Arc<Mutex<Option<RunningIndexer>>>,
}

impl EthereumStack {
    pub async fn start(database: &Path) -> Self {
        let state = StateHandle(Arc::new(Mutex::new(Node::default())));
        let node = start_node(state.clone()).await;
        let indexer_address = unused_address();
        let indexer_url = format!("http://{indexer_address}");
        let indexer = start_indexer(database, node.address, indexer_address).await;
        wait_ready(&format!("{indexer_url}/health/ready")).await;
        Self {
            state,
            rpc_url: format!("http://{}", node.address),
            indexer_url,
            node: Arc::new(Mutex::new(Some(node))),
            indexer: Arc::new(Mutex::new(Some(indexer))),
        }
    }

    pub fn append(&self, transactions: Vec<Transaction>) {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .append(transactions);
    }

    pub fn expect_broadcast(&self, from: String, to: String, value: u128) {
        self.expect_transaction(Transaction {
            hash: String::new(),
            from,
            to,
            value: format!("0x{value:x}"),
            logs: Vec::new(),
        });
    }

    /// Queues the canonical transaction facts attached to the next raw
    /// broadcast. The fixture computes and replaces `transaction.hash` from
    /// the submitted envelope.
    pub fn expect_transaction(&self, transaction: Transaction) {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .next_broadcasts
            .push_back(transaction);
    }

    /// Configures the raw result returned by `eth_call`, permitting token
    /// balance fixtures without teaching this generic node ABI semantics.
    pub fn eth_call_result(&self, result: impl Into<String>) {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .eth_call_result = Some(result.into());
    }

    pub fn receipt_gas_price(&self, value: u128) {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .receipt_gas_price = value;
    }

    pub fn receipt_gas_used(&self, value: u64) {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .receipt_gas_used = value;
    }

    pub fn broadcasts(&self) -> usize {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .broadcasts
    }

    /// Replaces the latest transaction block and its confirmation block with
    /// an empty canonical fork at the same heights.
    pub fn reorg_last_broadcast(&self) {
        let mut node = self
            .state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy");
        assert!(
            node.blocks.len() >= 2,
            "a broadcast fork requires two blocks"
        );
        let transaction_position = node.blocks.len() - 2;
        let parent = if transaction_position == 0 {
            GENESIS_HASH.to_owned()
        } else {
            node.blocks[transaction_position - 1].hash.clone()
        };
        let height = transaction_position as u64 + 1;
        let fork_hash = format!("0x{:064x}", height + (1_u64 << 32));
        let tip_hash = format!("0x{:064x}", height + 1 + (1_u64 << 32));
        node.blocks[transaction_position] = Block {
            hash: fork_hash.clone(),
            parent,
            transactions: Vec::new(),
        };
        node.blocks[transaction_position + 1] = Block {
            hash: tip_hash,
            parent: fork_hash,
            transactions: Vec::new(),
        };
    }

    pub async fn stop(self) {
        let indexer = self
            .indexer
            .lock()
            .expect("Ethereum Indexer lock must be healthy")
            .take();
        if let Some(indexer) = indexer {
            indexer.stop().await;
        }
        let node = self
            .node
            .lock()
            .expect("Ethereum node lock must be healthy")
            .take();
        if let Some(node) = node {
            node.stop().await;
        }
    }
}

#[derive(Clone)]
struct StateHandle(Arc<Mutex<Node>>);

struct Node {
    blocks: Vec<Block>,
    next_broadcasts: VecDeque<Transaction>,
    eth_call_result: Option<String>,
    broadcasts: usize,
    receipt_gas_price: u128,
    receipt_gas_used: u64,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            next_broadcasts: VecDeque::new(),
            eth_call_result: None,
            broadcasts: 0,
            receipt_gas_price: 0,
            receipt_gas_used: 21_000,
        }
    }
}

#[derive(Clone)]
struct Block {
    hash: String,
    parent: String,
    transactions: Vec<Transaction>,
}

impl Node {
    fn append(&mut self, transactions: Vec<Transaction>) {
        let height = self.blocks.len() as u64 + 1;
        let parent = self
            .blocks
            .last()
            .map_or_else(|| GENESIS_HASH.to_owned(), |block| block.hash.clone());
        self.blocks.push(Block {
            hash: format!("0x{height:064x}"),
            parent,
            transactions,
        });
    }
}

struct Server {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Server {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send(()).ok();
        }
        self.task.await.expect("Ethereum node must not panic");
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
            .expect("Ethereum Indexer shutdown sender must exist")
            .send(())
            .ok();
        self.task
            .await
            .expect("Ethereum Indexer must not panic")
            .expect("Ethereum Indexer must stop cleanly");
    }
}

async fn start_node(state: StateHandle) -> Server {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("Ethereum node listener must bind");
    let address = listener.local_addr().expect("node address must exist");
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/", post(request)).with_state(state),
        )
        .with_graceful_shutdown(async move {
            let _ignored = receiver.await;
        })
        .await
        .expect("Ethereum node must run");
    });
    Server {
        address,
        shutdown: Some(shutdown),
        task,
    }
}

async fn request(State(state): State<StateHandle>, Json(request): Json<Value>) -> Json<Value> {
    if let Some(batch) = request.as_array() {
        return Json(Value::Array(
            batch.iter().map(|value| response(&state, value)).collect(),
        ));
    }
    Json(response(&state, &request))
}

fn response(state: &StateHandle, request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"]
        .as_str()
        .expect("Ethereum JSON-RPC method must be text");
    let result = result(state, method, &request["params"]);
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn result(state: &StateHandle, method: &str, params: &Value) -> Value {
    match method {
        "eth_chainId" => json!("0x1"),
        "eth_getTransactionCount" => json!("0x0"),
        "eth_estimateGas" => json!("0x5208"),
        "eth_maxPriorityFeePerGas" => json!("0x1"),
        "eth_getBalance" => json!("0x8ac7230489e80000"),
        "eth_call" => {
            let node = state.0.lock().expect("Ethereum node lock must be healthy");
            json!(node.eth_call_result.as_deref().unwrap_or("0x0"))
        }
        "eth_blockNumber" => {
            let node = state.0.lock().expect("Ethereum node lock must be healthy");
            json!(format!("0x{:x}", node.blocks.len()))
        }
        "eth_sendRawTransaction" => broadcast(state, params),
        "eth_getBlockByNumber" => block_by_number(state, params),
        "eth_getBlockReceipts" => block_receipts(state, params),
        "eth_getTransactionReceipt" => transaction_receipt(state, params),
        other => panic!("unexpected Ethereum JSON-RPC method {other}"),
    }
}

fn broadcast(state: &StateHandle, params: &Value) -> Value {
    let envelope = params[0]
        .as_str()
        .and_then(|value| value.strip_prefix("0x"))
        .expect("raw transaction must have a hexadecimal prefix");
    let hash = format!(
        "0x{}",
        hex::encode(keccak256(
            hex::decode(envelope).expect("raw transaction must be hexadecimal")
        ))
    );
    let mut node = state.0.lock().expect("Ethereum node lock must be healthy");
    let mut transaction = node
        .next_broadcasts
        .pop_front()
        .expect("test must configure the next Ethereum broadcast");
    transaction.hash = hash.clone();
    node.broadcasts += 1;
    node.append(vec![transaction]);
    node.append(Vec::new());
    json!(hash)
}

fn block_by_number(state: &StateHandle, params: &Value) -> Value {
    let tag = params[0].as_str().expect("block tag must be text");
    let full = params[1]
        .as_bool()
        .expect("full block flag must be boolean");
    let node = state.0.lock().expect("Ethereum node lock must be healthy");
    let height = if tag == "latest" {
        node.blocks.len() as u64
    } else {
        u64::from_str_radix(tag.trim_start_matches("0x"), 16)
            .expect("block number must be hexadecimal")
    };
    if height == 0 {
        return block_value(
            0,
            GENESIS_HASH,
            &format!("0x{}", "00".repeat(32)),
            &[],
            full,
        );
    }
    node.blocks
        .get(height as usize - 1)
        .map_or(Value::Null, |block| {
            block_value(
                height,
                &block.hash,
                &block.parent,
                &block.transactions,
                full,
            )
        })
}

fn block_receipts(state: &StateHandle, params: &Value) -> Value {
    let hash = params[0].as_str().expect("block hash must be text");
    let node = state.0.lock().expect("Ethereum node lock must be healthy");
    let Some((height, block)) = node
        .blocks
        .iter()
        .enumerate()
        .find(|(_, block)| block.hash == hash)
    else {
        return json!([]);
    };
    Value::Array(
        block
            .transactions
            .iter()
            .enumerate()
            .map(|(index, transaction)| {
                receipt(
                    transaction,
                    height as u64 + 1,
                    &block.hash,
                    index,
                    node.receipt_gas_price,
                    node.receipt_gas_used,
                )
            })
            .collect(),
    )
}

fn transaction_receipt(state: &StateHandle, params: &Value) -> Value {
    let hash = params[0].as_str().expect("transaction hash must be text");
    let node = state.0.lock().expect("Ethereum node lock must be healthy");
    for (height, block) in node.blocks.iter().enumerate() {
        if let Some((index, transaction)) = block
            .transactions
            .iter()
            .enumerate()
            .find(|(_, transaction)| transaction.hash == hash)
        {
            return receipt(
                transaction,
                height as u64 + 1,
                &block.hash,
                index,
                node.receipt_gas_price,
                node.receipt_gas_used,
            );
        }
    }
    Value::Null
}

fn block_value(
    number: u64,
    hash: &str,
    parent: &str,
    transactions: &[Transaction],
    full: bool,
) -> Value {
    let transactions = transactions
        .iter()
        .enumerate()
        .map(|(index, transaction)| {
            if full {
                json!({
                    "hash": transaction.hash,
                    "from": transaction.from,
                    "to": transaction.to,
                    "value": transaction.value,
                    "transactionIndex": format!("0x{index:x}"),
                    "blockHash": hash,
                    "blockNumber": format!("0x{number:x}")
                })
            } else {
                json!(transaction.hash)
            }
        })
        .collect::<Vec<_>>();
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
        "gasUsed": if transactions.is_empty() { "0x0" } else { "0x5208" },
        "timestamp": format!("0x{:x}", 100 + number),
        "extraData": "0x",
        "mixHash": format!("0x{}", "cd".repeat(32)),
        "nonce": "0x0000000000000000",
        "baseFeePerGas": "0x1",
        "uncles": [],
        "transactions": transactions
    })
}

fn receipt(
    transaction: &Transaction,
    height: u64,
    block_hash: &str,
    index: usize,
    gas_price: u128,
    gas_used: u64,
) -> Value {
    let logs = transaction
        .logs
        .iter()
        .enumerate()
        .map(|(log_index, value)| {
            let mut value = value.clone();
            let object = value
                .as_object_mut()
                .expect("configured Ethereum log must be an object");
            object.insert("blockHash".to_owned(), json!(block_hash));
            object.insert("blockNumber".to_owned(), json!(format!("0x{height:x}")));
            object.insert("transactionHash".to_owned(), json!(transaction.hash));
            object.insert("transactionIndex".to_owned(), json!(format!("0x{index:x}")));
            object.insert("logIndex".to_owned(), json!(format!("0x{log_index:x}")));
            object.insert("removed".to_owned(), json!(false));
            value
        })
        .collect::<Vec<_>>();
    json!({
        "transactionHash": transaction.hash,
        "transactionIndex": format!("0x{index:x}"),
        "blockHash": block_hash,
        "blockNumber": format!("0x{height:x}"),
        "from": transaction.from,
        "to": transaction.to,
        "contractAddress": null,
        "status": "0x1",
        "gasUsed": format!("0x{gas_used:x}"),
        "effectiveGasPrice": format!("0x{gas_price:x}"),
        "logs": logs
    })
}

async fn start_indexer(database: &Path, rpc: SocketAddr, api: SocketAddr) -> RunningIndexer {
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
    let service = EthereumService::new(config).expect("Ethereum Indexer config must validate");
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
    for _ in 0..200 {
        if reqwest::get(url)
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("Ethereum Indexer did not become ready");
}

fn unused_address() -> SocketAddr {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("temporary listener must bind")
        .local_addr()
        .expect("temporary listener address must exist")
}
