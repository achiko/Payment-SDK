use std::fmt;

use alloy_primitives::{U256, hex};
use indexing::SourceError;

use crate::{TransactionId, Wei};

use super::transport::{Error, Failure};

pub(crate) enum CallError {
    Local(SourceError),
    Remote(Failure),
}

impl CallError {
    pub(super) fn is_already_known(&self) -> bool {
        let Self::Remote(failure) = self else {
            return false;
        };
        let message = failure.message.to_ascii_lowercase();
        message.contains("already known") || message.contains("known transaction")
    }

    pub(super) fn execution_revert_code(&self) -> Option<i64> {
        let Self::Remote(failure) = self else {
            return None;
        };
        let message = failure.message.to_ascii_lowercase();
        (message.contains("execution reverted") || message.contains("execution revert"))
            .then_some(failure.code)
    }

    pub(super) fn into_source(self, method: &'static str) -> SourceError {
        match self {
            Self::Local(error) => error,
            Self::Remote(failure) => {
                let retryable = matches!(failure.code, -32_605 | -32_603 | -32_005) || {
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
                };
                source_error(
                    format!(
                        "Ethereum JSON-RPC {method} failed with code {}",
                        failure.code
                    ),
                    retryable,
                )
            }
        }
    }
}

// design-lint: allow unclassified-free-function -- shared Ethereum RPC u64 quantity codec validates strict wire syntax before bounded native decoding for block, nonce and gas values without inventing a numeric wrapper
pub(super) fn parse_quantity_u64(value: &str) -> Result<u64, &'static str> {
    let digits = quantity_digits(value)?;
    u64::from_str_radix(digits, 16).map_err(|_| "hex quantity exceeds u64")
}

impl Wei {
    pub(super) fn to_quantity(&self) -> String {
        format!("{:#x}", U256::from_be_bytes(self.0))
    }

    pub(super) fn from_quantity(value: &str) -> Result<Self, &'static str> {
        let digits = quantity_digits(value)?;
        if digits.len() > 64 {
            return Err("hex quantity exceeds 256 bits");
        }
        U256::from_str_radix(digits, 16)
            .map(|value| Self(value.to_be_bytes()))
            .map_err(|_| "hex data contains invalid data")
    }
}

// design-lint: allow unclassified-free-function -- shared Ethereum RPC QUANTITY grammar validates prefix, nonempty digits, leading zero and ASCII hex before distinct numeric width checks
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

// design-lint: allow unclassified-free-function -- shared Ethereum RPC fixed-width DATA codec validates lowercase prefix and exact byte width for hashes and ABI words before library decoding
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

// design-lint: allow unclassified-free-function -- Ethereum RPC DATA codec preserves lowercase-prefix and byte-parity checks before library decoding with stable wire errors
pub(super) fn parse_data(value: &str) -> Result<Vec<u8>, &'static str> {
    let digits = value
        .strip_prefix("0x")
        .ok_or("hex data has no 0x prefix")?;
    if digits.len() % 2 != 0 {
        return Err("hex data has an invalid length");
    }
    // The decoder removes one prefix, so pass the original input to keep a
    // second prefix invalid rather than accepting it as another optional prefix.
    hex::decode(value).map_err(|_| "hex data contains invalid data")
}

impl TransactionId {
    pub(super) fn from_rpc(value: &str, method: &'static str) -> Result<Self, SourceError> {
        parse_fixed_data::<32>(value, "transaction hash")
            .map(Self)
            .map_err(|message| invalid_rpc_response(method, message))
    }
}

// design-lint: allow unclassified-free-function -- translates foreign JSON-RPC errors into foreign indexing source errors while preserving display text and retryability at the Ethereum RPC boundary
pub(super) fn map_json_rpc_error(error: Error) -> SourceError {
    source_error(error.to_string(), error.is_retryable())
}

// design-lint: allow unclassified-free-function -- shared Ethereum RPC response validation adds method context to foreign SourceError values and keeps malformed responses nonretryable
pub(super) fn invalid_rpc_response(
    method: &'static str,
    message: impl fmt::Display,
) -> SourceError {
    source_error(
        format!("Ethereum RPC {method} returned an invalid response: {message}"),
        false,
    )
}

