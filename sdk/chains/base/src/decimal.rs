use std::{error::Error, fmt, str::FromStr};

use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{Signed, Zero};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalErrorKind {
    Invalid,
    NegativeAmount,
    ExcessPrecision,
    Overflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecimalError {
    pub kind: DecimalErrorKind,
    pub message: String,
}

/// Sign component used by the stable, lossless [`DecimalParts`] representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalSign {
    Positive,
    Negative,
}

/// Canonical components for persistence without converting through text or
/// binary floating point.
///
/// `magnitude` is an unsigned, minimal-width, big-endian integer. Zero has an
/// empty magnitude and must use [`DecimalSign::Positive`]. The represented
/// value is `sign * magnitude * 10^-scale`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecimalParts {
    pub sign: DecimalSign,
    pub magnitude: Vec<u8>,
    pub scale: u32,
}

impl DecimalError {
    #[must_use]
    pub fn new(kind: DecimalErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid() -> Self {
        Self::new(
            DecimalErrorKind::Invalid,
            "decimal must use canonical base-10 notation",
        )
    }
}

impl fmt::Display for DecimalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DecimalError {}

/// Arbitrary-precision, base-10 fixed-point value.
///
/// No binary floating-point conversion is provided. Currency values therefore
/// remain exact regardless of their magnitude or number of fractional digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Decimal {
    coefficient: BigInt,
    scale: u32,
}

impl Decimal {
    #[must_use]
    pub fn zero() -> Self {
        Self::new(BigInt::ZERO, 0)
    }

    #[must_use]
    pub fn new(coefficient: BigInt, scale: u32) -> Self {
        Self::normalize(coefficient, scale)
    }

    #[must_use]
    pub fn from_atomic(units: BigUint, decimals: u32) -> Self {
        Self::new(BigInt::from(units), decimals)
    }

    #[must_use]
    pub fn coefficient(&self) -> &BigInt {
        &self.coefficient
    }

