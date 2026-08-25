use std::fmt;

use indexing::{BlockRef, SourceError};
use serde_json::{Value, json};

use crate::{Address, TransactionId, Wei};

use super::{
    BASIS_POINTS_DENOMINATOR,
    transport::{Error, Failure},
};

pub(crate) enum CallError {
    Local(SourceError),
    Remote(Failure),
}

impl CallError {
    pub(super) fn into_source(self, method: &'static str) -> SourceError {
        match self {
            Self::Local(error) => error,
            Self::Remote(failure) => source_error(
                format!(
                    "Ethereum JSON-RPC {method} failed with code {}",
                    failure.code
                ),
                remote_failure_is_retryable(&failure),
            ),
        }
    }
}

pub(super) fn remote_failure_is_retryable(failure: &Failure) -> bool {
    if matches!(failure.code, -32_605 | -32_603 | -32_005) {
        return true;
    }
    let message = failure.message.to_ascii_lowercase();
    [
        "rate limit",
        "too many requests",
        "temporarily unavailable",
        "timeout",
        "timed out",
        "try again",
        "overloaded",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

pub(super) fn is_already_known(failure: &Failure) -> bool {
    let message = failure.message.to_ascii_lowercase();
    message.contains("already known") || message.contains("known transaction")
}

pub(super) fn is_execution_revert(failure: &Failure) -> bool {
    let message = failure.message.to_ascii_lowercase();
    message.contains("execution reverted") || message.contains("execution revert")
}

pub(super) fn block_parameter(at: Option<BlockRef>) -> Result<Value, SourceError> {
    let Some(block) = at else {
        return Ok(Value::String("pending".to_owned()));
    };
    if block.hash.0.len() != 32 {
        return Err(source_error(
            "Ethereum balance block hash must contain exactly 32 bytes",
            false,
        ));
    }
    Ok(json!({
        "blockHash": data_hex(&block.hash.0),
        "requireCanonical": true,
    }))
}

pub(super) fn parse_quantity_u64(value: &str) -> Result<u64, &'static str> {
    let digits = quantity_digits(value)?;
    u64::from_str_radix(digits, 16).map_err(|_| "hex quantity exceeds u64")
}

pub(super) fn parse_quantity_wei(value: &str) -> Result<Wei, &'static str> {
    let digits = quantity_digits(value)?;
    if digits.len() > 64 {
        return Err("hex quantity exceeds 256 bits");
    }
    decode_hex_right_aligned::<32>(digits).map(Wei)
}

pub(super) fn quantity_digits(value: &str) -> Result<&str, &'static str> {
    let digits = value
        .strip_prefix("0x")
        .ok_or("hex quantity has no 0x prefix")?;
    if digits.is_empty() {
        return Err("hex quantity is empty");
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return Err("hex quantity contains a leading zero");
    }
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("hex quantity contains invalid data");
    }
    Ok(digits)
}

pub(super) fn parse_fixed_data<const N: usize>(
    value: &str,
    _field: &'static str,
) -> Result<[u8; N], &'static str> {
    let digits = value
        .strip_prefix("0x")
        .ok_or("hex data has no 0x prefix")?;
    if digits.len() != N * 2 {
        return Err("hex data has an invalid length");
    }
    decode_hex_right_aligned(digits)
}

pub(super) fn parse_data(value: &str) -> Result<Vec<u8>, &'static str> {
    let digits = value
        .strip_prefix("0x")
        .ok_or("hex data has no 0x prefix")?;
    if digits.len() % 2 != 0 {
        return Err("hex data has an invalid length");
    }
    digits
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0]).ok_or("hex data contains invalid data")?;
            let low = hex_nibble(pair[1]).ok_or("hex data contains invalid data")?;
            Ok((high << 4) | low)
        })
        .collect()
}

pub(super) fn decode_hex_right_aligned<const N: usize>(
    digits: &str,
) -> Result<[u8; N], &'static str> {
    if digits.len() > N * 2 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("hex data contains invalid data");
    }
    let mut decoded = [0_u8; N];
    let byte_offset = N - digits.len().div_ceil(2);
    let mut output = byte_offset;
    let mut input = 0;
    if digits.len() % 2 == 1 {
        decoded[output] =
            hex_nibble(digits.as_bytes()[0]).ok_or("hex data contains invalid data")?;
        output += 1;
        input = 1;
    }
    while input < digits.len() {
        let high = hex_nibble(digits.as_bytes()[input]).ok_or("hex data contains invalid data")?;
        let low =
            hex_nibble(digits.as_bytes()[input + 1]).ok_or("hex data contains invalid data")?;
        decoded[output] = (high << 4) | low;
        output += 1;
        input += 2;
    }
    Ok(decoded)
}

pub(super) fn parse_transaction_id(
    value: &str,
    method: &'static str,
) -> Result<TransactionId, SourceError> {
    parse_fixed_data::<32>(value, "transaction hash")
        .map(TransactionId)
        .map_err(|message| invalid_rpc_response(method, message))
}

pub(super) fn gas_limit_with_margin(
    estimated: u64,
    margin_basis_points: u32,
) -> Result<u64, SourceError> {
    let numerator = u128::from(estimated)
        .checked_mul(u128::from(margin_basis_points))
        .ok_or_else(|| invalid_rpc_response("eth_estimateGas", "gas margin overflowed"))?;
    let margin = numerator
        .checked_add(u128::from(BASIS_POINTS_DENOMINATOR - 1))
        .map(|value| value / u128::from(BASIS_POINTS_DENOMINATOR))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| invalid_rpc_response("eth_estimateGas", "gas margin exceeds u64"))?;
    estimated
        .checked_add(margin)
        .ok_or_else(|| invalid_rpc_response("eth_estimateGas", "gas limit with margin exceeds u64"))
}

pub(super) fn wei_quantity(value: &Wei) -> String {
    let Some(first_non_zero) = value.0.iter().position(|byte| *byte != 0) else {
        return "0x0".to_owned();
    };
    let bytes = &value.0[first_non_zero..];
    let mut encoded = String::with_capacity(2 + bytes.len() * 2);
    encoded.push_str("0x");
    if bytes[0] < 16 {
        encoded.push(hex_digit(bytes[0]));
    } else {
        encoded.push(hex_digit(bytes[0] >> 4));
        encoded.push(hex_digit(bytes[0] & 0x0f));
    }
    for byte in &bytes[1..] {
        encoded.push(hex_digit(byte >> 4));
        encoded.push(hex_digit(byte & 0x0f));
    }
    encoded
}

pub(super) fn address_hex(address: &Address) -> String {
    data_hex(&address.0)
}

pub(super) fn transaction_id_hex(id: &TransactionId) -> String {
    data_hex(&id.0)
}

pub(super) fn data_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(2 + bytes.len() * 2);
    encoded.push_str("0x");
    for byte in bytes {
        encoded.push(hex_digit(byte >> 4));
        encoded.push(hex_digit(byte & 0x0f));
    }
    encoded
}

pub(super) fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(super) fn hex_digit(nibble: u8) -> char {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    char::from(HEX[usize::from(nibble & 0x0f)])
}

pub(super) fn map_json_rpc_error(error: Error) -> SourceError {
    source_error(error.to_string(), error.is_retryable())
}

pub(super) fn invalid_rpc_response(
    method: &'static str,
    message: impl fmt::Display,
) -> SourceError {
    source_error(
        format!("Ethereum RPC {method} returned an invalid response: {message}"),
        false,
    )
}

pub(super) fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}
