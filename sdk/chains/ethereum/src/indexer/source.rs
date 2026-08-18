use std::sync::atomic::{AtomicU8, Ordering};

use indexing::{BlockHash, BlockHeight, BlockRef, BlockSource, BoxFuture, IndexScope, SourceError};
use json_rpc::{Client as JsonClient, Error, Failure, RawJson};
use serde_json::{Value, value::RawValue};

use crate::rpc::client::{CallError, Client};

use super::{
    Block,
    model::{ParsedBlock, ParsedReceipt, encode_hex, parse_quantity},
};

const RECEIPTS_UNKNOWN: u8 = 0;
const RECEIPTS_BY_BLOCK: u8 = 1;
const RECEIPTS_BY_TRANSACTION: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    pub scope: IndexScope,
    pub expected_chain_id: u64,
    pub expected_genesis_hash: BlockHash,
}

impl SourceConfig {
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.scope.chain.0 != "ethereum" {
            return Err(source_error(
                "Ethereum index source scope must use the ethereum chain ID",
                false,
            ));
        }
        if self.scope.network.trim().is_empty() {
            return Err(source_error(
                "Ethereum index source network slug must not be empty",
                false,
            ));
        }
        if self.expected_genesis_hash.0.len() != 32 {
            return Err(source_error(
                "configured Ethereum genesis hash must be 32 bytes",
                false,
            ));
        }
        Ok(())
    }
}

/// Authoritative numbered-block source over generic JSON-RPC framing.
///
/// Construction verifies `eth_chainId` and block zero before the value can be
/// used as a `BlockSource`. Receipt capability is discovered once and an
/// official method-not-found response permanently selects the batched fallback.
pub struct BlockClient<C> {
    client: Client<C>,
    config: SourceConfig,
    receipt_mode: AtomicU8,
}

