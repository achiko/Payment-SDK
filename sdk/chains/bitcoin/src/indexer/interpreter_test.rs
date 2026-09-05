use std::str::FromStr;

use base::Decimal;
use bitcoin::{
    Address, Amount, CompressedPublicKey, OutPoint, PublicKey, ScriptBuf, Sequence, Transaction,
    TxIn, TxOut, Txid, Witness, XOnlyPublicKey, absolute, consensus, hashes::Hash, hex::DisplayHex,
    secp256k1::Secp256k1, transaction::Version,
};
use indexing::{BlockHash, BlockHeight, MovementId, MovementKind};
use serde_json::{Number, Value, json};

use super::*;

#[derive(Clone)]
struct PreviousEvidence {
    value: u64,
    script: ScriptBuf,
    height: u64,
    coinbase: bool,
}

fn scope() -> IndexScope {
    IndexScope {
        chain: (*CHAIN_ID).clone(),
        network: "regtest".to_owned(),
    }
}

fn p2wpkh_address(prefix: u8) -> Address {
    let public_key = PublicKey::from_slice(&[
        prefix, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
        0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16,
        0xf8, 0x17, 0x98,
    ])
    .expect("test public key must parse");
    Address::p2wpkh(
        &CompressedPublicKey::try_from(public_key).expect("test public key must be compressed"),
        bitcoin::Network::Regtest,
    )
}

fn p2tr_address() -> Address {
    let key = XOnlyPublicKey::from_slice(&[
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
        0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8,
        0x17, 0x98,
    ])
    .expect("test x-only key must parse");
    Address::p2tr(
        &Secp256k1::verification_only(),
        key,
        None,
        bitcoin::Network::Regtest,
    )
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
    Value::Number(Number::from_str(&lexical).expect("test BTC number must encode"))
}

fn transaction_json(transaction: &Transaction, previous: &[Option<PreviousEvidence>]) -> Value {
    assert_eq!(transaction.input.len(), previous.len());
    let inputs: Vec<_> = transaction
        .input
        .iter()
        .zip(previous)
        .map(|(input, previous)| match previous {
            None => json!({"coinbase": "01"}),
            Some(previous) => json!({
                "txid": input.previous_output.txid.to_string(),
                "vout": input.previous_output.vout,
                "prevout": {
                    "generated": previous.coinbase,
                    "height": previous.height,
                    "value": btc_number(previous.value),
                    "scriptPubKey": {
                        "hex": previous.script.as_bytes().to_lower_hex_string()
                    }
                }
            }),
        })
        .collect();
    let outputs: Vec<_> = transaction
        .output
        .iter()
        .enumerate()
        .map(|(index, output)| {
            json!({
                "value": btc_number(output.value.to_sat()),
                "n": index,
                "scriptPubKey": {
                    "hex": output.script_pubkey.as_bytes().to_lower_hex_string()
                }
            })
        })
        .collect();
    json!({
        "txid": transaction.compute_txid().to_string(),
        "hex": consensus::serialize(transaction).to_lower_hex_string(),
        "vin": inputs,
        "vout": outputs
    })
}

fn block(transactions: Vec<Value>) -> Block {
    let native_hash = bitcoin::BlockHash::from_byte_array([0xaa; 32]);
    let parent = bitcoin::BlockHash::from_byte_array([0xbb; 32]);
    let raw_block = serde_json::to_vec(&json!({
        "hash": native_hash.to_string(),
        "height": 10,
        "previousblockhash": parent.to_string(),
        "time": 100,
        "nTx": transactions.len(),
        "tx": transactions
    }))
    .expect("test block JSON must encode");
    Block::parse(
        &raw_block,
        Some(BlockHeight(10)),
        Some(&BlockHash(native_hash.to_byte_array().to_vec())),
        Network::Regtest,
    )
    .expect("test block must parse once at its boundary")
}