    #[must_use]
    pub const fn scale(&self) -> u32 {
        self.scale
    }

    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.coefficient.is_zero()
    }

    /// Returns canonical, lossless components suitable for a versioned
    /// persistence format.
    #[must_use]
    pub fn parts(&self) -> DecimalParts {
        DecimalParts {
            sign: if self.coefficient.is_negative() {
                DecimalSign::Negative
            } else {
                DecimalSign::Positive
            },
            magnitude: if self.coefficient.is_zero() {
                Vec::new()
            } else {
                self.coefficient.magnitude().to_bytes_be()
            },
            scale: self.scale,
        }
    }

    /// Restores a decimal from canonical persistence components.
    pub fn from_parts(parts: DecimalParts) -> Result<Self, DecimalError> {
        if parts.magnitude.first() == Some(&0) {
            return Err(DecimalError::new(
                DecimalErrorKind::Invalid,
                "decimal magnitude must use minimal-width big-endian encoding",
            ));
        }
        if parts.magnitude.is_empty() && parts.sign == DecimalSign::Negative {
            return Err(DecimalError::new(
                DecimalErrorKind::Invalid,
                "zero decimal must not use a negative sign",
            ));
        }

        let magnitude = BigUint::from_bytes_be(&parts.magnitude);
        let sign = match parts.sign {
            DecimalSign::Positive => Sign::Plus,
            DecimalSign::Negative => Sign::Minus,
        };
        Ok(Self::new(
            BigInt::from_biguint(sign, magnitude),
            parts.scale,
        ))
    }

    /// Adds two exact decimal values after aligning their base-10 scales.
    pub fn checked_add(&self, other: &Self) -> Result<Self, DecimalError> {
        let scale = self.scale.max(other.scale);
        let left = scaled_coefficient(self, scale)?;
        let right = scaled_coefficient(other, scale)?;
        Ok(Self::new(left + right, scale))
    }

    /// Subtracts two exact decimal values after aligning their base-10 scales.
    pub fn checked_sub(&self, other: &Self) -> Result<Self, DecimalError> {
        let scale = self.scale.max(other.scale);
        let left = scaled_coefficient(self, scale)?;
        let right = scaled_coefficient(other, scale)?;
        Ok(Self::new(left - right, scale))
    }

    /// Validates the invariant shared by monetary amounts: they may be zero,
    /// but never negative.
    pub fn validate_amount(&self) -> Result<(), DecimalError> {
        if self.coefficient.is_negative() {
            return Err(DecimalError::new(
                DecimalErrorKind::NegativeAmount,
                "currency amount must not be negative",
            ));
        }
        Ok(())
    }

    pub fn to_atomic(&self, decimals: u32) -> Result<BigUint, DecimalError> {
        self.validate_amount()?;

        let coefficient = self.coefficient.magnitude();
        let units = if self.scale <= decimals {
            coefficient * power_of_ten(decimals - self.scale)
        } else {
            let divisor = power_of_ten(self.scale - decimals);
            if coefficient % &divisor != BigUint::ZERO {
                return Err(DecimalError::new(
                    DecimalErrorKind::ExcessPrecision,
                    format!("amount has more than {decimals} fractional digits"),
                ));
            }
            coefficient / divisor
        };
        Ok(units)
    }

    pub fn to_atomic_u64(&self, decimals: u32) -> Result<u64, DecimalError> {
        let bytes = self.to_atomic(decimals)?.to_bytes_be();
        if bytes.len() > 8 {
            return Err(DecimalError::new(
                DecimalErrorKind::Overflow,
                "atomic amount exceeds the u64 range",
            ));
        }
        let mut value = [0_u8; 8];
        value[8 - bytes.len()..].copy_from_slice(&bytes);
        Ok(u64::from_be_bytes(value))
    }

    pub fn to_atomic_be_bytes<const N: usize>(
        &self,
        decimals: u32,
    ) -> Result<[u8; N], DecimalError> {
        let bytes = self.to_atomic(decimals)?.to_bytes_be();
        if bytes.len() > N {
            return Err(DecimalError::new(
                DecimalErrorKind::Overflow,
                format!("atomic amount exceeds {N} bytes"),
            ));
        }
        let mut value = [0_u8; N];
        value[N - bytes.len()..].copy_from_slice(&bytes);
        Ok(value)
    }

    fn normalize(mut coefficient: BigInt, mut scale: u32) -> Self {
        if coefficient.is_zero() {
            return Self {
                coefficient,
                scale: 0,
            };
        }
        let ten = BigInt::from(10_u8);
        while scale > 0 && (&coefficient % &ten).is_zero() {
            coefficient /= &ten;
            scale -= 1;
        }
        Self { coefficient, scale }
    }
}

impl From<u64> for Decimal {
    fn from(value: u64) -> Self {
        Self::new(BigInt::from(value), 0)
    }
}

impl FromStr for Decimal {
    type Err = DecimalError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.trim() != value {
            return Err(DecimalError::invalid());
        }
        let (negative, unsigned) = match value.as_bytes().first() {
            Some(b'-') => (true, &value[1..]),
            Some(b'+') => (false, &value[1..]),
            _ => (false, value),
        };
        let mut parts = unsigned.split('.');
        let whole = parts.next().unwrap_or_default();
        let fraction = parts.next();
        if parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|digits| {
                digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(DecimalError::invalid());
        }
        let fraction = fraction.unwrap_or_default();
        let scale = u32::try_from(fraction.len()).map_err(|_| DecimalError::invalid())?;
        let digits = format!("{whole}{fraction}");
        let magnitude =
            BigUint::parse_bytes(digits.as_bytes(), 10).ok_or_else(DecimalError::invalid)?;
        let sign = if negative && !magnitude.is_zero() {
            Sign::Minus
        } else {
            Sign::Plus
        };
        Ok(Self::new(BigInt::from_biguint(sign, magnitude), scale))
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.coefficient.is_negative();
        let mut digits = self.coefficient.magnitude().to_str_radix(10);
        if self.scale > 0 {
            let scale = usize::try_from(self.scale).map_err(|_| fmt::Error)?;
            if digits.len() <= scale {
                digits.insert_str(0, &"0".repeat(scale + 1 - digits.len()));
            }
            digits.insert(digits.len() - scale, '.');
        }
        if negative {
            formatter.write_str("-")?;
        }
        formatter.write_str(&digits)
    }
}

