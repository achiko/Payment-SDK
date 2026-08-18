use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use bitcoin::{
    BlockHash as NativeBlockHash, ScriptBuf, Transaction as NativeTransaction, Txid, consensus,
    hashes::Hash, hex::FromHex,
};
use indexing::{BlockHash, BlockHeight, BlockRef};
use serde_json::{Map, Number, Value};

use crate::{Network, Satoshi, TransactionId};

use super::Outpoint;

const SATOSHIS_PER_BITCOIN: u64 = 100_000_000;

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
        let object = as_object(&value, "Bitcoin block result must be an object")?;
        let height = BlockHeight(required_u64(object, "height", "Bitcoin block height")?);
        if expected_height.is_some_and(|expected| expected != height) {
            return Err(ParseError::new(
                "Bitcoin block height does not match the requested height",
            ));
        }
        let hash = parse_block_hash(required_string(object, "hash", "Bitcoin block hash")?)?;
        if expected_hash.is_some_and(|expected| expected != &hash) {
            return Err(ParseError::new(
                "Bitcoin block hash does not match the requested hash",
            ));
        }
        let parent_hash = if height.0 == 0 {
            if object
                .get("previousblockhash")
                .is_some_and(|value| !value.is_null())
            {
                return Err(ParseError::new(
                    "Bitcoin genesis block unexpectedly has a parent hash",
                ));
            }
            None
        } else {
            Some(parse_block_hash(required_string(
                object,
                "previousblockhash",
                "Bitcoin previous block hash",
            )?)?)
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
                let output_index = u32::try_from(index)
                    .map_err(|_| ParseError::new("Bitcoin output index exceeds u32"))?;
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
                        address: address_for_script(&script, network),
                    },
                );
            }
            transactions.push(transaction);
        }

        Ok(Self {
            reference: BlockRef {
                height,
                hash,
                parent_hash,
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
        let object = as_object(value, "Bitcoin transaction must be an object")?;
        let txid = parse_txid(required_string(object, "txid", "Bitcoin transaction ID")?)?;
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
            let object = as_object(input, "Bitcoin transaction input must be an object")?;
            if native_input.previous_output.is_null() {
                if !coinbase || object.get("coinbase").and_then(Value::as_str).is_none() {
                    return Err(ParseError::new(
                        "Bitcoin null input is not a valid coinbase input",
                    ));
                }
                inputs.push(Input {
                    previous_output: None,
                });
                continue;
            }
            if coinbase {
                return Err(ParseError::new(
                    "Bitcoin coinbase transaction contains a non-coinbase input",
                ));
            }
            let previous_id = parse_txid(required_string(
                object,
                "txid",
                "Bitcoin input previous transaction ID",
            )?)?;
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
                    if local.is_some_and(|local| local != &resolved) {
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
            inputs.push(Input {
                previous_output: Some(previous_output),
            });
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
            let object = as_object(output, "Bitcoin transaction output must be an object")?;
            let output_index = required_u64(object, "n", "Bitcoin output index")?;
            if usize::try_from(output_index).ok() != Some(position) {
                return Err(ParseError::new(
                    "Bitcoin transaction output index does not match its position",
                ));
            }
            let value = parse_btc_amount(
                object
                    .get("value")
                    .ok_or_else(|| ParseError::new("Bitcoin output value is missing"))?,
                "Bitcoin output value",
            )?;
            if native_output.value.to_sat() != value {
                return Err(ParseError::new(
                    "Bitcoin output value does not match its consensus bytes",
                ));
            }
            let script = parse_script(
                object
                    .get("scriptPubKey")
                    .and_then(Value::as_object)
                    .ok_or_else(|| ParseError::new("Bitcoin output scriptPubKey is missing"))?,
            )?;
            if native_output.script_pubkey != script {
                return Err(ParseError::new(
                    "Bitcoin output script does not match its consensus bytes",
                ));
            }
            outputs.push(Output {
                value: Satoshi(value),
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

impl PreviousOutput {
    fn parse(
        prevout: &Map<String, Value>,
        outpoint: Outpoint,
        spending_height: BlockHeight,
        network: Network,
    ) -> Result<Self, ParseError> {
        if let Some(value) = prevout.get("value_satoshis") {
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
            return Ok(Self {
                outpoint,
                value: Satoshi(value),
                address,
            });
        }

        // Accept the original verbosity-3-compatible shape so durable raw block
        // records written before compact enrichment remain replayable.
        let value = Satoshi(parse_btc_amount(
            prevout
                .get("value")
                .ok_or_else(|| ParseError::new("Bitcoin prevout value is missing"))?,
            "Bitcoin prevout value",
        )?);
        let script = parse_script(
            prevout
                .get("scriptPubKey")
                .and_then(Value::as_object)
                .ok_or_else(|| ParseError::new("Bitcoin prevout scriptPubKey is missing"))?,
        )?;
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
            address: address_for_script(&script, network),
        })
    }
}

pub(super) fn address_for_script(script: &ScriptBuf, network: Network) -> Option<crate::Address> {
    bitcoin::Address::from_script(script, network.native())
        .ok()
        .map(|address| crate::Address::from_encoded(address.to_string()))
}

fn parse_script(object: &Map<String, Value>) -> Result<ScriptBuf, ParseError> {
    let hex = object
        .get("hex")
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::new("Bitcoin scriptPubKey hex is missing or invalid"))?;
    let bytes = Vec::<u8>::from_hex(hex)
        .map_err(|_| ParseError::new("Bitcoin scriptPubKey hex is invalid"))?;
    Ok(ScriptBuf::from_bytes(bytes))
}

fn as_object<'a>(
    value: &'a Value,
    message: &'static str,
) -> Result<&'a Map<String, Value>, ParseError> {
    value.as_object().ok_or_else(|| ParseError::new(message))
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<&'a str, ParseError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ParseError::new(format!("{context} is missing or invalid")))
}

fn required_u64(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<u64, ParseError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| ParseError::new(format!("{context} is missing or invalid")))
}

