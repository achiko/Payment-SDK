mod accounts;
mod blocks;
pub(crate) mod client;
mod config;
mod error;
mod transactions;
mod transport;
mod wire;

pub use accounts::{AccountClient, Accounts};
pub use client::Client;
pub use config::{HttpConfig, Limits};
pub use error::{BuildError, BuildErrorKind};
pub use transactions::{HttpAccounts, HttpTransactions, TransactionClient, Transactions};

use blocks::Methods;

const BASIS_POINTS_DENOMINATOR: u64 = 10_000;
const ERC20_BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];

impl HttpConfig {
    /// Builds focused account and transaction adapters over shared endpoints.
    pub fn connect(self) -> Result<(HttpAccounts, HttpTransactions), BuildError> {
        let methods = Methods::http(self)?;
        Ok((
            AccountClient::from_methods(methods.clone()),
            TransactionClient::from_methods(methods),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        num::NonZeroU32,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_primitives::keccak256;
    use futures_executor::block_on;
    use indexing::{BlockHash, BlockHeight, BlockRef};
    use json_rpc::Retry;
    use serde_json::{Value, json};

    use super::transport::{Call, Client as JsonClient, Error, Failure, RawJson};
    use super::wire::{data_hex, transaction_id_hex};
    use super::*;
    use crate::{Address, AssetKind, SignedTransaction, TransactionId, TransferRequest, Wei};

    #[derive(Clone)]
    struct ScriptedClient {
        state: Arc<Mutex<ScriptState>>,
    }

    struct ScriptState {
        replies: VecDeque<ExpectedReply>,
        requests: Vec<(String, Value)>,
    }

    struct ExpectedReply {
        method: &'static str,
        result: Result<RawJson, Failure>,
    }

    impl ScriptedClient {
        fn new(replies: Vec<ExpectedReply>) -> Self {
            Self {
                state: Arc::new(Mutex::new(ScriptState {
                    replies: replies.into(),
                    requests: Vec::new(),
                })),
            }
        }

        fn requests(&self) -> Vec<(String, Value)> {
            self.state
                .lock()
                .expect("script lock must be healthy")
                .requests
                .clone()
        }
    }

    impl JsonClient for ScriptedClient {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> crate::BoxFuture<'a, Result<Result<RawJson, Failure>, Error>> {
            let response = {
                let mut state = self.state.lock().expect("script lock must be healthy");
                let expected = state
                    .replies
                    .pop_front()
                    .expect("adapter made more requests than scripted");
                assert_eq!(method, expected.method);
                state.requests.push((method.to_owned(), params));
                expected.result
            };
            Box::pin(async move { Ok(response) })
        }

        fn batch<'a>(
            &'a self,
            _requests: Vec<Call>,
        ) -> crate::BoxFuture<'a, Result<Vec<Result<RawJson, Failure>>, Error>> {
            Box::pin(async { panic!("wallet RPC adapter does not issue JSON-RPC batches") })
        }
    }

    fn success(method: &'static str, value: Value) -> ExpectedReply {
        ExpectedReply {
            method,
            result: Ok(
                RawJson::from_serializable(&value).expect("scripted RPC result must serialize")
            ),
        }
    }

    fn failure(method: &'static str, code: i64, message: &str) -> ExpectedReply {
        ExpectedReply {
            method,
            result: Err(Failure {
                code,
                message: message.to_owned(),
                data: None,
            }),
        }
    }

    fn limits() -> Limits {
        Limits::new(
            1024,
            2_000,
            1_000_000,
            Wei::from_u128(1_000_000_000_000),
            Wei::from_u128(100_000_000_000),
            Wei::from_u128(1_000_000_000_000_000_000),
        )
        .expect("test RPC limits must be valid")
    }

    fn rpc(client: ScriptedClient) -> Methods<ScriptedClient> {
        Methods::with_client(client, 31_337, limits())
            .expect("test RPC configuration must be valid")
    }

    fn transfer(data: Vec<u8>) -> TransferRequest {
        TransferRequest {
            from: Address([0x11; 20]),
            to: Some(Address([0x22; 20])),
            value: Wei::from_u128(7),
            data,
        }
    }