fn power_of_ten(exponent: u32) -> BigUint {
    BigUint::from(10_u8).pow(exponent)
}

fn scaled_coefficient(value: &Decimal, scale: u32) -> Result<BigInt, DecimalError> {
    let exponent = scale.checked_sub(value.scale).ok_or_else(|| {
        DecimalError::new(
            DecimalErrorKind::Invalid,
            "target scale must not be smaller than the decimal scale",
        )
    })?;
    Ok(&value.coefficient * BigInt::from(power_of_ten(exponent)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_human_units_to_exact_atomic_units() {
        assert_eq!(
            "1".parse::<Decimal>().unwrap().to_atomic_u64(8).unwrap(),
            100_000_000
        );
        assert_eq!(
            "1".parse::<Decimal>()
                .unwrap()
                .to_atomic(18)
                .unwrap()
                .to_string(),
            "1000000000000000000"
        );
        assert_eq!(
            "0.00000001"
                .parse::<Decimal>()
                .unwrap()
                .to_atomic_u64(8)
                .unwrap(),
            1
        );
    }

    #[test]
    fn preserves_unbounded_magnitude_and_exact_display() {
        let value = "123456789012345678901234567890.000000000000000001"
            .parse::<Decimal>()
            .unwrap();
        assert_eq!(
            value.to_string(),
            "123456789012345678901234567890.000000000000000001"
        );
    }

    #[test]
    fn rejects_negative_currency_and_excess_precision() {
        assert_eq!(
            "-1".parse::<Decimal>()
                .unwrap()
                .to_atomic(8)
                .unwrap_err()
                .kind,
            DecimalErrorKind::NegativeAmount
        );
        assert_eq!(
            "0.000000001"
                .parse::<Decimal>()
                .unwrap()
                .to_atomic(8)
                .unwrap_err()
                .kind,
            DecimalErrorKind::ExcessPrecision
        );
    }

    #[test]
    fn zero_is_canonical_and_nonnegative() {
        let zero = Decimal::zero();
        assert!(zero.is_zero());
        assert_eq!(zero.scale(), 0);
        assert_eq!(zero.to_string(), "0");
        assert_eq!(zero.validate_amount(), Ok(()));
    }

    #[test]
    fn checked_arithmetic_aligns_scales_and_normalizes_results() {
        let left = "12345678901234567890.125".parse::<Decimal>().unwrap();
        let right = "0.875".parse::<Decimal>().unwrap();
        assert_eq!(
            left.checked_add(&right).unwrap().to_string(),
            "12345678901234567891"
        );
        assert_eq!(
            right.checked_sub(&left).unwrap().to_string(),
            "-12345678901234567889.25"
        );
    }

    #[test]
    fn monetary_validation_rejects_negative_arithmetic_results() {
        let result = Decimal::from(1).checked_sub(&Decimal::from(2)).unwrap();
        assert_eq!(
            result.validate_amount().unwrap_err().kind,
            DecimalErrorKind::NegativeAmount
        );
    }

    #[test]
    fn persistence_parts_round_trip_without_precision_loss() {
        for value in [
            "0",
            "-0.000000000000000001",
            "123456789012345678901234567890.000000000000000001",
        ] {
            let decimal = value.parse::<Decimal>().unwrap();
            assert_eq!(Decimal::from_parts(decimal.parts()).unwrap(), decimal);
        }
    }

    #[test]
    fn persistence_parts_reject_noncanonical_sign_and_magnitude() {
        let negative_zero = DecimalParts {
            sign: DecimalSign::Negative,
            magnitude: Vec::new(),
            scale: 0,
        };
        assert_eq!(
            Decimal::from_parts(negative_zero).unwrap_err().kind,
            DecimalErrorKind::Invalid
        );

        let leading_zero = DecimalParts {
            sign: DecimalSign::Positive,
            magnitude: vec![0, 1],
            scale: 0,
        };
        assert_eq!(
            Decimal::from_parts(leading_zero).unwrap_err().kind,
            DecimalErrorKind::Invalid
        );
    }
}