fn coinbase(output: TxOut) -> Transaction {
    Transaction {
        version: Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(vec![1, 1]),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![output],
    }
}

fn indexed_address(address: &Address) -> CanonicalAddress {
    let address = crate::Address::from_encoded(address.to_string());
    address.canonical(&scope())
}

#[test]
fn rejects_non_object_values_at_each_block_parsing_boundary() {
    let transaction = coinbase(TxOut {
        value: Amount::from_sat(10_000),
        script_pubkey: p2wpkh_address(0x02).script_pubkey(),
    });
    let native_hash = bitcoin::BlockHash::from_byte_array([0xaa; 32]);
    let expected_hash = BlockHash(native_hash.to_byte_array().to_vec());
    let valid = json!({
        "hash": native_hash.to_string(),
        "height": 10,
        "previousblockhash": bitcoin::BlockHash::from_byte_array([0xbb; 32]).to_string(),
        "time": 100,
        "nTx": 1,
        "tx": [transaction_json(&transaction, &[None])]
    });
    Block::parse(
        &serde_json::to_vec(&valid).expect("test block JSON must encode"),
        Some(BlockHeight(10)),
        Some(&expected_hash),
        Network::Regtest,
    )
    .expect("unmodified block must parse");

    for (path, message) in [
        ("", "Bitcoin block result must be an object"),
        ("/tx/0", "Bitcoin transaction must be an object"),
        ("/tx/0/vin/0", "Bitcoin transaction input must be an object"),
        (
            "/tx/0/vout/0",
            "Bitcoin transaction output must be an object",
        ),
    ] {
        for invalid in [
            Value::Null,
            json!(false),
            json!(1),
            json!("invalid"),
            json!([]),
        ] {
            let mut value = valid.clone();
            *value.pointer_mut(path).expect("test path must exist") = invalid;
            let error = Block::parse(
                &serde_json::to_vec(&value).expect("test block JSON must encode"),
                Some(BlockHeight(10)),
                Some(&expected_hash),
                Network::Regtest,
            )
            .expect_err("non-object value must fail while parsing the block");

            assert_eq!(error.kind, crate::ChainErrorKind::InvalidTransaction);
            assert_eq!(error.message, message, "object boundary {path}");
        }
    }
}

#[test]
fn ignores_transactions_unrelated_to_the_address_filter() {
    let destination = p2wpkh_address(0x02);
    let unrelated = p2tr_address();
    let transaction = coinbase(TxOut {
        value: Amount::from_sat(10_000),
        script_pubkey: destination.script_pubkey(),
    });
    let block = block(vec![transaction_json(&transaction, &[None])]);

    let interpreted = BlockInterpreter::new(scope(), Network::Regtest)
        .expect("scope must be valid")
        .inspect(&block, &[indexed_address(&unrelated)])
        .expect("unrelated transaction must still interpret");

    assert!(interpreted.transactions.is_empty());
    assert!(interpreted.outputs.created.is_empty());
}

#[test]
fn rejects_an_address_filter_from_another_scope() {
    let address = p2wpkh_address(0x02);
    let mut foreign = indexed_address(&address);
    foreign.scope.network = "other".to_owned();

    let error = BlockInterpreter::new(scope(), Network::Regtest)
        .expect("scope must be valid")
        .inspect(&block(Vec::new()), &[foreign])
        .expect_err("foreign scope must be rejected");

    assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
}