impl<C> BlockClient<C>
where
    C: JsonClient,
{
    pub async fn connect(client: C, config: SourceConfig) -> Result<Self, SourceError> {
        Self::from_rpc(Client::new(client), config).await
    }

    pub async fn from_rpc(client: Client<C>, config: SourceConfig) -> Result<Self, SourceError> {
        config.validate()?;
        let source = Self {
            client,
            config,
            receipt_mode: AtomicU8::new(RECEIPTS_UNKNOWN),
        };
        source.verify_chains().await?;
        Ok(source)
    }

    #[must_use]
    pub fn config(&self) -> &SourceConfig {
        &self.config
    }

    async fn verify_chains(&self) -> Result<(), SourceError> {
        let chain_id = self
            .request_result("eth_chainId", serde_json::json!([]))
            .await?;
        let chain_id: String = chain_id.deserialize().map_err(map_json_rpc_error)?;
        let chain_id = parse_quantity(&chain_id, "chain ID")
            .map_err(|error| source_error(error.to_string(), false))?;
        let chain_id = u64::try_from(chain_id)
            .map_err(|_| source_error("Ethereum chain ID exceeds u64", false))?;
        if chain_id != self.config.expected_chain_id {
            return Err(source_error(
                "Ethereum RPC chain ID does not match configuration",
                false,
            ));
        }

        let raw_genesis = self
            .request_result("eth_getBlockByNumber", serde_json::json!(["0x0", false]))
            .await?;
        if is_json_null(&raw_genesis)? {
            return Err(source_error(
                "Ethereum RPC does not expose the genesis block",
                false,
            ));
        }
        let genesis = ParsedBlock::parse(raw_genesis.as_bytes(), Some(BlockHeight(0)), false)
            .map_err(|error| source_error(error.to_string(), false))?;
        if genesis.reference.hash != self.config.expected_genesis_hash {
            return Err(source_error(
                "Ethereum RPC genesis hash does not match configuration",
                false,
            ));
        }
        Ok(())
    }

    async fn fetch_block(
        &self,
        tag: String,
        expected_height: Option<BlockHeight>,
        full_transactions: bool,
    ) -> Result<(RawJson, super::model::ParsedBlock), SourceError> {
        let raw = self
            .request_result(
                "eth_getBlockByNumber",
                serde_json::json!([tag, full_transactions]),
            )
            .await?;
        if is_json_null(&raw)? {
            return Err(source_error(
                "Ethereum RPC does not currently expose the requested block",
                true,
            ));
        }
        let parsed = ParsedBlock::parse(raw.as_bytes(), expected_height, full_transactions)
            .map_err(|error| source_error(error.to_string(), true))?;
        Ok((raw, parsed))
    }

    async fn fetch_receipts(
        &self,
        block: &super::model::ParsedBlock,
    ) -> Result<Vec<Vec<u8>>, SourceError> {
        if block.transactions.is_empty() {
            return Ok(Vec::new());
        }

        let mode = self.receipt_mode.load(Ordering::Acquire);
        if mode != RECEIPTS_BY_TRANSACTION {
            match self.fetch_block_receipts(block).await {
                Ok(receipts) => {
                    self.receipt_mode
                        .store(RECEIPTS_BY_BLOCK, Ordering::Release);
                    return Ok(receipts);
                }
                Err(CallFailure {
                    remote_code: Some(-32_601),
                    ..
                }) => {
                    self.receipt_mode
                        .store(RECEIPTS_BY_TRANSACTION, Ordering::Release);
                }
                Err(error) => return Err(error.error),
            }
        }

        self.fetch_transaction_receipts(block).await
    }

    async fn fetch_block_receipts(
        &self,
        block: &super::model::ParsedBlock,
    ) -> Result<Vec<Vec<u8>>, CallFailure> {
        let hash = encode_hex(&block.reference.hash.0);
        let raw = self
            .request_result_detailed("eth_getBlockReceipts", serde_json::json!([hash]))
            .await?;
        let values: Vec<Box<RawValue>> = raw.deserialize().map_err(|error| CallFailure {
            remote_code: None,
            error: map_json_rpc_error(error),
        })?;
        Ok(values
            .into_iter()
            .map(|value| value.get().as_bytes().to_vec())
            .collect())
    }

    async fn fetch_transaction_receipts(
        &self,
        block: &super::model::ParsedBlock,
    ) -> Result<Vec<Vec<u8>>, SourceError> {
        let mut requests = Vec::with_capacity(block.transactions.len());
        for transaction in &block.transactions {
            let hash = encode_hex(&transaction.hash);
            requests.push(("eth_getTransactionReceipt", serde_json::json!([hash])));
        }
        self.client
            .batch(requests)
            .await?
            .into_iter()
            .map(|result| {
                let raw = match result {
                    Ok(raw) => raw,
                    Err(failure) => return Err(map_remote_failure(failure)),
                };
                if is_json_null(&raw)? {
                    return Err(source_error(
                        "Ethereum transaction receipt is temporarily unavailable",
                        true,
                    ));
                }
                Ok(raw.0)
            })
            .collect()
    }

    async fn request_result(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, SourceError> {
        self.request_result_detailed(method, params)
            .await
            .map_err(|failure| failure.error)
    }

    async fn request_result_detailed(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, CallFailure> {
        match self.client.call(method, params).await {
            Ok(result) => Ok(result),
            Err(CallError::Local(error)) => Err(CallFailure::local(error)),
            Err(CallError::Remote(failure)) => Err(CallFailure {
                remote_code: Some(failure.code),
                error: map_remote_failure(failure),
            }),
        }
    }
}

impl<C> BlockSource for BlockClient<C>
where
    C: JsonClient,
{
    type Block = Block;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move {
            let (_, block) = self.fetch_block("latest".to_owned(), None, false).await?;
            Ok(block.reference)
        })
    }

    fn block_at<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Self::Block, SourceError>> {
        Box::pin(async move {
            let tag = format!("0x{:x}", height.0);
            let (raw_block, parsed) = self.fetch_block(tag, Some(height), true).await?;
            let raw_receipts = self.fetch_receipts(&parsed).await?;
            ParsedReceipt::parse_all(&raw_receipts, &parsed)
                .map_err(|error| source_error(error.to_string(), true))?;
            Ok(Block {
                reference: parsed.reference,
                raw_block: raw_block.0,
                raw_receipts,
            })
        })
    }

    fn canonical_hash<'a>(
        &'a self,
        height: BlockHeight,
    ) -> BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
        Box::pin(async move {
            let tag = format!("0x{:x}", height.0);
            let raw = self
                .request_result("eth_getBlockByNumber", serde_json::json!([tag, false]))
                .await?;
            if is_json_null(&raw)? {
                return Ok(None);
            }
            let block = ParsedBlock::parse(raw.as_bytes(), Some(height), false)
                .map_err(|error| source_error(error.to_string(), true))?;
            Ok(Some(block.reference.hash))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub announced_height: BlockHeight,
}

/// Parses a `newHeads` notification into a wake-only hint.
///
/// No announced hash or parent is exposed, preventing callers from treating a
/// lossy subscription as canonical evidence. Every hint must trigger numbered
/// HTTP reconciliation; duplicates, gaps, and same-height replacements are all
/// equivalent wake-ups.
pub fn parse_new_heads_wake(message: &[u8]) -> Result<Head, SourceError> {
    parse_new_heads_notification(message).map(|(_, wake)| wake)
}

pub(super) fn parse_new_heads_notification(message: &[u8]) -> Result<(String, Head), SourceError> {
    let value: Value = serde_json::from_slice(message)
        .map_err(|_| source_error("Ethereum newHeads notification is not valid JSON", true))?;
    let method = value.get("method").and_then(Value::as_str);
    if method != Some("eth_subscription") {
        return Err(source_error(
            "Ethereum WebSocket message is not a subscription notification",
            true,
        ));
    }
    let params = value
        .get("params")
        .ok_or_else(|| source_error("Ethereum newHeads notification has no params", true))?;
    let subscription_id = params
        .get("subscription")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            source_error(
                "Ethereum newHeads notification has no subscription ID",
                true,
            )
        })?;
    let number = params
        .get("result")
        .and_then(|result| result.get("number"))
        .and_then(Value::as_str)
        .ok_or_else(|| source_error("Ethereum newHeads notification has no number", true))?;
    let number = parse_quantity(number, "newHeads number")
        .map_err(|error| source_error(error.to_string(), true))?;
    let announced_height = u64::try_from(number)
        .map(BlockHeight)
        .map_err(|_| source_error("Ethereum newHeads number exceeds u64", true))?;
    Ok((subscription_id.to_owned(), Head { announced_height }))
}

