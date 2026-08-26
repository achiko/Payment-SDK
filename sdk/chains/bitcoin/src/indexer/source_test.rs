use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness, absolute, consensus,
    hashes::Hash, hex::DisplayHex, transaction::Version,
};
use futures_executor::block_on;
use json_rpc::{Call, Error, Failure, RawJson};
use serde_json::{Value, json};

use super::*;

#[derive(Clone)]
struct ScriptedClient {
    replies: Arc<Mutex<VecDeque<ExpectedReply>>>,
}

struct ExpectedReply {
    method: &'static str,
    params: Option<Value>,
    result: Result<Value, i64>,
}

impl ScriptedClient {
    fn new(replies: Vec<ExpectedReply>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
        }
    }

    fn assert_exhausted(&self) {
        assert!(
            self.replies
                .lock()
                .expect("script lock must be healthy")
                .is_empty(),
            "source made fewer requests than scripted"
        );
    }
}

impl Client for ScriptedClient {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Result<RawJson, Failure>, Error>> {
        let expected = self
            .replies
            .lock()
            .expect("script lock must be healthy")
            .pop_front()
            .expect("source made more requests than scripted");
        assert_eq!(method, expected.method);
        if let Some(expected_params) = expected.params {
            assert_eq!(params, expected_params);
        }
        let result = expected
            .result
            .map(|value| {
                RawJson::from_serializable(&value).expect("scripted source result must encode")
            })
            .map_err(|code| Failure {
                code,
                message: "scripted source failure".to_owned(),
                data: None,
            });
        Box::pin(async move { Ok(result) })
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
        _requests: Vec<Call>,
    ) -> BoxFuture<'a, Result<Vec<Result<RawJson, Failure>>, Error>> {
        let _unused = Failure {
            code: -1,
            message: String::new(),
            data: None,
        };
        Box::pin(async move { Ok(Vec::new()) })
    }
}

fn reply(method: &'static str, result: Value) -> ExpectedReply {
    ExpectedReply {
        method,
        params: None,
        result: Ok(result),
    }
}

fn reply_for(method: &'static str, params: Value, result: Value) -> ExpectedReply {
    ExpectedReply {
        method,
        params: Some(params),
        result: Ok(result),
    }
}

fn failure(method: &'static str, code: i64) -> ExpectedReply {
    ExpectedReply {
        method,
        params: None,
        result: Err(code),
    }
}

fn hash(number: u8) -> String {
    let mut bytes = [0_u8; 32];
    bytes[0] = number;
    bitcoin::BlockHash::from_byte_array(bytes).to_string()
}

fn block_result() -> Value {
    let transaction = Transaction {
        version: Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(vec![1, 1]),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(5_000_000_000),
            script_pubkey: ScriptBuf::new(),
        }],
    };
    json!({
        "hash": hash(2),
        "height": 10,
        "previousblockhash": hash(3),
        "time": 100,
        "nTx": 1,
        "tx": [{
            "txid": transaction.compute_txid().to_string(),
            "hex": consensus::serialize(&transaction).to_lower_hex_string(),
            "vin": [{"coinbase": "01"}],
            "vout": [{
                "value": 50.00000000,
                "n": 0,
                "scriptPubKey": {"hex": ""}
            }]
        }]
    })
}

fn btc_number(satoshis: u64) -> Value {
    let whole = satoshis / 100_000_000;
    let remainder = satoshis % 100_000_000;
    let lexical = if remainder == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{remainder:08}")
            .trim_end_matches('0')
            .to_owned()
    };
    serde_json::from_str(&lexical).expect("test Bitcoin amount must encode")
}

fn transaction_result(transaction: &Transaction, inputs: Vec<Value>) -> Value {
    let outputs = transaction
        .output
        .iter()
        .enumerate()
        .map(|(index, output)| {
            json!({
                "value": btc_number(output.value.to_sat()),
                "n": index,
                "scriptPubKey": {
                    "hex": output.script_pubkey.as_bytes().to_lower_hex_string(),
                },
            })
        })
        .collect::<Vec<_>>();
    json!({
        "txid": transaction.compute_txid().to_string(),
        "hex": consensus::serialize(transaction).to_lower_hex_string(),
        "vin": inputs,
        "vout": outputs,
    })
}

fn external_prevout_block() -> (Value, Transaction, Transaction, Transaction) {
    let previous = Transaction {
        version: Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(vec![1, 1]),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(123_456_789),
                script_pubkey: ScriptBuf::from_bytes([vec![0x00, 0x14], vec![0x11; 20]].concat()),
            },
            TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: ScriptBuf::from_bytes([vec![0x51, 0x20], vec![0x22; 32]].concat()),
            },
            TxOut {
                value: Amount::from_sat(25_000),
                // This historical script is deliberately large. The
                // source must derive the absence of a canonical address
                // and discard the script before returning the parsed block.
                script_pubkey: ScriptBuf::from_bytes(vec![0x61; 65_536]),
            },
        ],
    };
    let spending = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![
            TxIn {
                previous_output: OutPoint {
                    txid: previous.compute_txid(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            },
            TxIn {
                previous_output: OutPoint {
                    txid: previous.compute_txid(),
                    vout: 1,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            },
            TxIn {
                previous_output: OutPoint {
                    txid: previous.compute_txid(),
                    vout: 2,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            },
        ],
        output: vec![TxOut {
            value: Amount::from_sat(123_521_789),
            script_pubkey: ScriptBuf::new(),
        }],
    };
    let child = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: spending.compute_txid(),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(123_511_789),
            script_pubkey: ScriptBuf::new(),
        }],
    };
    let block = json!({
        "hash": hash(2),
        "height": 10,
        "previousblockhash": hash(3),
        "time": 100,
        "nTx": 2,
        "tx": [
            transaction_result(
                &spending,
                vec![
                    json!({"txid": previous.compute_txid().to_string(), "vout": 0}),
                    json!({"txid": previous.compute_txid().to_string(), "vout": 1}),
                    json!({"txid": previous.compute_txid().to_string(), "vout": 2}),
                ],
            ),
            transaction_result(
                &child,
                vec![json!({"txid": spending.compute_txid().to_string(), "vout": 0})],
            ),
        ],
    });
    (block, previous, spending, child)
}

fn config() -> Config {
    Config {
        scope: IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: "regtest".to_owned(),
        },
        network: Network::Regtest,
        expected_genesis_hash: parse_bitcoin_block_hash(&hash(1))
            .expect("test genesis hash must parse"),
    }
}

fn connect_replies() -> Vec<ExpectedReply> {
    vec![
        reply("getnetworkinfo", json!({"version": 310000})),
        reply(
            "getblockchaininfo",
            json!({
                "chain": "regtest",
                "blocks": 10,
                "headers": 10,
                "bestblockhash": hash(2),
                "initialblockdownload": false,
                "pruned": false
            }),
        ),
        reply(
            "getindexinfo",
            json!({"txindex": {"synced": true, "best_block_height": 10}}),
        ),
        reply("getblockhash", Value::String(hash(1))),
    ]
}

#[path = "source_cases.rs"]
mod cases;