    #[test]
    fn focused_adapters_reuse_the_same_rpc() {
        let client = ScriptedClient::new(vec![success("eth_getBalance", json!("0x2a"))]);
        let shared = Client::new(client.clone());
        let accounts = AccountClient::new(shared.clone(), 31_337)
            .expect("account adapter configuration must be valid");
        let _transactions = TransactionClient::new(shared, 31_337, limits())
            .expect("transaction adapter configuration must be valid");

        assert_eq!(
            block_on(accounts.balance(Address([0x11; 20]), &AssetKind::Native, None))
                .expect("focused account call must succeed"),
            Wei::from_u128(42)
        );
        assert_eq!(
            client
                .requests()
                .into_iter()
                .map(|(method, _)| method)
                .collect::<Vec<_>>(),
            ["eth_getBalance"]
        );
    }

    #[test]
    fn reads_native_and_erc20_balances_with_exact_block_behavior() {
        let client = ScriptedClient::new(vec![
            success("eth_getBalance", json!("0x2a")),
            success("eth_call", json!(format!("0x{}", "00".repeat(31) + "2b"))),
        ]);
        let rpc = rpc(client.clone());
        let block = BlockRef {
            height: BlockHeight(9),
            hash: BlockHash(vec![0xaa; 32]),
            parent_hash: None,
            timestamp: None,
        };

        assert_eq!(
            block_on(rpc.balance(Address([0x11; 20]), &AssetKind::Native, None))
                .expect("native balance must parse"),
            Wei::from_u128(42)
        );
        assert_eq!(
            block_on(rpc.balance(
                Address([0x11; 20]),
                &AssetKind::Erc20(Address([0x33; 20])),
                Some(block),
            ))
            .expect("token balance must parse"),
            Wei::from_u128(43)
        );

        let requests = client.requests();
        assert_eq!(requests[0].1[1], json!("pending"));
        assert_eq!(
            requests[1].1[1],
            json!({
                "blockHash": format!("0x{}", "aa".repeat(32)),
                "requireCanonical": true,
            })
        );
        assert_eq!(
            requests[1].1[0]["data"],
            json!(format!("0x70a08231{}{}", "00".repeat(12), "11".repeat(20)))
        );
    }

