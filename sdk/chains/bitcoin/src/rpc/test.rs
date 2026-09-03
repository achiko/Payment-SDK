use super::*;
use super::{transport::Client as Transport, wire::fee_rate_json};
use crate::{FeeRate, Network, Satoshi, SignedTransaction, TransactionId};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute,
    consensus, hashes::Hash, transaction::Version,
};
use futures_executor::block_on;
use json_rpc::{Call, RawJson};
use serde_json::{Number, Value};
use std::str::FromStr;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use super::transport::{Error, Failure};
use crate::BoxFuture;

#[derive(Clone)]
struct ScriptedClient {
    replies: Arc<Mutex<VecDeque<ExpectedReply>>>,
}

struct ExpectedReply {
    method: &'static str,
    result: ExpectedResult,
}

enum ExpectedResult {
    Response(Result<Value, i64>),
    Transport(Error),
}

impl ScriptedClient {
    fn new(replies: Vec<ExpectedReply>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
        }
    }

    fn respond(&self, method: &str) -> Result<Result<RawJson, Failure>, Error> {
        let expected = self
            .replies
            .lock()
            .expect("script lock must be healthy")
            .pop_front()
            .expect("Core client made more calls than scripted");
        assert_eq!(method, expected.method);
        match expected.result {
            ExpectedResult::Response(result) => Ok(result
                .map(|value| RawJson::from_serializable(&value).expect("reply JSON must encode"))
                .map_err(|code| Failure {
                    code,
                    message: "scripted failure".to_owned(),
                    data: None,
                })),
            ExpectedResult::Transport(error) => Err(error),
        }
    }
}

impl Transport for ScriptedClient {
    fn request<'a>(
        &'a self,
        method: &'a str,
        _params: Value,
    ) -> BoxFuture<'a, Result<Result<RawJson, Failure>, Error>> {
        assert_ne!(
            method, "sendrawtransaction",
            "Bitcoin submission must use one transport execution"
        );
        let result = self.respond(method);
        Box::pin(async move { result })
    }

    fn request_once<'a>(
        &'a self,
        method: &'a str,
        _params: Value,
    ) -> BoxFuture<'a, Result<Result<RawJson, Failure>, Error>> {
        let result = self.respond(method);
        Box::pin(async move { result })
    }

    fn batch<'a>(
        &'a self,
        _requests: Vec<Call>,
    ) -> BoxFuture<'a, Result<Vec<Result<RawJson, Failure>>, Error>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
}

fn success(method: &'static str, result: Value) -> ExpectedReply {
    ExpectedReply {
        method,
        result: ExpectedResult::Response(Ok(result)),
    }
}

fn remote_failure(method: &'static str, code: i64) -> ExpectedReply {
    ExpectedReply {
        method,
        result: ExpectedResult::Response(Err(code)),
    }
}

fn transport_failure(method: &'static str, kind: json_rpc::ErrorKind) -> ExpectedReply {
    ExpectedReply {
        method,
        result: ExpectedResult::Transport(Error {
            kind,
            message: "scripted transport failure".to_owned(),
        }),
    }
}

fn readiness_replies() -> Vec<ExpectedReply> {
    vec![
        success("getnetworkinfo", serde_json::json!({"version": 310000})),
        success(
            "getblockchaininfo",
            serde_json::json!({
                "chain": "regtest",
                "blocks": 10,
                "headers": 10,
                "bestblockhash": format!("{:064x}", 2),
                "initialblockdownload": false,
                "pruned": false
            }),
        ),
        success(
            "getindexinfo",
            serde_json::json!({"txindex": {"synced": true, "best_block_height": 10}}),
        ),
        success("getblockhash", Value::String(format!("{:064x}", 1))),
    ]
}

fn config() -> CoreConfig {
    CoreConfig {
        expected_network: Network::Regtest,
        expected_genesis_hash: parse_bitcoin_block_hash(&format!("{:064x}", 1))
            .expect("test genesis hash must parse"),
    }
}

fn signed_transaction() -> SignedTransaction {
    let transaction = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([3; 32]), 0),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: ScriptBuf::new(),
        }],
    };
    let id = TransactionId::from(transaction.compute_txid());
    SignedTransaction::from_consensus_bytes(id, consensus::serialize(&transaction))
        .expect("test transaction must be internally consistent")
}

#[test]
fn connect_validates_core_31_identity_and_readiness() {
    let core = block_on(Client::connect(
        ScriptedClient::new(readiness_replies()),
        config(),
    ))
    .expect("valid scripted Core node must connect");

    assert_eq!(core.config(), &config());
}

#[test]
fn connect_rejects_pruned_node() {
    let replies = vec![
        success("getnetworkinfo", serde_json::json!({"version": 310000})),
        success(
            "getblockchaininfo",
            serde_json::json!({
                "chain": "regtest",
                "blocks": 10,
                "headers": 10,
                "bestblockhash": format!("{:064x}", 2),
                "initialblockdownload": false,
                "pruned": true
            }),
        ),
    ];
    let error = block_on(Client::connect(ScriptedClient::new(replies), config()))
        .err()
        .expect("pruned Core node must fail");

    assert!(!error.retryable);
    assert!(error.message.contains("unpruned"));
}

#[test]
fn core_warmup_failure_is_retryable() {
    let error = block_on(Client::connect(
        ScriptedClient::new(vec![remote_failure("getnetworkinfo", -28)]),
        config(),
    ))
    .err()
    .expect("Core warmup must not connect");

    assert!(error.retryable);
}

