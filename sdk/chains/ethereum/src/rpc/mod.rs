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
    use indexing::{BlockHash, BlockHeight, BlockPosition, BlockRef};
    use json_rpc::Retry;
    use serde_json::{Value, json};

    use super::transport::{Call, Client as JsonClient, Error, Failure, RawJson};
    use super::wire::{data_hex, transaction_id_hex};
    use super::*;
    use crate::{
        Address, AssetKind, ChainErrorKind, SignedTransaction, TransactionId, TransferRequest, Wei,
    };

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

        fn push_replies(&self, replies: impl IntoIterator<Item = ExpectedReply>) {
            self.state
                .lock()
                .expect("script lock must be healthy")
                .replies
                .extend(replies);
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

        fn request_once<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> crate::BoxFuture<'a, Result<Result<RawJson, Failure>, Error>> {
            self.request(method, params)
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

    fn account_rpc(client: ScriptedClient) -> AccountClient<ScriptedClient> {
        AccountClient::from_methods(rpc(client))
    }

    fn native_transfer() -> TransferRequest {
        TransferRequest::native_atomic(Address([0x11; 20]), Address([0x22; 20]), Wei::from_u128(7))
    }

    fn token_transfer() -> TransferRequest {
        TransferRequest::erc20(
            Address([0x11; 20]),
            Address([0x33; 20]),
            Address([0x22; 20]),
            Wei::from_u128(7),
        )
    }

    fn signer(seed: u8) -> base::KeyPair<Address> {
        let secret = vec![seed; 32];
        let key = crypto::SecretKey::new(secret.clone()).expect("test key must be valid");
        let public = key
            .public_key(crypto::PublicKeyFormat::Raw)
            .expect("test public key must derive");
        let hash = keccak256(&public.bytes);
        let mut bytes = [0_u8; 20];
        bytes.copy_from_slice(&hash[12..]);
        base::KeyPair::new(Address(bytes), secret).expect("test signer must construct")
    }

    fn abi_word(value: u8) -> Value {
        json!(format!("0x{}{value:02x}", "00".repeat(31)))
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
    fn reads_balances_and_nonce_with_canonical_addresses_and_exact_blocks() {
        let client = ScriptedClient::new(vec![
            success("eth_getBalance", json!("0x2a")),
            success("eth_call", json!(format!("0x{}", "00".repeat(31) + "2b"))),
            success("eth_getTransactionCount", json!("0x3")),
        ]);
        let rpc = account_rpc(client.clone());
        let block = BlockRef {
            position: BlockPosition(9),
            height: BlockHeight(9),
            hash: BlockHash(vec![0xaa; 32]),
            parent: None,
            timestamp: None,
        };

        assert_eq!(
            block_on(rpc.balance(Address([0x0a; 20]), &AssetKind::Native, None))
                .expect("native balance must parse"),
            Wei::from_u128(42)
        );
        assert_eq!(
            block_on(rpc.balance(
                Address([0x0a; 20]),
                &AssetKind::Erc20(Address([0x0b; 20])),
                Some(block),
            ))
            .expect("token balance must parse"),
            Wei::from_u128(43)
        );
        assert_eq!(
            block_on(rpc.nonce(Address([0x0a; 20]))).expect("nonce must parse"),
            3
        );

        let requests = client.requests();
        assert_eq!(requests[0].1[0], json!(format!("0x{}", "0a".repeat(20))));
        assert_eq!(requests[0].1[1], json!("pending"));
        assert_eq!(
            requests[1].1[0]["to"],
            json!(format!("0x{}", "0b".repeat(20)))
        );
        assert_eq!(
            requests[1].1[1],
            json!({
                "blockHash": format!("0x{}", "aa".repeat(32)),
                "requireCanonical": true,
            })
        );
        assert_eq!(
            requests[1].1[0]["data"],
            json!(format!("0x70a08231{}{}", "00".repeat(12), "0a".repeat(20)))
        );
        assert_eq!(
            requests[2].1,
            json!([format!("0x{}", "0a".repeat(20)), "pending"])
        );
    }

    #[test]
    fn validates_erc20_code_and_typed_probes_at_one_canonical_block() {
        let client = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success(
                "eth_getBlockByNumber",
                json!({"hash": format!("0x{}", "aa".repeat(32))}),
            ),
            success("eth_getCode", json!("0x6000")),
            success("eth_call", abi_word(6)),
            success("eth_call", abi_word(0)),
        ]);
        let rpc = account_rpc(client.clone());
        let token = Address([0x0b; 20]);

        block_on(rpc.validate_token(&token, 6)).expect("canonical ERC-20 probes must pass");

        let expected_block = json!({
            "blockHash": format!("0x{}", "aa".repeat(32)),
            "requireCanonical": true,
        });
        let requests = client.requests();
        let encoded_token = json!(format!("0x{}", "0b".repeat(20)));
        assert_eq!(requests[2].1[0], encoded_token);
        assert_eq!(requests[3].1[0]["to"], encoded_token);
        assert_eq!(requests[4].1[0]["to"], encoded_token);
        assert_eq!(requests[2].1[1], expected_block);
        assert_eq!(requests[3].1[1], expected_block);
        assert_eq!(requests[4].1[1], expected_block);
        assert_eq!(requests[3].1[0]["data"], json!("0x313ce567"));
        assert_eq!(
            requests[4].1[0]["data"],
            json!(format!("0x70a08231{}", "00".repeat(32)))
        );
    }

    #[test]
    fn token_validation_rejects_empty_code_wrong_decimals_and_malformed_balance() {
        let latest = || {
            success(
                "eth_getBlockByNumber",
                json!({"hash": format!("0x{}", "aa".repeat(32))}),
            )
        };
        let token = Address([0x33; 20]);

        let empty_code = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            latest(),
            success("eth_getCode", json!("0x")),
        ]);
        assert!(
            block_on(account_rpc(empty_code).validate_token(&token, 6))
                .expect_err("an EOA token address must fail")
                .message
                .contains("no deployed code")
        );

        let wrong_decimals = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            latest(),
            success("eth_getCode", json!("0x6000")),
            success("eth_call", abi_word(18)),
        ]);
        assert!(
            block_on(account_rpc(wrong_decimals).validate_token(&token, 6))
                .expect_err("configured decimals mismatch must fail")
                .message
                .contains("do not match")
        );

        let malformed_balance = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            latest(),
            success("eth_getCode", json!("0x6000")),
            success("eth_call", abi_word(6)),
            success("eth_call", json!("0x01")),
        ]);
        assert!(
            block_on(account_rpc(malformed_balance).validate_token(&token, 6))
                .expect_err("short balanceOf result must fail")
                .message
                .contains("invalid length")
        );
    }

    #[test]
    fn builds_checked_eip1559_context_for_native_transfer() {
        let client = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            success(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
            success("eth_getBalance", json!("0xde0b6b3a7640000")),
        ]);
        let rpc = rpc(client.clone());

        let request = TransferRequest::native_atomic(
            Address([0x0a; 20]),
            Address([0x0b; 20]),
            Wei::from_u128(7),
        );
        let context =
            block_on(rpc.build_context(&request, 4)).expect("bounded build context must succeed");

        assert_eq!(context.chain_id, 31_337);
        assert_eq!(context.nonce, 4);
        assert_eq!(context.gas_limit, 25_200);
        assert_eq!(
            context.max_priority_fee_per_gas,
            Wei::from_u128(1_000_000_000)
        );
        assert_eq!(context.max_fee_per_gas, Wei::from_u128(5_000_000_000));
        let requests = client.requests();
        assert_eq!(
            requests[1].1[0]["from"],
            json!(format!("0x{}", "0a".repeat(20)))
        );
        assert_eq!(
            requests[1].1[0]["to"],
            json!(format!("0x{}", "0b".repeat(20)))
        );
        assert_eq!(requests[1].1[0]["data"], json!("0x"));
        assert_eq!(requests[1].1[0]["value"], json!("0x7"));
    }

    #[test]
    fn simulates_typed_erc20_transfer_and_checks_both_balances() {
        let client = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_call", abi_word(8)),
            success("eth_call", abi_word(1)),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            success(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
            success("eth_getBalance", json!("0xde0b6b3a7640000")),
        ]);
        let rpc = rpc(client.clone());

        block_on(rpc.build_context(&token_transfer(), 4))
            .expect("simulated funded ERC-20 transfer must build");

        let requests = client.requests();
        let expected_data = format!(
            "0xa9059cbb{}{}{}07",
            "00".repeat(12),
            "22".repeat(20),
            "00".repeat(31)
        );
        assert_eq!(
            requests[2].1[0]["to"],
            json!(Address([0x33; 20]).to_string())
        );
        assert_eq!(requests[2].1[0]["value"], json!("0x0"));
        assert_eq!(requests[2].1[0]["data"], json!(expected_data));
        assert_eq!(requests[2].1[1], json!("pending"));
        assert_eq!(requests[3].1[0], requests[2].1[0]);
        assert_eq!(
            requests[1].1[0]["data"],
            json!(format!("0x70a08231{}{}", "00".repeat(12), "11".repeat(20)))
        );
    }

    #[test]
    fn erc20_simulation_rejects_false_empty_and_malformed_returns() {
        for (result, expected, kind) in [
            (abi_word(0), "returned false", ChainErrorKind::Rejected),
            (json!("0x"), "invalid length", ChainErrorKind::Rejected),
            (abi_word(2), "invalid ABI result", ChainErrorKind::Rejected),
        ] {
            let client = ScriptedClient::new(vec![
                success("eth_chainId", json!("0x7a69")),
                success("eth_call", abi_word(8)),
                success("eth_call", result),
            ]);

            let error = block_on(rpc(client.clone()).build_context(&token_transfer(), 0))
                .expect_err("non-canonical ERC-20 transfer success must fail closed");

            assert_eq!(error.kind, kind);
            assert!(error.message.contains(expected), "{}", error.message);
            assert_eq!(client.requests().len(), 3);
        }
    }

    #[test]
    fn erc20_simulation_remote_revert_is_rejected_without_provider_details() {
        let client = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_call", abi_word(8)),
            failure(
                "eth_call",
                -32_000,
                "execution reverted: Bearer secret https://user:password@example.invalid",
            ),
        ]);

        let error = block_on(rpc(client).build_context(&token_transfer(), 0))
            .expect_err("a deterministic ERC-20 simulation revert must fail");

        assert_eq!(error.kind, ChainErrorKind::Rejected);
        assert!(error.message.contains("-32000"));
        for secret in ["Bearer secret", "password", "example.invalid"] {
            assert!(!error.message.contains(secret));
        }
    }

    #[test]
    fn erc20_preflight_rejects_insufficient_native_gas_and_token_balance() {
        let no_gas = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_call", abi_word(8)),
            success("eth_call", abi_word(1)),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            success(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
            success("eth_getBalance", json!("0x0")),
        ]);
        let error = block_on(rpc(no_gas).build_context(&token_transfer(), 0))
            .expect_err("token transfer without native gas must fail");
        assert_eq!(error.kind, ChainErrorKind::InsufficientFunds);
        assert!(error.message.contains("ERC-20 maximum fee"));

        let no_tokens = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_call", abi_word(0)),
        ]);
        let error = block_on(rpc(no_tokens.clone()).build_context(&token_transfer(), 0))
            .expect_err("token transfer above balance must fail");
        assert_eq!(error.kind, ChainErrorKind::InsufficientFunds);
        assert!(error.message.contains("ERC-20 balance is insufficient"));
        assert_eq!(no_tokens.requests().len(), 2);
    }

    #[test]
    fn native_preflight_checks_value_plus_worst_case_fee() {
        let client = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            success(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
            success("eth_getBalance", json!("0x7")),
        ]);

        let error = block_on(rpc(client).build_context(&native_transfer(), 0))
            .expect_err("native value without fee balance must fail");

        assert_eq!(error.kind, ChainErrorKind::InsufficientFunds);
        assert!(error.message.contains("value and maximum fee"));
    }

    #[test]
    fn wrong_chain_id_fails_before_transaction_queries() {
        let client = ScriptedClient::new(vec![success("eth_chainId", json!("0x1"))]);
        let rpc = rpc(client.clone());

        let error = block_on(rpc.build_context(&native_transfer(), 9))
            .expect_err("wrong chain identity must fail closed");

        assert_eq!(error.kind, ChainErrorKind::Divergent);
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
            failure(
                "eth_estimateGas",
                -32_000,
                "execution reverted: Bearer secret https://user:password@example.invalid",
            ),
        ]);
        let rpc = rpc(client);

        let error = block_on(rpc.build_context(&native_transfer(), 0))
            .expect_err("a deterministic revert must fail");

        assert_eq!(error.kind, ChainErrorKind::Rejected);
        assert!(error.message.contains("-32000"));
        for secret in ["Bearer secret", "password", "example.invalid"] {
            assert!(!error.message.contains(secret));
        }
    }

    #[test]
    fn exact_envelope_broadcast_rejects_a_mismatched_provider_hash() {
        let envelope = vec![0x02, 0x01, 0x02, 0x03];
        let id = TransactionId(keccak256(&envelope).0);
        let local_id = base::TransactionId::new(id.to_string());
        let provider_candidate = format!("0x{}", "dd".repeat(32));
        let client = ScriptedClient::new(vec![success(
            "eth_sendRawTransaction",
            json!(provider_candidate.clone()),
        )]);
        let rpc = rpc(client.clone());

        let error = block_on(rpc.broadcast(SignedTransaction {
            id,
            envelope: envelope.clone(),
        }))
        .expect_err("provider hash mismatch must fail");

        assert_eq!(error.kind, base::TransactionErrorKind::Unavailable);
        assert_eq!(error.ambiguous_transaction_id, Some(local_id));
        assert_ne!(
            error
                .ambiguous_transaction_id
                .as_ref()
                .map(base::TransactionId::as_str),
            Some(provider_candidate.as_str())
        );
        assert_eq!(client.requests()[0].1, json!([data_hex(&envelope)]));
    }

    #[test]
    fn mismatched_local_id_is_rejected_before_submission_without_ambiguity() {
        let envelope = vec![0x02, 0x10, 0x20, 0x30];
        let client = ScriptedClient::new(Vec::new());
        let rpc = rpc(client.clone());

        let error = block_on(rpc.broadcast(SignedTransaction {
            id: TransactionId([0x99; 32]),
            envelope,
        }))
        .expect_err("a local ID that does not match the envelope must fail before RPC");

        assert_eq!(error.kind, base::TransactionErrorKind::InvalidTransaction);
        assert_eq!(error.ambiguous_transaction_id, None);
        assert!(client.requests().is_empty());
    }

    #[test]
    fn unknown_remote_submission_failure_remains_ambiguous() {
        let envelope = vec![0x02, 0x04, 0x05, 0x06];
        let id = TransactionId(keccak256(&envelope).0);
        let local_id = base::TransactionId::new(id.to_string());
        let client = ScriptedClient::new(vec![failure(
            "eth_sendRawTransaction",
            -32_000,
            "backend failed after upstream acceptance: Bearer secret",
        )]);

        let error = block_on(rpc(client).broadcast(SignedTransaction { id, envelope }))
            .expect_err("an unclassified post-attempt remote failure must stay ambiguous");

        assert_eq!(error.kind, base::TransactionErrorKind::Unavailable);
        assert_eq!(error.ambiguous_transaction_id, Some(local_id));
        assert_eq!(
            error.message,
            "Ethereum submission outcome is ambiguous: Ethereum JSON-RPC eth_sendRawTransaction failed with code -32000"
        );
        assert!(!error.message.contains("Bearer secret"));
    }

    #[test]
    fn missing_and_malformed_submission_results_keep_the_exact_local_id() {
        for result in [Value::Null, json!(7)] {
            let envelope = vec![0x02, 0x07, 0x08, 0x09];
            let id = TransactionId(keccak256(&envelope).0);
            let local_id = base::TransactionId::new(id.to_string());
            let client = ScriptedClient::new(vec![success("eth_sendRawTransaction", result)]);

            let error = block_on(rpc(client).broadcast(SignedTransaction { id, envelope }))
                .expect_err("a missing or malformed result must remain ambiguous");

            assert_eq!(error.kind, base::TransactionErrorKind::Unavailable);
            assert_eq!(error.ambiguous_transaction_id, Some(local_id));
        }
    }

    #[tokio::test]
    async fn coordinator_retains_unknown_remote_failure_for_exact_replay() {
        let signer = signer(10);
        let request = TransferRequest::native_atomic(
            signer.address.clone(),
            Address([0x44; 20]),
            Wei::from_u128(7),
        );
        let client = ScriptedClient::new(vec![
            success("eth_getTransactionCount", json!("0x5")),
            success("eth_chainId", json!("0x7a69")),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x1")),
            success("eth_getBlockByNumber", json!({"baseFeePerGas": "0x1"})),
            success("eth_getBalance", json!("0xde0b6b3a7640000")),
            success("eth_getBalance", json!("0xde0b6b3a7640000")),
            failure(
                "eth_sendRawTransaction",
                -32_000,
                "backend failed after upstream acceptance",
            ),
        ]);
        let methods = Arc::new(rpc(client.clone()));
        let coordinator = crate::TransactionCoordinator::new(methods.clone(), methods);
        let signed = coordinator
            .prepare_one(crate::transaction::Preparation::signer(
                request, 31_337, &signer,
            ))
            .await
            .expect("transaction must prepare");

        let error = coordinator
            .broadcast(signed.clone())
            .await
            .expect_err("unclassified remote failure must be ambiguous");
        assert_eq!(
            error.ambiguous_transaction_id,
            Some(base::TransactionId::new(signed.id.to_string()))
        );
        client.push_replies([
            success("eth_getTransactionByHash", Value::Null),
            success(
                "eth_sendRawTransaction",
                json!(transaction_id_hex(&signed.id)),
            ),
        ]);

        assert_eq!(
            coordinator
                .broadcast(signed.clone())
                .await
                .expect("reserved exact envelope must replay"),
            signed.id
        );
        let submissions = client
            .requests()
            .into_iter()
            .filter(|(method, _)| method == "eth_sendRawTransaction")
            .map(|(_, params)| params)
            .collect::<Vec<_>>();
        assert_eq!(
            submissions,
            [
                json!([data_hex(&signed.envelope)]),
                json!([data_hex(&signed.envelope)])
            ]
        );
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
        let local_id = base::TransactionId::new(id.to_string());
        let error = block_on(mismatched_rpc.broadcast(SignedTransaction { id, envelope }))
            .expect_err("different known hash must not be accepted");
        assert_eq!(error.kind, base::TransactionErrorKind::Unavailable);
        assert_eq!(error.ambiguous_transaction_id, Some(local_id));
    }

    #[test]
    fn known_requires_the_exact_transaction_hash() {
        let id = TransactionId([0xaa; 32]);
        let absent = ScriptedClient::new(vec![success("eth_getTransactionByHash", Value::Null)]);
        assert!(!block_on(rpc(absent).known(&id)).expect("null lookup must mean not known"));

        let matching = ScriptedClient::new(vec![success(
            "eth_getTransactionByHash",
            json!({"hash": transaction_id_hex(&id)}),
        )]);
        assert!(block_on(rpc(matching).known(&id)).expect("matching hash must be known"));

        let mismatched = ScriptedClient::new(vec![success(
            "eth_getTransactionByHash",
            json!({"hash": format!("0x{}", "bb".repeat(32))}),
        )]);
        let error = block_on(rpc(mismatched).known(&id))
            .expect_err("mismatched transaction object must fail closed");
        assert!(!error.retryable);
        assert!(error.message.contains("does not match"));
    }

    #[test]
    fn configured_input_and_fee_ceilings_fail_closed() {
        let no_calls = ScriptedClient::new(Vec::new());
        let small_input = Limits::new(
            32,
            2_000,
            1_000_000,
            Wei::from_u128(1_000_000_000_000),
            Wei::from_u128(100_000_000_000),
            Wei::from_u128(1_000_000_000_000_000_000),
        )
        .expect("test RPC limits must be valid");
        let bounded_rpc = Methods::with_client(no_calls.clone(), 31_337, small_input)
            .expect("test RPC configuration must be valid");
        let error = block_on(bounded_rpc.build_context(&token_transfer(), 0))
            .expect_err("oversized input must fail before RPC");
        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
        assert!(no_calls.requests().is_empty());

        let high_fee = ScriptedClient::new(vec![
            success("eth_chainId", json!("0x7a69")),
            success("eth_estimateGas", json!("0x5208")),
            success("eth_maxPriorityFeePerGas", json!("0x1")),
            success(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x100000000000"}),
            ),
        ]);
        let error = block_on(rpc(high_fee).build_context(&native_transfer(), 0))
            .expect_err("fee above the configured ceiling must fail");
        assert_eq!(error.kind, ChainErrorKind::FeeUnavailable);
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
