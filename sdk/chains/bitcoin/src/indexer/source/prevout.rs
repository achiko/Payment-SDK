use std::collections::{BTreeMap, BTreeSet};

use bitcoin::{Transaction, consensus, hex::FromHex};
use indexing::SourceError;
use serde_json::{Map, Value};

use crate::TransactionId;
use crate::rpc::source_error;

use super::{
    MAX_COMPACT_ADDRESS_BYTES, MAX_COMPACT_PREVOUT_JSON_BYTES, MAX_EXTERNAL_PREVOUTS_PER_BLOCK,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ResolvedOutput {
    pub(super) value_satoshis: u64,
    pub(super) address: Option<crate::Address>,
}

impl ResolvedOutput {
    pub(super) fn compact_json(&self) -> Result<Value, SourceError> {
        validate_compact_address(self.address.as_ref())?;
        let prevout = serde_json::json!({
            "value_satoshis": self.value_satoshis,
            "address": self.address.as_ref().map(|address| address.encoded()),
        });
        let encoded_length = serde_json::to_vec(&prevout)
            .map_err(|_| source_error("Bitcoin compact prevout JSON could not be encoded", true))?
            .len();
        if encoded_length > MAX_COMPACT_PREVOUT_JSON_BYTES {
            return Err(source_error(
                "Bitcoin compact prevout JSON exceeds its per-input bound",
                false,
            ));
        }
        Ok(prevout)
    }
}

pub(super) fn decode_consensus_transaction(
    object: &Map<String, Value>,
    expected_id: TransactionId,
) -> Result<Transaction, SourceError> {
    let raw = Vec::<u8>::from_hex(required_string(object, "hex", "Bitcoin raw transaction")?)
        .map_err(|_| source_error("Bitcoin transaction hex is invalid", true))?;
    let transaction: Transaction = consensus::deserialize(&raw)
        .map_err(|_| source_error("Bitcoin transaction consensus bytes are invalid", true))?;
    if TransactionId::from(transaction.compute_txid()) != expected_id {
        return Err(source_error(
            "Bitcoin transaction ID does not match its consensus bytes",
            true,
        ));
    }
    Ok(transaction)
}

pub(super) fn validate_input_claims(
    value: &Value,
    transaction: &Transaction,
) -> Result<(), SourceError> {
    let inputs = value
        .as_object()
        .and_then(|object| object.get("vin"))
        .and_then(Value::as_array)
        .ok_or_else(|| source_error("Bitcoin transaction inputs must be an array", true))?;
    if inputs.len() != transaction.input.len() {
        return Err(source_error(
            "Bitcoin transaction input count does not match its consensus bytes",
            true,
        ));
    }
    for (index, (input, native_input)) in inputs.iter().zip(&transaction.input).enumerate() {
        let object = input
            .as_object()
            .ok_or_else(|| source_error("Bitcoin transaction input must be an object", true))?;
        if native_input.previous_output.is_null() {
            if !transaction.is_coinbase()
                || object.get("coinbase").and_then(Value::as_str).is_none()
            {
                return Err(source_error(
                    "Bitcoin null input is not a valid coinbase input",
                    true,
                ));
            }
            continue;
        }
        let previous_id = required_string(object, "txid", "Bitcoin input previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| source_error("Bitcoin input previous transaction ID is invalid", true))?;
        let output_index = required_u32(object, "vout", "Bitcoin input output index")?;
        if bitcoin::Txid::from(previous_id) != native_input.previous_output.txid
            || output_index != native_input.previous_output.vout
        {
            return Err(source_error(
                format!("Bitcoin input {index} outpoint does not match its consensus bytes"),
                true,
            ));
        }
    }
    Ok(())
}

pub(super) fn record_external_prevout(
    outputs: &mut BTreeMap<TransactionId, BTreeSet<u32>>,
    count: &mut usize,
    transaction_id: TransactionId,
    output_index: u32,
) -> Result<(), SourceError> {
    if outputs
        .get(&transaction_id)
        .is_some_and(|indexes| indexes.contains(&output_index))
    {
        return Err(source_error(
            "Bitcoin block spends the same external outpoint more than once",
            false,
        ));
    }
    if *count >= MAX_EXTERNAL_PREVOUTS_PER_BLOCK {
        return Err(source_error(
            format!(
                "Bitcoin block exceeds the {MAX_EXTERNAL_PREVOUTS_PER_BLOCK} external-prevout safety bound"
            ),
            false,
        ));
    }
    outputs
        .entry(transaction_id)
        .or_default()
        .insert(output_index);
    *count = count
        .checked_add(1)
        .ok_or_else(|| source_error("Bitcoin external prevout count overflowed", false))?;
    Ok(())
}

pub(super) fn validate_compact_address(
    address: Option<&crate::Address>,
) -> Result<(), SourceError> {
    let Some(address) = address else {
        return Ok(());
    };
    if address.encoded().len() > MAX_COMPACT_ADDRESS_BYTES
        || !address
            .encoded()
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(source_error(
            "Bitcoin canonical prevout address exceeds the compact data bound",
            false,
        ));
    }
    Ok(())
}

pub(super) fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<&'a str, SourceError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| source_error(format!("{context} is missing or invalid"), true))
}

pub(super) fn required_u32(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<u32, SourceError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| source_error(format!("{context} is missing or invalid"), true))
        .and_then(|value| {
            u32::try_from(value).map_err(|_| source_error(format!("{context} exceeds u32"), true))
        })
}
