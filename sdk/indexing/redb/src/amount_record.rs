use std::str::FromStr;

use base::Decimal;
use indexing::IndexError;

/// Stable storage representation for an exact monetary value.
///
/// Canonical base-10 text is independent of the in-memory big-integer
/// implementation. The surrounding private repository record identifies this
/// representation on disk.
pub(super) fn encode(value: &Decimal) -> String {
    value.to_string()
}

pub(super) fn decode(encoded: &str) -> Result<Decimal, IndexError> {
    let value = Decimal::from_str(encoded)
        .map_err(|_| crate::Repository::record_error("stored amount is not a valid decimal"))?;
    if value.to_string() != encoded {
        return Err(crate::Repository::record_error(
            "stored amount is not canonical",
        ));
    }
    value.to_atomic(value.scale()).map_err(|_| {
        crate::Repository::record_error("stored monetary amount must not be negative")
    })?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use indexing::IndexErrorKind;

    use super::*;

    #[test]
    fn round_trips_large_scaled_values_exactly() {
        let value =
            Decimal::from_str("1234567890123456789012345678901234567890.000000000000000001")
                .expect("test decimal must parse");

        assert_eq!(decode(&encode(&value)).expect("amount must decode"), value);
    }

    #[test]
    fn rejects_negative_and_noncanonical_values() {
        for (encoded, message) in [
            ("-1", "stored monetary amount must not be negative"),
            ("+1", "stored amount is not canonical"),
            ("01", "stored amount is not canonical"),
            ("1.0", "stored amount is not canonical"),
            ("not-a-number", "stored amount is not a valid decimal"),
        ] {
            let error = decode(encoded).expect_err("invalid stored amount must be rejected");
            assert_eq!(error.kind, IndexErrorKind::Store, "{encoded}");
            assert_eq!(error.message, message, "{encoded}");
            assert!(!error.retryable, "{encoded}");
        }
    }
}
