use std::{error::Error, fmt};

use base::{Decimal, DecimalErrorKind};

pub(crate) const DECIMALS: u32 = 9;

/// Exact native Solana atomic value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lamport(u64);

impl Lamport {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_atomic(value: u64) -> Self {
        Self(value)
    }

    /// Converts a positive public SOL amount without rounding or truncation.
    pub fn from_decimal(amount: &Decimal) -> Result<Self, LamportError> {
        let value = amount
            .to_atomic_u64(DECIMALS)
            .map_err(LamportError::from_decimal)?;
        if value == 0 {
            return Err(LamportError::new(
                LamportErrorKind::Zero,
                "native SOL amount must be greater than zero",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn atomic(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn decimal(self) -> Decimal {
        Decimal::from_atomic(self.0.into(), DECIMALS)
    }

    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    #[must_use]
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LamportErrorKind {
    Invalid,
    Zero,
    Negative,
    ExcessPrecision,
    Overflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LamportError {
    kind: LamportErrorKind,
    message: String,
}

impl LamportError {
    fn new(kind: LamportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn from_decimal(error: base::DecimalError) -> Self {
        let kind = match error.kind {
            DecimalErrorKind::Invalid => LamportErrorKind::Invalid,
            DecimalErrorKind::NegativeAmount => LamportErrorKind::Negative,
            DecimalErrorKind::ExcessPrecision => LamportErrorKind::ExcessPrecision,
            DecimalErrorKind::Overflow => LamportErrorKind::Overflow,
        };
        Self::new(kind, error.message)
    }

    #[must_use]
    pub const fn kind(&self) -> LamportErrorKind {
        self.kind
    }
}

impl fmt::Display for LamportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for LamportError {}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn converts_exact_decimal_boundaries_without_floating_point() {
        let cases = [
            ("1", 1_000_000_000_u64, "1"),
            ("0.000000001", 1, "0.000000001"),
            ("1.0000000000", 1_000_000_000, "1"),
            ("18446744073.709551615", u64::MAX, "18446744073.709551615"),
        ];

        for (input, atomic, rendered) in cases {
            let amount = Decimal::from_str(input).expect("fixture decimal must parse");
            let lamports = Lamport::from_decimal(&amount).expect("exact amount must convert");
            assert_eq!(lamports.atomic(), atomic);
            assert_eq!(lamports.decimal().to_string(), rendered);
        }
    }

    #[test]
    fn rejects_zero_negative_fractional_and_overflow_amounts() {
        let cases = [
            ("0", LamportErrorKind::Zero),
            ("-0.000000001", LamportErrorKind::Negative),
            ("0.0000000001", LamportErrorKind::ExcessPrecision),
            ("18446744073.709551616", LamportErrorKind::Overflow),
        ];

        for (input, kind) in cases {
            let amount = Decimal::from_str(input).expect("fixture decimal must parse");
            assert_eq!(
                Lamport::from_decimal(&amount)
                    .expect_err("invalid native amount must fail")
                    .kind(),
                kind
            );
        }
    }

    #[test]
    fn atomic_zero_is_valid_for_observed_balances() {
        assert_eq!(Lamport::ZERO.atomic(), 0);
        assert_eq!(Lamport::ZERO.decimal(), Decimal::zero());
    }

    #[test]
    fn checked_arithmetic_never_wraps() {
        assert_eq!(
            Lamport::from_atomic(2).checked_add(Lamport::from_atomic(3)),
            Some(Lamport::from_atomic(5))
        );
        assert_eq!(
            Lamport::from_atomic(u64::MAX).checked_add(Lamport::from_atomic(1)),
            None
        );
        assert_eq!(
            Lamport::from_atomic(5).checked_sub(Lamport::from_atomic(3)),
            Some(Lamport::from_atomic(2))
        );
        assert_eq!(
            Lamport::from_atomic(0).checked_sub(Lamport::from_atomic(1)),
            None
        );
    }
}
