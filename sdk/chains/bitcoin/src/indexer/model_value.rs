use serde_json::{Map, Number, Value};

use crate::Satoshi;

use super::ParseError;

const SATOSHIS_PER_BITCOIN: u64 = 100_000_000;

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

impl Satoshi {
    pub(super) fn from_block_json(
        value: &Value,
        context: &'static str,
    ) -> Result<Self, ParseError> {
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
            .map(Self)
            .ok_or_else(|| ParseError::new(format!("{context} exceeds u64 satoshis")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_amount_json_keeps_exact_precision_and_range() {
        for (json, expected) in [
            ("0", 0),
            ("0.00000001", 1),
            ("1.23000000", 123_000_000),
            ("184467440737.09551615", u64::MAX),
        ] {
            let value = serde_json::from_str(json).unwrap();
            assert_eq!(
                Satoshi::from_block_json(&value, "amount").unwrap(),
                Satoshi(expected)
            );
        }
    }

    #[test]
    fn block_amount_json_keeps_rejection_context() {
        for (json, suffix) in [
            ("null", "must be a JSON number"),
            ("\"1\"", "must be a JSON number"),
            ("true", "must be a JSON number"),
            ("{}", "must be a JSON number"),
            ("[]", "must be a JSON number"),
            ("-1", "must be a non-negative fixed-point decimal"),
            ("1e0", "must be a non-negative fixed-point decimal"),
            ("1E2", "must be a non-negative fixed-point decimal"),
            ("0.000000001", "is not an exact Bitcoin amount"),
            ("1.000000000", "is not an exact Bitcoin amount"),
            ("184467440737.09551616", "exceeds u64 satoshis"),
            ("184467440738", "exceeds u64 satoshis"),
            ("18446744073709551616", "exceeds u64 satoshis"),
        ] {
            let value = serde_json::from_str(json).unwrap();
            let error = Satoshi::from_block_json(&value, "amount").unwrap_err();
            assert_eq!(error.to_string(), format!("amount {suffix}"), "{json}");
        }
    }
}
