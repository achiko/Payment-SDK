use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use futures_executor::block_on;
use indexing::ChainId;
use json_rpc::Call;
use serde_json::json;

use super::*;

const GENESIS_HASH: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PARENT_HASH: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TX_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const TX_HASH_2: &str = "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const FROM: &str = "0x1111111111111111111111111111111111111111";
const TO: &str = "0x2222222222222222222222222222222222222222";

#[derive(Clone)]
struct ScriptedClient {
    state: Arc<Mutex<ScriptState>>,
}

struct ScriptState {
    replies: VecDeque<ExpectedReply>,
    methods: Vec<String>,
}

struct ExpectedReply {
    method: &'static str,
    result: Result<RawJson, i64>,
}

impl ScriptedClient {
    fn new(replies: Vec<ExpectedReply>) -> Self {
        Self {
            state: Arc::new(Mutex::new(ScriptState {
                replies: replies.into(),
                methods: Vec::new(),
            })),
        }
    }

    fn methods(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("script lock must be healthy")
            .methods
            .clone()
    }

    fn response(&self, method: &str) -> Result<RawJson, Failure> {
        let mut state = self.state.lock().expect("script lock must be healthy");
        let expected = state
            .replies
            .pop_front()
            .expect("source made more requests than scripted");
        assert_eq!(method, expected.method);
        state.methods.push(method.to_owned());
        expected.result.map_err(|code| Failure {
            code,
            message: "scripted failure".to_owned(),
            data: None,
        })
    }
}

impl JsonClient for ScriptedClient {
    fn request<'a>(
        &'a self,
        method: &'a str,
        _params: Value,
    ) -> BoxFuture<'a, Result<Result<RawJson, Failure>, Error>> {
        let response = self.response(method);
        Box::pin(async move { Ok(response) })
    }

    fn request_once<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Result<RawJson, Failure>, Error>> {
        self.request(method, params)
    }

    fn batch<'a>(
        &'a self,
        requests: Vec<Call>,
    ) -> BoxFuture<'a, Result<Vec<Result<RawJson, Failure>>, Error>> {
        let responses: Vec<_> = requests
            .into_iter()
            .map(|request| self.response(&request.method))
            .collect();
        Box::pin(async move { Ok(responses) })
    }
}

fn success(method: &'static str, value: Value) -> ExpectedReply {
    ExpectedReply {
        method,
        result: Ok(
            RawJson::from_serializable(&value).expect("scripted JSON-RPC result must encode")
        ),
    }
}

fn raw_success(method: &'static str, value: Vec<u8>) -> ExpectedReply {
    ExpectedReply {
        method,
        result: Ok(RawJson::new(value).expect("scripted raw result must be valid JSON")),
    }
}

fn failure(method: &'static str, code: i64) -> ExpectedReply {
    ExpectedReply {
        method,
        result: Err(code),
    }
}

fn block(number: u64, hash: &str, parent: &str, transactions: Vec<Value>) -> Value {
    json!({
        "hash": hash,
        "parentHash": parent,
        "sha3Uncles": "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "miner": "0x0000000000000000000000000000000000000000",
        "stateRoot": "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        "transactionsRoot": "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "receiptsRoot": "0xabababababababababababababababababababababababababababababababab",
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "difficulty": "0x0",
        "number": format!("0x{number:x}"),
        "gasLimit": "0x1c9c380",
        "gasUsed": "0x5208",
        "timestamp": "0x64",
        "extraData": "0x",
        "mixHash": "0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
        "nonce": "0x0000000000000000",
        "uncles": [],
        "transactions": transactions
    })
}

fn transaction() -> Value {
    transaction_at(TX_HASH, 0)
}

fn transaction_at(hash: &str, index: u64) -> Value {
    json!({
        "hash": hash,
        "from": FROM,
        "to": TO,
        "value": "0x1",
        "transactionIndex": format!("0x{index:x}"),
        "blockHash": BLOCK_HASH,
        "blockNumber": "0xa"
    })
}

fn receipt() -> Value {
    receipt_at(TX_HASH, 0)
}

fn receipt_at(hash: &str, index: u64) -> Value {
    json!({
        "transactionHash": hash,
        "transactionIndex": format!("0x{index:x}"),
        "blockHash": BLOCK_HASH,
        "blockNumber": "0xa",
        "from": FROM,
        "to": TO,
        "contractAddress": null,
        "status": "0x1",
        "gasUsed": "0x5208",
        "effectiveGasPrice": "0x2",
        "logs": []
    })
}

fn raw_receipt_at(hash: &str, index: u64, evidence_number: &str) -> Vec<u8> {
    format!(
        r#"{{
  "logs" : [ ],
  "evidenceNumber" : {evidence_number},
  "effectiveGasPrice" : "0x2",
  "gasUsed" : "0x5208",
  "status" : "0x1",
  "contractAddress" : null,
  "to" : "{TO}",
  "from" : "{FROM}",
  "blockNumber" : "0xa",
  "blockHash" : "{BLOCK_HASH}",
  "transactionIndex" : "0x{index:x}",
  "transactionHash" : "{hash}"
}}"#,
    )
    .into_bytes()
}

fn config() -> SourceConfig {
    SourceConfig {
        scope: IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: "dev".to_owned(),
        },
        expected_chain_id: 31_337,
        expected_genesis_hash: BlockHash(
            GENESIS_HASH
                .parse::<alloy_primitives::B256>()
                .expect("test genesis hash must be valid")
                .0
                .to_vec(),
        ),
    }
}