    #[test]
    fn builds_checked_eip1559_context_and_preserves_input() {
        let client = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_getTransactionCount", json!("0x4")),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            success(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
        ]);
        let rpc = rpc(client.clone());

        let context = block_on(rpc.build_context(&transfer(vec![0xde, 0xad])))
            .expect("bounded build context must succeed");

        assert_eq!(context.chain_id, 31_337);
        assert_eq!(context.nonce, 4);
        assert_eq!(context.gas_limit, 25_200);
        assert_eq!(
            context.max_priority_fee_per_gas,
            Wei::from_u128(1_000_000_000)
        );
        assert_eq!(context.max_fee_per_gas, Wei::from_u128(5_000_000_000));
        let requests = client.requests();
        assert_eq!(requests[2].1[0]["data"], json!("0xdead"));
        assert_eq!(requests[2].1[0]["value"], json!("0x7"));
    }

    #[test]
    fn wrong_chain_id_fails_before_transaction_queries() {
        let client = ScriptedClient::new(vec![success("eth_chainId", json!("0x1"))]);
        let rpc = rpc(client.clone());

        let error = block_on(rpc.build_context(&transfer(Vec::new())))
            .expect_err("wrong chain identity must fail closed");

        assert!(!error.retryable);
        assert_eq!(client.requests().len(), 1);
    }

    #[test]
    fn malformed_quantities_and_abi_results_are_rejected() {
        let client = ScriptedClient::new(vec![
            success("eth_getBalance", json!("0x00")),
            success("eth_call", json!("0x01")),
        ]);
        let rpc = rpc(client);

        assert!(
            block_on(rpc.balance(Address([1; 20]), &AssetKind::Native, None))
                .expect_err("non-canonical quantity must fail")
                .message
                .contains("leading zero")
        );
        assert!(
            block_on(rpc.balance(Address([1; 20]), &AssetKind::Erc20(Address([2; 20])), None,))
                .expect_err("short ABI word must fail")
                .message
                .contains("invalid length")
        );
    }

    #[test]
    fn estimate_gas_revert_is_terminal_and_provider_message_is_not_exposed() {
        let client = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_getTransactionCount", json!("0x0")),
            failure(
                "eth_estimateGas",
                -32_000,
                "execution reverted: Bearer secret https://user:password@example.invalid",
            ),
        ]);
        let rpc = rpc(client);

        let error = block_on(rpc.build_context(&transfer(Vec::new())))
            .expect_err("a deterministic revert must fail");

        assert!(!error.retryable);
        assert!(error.message.contains("-32000"));
        for secret in ["Bearer secret", "password", "example.invalid"] {
            assert!(!error.message.contains(secret));
        }
    }

    #[test]
    fn exact_envelope_broadcast_rejects_a_mismatched_provider_hash() {
        let envelope = vec![0x02, 0x01, 0x02, 0x03];
        let id = TransactionId(keccak256(&envelope).0);
        let client = ScriptedClient::new(vec![success(
            "eth_sendRawTransaction",
            json!(format!("0x{}", "dd".repeat(32))),
        )]);
        let rpc = rpc(client.clone());

        let error = block_on(rpc.broadcast(SignedTransaction {
            id,
            envelope: envelope.clone(),
        }))
        .expect_err("provider hash mismatch must fail");

        assert!(!error.retryable);
        assert_eq!(client.requests()[0].1, json!([data_hex(&envelope)]));
    }

    #[test]
    fn already_known_succeeds_only_after_matching_hash_lookup() {
        let envelope = vec![0x02, 0xaa, 0xbb];
        let id = TransactionId(keccak256(&envelope).0);
        let matching = ScriptedClient::new(vec![
            failure("eth_sendRawTransaction", -32_000, "already known"),
            success(
                "eth_getTransactionByHash",
                json!({"hash": transaction_id_hex(&id)}),
            ),
        ]);
        let matching_rpc = rpc(matching);

        assert_eq!(
            block_on(matching_rpc.broadcast(SignedTransaction {
                id: id.clone(),
                envelope: envelope.clone(),
            }))
            .expect("matching already-known transaction must be idempotent"),
            id
        );

        let mismatched = ScriptedClient::new(vec![
            failure("eth_sendRawTransaction", -32_000, "already known"),
            success(
                "eth_getTransactionByHash",
                json!({"hash": format!("0x{}", "ee".repeat(32))}),
            ),
        ]);
        let mismatched_rpc = rpc(mismatched);
        let error = block_on(mismatched_rpc.broadcast(SignedTransaction { id, envelope }))
            .expect_err("different known hash must not be accepted");
        assert!(!error.retryable);
    }

    #[test]
    fn configured_input_and_fee_ceilings_fail_closed() {
        let no_calls = ScriptedClient::new(Vec::new());
        let bounded_rpc = rpc(no_calls.clone());
        let error = block_on(bounded_rpc.build_context(&transfer(vec![0; 1025])))
            .expect_err("oversized input must fail before RPC");
        assert!(!error.retryable);
        assert!(no_calls.requests().is_empty());

        let high_fee = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_getTransactionCount", json!("0x0")),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x1")),
            success(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x100000000000"}),
            ),
        ]);
        let error = block_on(rpc(high_fee).build_context(&transfer(Vec::new())))
            .expect_err("fee above the configured ceiling must fail");
        assert!(!error.retryable);
        assert!(error.message.contains("configured ceiling"));
    }

    #[test]
    fn debug_and_build_errors_redact_endpoint_and_authorization() {
        let retry = Retry::new(
            NonZeroU32::new(2).expect("two is non-zero"),
            Duration::from_millis(1),
            Duration::from_millis(2),
        )
        .expect("test retry policy must be valid");
        let config = HttpConfig::new(
            "https://user:password@example.invalid/rpc?key=query-secret",
            31_337,
            Duration::from_secs(1),
            1024,
            retry,
            limits(),
        )
        .expect("test HTTP RPC configuration must be valid")
        .with_header("authorization", "Bearer header-secret");
        let config_debug = format!("{config:?}");
        let rpc = Methods::http(config).expect("test HTTP adapter must construct");
        let rpc_debug = format!("{rpc:?}");

        for output in [config_debug, rpc_debug] {
            for secret in ["password", "query-secret", "header-secret"] {
                assert!(!output.contains(secret));
            }
            assert!(output.contains("[REDACTED]"));
        }

        let invalid = HttpConfig::new(
            "https://user:invalid-secret@[",
            31_337,
            Duration::from_secs(1),
            1024,
            Retry::no_retry(),
            limits(),
        )
        .expect("syntax validation is delegated to the HTTP transport");
        let error = Methods::http(invalid).expect_err("invalid endpoint must fail");
        assert_eq!(error.kind, BuildErrorKind::HttpTransport);
        assert!(!format!("{error:?}").contains("invalid-secret"));
        assert!(!error.to_string().contains("invalid-secret"));
    }
}
