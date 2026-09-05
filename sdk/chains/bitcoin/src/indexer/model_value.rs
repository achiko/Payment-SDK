use bitcoin::{BlockHash as NativeBlockHash, ScriptBuf, hashes::Hash, hex::FromHex};
use indexing::BlockHash;
use serde_json::{Map, Number, Value};

use crate::TransactionId;

use super::ParseError;

const SATOSHIS_PER_BITCOIN: u64 = 100_000_000;

pub(super) fn parse_script(object: &Map<String, Value>) -> Result<ScriptBuf, ParseError> {
    let hex = object
        .get("hex")
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::new("Bitcoin scriptPubKey hex is missing or invalid"))?;
    let bytes = Vec::<u8>::from_hex(hex)
        .map_err(|_| ParseError::new("Bitcoin scriptPubKey hex is invalid"))?;
    Ok(ScriptBuf::from_bytes(bytes))
}

pub(super) fn required_string<'a>(
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

pub(super) fn required_u64(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<u64, ParseError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| ParseError::new(format!("{context} is missing or invalid")))
}

pub(super) fn required_u32(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<u32, ParseError> {
    required_u64(object, field, context).and_then(|value| {
        u32::try_from(value).map_err(|_| ParseError::new(format!("{context} exceeds u32")))
    })
}

pub(super) fn required_bool(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<bool, ParseError> {
    object
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| ParseError::new(format!("{context} is missing or invalid")))
}

pub(super) fn parse_txid(value: &str) -> Result<TransactionId, ParseError> {
    value
        .parse::<TransactionId>()
        .map_err(|_| ParseError::new("Bitcoin transaction ID is invalid"))
}

pub(super) fn parse_block_hash(value: &str) -> Result<BlockHash, ParseError> {
    value
        .parse::<NativeBlockHash>()
        .map(|hash| BlockHash(hash.to_byte_array().to_vec()))
        .map_err(|_| ParseError::new("Bitcoin block hash is invalid"))
}

pub(super) fn parse_btc_amount(value: &Value, context: &'static str) -> Result<u64, ParseError> {
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
