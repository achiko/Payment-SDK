use std::str::FromStr;

use base::Decimal;
use indexing::{IndexError, IndexErrorKind};

/// Stable storage representation for an exact monetary value.
///
/// Canonical base-10 text is independent of the in-memory big-integer
/// implementation. The surrounding repository and projection record versions
/// identify this representation on disk.
pub(super) fn encode(value: &Decimal) -> String {
    value.to_string()
}

pub(super) fn decode(encoded: &str) -> Result<Decimal, IndexError> {
    let value = Decimal::from_str(encoded)
        .map_err(|_| amount_error("stored amount is not a valid decimal"))?;
    if value.to_string() != encoded {
        return Err(amount_error("stored amount is not canonical"));
    }
    value
        .to_atomic(value.scale())
        .map_err(|_| amount_error("stored monetary amount must not be negative"))?;
    Ok(value)
}

fn amount_error(message: &'static str) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}

#[cfg(test)]
mod tests {
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
        for encoded in ["-1", "+1", "01", "1.0", "not-a-number"] {
            assert!(decode(encoded).is_err(), "{encoded} must be rejected");
        }
    }
}
