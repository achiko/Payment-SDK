use std::{
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
};

use alloy_primitives::keccak256;
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use tokio::{sync::oneshot, task::JoinHandle};

pub const GENESIS_HASH: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

#[derive(Clone, Debug)]
pub struct Transaction {
    pub hash: String,
    pub from: String,
    pub to: String,
    pub value: String,
}

impl Transaction {
    pub fn native(hash_byte: u8, from: String, to: String, value: u128) -> Self {
        Self {
            hash: format!("0x{}", hex::encode([hash_byte; 32])),
            from,
            to,
            value: format!("0x{value:x}"),
        }
    }
}

#[derive(Clone)]
pub struct EthereumNode {
    state: StateHandle,
    pub rpc_url: String,
    node: Arc<Mutex<Option<Server>>>,
}

impl EthereumNode {
    pub async fn start() -> Self {
        let state = StateHandle(Arc::new(Mutex::new(Node::default())));
        let node = start_node(state.clone()).await;
        Self {
            state,
            rpc_url: format!("http://{}", node.address),
            node: Arc::new(Mutex::new(Some(node))),
        }
    }

    pub fn append(&self, transactions: Vec<Transaction>) {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .append(transactions);
    }

    /// Replaces the visible chain with a same-height empty branch.
    pub fn reorg(&self) {
        let mut node = self
            .state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy");
        assert_eq!(node.blocks.len(), 2, "reorg a two-block fixture branch");
        node.blocks = vec![
            Block {
                hash: format!("0x{}", "aa".repeat(32)),
                parent: GENESIS_HASH.to_owned(),
                transactions: Vec::new(),
            },
            Block {
                hash: format!("0x{}", "bb".repeat(32)),
                parent: format!("0x{}", "aa".repeat(32)),
                transactions: Vec::new(),
            },
        ];
    }

    /// Describes the transaction that the wallet API is expected to submit.
    pub fn expect_send(&self, from: String, to: String, value: u128) {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .expected
            .push_back(Transaction::native(0, from, to, value));
    }

    /// Rejects the next submission after `accepted` further transactions.
    pub fn reject_after(&self, accepted: usize) {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .reject_after = Some(accepted);
    }

    /// Includes the submitted transaction and mines one confirmation block.
    pub fn confirm(&self) {
        let mut node = self
            .state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy");
        assert!(
            !node.submitted.is_empty(),
            "a transaction must be submitted before confirmation"
        );
        let transactions = std::mem::take(&mut node.submitted);
        let spent = transactions.iter().fold(0_u128, |total, transaction| {
            total
                .checked_add(
                    u128::from_str_radix(transaction.value.trim_start_matches("0x"), 16)
                        .expect("configured transaction value must be hexadecimal"),
                )
                .expect("submitted values must fit")
        });
        node.balance = node
            .balance
            .checked_sub(spent)
            .expect("submitted values must not exceed the fixture balance");
        node.append(transactions);
        node.append(Vec::new());
    }

    #[must_use]
    pub fn submitted_ids(&self) -> Vec<String> {
        self.state
            .0
            .lock()
            .expect("Ethereum node lock must be healthy")
            .submitted
            .iter()
            .map(|transaction| transaction.hash.clone())
            .collect()
    }

    pub async fn stop(self) {
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
    expected: VecDeque<Transaction>,
    submitted: Vec<Transaction>,
    balance: u128,
    reject_after: Option<usize>,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            expected: VecDeque::new(),
            submitted: Vec::new(),
            balance: 10_000_000_000_000_000_000,
            reject_after: None,
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
    if method == "eth_sendRawTransaction" {
        return match submit(state, &request["params"]) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
    }
    let result = result(state, method, &request["params"]);
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn result(state: &StateHandle, method: &str, params: &Value) -> Value {
    match method {
        "eth_chainId" => json!("0x1"),
        "eth_getBalance" => {
            let node = state.0.lock().expect("Ethereum node lock must be healthy");
            json!(format!("0x{:x}", node.balance))
        }
        "eth_blockNumber" => {
            let node = state.0.lock().expect("Ethereum node lock must be healthy");
            json!(format!("0x{:x}", node.blocks.len()))
        }
        "eth_getTransactionCount" => json!("0x0"),
        "eth_estimateGas" => json!("0x5208"),
        "eth_maxPriorityFeePerGas" => json!("0x1"),
        "eth_getBlockByNumber" => block_by_number(state, params),
        "eth_getBlockReceipts" => block_receipts(state, params),
        "eth_getTransactionReceipt" => transaction_receipt(state, params),
        other => panic!("unexpected Ethereum JSON-RPC method {other}"),
    }
}

fn submit(state: &StateHandle, params: &Value) -> Result<Value, Value> {
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
    if let Some(remaining) = node.reject_after.as_mut() {
        if *remaining == 0 {
            node.reject_after = None;
            return Err(json!({"code": -32000, "message": "fixture rejected transaction"}));
        }
        *remaining -= 1;
    }
    let mut transaction = node
        .expected
        .pop_front()
        .expect("test must describe the expected Ethereum transaction");
    transaction.hash = hash.clone();
    node.submitted.push(transaction);
    Ok(json!(hash))
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
                    0,
                    21_000,
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
                0,
                21_000,
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
        "logs": []
    })
}