#[derive(Debug)]
struct CallFailure {
    remote_code: Option<i64>,
    error: SourceError,
}

impl CallFailure {
    fn local(error: SourceError) -> Self {
        Self {
            remote_code: None,
            error,
        }
    }
}

fn map_json_rpc_error(error: Error) -> SourceError {
    source_error(error.to_string(), error.is_retryable())
}

fn map_remote_failure(failure: Failure) -> SourceError {
    source_error(
        format!(
            "Ethereum JSON-RPC request failed with code {}",
            failure.code
        ),
        failure.is_server_error(),
    )
}

fn is_json_null(raw: &RawJson) -> Result<bool, SourceError> {
    raw.deserialize::<Value>()
        .map(|value| value.is_null())
        .map_err(map_json_rpc_error)
}

fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use futures_executor::block_on;
    use indexing::ChainId;
    use json_rpc::{Request, Response};
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

        fn response(&self, request: Request) -> Response {
            let mut state = self.state.lock().expect("script lock must be healthy");
            let expected = state
                .replies
                .pop_front()
                .expect("source made more requests than scripted");
            assert_eq!(request.method, expected.method);
            state.methods.push(request.method);
            Response {
                id: request.id,
                result: expected.result.map_err(|code| Failure {
                    code,
                    message: "scripted failure".to_owned(),
                    data: None,
                }),
            }
        }
    }

    impl JsonClient for ScriptedClient {
        fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, Error>> {
            let response = self.response(request);
            Box::pin(async move { Ok(response) })
        }

        fn batch<'a>(
            &'a self,
            requests: Vec<Request>,
        ) -> BoxFuture<'a, Result<Vec<Response>, Error>> {
            let mut responses: Vec<_> = requests
                .into_iter()
                .map(|request| self.response(request))
                .collect();
            responses.reverse();
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
    fn new_heads_exposes_only_a_wake_height() {
        let wake = parse_new_heads_wake(
            br#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0x1","result":{"number":"0xc","hash":"0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"}}}"#,
        )
        .expect("well-formed notification must become a wake hint");
        assert_eq!(
            wake,
            Head {
                announced_height: BlockHeight(12)
            }
        );
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
        let canonical = block_on(source.block_at(BlockHeight(10)))
            .expect("numbered full block must load through receipt fallback");
        assert_eq!(canonical.reference.hash, BlockHash(vec![0xaa; 32]));
        assert_eq!(canonical.raw_block, raw_full_block);
        assert_eq!(canonical.raw_receipts.len(), 1);
        assert_eq!(
            block_on(source.canonical_hash(BlockHeight(10)))
                .expect("canonical hash lookup must succeed"),
            Some(BlockHash(vec![0xaa; 32]))
        );
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

        let block = block_on(source.block_at(BlockHeight(10)))
            .expect("block receipt method must retain valid receipt evidence");

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

        let block = block_on(source.block_at(BlockHeight(10)))
            .expect("out-of-order batch responses must be associated by request ID");

        assert_eq!(block.raw_receipts, vec![first_receipt, second_receipt]);
    }
}
