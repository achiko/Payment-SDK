use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use bitcoin::{
    BlockHash as NativeBlockHash, ScriptBuf, Transaction as NativeTransaction, Txid, consensus,
    hashes::Hash, hex::FromHex,
};
use indexing::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
use serde_json::{Map, Value};

use crate::{Network, Satoshi, TransactionId};

use super::Outpoint;

#[path = "model_value.rs"]
mod value;

use value::{required_bool, required_string, required_u32, required_u64};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BlockData {
    pub reference: BlockRef,
    pub transactions: Vec<Transaction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Transaction {
    pub id: TransactionId,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub coinbase: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Input {
    pub previous_output: Option<PreviousOutput>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PreviousOutput {
    pub outpoint: Outpoint,
    pub value: Satoshi,
    pub address: Option<crate::Address>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Output {
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParseError {
    message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl BlockData {
    pub(super) fn parse(
        raw: &[u8],
        expected_height: Option<BlockHeight>,
        expected_hash: Option<&BlockHash>,
        network: Network,
    ) -> Result<Self, ParseError> {
        let value: Value = serde_json::from_slice(raw)
            .map_err(|_| ParseError::new("Bitcoin block result is not valid JSON"))?;
        let object = value
            .as_object()
            .ok_or_else(|| ParseError::new("Bitcoin block result must be an object"))?;
        let height = BlockHeight(required_u64(object, "height", "Bitcoin block height")?);
        if expected_height.is_some_and(|expected| expected != height) {
            return Err(ParseError::new(
                "Bitcoin block height does not match the requested height",
            ));
        }
        let hash = required_string(object, "hash", "Bitcoin block hash")?
            .parse::<NativeBlockHash>()
            .map(|hash| BlockHash(hash.to_byte_array().to_vec()))
            .map_err(|_| ParseError::new("Bitcoin block hash is invalid"))?;
        if expected_hash.is_some_and(|expected| expected != &hash) {
            return Err(ParseError::new(
                "Bitcoin block hash does not match the requested hash",
            ));
        }
        let has_parent = object
            .get("previousblockhash")
            .is_some_and(|value| !value.is_null());
        if height.0 == 0 && has_parent {
            return Err(ParseError::new(
                "Bitcoin genesis block unexpectedly has a parent hash",
            ));
        }
        let parent = if height.0 == 0 {
            None
        } else {
            Some(BlockParent {
                position: BlockPosition(height.0 - 1),
                hash: required_string(object, "previousblockhash", "Bitcoin previous block hash")?
                    .parse::<NativeBlockHash>()
                    .map(|hash| BlockHash(hash.to_byte_array().to_vec()))
                    .map_err(|_| ParseError::new("Bitcoin block hash is invalid"))?,
            })
        };
        let timestamp = required_u64(object, "time", "Bitcoin block timestamp")?;
        let transaction_values = object
            .get("tx")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::new("Bitcoin block transactions must be an array"))?;
        let declared_count = required_u64(object, "nTx", "Bitcoin block transaction count")?;
        if usize::try_from(declared_count).ok() != Some(transaction_values.len()) {
            return Err(ParseError::new(
                "Bitcoin block transaction count does not match its transaction array",
            ));
        }

        let mut transactions = Vec::with_capacity(transaction_values.len());
        let mut transaction_ids = BTreeSet::new();
        let mut same_block_outputs = BTreeMap::new();
        for transaction in transaction_values {
            let transaction =
                Transaction::parse(transaction, height, network, &same_block_outputs)?;
            if !transaction_ids.insert(transaction.id) {
                return Err(ParseError::new(
                    "Bitcoin block contains duplicate transaction IDs",
                ));
            }
            for (index, output) in transaction.outputs.iter().enumerate() {
                let Ok(output_index) = u32::try_from(index) else {
                    return Err(ParseError::new("Bitcoin output index exceeds u32"));
                };
                let script = ScriptBuf::from_bytes(output.script_pubkey.clone());
                same_block_outputs.insert(
                    Outpoint {
                        transaction_id: transaction.id,
                        output_index,
                    },
                    PreviousOutput {
                        outpoint: Outpoint {
                            transaction_id: transaction.id,
                            output_index,
                        },
                        value: output.value,
                        address: crate::Address::from_script_for_network(&script, network),
                    },
                );
            }
            transactions.push(transaction);
        }

        Ok(Self {
            reference: BlockRef {
                position: BlockPosition(height.0),
                height,
                hash,
                parent,
                timestamp: Some(timestamp),
            },
            transactions,
        })
    }
}

impl Transaction {
    fn parse(
        value: &Value,
        block_height: BlockHeight,
        network: Network,
        same_block_outputs: &BTreeMap<Outpoint, PreviousOutput>,
    ) -> Result<Self, ParseError> {
        let object = value
            .as_object()
            .ok_or_else(|| ParseError::new("Bitcoin transaction must be an object"))?;
        let txid = required_string(object, "txid", "Bitcoin transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| ParseError::new("Bitcoin transaction ID is invalid"))?;
        let raw = Vec::<u8>::from_hex(required_string(object, "hex", "Bitcoin raw transaction")?)
            .map_err(|_| ParseError::new("Bitcoin transaction hex is invalid"))?;
        let native: NativeTransaction = consensus::deserialize(&raw)
            .map_err(|_| ParseError::new("Bitcoin transaction consensus bytes are invalid"))?;
        if TransactionId::from(native.compute_txid()) != txid {
            return Err(ParseError::new(
                "Bitcoin transaction ID does not match its consensus bytes",
            ));
        }
        let coinbase = native.is_coinbase();

        let input_values = object
            .get("vin")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::new("Bitcoin transaction inputs must be an array"))?;
        if input_values.len() != native.input.len() {
            return Err(ParseError::new(
                "Bitcoin transaction input count does not match its consensus bytes",
            ));
        }
        let mut inputs = Vec::with_capacity(input_values.len());
        for (index, (input, native_input)) in input_values.iter().zip(&native.input).enumerate() {
            inputs.push(Input::parse(
                input,
                native_input,
                index,
                coinbase,
                block_height,
                network,
                same_block_outputs,
            )?);
        }

        let output_values = object
            .get("vout")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::new("Bitcoin transaction outputs must be an array"))?;
        if output_values.len() != native.output.len() {
            return Err(ParseError::new(
                "Bitcoin transaction output count does not match its consensus bytes",
            ));
        }
        let mut outputs = Vec::with_capacity(output_values.len());
        for (position, (output, native_output)) in
            output_values.iter().zip(&native.output).enumerate()
        {
            let object = output
                .as_object()
                .ok_or_else(|| ParseError::new("Bitcoin transaction output must be an object"))?;
            let output_index = required_u64(object, "n", "Bitcoin output index")?;
            if usize::try_from(output_index).ok() != Some(position) {
                return Err(ParseError::new(
                    "Bitcoin transaction output index does not match its position",
                ));
            }
            let value = Satoshi::from_block_json(
                object
                    .get("value")
                    .ok_or_else(|| ParseError::new("Bitcoin output value is missing"))?,
                "Bitcoin output value",
            )?;
            if native_output.value.to_sat() != value.0 {
                return Err(ParseError::new(
                    "Bitcoin output value does not match its consensus bytes",
                ));
            }
            let script = object
                .get("scriptPubKey")
                .and_then(Value::as_object)
                .ok_or_else(|| ParseError::new("Bitcoin output scriptPubKey is missing"))?;
            let hex = script
                .get("hex")
                .and_then(Value::as_str)
                .ok_or_else(|| ParseError::new("Bitcoin scriptPubKey hex is missing or invalid"))?;
            let bytes = Vec::<u8>::from_hex(hex)
                .map_err(|_| ParseError::new("Bitcoin scriptPubKey hex is invalid"))?;
            let script = ScriptBuf::from_bytes(bytes);
            if native_output.script_pubkey != script {
                return Err(ParseError::new(
                    "Bitcoin output script does not match its consensus bytes",
                ));
            }
            outputs.push(Output {
                value,
                script_pubkey: script.into_bytes(),
            });
        }

        Ok(Self {
            id: txid,
            inputs,
            outputs,
            coinbase,
        })
    }
}

impl Input {
    fn parse(
        input: &Value,
        native_input: &bitcoin::TxIn,
        index: usize,
        coinbase: bool,
        block_height: BlockHeight,
        network: Network,
        same_block_outputs: &BTreeMap<Outpoint, PreviousOutput>,
    ) -> Result<Self, ParseError> {
        let object = input
            .as_object()
            .ok_or_else(|| ParseError::new("Bitcoin transaction input must be an object"))?;
        if native_input.previous_output.is_null() {
            if !coinbase || object.get("coinbase").and_then(Value::as_str).is_none() {
                return Err(ParseError::new(
                    "Bitcoin null input is not a valid coinbase input",
                ));
            }
            return Ok(Self {
                previous_output: None,
            });
        }
        if coinbase {
            return Err(ParseError::new(
                "Bitcoin coinbase transaction contains a non-coinbase input",
            ));
        }
        let previous_id = required_string(object, "txid", "Bitcoin input previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| ParseError::new("Bitcoin transaction ID is invalid"))?;
        let output_index = required_u32(object, "vout", "Bitcoin input output index")?;
        if native_input.previous_output.txid != Txid::from(previous_id)
            || native_input.previous_output.vout != output_index
        {
            return Err(ParseError::new(format!(
                "Bitcoin input {index} outpoint does not match its consensus bytes"
            )));
        }
        let outpoint = Outpoint {
            transaction_id: previous_id,
            output_index,
        };
        let local = same_block_outputs.get(&outpoint);
        let previous_output = match object.get("prevout").and_then(Value::as_object) {
            Some(prevout) => {
                let resolved = PreviousOutput::parse(prevout, outpoint, block_height, network)?;
                let conflicts = local.is_some_and(|local| local != &resolved);
                if conflicts {
                    return Err(ParseError::new(format!(
                        "Bitcoin input {index} prevout conflicts with an earlier same-block output"
                    )));
                }
                resolved
            }
            None => local.cloned().ok_or_else(|| {
                ParseError::new(format!(
                    "Bitcoin input {index} has no resolved previous output"
                ))
            })?,
        };
        Ok(Self {
            previous_output: Some(previous_output),
        })
    }
}

impl PreviousOutput {
    fn parse(
        prevout: &Map<String, Value>,
        outpoint: Outpoint,
        spending_height: BlockHeight,
        network: Network,
    ) -> Result<Self, ParseError> {
        let Some(value) = prevout.get("value_satoshis") else {
            return Self::parse_verbose(prevout, outpoint, spending_height, network);
        };
        let value = value
            .as_u64()
            .ok_or_else(|| ParseError::new("Bitcoin compact prevout value is invalid"))?;
        let address = match prevout
            .get("address")
            .ok_or_else(|| ParseError::new("Bitcoin compact prevout address fact is missing"))?
        {
            Value::Null => None,
            Value::String(address) => {
                let canonical =
                    crate::Address::parse_for_network(address, network).map_err(|_| {
                        ParseError::new(
                            "Bitcoin compact prevout address is invalid or wrong-network",
                        )
                    })?;
                if canonical.encoded() != address {
                    return Err(ParseError::new(
                        "Bitcoin compact prevout address is not canonical",
                    ));
                }
                Some(canonical)
            }
            _ => {
                return Err(ParseError::new(
                    "Bitcoin compact prevout address fact is invalid",
                ));
            }
        };
        Ok(Self {
            outpoint,
            value: Satoshi(value),
            address,
        })
    }

    fn parse_verbose(
        prevout: &Map<String, Value>,
        outpoint: Outpoint,
        spending_height: BlockHeight,
        network: Network,
    ) -> Result<Self, ParseError> {
        // Direct Block::parse callers may supply Bitcoin Core's
        // verbosity-3-compatible previous-output shape.
        let value = Satoshi::from_block_json(
            prevout
                .get("value")
                .ok_or_else(|| ParseError::new("Bitcoin prevout value is missing"))?,
            "Bitcoin prevout value",
        )?;
        let script = prevout
            .get("scriptPubKey")
            .and_then(Value::as_object)
            .ok_or_else(|| ParseError::new("Bitcoin prevout scriptPubKey is missing"))?;
        let hex = script
            .get("hex")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::new("Bitcoin scriptPubKey hex is missing or invalid"))?;
        let bytes = Vec::<u8>::from_hex(hex)
            .map_err(|_| ParseError::new("Bitcoin scriptPubKey hex is invalid"))?;
        let script = ScriptBuf::from_bytes(bytes);
        let created_height = BlockHeight(required_u64(
            prevout,
            "height",
            "Bitcoin prevout creation height",
        )?);
        if created_height > spending_height {
            return Err(ParseError::new(
                "Bitcoin input previous output was created after the spending block",
            ));
        }
        let _coinbase = required_bool(prevout, "generated", "Bitcoin prevout coinbase flag")?;
        Ok(Self {
            outpoint,
            value,
            address: crate::Address::from_script_for_network(&script, network),
        })
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::hex::DisplayHex;

    use super::*;

    #[test]
    fn genesis_parent_rules_preserve_identity_validation_order() {
        let original = serde_json::json!({
            "height": 0, "hash": "00".repeat(32), "time": 2, "tx": [], "nTx": 0,
        });
        for parent in [None, Some(Value::Null)] {
            let mut value = original.clone();
            if let Some(parent) = parent {
                value["previousblockhash"] = parent;
            }
            let block = BlockData::parse(
                &serde_json::to_vec(&value).unwrap(),
                Some(BlockHeight(0)),
                Some(&BlockHash(vec![0; 32])),
                Network::Regtest,
            )
            .unwrap();
            assert_eq!(block.reference.parent, None);
            assert_eq!(block.reference.position, BlockPosition(0));
        }

        for parent in [serde_json::json!("invalid"), serde_json::json!(false)] {
            let mut value = original.clone();
            value["previousblockhash"] = parent;
            value["time"] = Value::Null;
            let raw = serde_json::to_vec(&value).unwrap();
            let error = BlockData::parse(&raw, None, None, Network::Regtest).unwrap_err();
            assert_eq!(
                error.to_string(),
                "Bitcoin genesis block unexpectedly has a parent hash"
            );
            let error =
                BlockData::parse(&raw, None, Some(&BlockHash(vec![1; 32])), Network::Regtest)
                    .unwrap_err();
            assert_eq!(
                error.to_string(),
                "Bitcoin block hash does not match the requested hash"
            );
        }
    }

    #[test]
    fn prevout_formats_preserve_compact_priority_and_validation() {
        let outpoint = Outpoint {
            transaction_id: TransactionId([3; 32]),
            output_index: 2,
        };
        let script = ScriptBuf::from_bytes([vec![0, 20], vec![7; 20]].concat());
        let address = crate::Address::from_script_for_network(&script, Network::Regtest).unwrap();
        let wrong_network =
            crate::Address::from_script_for_network(&script, Network::Mainnet).unwrap();
        let verbose = serde_json::json!({
            "value": "0.00000010".parse::<serde_json::Number>().unwrap(),
            "scriptPubKey": { "hex": script.as_bytes().to_lower_hex_string() },
            "height": 10, "generated": false,
        });
        let parsed = PreviousOutput::parse(
            verbose.as_object().unwrap(),
            outpoint,
            BlockHeight(10),
            Network::Regtest,
        )
        .unwrap();
        assert_eq!(
            parsed,
            PreviousOutput {
                outpoint,
                value: Satoshi(10),
                address: Some(address.clone()),
            }
        );

        let mut compact = verbose.clone();
        compact["value_satoshis"] = serde_json::json!(u64::MAX);
        compact["address"] = serde_json::json!(address.encoded());
        compact["height"] = Value::Null;
        assert_eq!(
            PreviousOutput::parse(
                compact.as_object().unwrap(),
                outpoint,
                BlockHeight(10),
                Network::Regtest,
            )
            .unwrap(),
            PreviousOutput {
                outpoint,
                value: Satoshi(u64::MAX),
                address: Some(address.clone()),
            }
        );

        for (field, value, message) in [
            (
                "value_satoshis",
                Value::Null,
                "Bitcoin compact prevout value is invalid",
            ),
            (
                "address",
                serde_json::json!(false),
                "Bitcoin compact prevout address fact is invalid",
            ),
            (
                "address",
                serde_json::json!(wrong_network.encoded()),
                "Bitcoin compact prevout address is invalid or wrong-network",
            ),
            (
                "address",
                serde_json::json!(address.encoded().to_uppercase()),
                "Bitcoin compact prevout address is not canonical",
            ),
        ] {
            let mut invalid = compact.clone();
            invalid[field] = value;
            let error = PreviousOutput::parse(
                invalid.as_object().unwrap(),
                outpoint,
                BlockHeight(10),
                Network::Regtest,
            )
            .unwrap_err();
            assert_eq!(error.to_string(), message);
        }

        let mut invalid = verbose;
        invalid["height"] = serde_json::json!(11);
        invalid["generated"] = Value::Null;
        let error = PreviousOutput::parse(
            invalid.as_object().unwrap(),
            outpoint,
            BlockHeight(10),
            Network::Regtest,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Bitcoin input previous output was created after the spending block"
        );
    }

    #[test]
    fn transaction_inputs_validate_supplied_prevouts_before_local_comparison() {
        let outpoint = Outpoint {
            transaction_id: TransactionId([3; 32]),
            output_index: 2,
        };
        let local = PreviousOutput {
            outpoint,
            value: Satoshi(10),
            address: None,
        };
        let same_block_outputs = BTreeMap::from([(outpoint, local.clone())]);
        let native = NativeTransaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::new(
                    Txid::from(outpoint.transaction_id),
                    outpoint.output_index,
                ),
                script_sig: ScriptBuf::new(),
                sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: bitcoin::Witness::new(),
            }],
            output: Vec::new(),
        };
        let original = serde_json::json!({
            "txid": native.compute_txid().to_string(),
            "hex": consensus::serialize(&native).to_lower_hex_string(),
            "vin": [{"txid": outpoint.transaction_id.to_string(), "vout": 2}],
            "vout": [],
        });
        for (prevout, expected_error) in [
            (None, None),
            (Some(Value::Null), None),
            (
                Some(serde_json::json!({"value_satoshis": 10, "address": null})),
                None,
            ),
            (
                Some(serde_json::json!({"value_satoshis": 11, "address": null})),
                Some("Bitcoin input 0 prevout conflicts with an earlier same-block output"),
            ),
            (
                Some(serde_json::json!({"value_satoshis": "invalid", "address": null})),
                Some("Bitcoin compact prevout value is invalid"),
            ),
            (
                Some(serde_json::json!({"value_satoshis": 11})),
                Some("Bitcoin compact prevout address fact is missing"),
            ),
        ] {
            let mut transaction = original.clone();
            if let Some(prevout) = prevout {
                transaction["vin"][0]["prevout"] = prevout;
            }
            let result = Transaction::parse(
                &transaction,
                BlockHeight(10),
                Network::Regtest,
                &same_block_outputs,
            );
            if let Some(message) = expected_error {
                assert_eq!(result.unwrap_err().to_string(), message);
                continue;
            }
            assert_eq!(
                result.unwrap().inputs,
                vec![Input {
                    previous_output: Some(local.clone()),
                }]
            );
        }

        let mut invalid_outpoint = original;
        invalid_outpoint["vin"][0]["vout"] = serde_json::json!(3);
        invalid_outpoint["vin"][0]["prevout"] = serde_json::json!({"value_satoshis": "invalid"});
        let error = Transaction::parse(
            &invalid_outpoint,
            BlockHeight(10),
            Network::Regtest,
            &same_block_outputs,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Bitcoin input 0 outpoint does not match its consensus bytes"
        );
    }

    #[test]
    fn block_hashes_keep_native_byte_order_and_case_acceptance() {
        let hash = "1F1E1D1C1B1A191817161514131211100F0E0D0C0B0A09080706050403020100";
        let parent = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let raw = serde_json::json!({
            "height": 1, "hash": hash, "previousblockhash": parent,
            "time": 2, "tx": [], "nTx": 0,
        });
        let block = BlockData::parse(
            &serde_json::to_vec(&raw).unwrap(),
            None,
            None,
            Network::Regtest,
        )
        .unwrap();
        assert_eq!(block.reference.hash, BlockHash((0_u8..32).collect()));
        assert_eq!(
            block.reference.parent.unwrap().hash,
            BlockHash((0_u8..32).rev().collect())
        );
    }

    #[test]
    fn block_hash_errors_keep_context_and_height_validation_precedence() {
        let invalid = serde_json::json!({"height": 1, "hash": "invalid"});
        let raw = serde_json::to_vec(&invalid).unwrap();
        let error =
            BlockData::parse(&raw, Some(BlockHeight(2)), None, Network::Regtest).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Bitcoin block height does not match the requested height"
        );
        let error = BlockData::parse(&raw, None, None, Network::Regtest).unwrap_err();
        assert_eq!(error.to_string(), "Bitcoin block hash is invalid");

        for field in ["hash", "previousblockhash"] {
            for value in ["00".repeat(31), "00".repeat(33), "gg".repeat(32)] {
                let mut block = serde_json::json!({
                    "height": 1, "hash": "00".repeat(32), "previousblockhash": "00".repeat(32),
                    "time": 2, "tx": [], "nTx": 0,
                });
                block[field] = Value::String(value);
                let error = BlockData::parse(
                    &serde_json::to_vec(&block).unwrap(),
                    None,
                    None,
                    Network::Regtest,
                )
                .unwrap_err();
                assert_eq!(error.to_string(), "Bitcoin block hash is invalid");
            }
        }
    }
}