// design-lint: allow unclassified-free-function -- shared Ethereum RPC boundary constructs foreign SourceError values while each wire decoder and RPC caller retains its message and retryability policy
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
    fn remote_retry_policy_preserves_codes_ascii_substrings_and_redaction() {
        for (code, message, retryable) in [
            (-32_605, "unrelated", true),
            (-32_603, "unrelated", true),
            (-32_005, "unrelated", true),
            (-32_000, "unrelated", false),
            (429, "unrelated", false),
            (3, "execution reverted", false),
            (3, "RATE LIMIT", true),
            (3, "prefix TOO MANY REQUESTS suffix", true),
            (3, "TEMPORARILY UNAVAILABLE", true),
            (3, "timeout", true),
            (3, "timed out", true),
            (3, "try again", true),
            (3, "overloaded", true),
            (3, "rate\tlimit", false),
            (3, "tímeout", false),
        ] {
            let error = CallError::Remote(Failure {
                code,
                message: message.to_owned(),
                data: Some(
                    json_rpc::RawJson::from_serializable(&serde_json::json!({"secret":"hidden"}))
                        .unwrap(),
                ),
            })
            .into_source("eth_call");
            assert_eq!(error.retryable, retryable, "{code}: {message}");
            assert_eq!(
                error.message,
                format!("Ethereum JSON-RPC eth_call failed with code {code}")
            );
        }
        for retryable in [false, true] {
            let error = CallError::Local(SourceError {
                message: "rate limit local message".into(),
                retryable,
            })
            .into_source("eth_call");
            assert_eq!(error.retryable, retryable);
            assert_eq!(error.message, "rate limit local message");
        }
    }

    #[test]
    fn quantities_validate_syntax_before_numeric_width() {
        assert_eq!(parse_quantity_u64("0xffffffffffffffff"), Ok(u64::MAX));
        assert_eq!(
            parse_quantity_u64("0x10000000000000000"),
            Err("hex quantity exceeds u64")
        );
        for (text, message) in [
            ("0X1", "hex quantity has no 0x prefix"),
            ("0x", "hex quantity is empty"),
            ("0x0g", "hex quantity contains a leading zero"),
            ("0x_", "hex quantity contains invalid data"),
            ("0x1_0", "hex quantity contains invalid data"),
            ("0xé", "hex quantity contains invalid data"),
            ("0x0x1", "hex quantity contains a leading zero"),
            ("0x10000000000000000g", "hex quantity contains invalid data"),
        ] {
            assert_eq!(parse_quantity_u64(text), Err(message));
            assert_eq!(Wei::from_quantity(text), Err(message));
        }
    }

    #[test]
    fn rpc_transaction_id_keeps_mixed_case_bytes_and_method_errors() {
        let value = format!("0x{}", "aB".repeat(32));
        assert_eq!(
            TransactionId::from_rpc(&value, "eth_sendRawTransaction").unwrap(),
            TransactionId([0xab; 32])
        );
        for (text, reason) in [
            ("0X00", "hex data has no 0x prefix"),
            ("0xzz", "hex data has an invalid length"),
        ] {
            let error = TransactionId::from_rpc(text, "eth_getTransactionByHash").unwrap_err();
            assert_eq!(
                error.message,
                format!(
                    "Ethereum RPC eth_getTransactionByHash returned an invalid response: {reason}"
                )
            );
            assert!(!error.retryable);
        }
        let error =
            TransactionId::from_rpc(&format!("0x{}", "zz".repeat(32)), "eth_sendRawTransaction")
                .unwrap_err();
        assert_eq!(
            error.message,
            "Ethereum RPC eth_sendRawTransaction returned an invalid response: hex data contains invalid data"
        );
        assert!(!error.retryable);
    }

    #[test]
    fn call_error_classification_uses_only_remote_ascii_message_matching() {
        for (message, known, revert) in [
            ("ALREADY KNOWN", true, false),
            ("prefix Known Transaction suffix", true, false),
            ("unknown transaction", true, false),
            ("EXECUTION REVERTED: reason", false, true),
            ("prefix execution revert suffix", false, true),
            ("execution failed", false, false),
            ("already\tknown", false, false),
            ("ÉXECUTION REVERTED", false, false),
        ] {
            let error = CallError::Remote(Failure {
                code: -32_000,
                message: message.to_owned(),
                data: None,
            });
            assert_eq!(error.is_already_known(), known);
            assert_eq!(error.execution_revert_code(), revert.then_some(-32_000));
        }
        let local = CallError::Local(SourceError {
            message: "already known execution reverted".to_owned(),
            retryable: true,
        });
        assert!(!local.is_already_known());
        assert_eq!(local.execution_revert_code(), None);
        let converted = local.into_source("eth_call");
        assert_eq!(converted.message, "already known execution reverted");
        assert!(converted.retryable);
    }

    #[test]
    fn transport_adapter_preserves_message_and_exact_retryability() {
        for (kind, retryable) in [
            (json_rpc::ErrorKind::InvalidConfiguration, false),
            (json_rpc::ErrorKind::InvalidRequest, false),
            (json_rpc::ErrorKind::Timeout, true),
            (json_rpc::ErrorKind::Unavailable, true),
            (json_rpc::ErrorKind::HttpStatus(429), true),
            (json_rpc::ErrorKind::HttpStatus(500), false),
            (json_rpc::ErrorKind::HttpStatus(502), true),
            (json_rpc::ErrorKind::HttpStatus(503), true),
            (json_rpc::ErrorKind::HttpStatus(504), true),
            (json_rpc::ErrorKind::ResponseTooLarge, false),
            (json_rpc::ErrorKind::InvalidResponse, false),
        ] {
            let source = map_json_rpc_error(Error {
                kind,
                message: "transport result".to_owned(),
            });
            assert_eq!(source.message, "transport result");
            assert_eq!(source.retryable, retryable);
        }
    }

    #[test]
    fn invalid_response_adapter_keeps_method_context_and_terminal_classification() {
        let error = invalid_rpc_response("eth_call", "hex data has an invalid length");
        assert_eq!(
            error.message,
            "Ethereum RPC eth_call returned an invalid response: hex data has an invalid length"
        );
        assert!(!error.retryable);
    }

    #[test]
    fn quantities_encode_zero_odd_nibbles_and_the_entire_256_bit_range() {
        for (value, expected) in [
            (Wei::ZERO, "0x0".to_owned()),
            (Wei::from_u128(15), "0xf".to_owned()),
            (Wei::from_u128(16), "0x10".to_owned()),
            (Wei::from_u128(0xabc), "0xabc".to_owned()),
            (Wei::from_u128(u128::MAX), format!("0x{}", "f".repeat(32))),
            (Wei([255; 32]), format!("0x{}", "f".repeat(64))),
        ] {
            assert_eq!(value.to_quantity(), expected);
            assert_eq!(Wei::from_quantity(&expected), Ok(value));
        }
        let mut value = [0; 32];
        value[0] = 1;
        value[31] = 0xab;
        assert_eq!(
            Wei(value).to_quantity(),
            format!("0x1{}ab", "00".repeat(30))
        );
    }

    #[test]
    fn variable_data_preserves_bytes_and_exact_validation_errors() {
        assert_eq!(parse_data("0x"), Ok(Vec::new()));
        assert_eq!(parse_data("0x000aBcFF"), Ok(vec![0, 10, 188, 255]));
        let every_byte: Vec<u8> = (0..=255).collect();
        assert_eq!(
            parse_data(&hex::encode_prefixed(&every_byte)),
            Ok(every_byte)
        );
        for (input, message) in [
            ("", "hex data has no 0x prefix"),
            ("0X00", "hex data has no 0x prefix"),
            ("0xg", "hex data has an invalid length"),
            ("0x€", "hex data has an invalid length"),
            ("0xé", "hex data contains invalid data"),
            ("0x0x", "hex data contains invalid data"),
            ("0x0x12", "hex data contains invalid data"),
            ("0x0X12", "hex data contains invalid data"),
            ("0x+1", "hex data contains invalid data"),
            ("0x 1", "hex data contains invalid data"),
            ("0xzz", "hex data contains invalid data"),
        ] {
            assert_eq!(parse_data(input), Err(message));
        }
    }

    #[test]
    fn quantities_preserve_big_endian_values_and_strict_wire_syntax() {
        for (encoded, expected) in [
            ("0x0", Wei::from_u128(0)),
            ("0xf", Wei::from_u128(15)),
            ("0xAbC", Wei::from_u128(0xabc)),
        ] {
            assert_eq!(Wei::from_quantity(encoded), Ok(expected));
        }
        assert_eq!(
            Wei::from_quantity(&format!("0x{}", "f".repeat(64))),
            Ok(Wei([255; 32]))
        );
        for (encoded, message) in [
            ("1", "hex quantity has no 0x prefix"),
            ("0x", "hex quantity is empty"),
            ("0x01", "hex quantity contains a leading zero"),
            ("0xg", "hex quantity contains invalid data"),
            ("0xé", "hex quantity contains invalid data"),
        ] {
            assert_eq!(Wei::from_quantity(encoded), Err(message));
        }
        assert_eq!(
            Wei::from_quantity(&format!("0x1{}", "0".repeat(64))),
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
            TransactionId(hash).to_string(),
            format!("0x{}abcd", "00".repeat(30))
        );
    }
}