#[test]
fn same_block_spend_nets_utxo_state_while_emitting_movements() {
    let source = p2wpkh_address(0x02);
    let destination = p2tr_address();
    let funding = coinbase(TxOut {
        value: Amount::from_sat(50_000),
        script_pubkey: source.script_pubkey(),
    });
    let spending = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(funding.compute_txid(), 0),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(49_000),
            script_pubkey: destination.script_pubkey(),
        }],
    };
    let block = block(vec![transaction_json(&funding, &[None]), {
        let mut value = transaction_json(
            &spending,
            &[Some(PreviousEvidence {
                value: 50_000,
                script: source.script_pubkey(),
                height: 10,
                coinbase: true,
            })],
        );
        value["vin"][0]
            .as_object_mut()
            .expect("test input must be an object")
            .remove("prevout");
        value
    }]);

    let interpreted = BlockInterpreter::new(scope(), Network::Regtest)
        .expect("scope must be valid")
        .inspect(
            &block,
            &[indexed_address(&source), indexed_address(&destination)],
        )
        .expect("valid relevant block must interpret");

    assert_eq!(interpreted.transactions.len(), 2);
    let spend = &interpreted.transactions[1];
    assert_eq!(spend.movements.len(), 2);
    assert_eq!(spend.movements[0].kind(), MovementKind::Input);
    assert_eq!(spend.movements[1].kind(), MovementKind::Output);
    assert_eq!(
        spend.movements[0].id(),
        &MovementId(format!("{}:vin:0", spending.compute_txid()))
    );
    assert_eq!(
        spend
            .fee
            .as_ref()
            .expect("normal transaction has a fee")
            .amount,
        Decimal::from(1_000_u64)
    );
    assert_eq!(
        spend
            .fee
            .as_ref()
            .expect("normal transaction has a fee")
            .amount
            .scale(),
        0
    );
    assert_eq!(interpreted.outputs.created.len(), 1);
    assert!(interpreted.outputs.spent.is_empty());
    assert!(interpreted.outputs.tracked_spends.is_empty());
}

#[test]
fn indexed_address_spend_is_recorded_directly() {
    let source = p2wpkh_address(0x02);
    let destination = p2tr_address();
    let transaction = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([9; 32]), 1),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(9_000),
            script_pubkey: destination.script_pubkey(),
        }],
    };
    let block = block(vec![transaction_json(
        &transaction,
        &[Some(PreviousEvidence {
            value: 10_000,
            script: source.script_pubkey(),
            height: 4,
            coinbase: false,
        })],
    )]);

    let interpreted = BlockInterpreter::new(scope(), Network::Regtest)
        .expect("scope must be valid")
        .inspect(&block, &[indexed_address(&source)])
        .expect("indexed address spend must interpret");

    assert_eq!(interpreted.transactions.len(), 1);
    assert_eq!(interpreted.outputs.spent.len(), 1);
    assert!(interpreted.outputs.created.is_empty());
    assert!(interpreted.outputs.tracked_spends.is_empty());
}

#[test]
fn missing_resolved_prevout_fails_before_commit() {
    let destination = p2wpkh_address(0x02);
    let transaction = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([3; 32]), 0),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: destination.script_pubkey(),
        }],
    };
    let mut value = transaction_json(
        &transaction,
        &[Some(PreviousEvidence {
            value: 2_000,
            script: destination.script_pubkey(),
            height: 9,
            coinbase: false,
        })],
    );
    value["vin"][0]
        .as_object_mut()
        .expect("test input must be an object")
        .remove("prevout");
    let native_hash = bitcoin::BlockHash::from_byte_array([0xaa; 32]);
    let parent = bitcoin::BlockHash::from_byte_array([0xbb; 32]);
    let raw = serde_json::to_vec(&json!({
        "hash": native_hash.to_string(),
        "height": 10,
        "previousblockhash": parent.to_string(),
        "time": 100,
        "nTx": 1,
        "tx": [value]
    }))
    .expect("test block JSON must encode");

    let error = Block::parse(
        &raw,
        Some(BlockHeight(10)),
        Some(&BlockHash(native_hash.to_byte_array().to_vec())),
        Network::Regtest,
    )
    .expect_err("missing prevout evidence must fail while parsing the block");

    assert_eq!(error.kind, crate::ChainErrorKind::InvalidTransaction);
    assert!(error.message.contains("resolved previous output"));
}