#[test]
fn fee_estimate_converts_exact_btc_per_kvb_without_float() {
    let mut replies = readiness_replies();
    replies.push(success(
        "estimatesmartfee",
        serde_json::json!({"feerate": 0.00001001, "blocks": 6}),
    ));
    let core = block_on(Client::connect(ScriptedClient::new(replies), config()))
        .expect("valid scripted Core node must connect");

    let rate = block_on(core.fees().estimate(6)).expect("fee estimate must parse");

    assert_eq!(rate.satoshis_per_kvb(), 1_001);
}

#[test]
fn preflight_preserves_rejection_reason_and_exact_fee() {
    let signed = signed_transaction();
    let base_fee = Number::from_str("0.00000123").expect("test fixed-point fee must encode");
    let mut replies = readiness_replies();
    replies.push(success(
        "testmempoolaccept",
        serde_json::json!([{
            "txid": signed.id().to_string(),
            "allowed": false,
            "reject-reason": "missing-inputs",
            "vsize": 82,
            "fees": {"base": base_fee}
        }]),
    ));
    let core = block_on(Client::connect(ScriptedClient::new(replies), config()))
        .expect("valid scripted Core node must connect");

    let result = block_on(core.transactions().preflight(&signed, FeeRate::new(10_000)))
        .expect("preflight result must parse");

    assert!(!result.allowed);
    assert_eq!(result.reject_reason.as_deref(), Some("missing-inputs"));
    assert_eq!(result.base_fee, Some(Satoshi(123)));
}

#[test]
fn core_max_fee_rate_boundary_is_enforced_before_rpc() {
    assert!(fee_rate_json(FeeRate::new(BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB,)).is_ok());
    let error = fee_rate_json(FeeRate::new(BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB + 1))
        .expect_err("fee rates above Core's limit must fail locally");
    assert!(!error.retryable);
}

#[test]
fn broadcast_rejects_a_mismatched_returned_txid() {
    let signed = signed_transaction();
    let local_id = signed.id();
    let mut replies = readiness_replies();
    replies.push(success(
        "sendrawtransaction",
        Value::String(format!("{:064x}", 9)),
    ));
    let core = block_on(Client::connect(ScriptedClient::new(replies), config()))
        .expect("valid scripted Core node must connect");

    let error = block_on(core.transactions().broadcast(signed, FeeRate::new(10_000)))
        .expect_err("mismatched broadcast ID must fail");

    assert_eq!(error.kind, base::TransactionErrorKind::Unavailable);
    assert!(error.message.contains("different transaction ID"));
    assert_eq!(
        error.ambiguous_transaction_id,
        Some(base::TransactionId::new(local_id.to_string()))
    );
}

#[test]
fn broadcast_remote_rejection_is_definite_and_has_no_ambiguity() {
    let signed = signed_transaction();
    let mut replies = readiness_replies();
    replies.push(remote_failure("sendrawtransaction", -26));
    let core = block_on(Client::connect(ScriptedClient::new(replies), config()))
        .expect("valid scripted Core node must connect");

    let error = block_on(core.transactions().broadcast(signed, FeeRate::new(10_000)))
        .expect_err("a remote Bitcoin rejection must fail definitively");

    assert_eq!(error.kind, base::TransactionErrorKind::Rejected);
    assert_eq!(error.ambiguous_transaction_id, None);
}

#[test]
fn malformed_broadcast_response_keeps_the_exact_local_id_as_ambiguous() {
    let signed = signed_transaction();
    let local_id = signed.id();
    let mut replies = readiness_replies();
    replies.push(success("sendrawtransaction", Value::Bool(true)));
    let core = block_on(Client::connect(ScriptedClient::new(replies), config()))
        .expect("valid scripted Core node must connect");

    let error = block_on(core.transactions().broadcast(signed, FeeRate::new(10_000)))
        .expect_err("a malformed Bitcoin acknowledgement must remain ambiguous");

    assert_eq!(error.kind, base::TransactionErrorKind::Unavailable);
    assert_eq!(
        error.ambiguous_transaction_id,
        Some(base::TransactionId::new(local_id.to_string()))
    );
}

#[test]
fn broadcast_transport_failure_keeps_the_exact_local_id_as_ambiguous() {
    let signed = signed_transaction();
    let local_id = signed.id();
    let mut replies = readiness_replies();
    replies.push(transport_failure(
        "sendrawtransaction",
        json_rpc::ErrorKind::Timeout,
    ));
    let core = block_on(Client::connect(ScriptedClient::new(replies), config()))
        .expect("valid scripted Core node must connect");

    let error = block_on(core.transactions().broadcast(signed, FeeRate::new(10_000)))
        .expect_err("a Bitcoin transport failure may follow submission");

    assert_eq!(error.kind, base::TransactionErrorKind::Unavailable);
    assert_eq!(
        error.ambiguous_transaction_id,
        Some(base::TransactionId::new(local_id.to_string()))
    );
}

#[test]
fn broadcast_fee_rejection_happens_before_wire_and_has_no_ambiguity() {
    let signed = signed_transaction();
    let core = block_on(Client::connect(
        ScriptedClient::new(readiness_replies()),
        config(),
    ))
    .expect("valid scripted Core node must connect");

    let error = block_on(core.transactions().broadcast(
        signed,
        FeeRate::new(BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB + 1),
    ))
    .expect_err("an invalid Bitcoin fee ceiling must fail before submission");

    assert_eq!(error.kind, base::TransactionErrorKind::Fee);
    assert_eq!(error.ambiguous_transaction_id, None);
}
