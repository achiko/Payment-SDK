use std::collections::{BTreeMap, BTreeSet};

use bitcoin::{Transaction, consensus, hex::FromHex};
use indexing::SourceError;
use serde_json::{Map, Value};

use crate::rpc::source_error;
use crate::{Address, TransactionId};

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
        self.address
            .as_ref()
            .map(Address::validate_compact_prevout)
            .transpose()?;
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

// design-lint: allow unclassified-free-function -- shared Bitcoin RPC-to-consensus conversion verifies JSON bytes against an independent expected txid and preserves source retryability without RPC policy on transaction IDs
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

// design-lint: allow unclassified-free-function -- Bitcoin RPC boundary compares independent foreign JSON and consensus input claims before prevout acquisition, preserving coinbase and outpoint validation order and source retryability
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

impl Address {
    pub(super) fn validate_compact_prevout(&self) -> Result<(), SourceError> {
        if self.encoded().len() > MAX_COMPACT_ADDRESS_BYTES
            || !self
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_prevout_preserves_optional_address_and_exact_satoshis() {
        for address in [None, Some(String::new()), Some("A1".repeat(64))] {
            let output = ResolvedOutput {
                value_satoshis: u64::MAX,
                address: address.as_ref().map(Address::from_encoded),
            };
            assert_eq!(
                output.compact_json().unwrap(),
                serde_json::json!({
                    "value_satoshis": u64::MAX,
                    "address": address,
                })
            );
        }
    }

    #[test]
    fn compact_prevout_rejects_oversized_and_non_alphanumeric_addresses() {
        for address in [
            "a".repeat(129),
            "a b".to_owned(),
            "a\"b".to_owned(),
            "é".to_owned(),
        ] {
            let error = ResolvedOutput {
                value_satoshis: 1,
                address: Some(Address::from_encoded(address)),
            }
            .compact_json()
            .unwrap_err();
            assert_eq!(
                error.message,
                "Bitcoin canonical prevout address exceeds the compact data bound"
            );
            assert!(!error.retryable);
        }
    }
}