#[test]
fn verifies_identity_and_falls_back_to_batched_receipts() {
    let full_block = block(10, BLOCK_HASH, PARENT_HASH, vec![transaction()]);
    let raw_full_block =
        serde_json::to_vec_pretty(&full_block).expect("raw block fixture must encode");
    let header = block(
        10,
        BLOCK_HASH,
        PARENT_HASH,
        vec![Value::String(TX_HASH.to_owned())],
    );
    let client = ScriptedClient::new(vec![
        success("eth_chainId", json!("0x7a69")),
        success(
            "eth_getBlockByNumber",
            block(
                0,
                GENESIS_HASH,
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                Vec::new(),
            ),
        ),
        raw_success("eth_getBlockByNumber", raw_full_block.clone()),
        failure("eth_getBlockReceipts", -32_601),
        success("eth_getTransactionReceipt", receipt()),
        success("eth_getBlockByNumber", header),
    ]);

    let source = block_on(BlockClient::connect(client.clone(), config()))
        .expect("matching chain identity must connect");
    let canonical = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
        .expect("numbered full block must load through receipt fallback")
        .pop()
        .expect("dense range contains its block");
    assert_eq!(canonical.reference.hash, BlockHash(vec![0xaa; 32]));
    assert_eq!(canonical.reference.position, BlockPosition(10));
    assert_eq!(canonical.reference.height, BlockHeight(10));
    assert_eq!(
        canonical.reference.parent,
        Some(indexing::BlockParent {
            position: BlockPosition(9),
            hash: BlockHash(vec![0xbb; 32]),
        })
    );
    assert_eq!(canonical.raw_block, raw_full_block);
    assert_eq!(canonical.raw_receipts.len(), 1);
    assert_eq!(
        block_on(source.canonical_at(BlockPosition(10)))
            .expect("canonical reference lookup must succeed")
            .map(|block| block.hash),
        Some(BlockHash(vec![0xaa; 32]))
    );
    let zero_limit = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 0))
        .expect_err("zero returned-block limit must fail before RPC");
    assert!(!zero_limit.retryable);
    let methods = client.methods();
    assert_eq!(
        methods,
        vec![
            "eth_chainId",
            "eth_getBlockByNumber",
            "eth_getBlockByNumber",
            "eth_getBlockReceipts",
            "eth_getTransactionReceipt",
            "eth_getBlockByNumber",
        ]
    );
    assert!(methods.iter().all(|method| {
        !method.contains("pending")
            && !method.contains("trace")
            && !method.contains("finalized")
            && !method.contains("safe")
    }));
}

#[test]
fn chains_mismatch_fails_closed_before_genesis_read() {
    let client = ScriptedClient::new(vec![success("eth_chainId", json!("0x1"))]);
    let error = match block_on(BlockClient::connect(client.clone(), config())) {
        Ok(_) => panic!("wrong chain ID must fail closed"),
        Err(error) => error,
    };
    assert!(!error.retryable);
    assert_eq!(client.methods(), vec!["eth_chainId"]);
}

#[test]
fn block_receipt_method_preserves_each_exact_receipt_result() {
    let raw_receipt = raw_receipt_at(TX_HASH, 0, "6.0200e+02");
    let raw_receipt_array = [b"[\n  ".as_slice(), &raw_receipt, b"\n]".as_slice()].concat();
    let client = ScriptedClient::new(vec![
        success("eth_chainId", json!("0x7a69")),
        success(
            "eth_getBlockByNumber",
            block(
                0,
                GENESIS_HASH,
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                Vec::new(),
            ),
        ),
        success(
            "eth_getBlockByNumber",
            block(10, BLOCK_HASH, PARENT_HASH, vec![transaction()]),
        ),
        raw_success("eth_getBlockReceipts", raw_receipt_array),
    ]);
    let source = block_on(BlockClient::connect(client, config()))
        .expect("matching chain identity must connect");

    let block = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
        .expect("block receipt method must retain valid receipt evidence")
        .pop()
        .expect("dense range contains its block");

    assert_eq!(block.raw_receipts, vec![raw_receipt]);
}

#[test]
fn batched_receipt_fallback_restores_transaction_order() {
    let first_receipt = raw_receipt_at(TX_HASH, 0, "4.20e+01");
    let second_receipt = raw_receipt_at(TX_HASH_2, 1, "2.00e0");
    let client = ScriptedClient::new(vec![
        success("eth_chainId", json!("0x7a69")),
        success(
            "eth_getBlockByNumber",
            block(
                0,
                GENESIS_HASH,
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                Vec::new(),
            ),
        ),
        success(
            "eth_getBlockByNumber",
            block(
                10,
                BLOCK_HASH,
                PARENT_HASH,
                vec![transaction_at(TX_HASH, 0), transaction_at(TX_HASH_2, 1)],
            ),
        ),
        failure("eth_getBlockReceipts", -32_601),
        raw_success("eth_getTransactionReceipt", first_receipt.clone()),
        raw_success("eth_getTransactionReceipt", second_receipt.clone()),
    ]);
    let source = block_on(BlockClient::connect(client, config()))
        .expect("matching chain identity must connect");

    let block = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
        .expect("out-of-order batch responses must be associated by request ID")
        .pop()
        .expect("dense range contains its block");

    assert_eq!(block.raw_receipts, vec![first_receipt, second_receipt]);
}