fn required_u32(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<u32, ParseError> {
    required_u64(object, field, context).and_then(|value| {
        u32::try_from(value).map_err(|_| ParseError::new(format!("{context} exceeds u32")))
    })
}

fn required_bool(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<bool, ParseError> {
    object
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| ParseError::new(format!("{context} is missing or invalid")))
}

fn parse_txid(value: &str) -> Result<TransactionId, ParseError> {
    value
        .parse::<TransactionId>()
        .map_err(|_| ParseError::new("Bitcoin transaction ID is invalid"))
}

fn parse_block_hash(value: &str) -> Result<BlockHash, ParseError> {
    value
        .parse::<NativeBlockHash>()
        .map(|hash| BlockHash(hash.to_byte_array().to_vec()))
        .map_err(|_| ParseError::new("Bitcoin block hash is invalid"))
}

fn parse_btc_amount(value: &Value, context: &'static str) -> Result<u64, ParseError> {
    let lexical = value
        .as_number()
        .map(Number::to_string)
        .ok_or_else(|| ParseError::new(format!("{context} must be a JSON number")))?;
    if lexical.starts_with('-') || lexical.contains(['e', 'E', '+']) {
        return Err(ParseError::new(format!(
            "{context} must be a non-negative fixed-point decimal"
        )));
    }
    let mut parts = lexical.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 8
    {
        return Err(ParseError::new(format!(
            "{context} is not an exact Bitcoin amount"
        )));
    }
    let whole = whole
        .parse::<u64>()
        .map_err(|_| ParseError::new(format!("{context} exceeds u64 satoshis")))?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        let value = fraction
            .parse::<u64>()
            .map_err(|_| ParseError::new(format!("{context} is invalid")))?;
        let padding = u32::try_from(8_usize.saturating_sub(fraction.len()))
            .map_err(|_| ParseError::new(format!("{context} precision is invalid")))?;
        value
            .checked_mul(10_u64.pow(padding))
            .ok_or_else(|| ParseError::new(format!("{context} exceeds u64 satoshis")))?
    };
    whole
        .checked_mul(SATOSHIS_PER_BITCOIN)
        .and_then(|value| value.checked_add(fractional))
        .ok_or_else(|| ParseError::new(format!("{context} exceeds u64 satoshis")))
}
