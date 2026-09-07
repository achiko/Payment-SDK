use std::fmt;

use alloy_primitives::{U256, hex};
use indexing::SourceError;

use crate::{TransactionId, Wei};

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

pub(super) fn parse_quantity_u64(value: &str) -> Result<u64, &'static str> {
    let digits = quantity_digits(value)?;
    u64::from_str_radix(digits, 16).map_err(|_| "hex quantity exceeds u64")
}

pub(super) fn parse_quantity_wei(value: &str) -> Result<Wei, &'static str> {
    let digits = quantity_digits(value)?;
    if digits.len() > 64 {
        return Err("hex quantity exceeds 256 bits");
    }
    U256::from_str_radix(digits, 16)
        .map(|value| Wei(value.to_be_bytes()))
        .map_err(|_| "hex data contains invalid data")
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
    hex::decode_to_array(digits).map_err(|_| "hex data contains invalid data")
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

pub(super) fn transaction_id_hex(id: &TransactionId) -> String {
    hex::encode_prefixed(id.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities_preserve_big_endian_values_and_strict_wire_syntax() {
        for (encoded, expected) in [
            ("0x0", Wei::from_u128(0)),
            ("0xf", Wei::from_u128(15)),
            ("0xAbC", Wei::from_u128(0xabc)),
        ] {
            assert_eq!(parse_quantity_wei(encoded), Ok(expected));
        }
        assert_eq!(
            parse_quantity_wei(&format!("0x{}", "f".repeat(64))),
            Ok(Wei([255; 32]))
        );
        for (encoded, message) in [
            ("1", "hex quantity has no 0x prefix"),
            ("0x", "hex quantity is empty"),
            ("0x01", "hex quantity contains a leading zero"),
            ("0xg", "hex quantity contains invalid data"),
            ("0xé", "hex quantity contains invalid data"),
        ] {
            assert_eq!(parse_quantity_wei(encoded), Err(message));
        }
        assert_eq!(
            parse_quantity_wei(&format!("0x1{}", "0".repeat(64))),
            Err("hex quantity exceeds 256 bits")
        );
    }

    #[test]
    fn fixed_data_preserves_leading_zeroes_and_requires_exact_width() {
        assert_eq!(parse_fixed_data::<3>("0x000aBc", "test"), Ok([0, 10, 188]));
        assert_eq!(parse_fixed_data::<0>("0x", "test"), Ok([]));
        assert_eq!(
            parse_fixed_data::<2>("0xabc", "test"),
            Err("hex data has an invalid length")
        );
        assert_eq!(
            parse_fixed_data::<2>("abcd", "test"),
            Err("hex data has no 0x prefix")
        );
        assert_eq!(
            parse_fixed_data::<2>("0x0x12", "test"),
            Err("hex data contains invalid data")
        );
        assert_eq!(
            parse_fixed_data::<2>("0x12zz", "test"),
            Err("hex data contains invalid data")
        );
        assert_eq!(
            parse_fixed_data::<1>("0xé", "test"),
            Err("hex data contains invalid data")
        );
        let mut hash = [0; 32];
        hash[30] = 0xab;
        hash[31] = 0xcd;
        assert_eq!(
            transaction_id_hex(&TransactionId(hash)),
            format!("0x{}abcd", "00".repeat(30))
        );
    }
}
