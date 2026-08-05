use std::{error::Error, fmt, str::FromStr};

const MAX_DECIMAL_DIGITS: usize = 78;

/// Unsigned integer in atomic units, encoded as a 256-bit big-endian magnitude.
/// Display precision belongs to asset metadata, never this value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AtomicAmount(pub [u8; 32]);

impl AtomicAmount {
    pub const ZERO: Self = Self([0; 32]);

    #[must_use]
    pub const fn zero() -> Self {
        Self::ZERO
    }

    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|byte| *byte == 0)
    }

    /// Adds two unsigned 256-bit amounts.
    ///
    /// # Errors
    ///
    /// Returns [`AtomicAmountArithmeticError::Overflow`] when the result cannot
    /// be represented by an unsigned 256-bit magnitude.
    pub fn checked_add(&self, other: &Self) -> Result<Self, AtomicAmountArithmeticError> {
        let mut result = [0; 32];
        let mut carry = 0_u16;

        for index in (0..result.len()).rev() {
            let sum = u16::from(self.0[index]) + u16::from(other.0[index]) + carry;
            result[index] = (sum & 0xff) as u8;
            carry = sum >> 8;
        }

        if carry == 0 {
            Ok(Self(result))
        } else {
            Err(AtomicAmountArithmeticError::Overflow)
        }
    }

    /// Subtracts one unsigned 256-bit amount from another.
    ///
    /// # Errors
    ///
    /// Returns [`AtomicAmountArithmeticError::Underflow`] when `other` is
    /// greater than `self`.
    pub fn checked_sub(&self, other: &Self) -> Result<Self, AtomicAmountArithmeticError> {
        let mut result = [0; 32];
        let mut borrow = 0_i16;

        for index in (0..result.len()).rev() {
            let difference = i16::from(self.0[index]) - i16::from(other.0[index]) - borrow;
            if difference < 0 {
                result[index] = (difference + 256) as u8;
                borrow = 1;
            } else {
                result[index] = difference as u8;
                borrow = 0;
            }
        }

        if borrow == 0 {
            Ok(Self(result))
        } else {
            Err(AtomicAmountArithmeticError::Underflow)
        }
    }

    /// Parses a canonical unsigned decimal string into a 256-bit amount.
    ///
    /// Canonical input is either `"0"` or a non-zero ASCII digit followed by
    /// zero or more ASCII digits. Signs, whitespace, separators, and leading
    /// zeroes are rejected.
    ///
    /// # Errors
    ///
    /// Returns a structured [`AtomicAmountParseError`] for invalid or
    /// out-of-range input.
    pub fn from_decimal_str(input: &str) -> Result<Self, AtomicAmountParseError> {
        parse_decimal(input)
    }

    /// Formats this amount as its canonical unsigned decimal representation.
    #[must_use]
    pub fn to_decimal_string(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for AtomicAmount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return formatter.write_str("0");
        }

        let mut magnitude = self.0;
        let mut digits = [0_u8; MAX_DECIMAL_DIGITS];
        let mut start = digits.len();

        while magnitude.iter().any(|byte| *byte != 0) {
            let mut remainder = 0_u16;
            for byte in &mut magnitude {
                let dividend = (remainder << 8) + u16::from(*byte);
                *byte = (dividend / 10) as u8;
                remainder = dividend % 10;
            }

            start -= 1;
            digits[start] = b'0' + remainder as u8;
        }

        let decimal = std::str::from_utf8(&digits[start..]).map_err(|_| fmt::Error)?;
        formatter.write_str(decimal)
    }
}

impl FromStr for AtomicAmount {
    type Err = AtomicAmountParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::from_decimal_str(input)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicAmountArithmeticError {
    Overflow,
    Underflow,
}

impl fmt::Display for AtomicAmountArithmeticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => {
                formatter.write_str("atomic amount addition overflowed the unsigned 256-bit range")
            }
            Self::Underflow => formatter
                .write_str("atomic amount subtraction underflowed the unsigned 256-bit range"),
        }
    }
}

impl Error for AtomicAmountArithmeticError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicAmountParseError {
    Empty,
    Negative,
    LeadingZero,
    InvalidCharacter { byte_index: usize, character: char },
    Overflow,
}

impl fmt::Display for AtomicAmountParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("atomic amount must not be empty"),
            Self::Negative => formatter.write_str("atomic amount must not be negative"),
            Self::LeadingZero => formatter.write_str(
                "atomic amount must use canonical decimal notation without leading zeroes",
            ),
            Self::InvalidCharacter {
                byte_index,
                character,
            } => write!(
                formatter,
                "atomic amount contains invalid character `{}` at byte {byte_index}",
                character.escape_default()
            ),
            Self::Overflow => {
                formatter.write_str("atomic amount exceeds the unsigned 256-bit maximum")
            }
        }
    }
}

impl Error for AtomicAmountParseError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SignedAtomicAmount {
    pub negative: bool,
    pub magnitude: AtomicAmount,
}

fn parse_decimal(input: &str) -> Result<AtomicAmount, AtomicAmountParseError> {
    if input.is_empty() {
        return Err(AtomicAmountParseError::Empty);
    }
    if input.starts_with('-') {
        return Err(AtomicAmountParseError::Negative);
    }

    for (byte_index, character) in input.char_indices() {
        if !character.is_ascii_digit() {
            return Err(AtomicAmountParseError::InvalidCharacter {
                byte_index,
                character,
            });
        }
    }

    if input.len() > 1 && input.starts_with('0') {
        return Err(AtomicAmountParseError::LeadingZero);
    }

    let mut magnitude = [0_u8; 32];
    for digit in input.bytes().map(|byte| byte - b'0') {
        let mut carry = u16::from(digit);
        for byte in magnitude.iter_mut().rev() {
            let product = u16::from(*byte) * 10 + carry;
            *byte = (product & 0xff) as u8;
            carry = product >> 8;
        }
        if carry != 0 {
            return Err(AtomicAmountParseError::Overflow);
        }
    }

    Ok(AtomicAmount(magnitude))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_DECIMAL: &str =
        "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    const ABOVE_MAX_DECIMAL: &str =
        "115792089237316195423570985008687907853269984665640564039457584007913129639936";

    fn one() -> AtomicAmount {
        let mut bytes = [0; 32];
        bytes[31] = 1;
        AtomicAmount(bytes)
    }

    #[test]
    fn zero_and_one_use_canonical_decimal() {
        assert_eq!(AtomicAmount::zero(), AtomicAmount::ZERO);
        assert!(AtomicAmount::ZERO.is_zero());
        assert!(!one().is_zero());
        assert_eq!(AtomicAmount::ZERO.to_decimal_string(), "0");
        assert_eq!(one().to_decimal_string(), "1");
        assert_eq!(AtomicAmount::from_decimal_str("0"), Ok(AtomicAmount::ZERO));
        assert_eq!(AtomicAmount::from_decimal_str("1"), Ok(one()));
    }

    #[test]
    fn maximum_value_round_trips_through_display_and_from_str() {
        let maximum = AtomicAmount([u8::MAX; 32]);

        assert_eq!(maximum.to_string(), MAX_DECIMAL);
        assert_eq!(MAX_DECIMAL.parse::<AtomicAmount>(), Ok(maximum));
    }

    #[test]
    fn decimal_conversion_preserves_values_across_the_full_width() {
        let mut bytes = [0; 32];
        bytes[0] = 1;
        bytes[15] = 0x80;
        bytes[31] = 0xff;
        let amount = AtomicAmount(bytes);
        let decimal = amount.to_decimal_string();

        assert_eq!(AtomicAmount::from_decimal_str(&decimal), Ok(amount));
        assert!(!decimal.starts_with('0'));
    }

    #[test]
    fn decimal_parser_returns_structured_input_errors() {
        assert_eq!(
            AtomicAmount::from_decimal_str(""),
            Err(AtomicAmountParseError::Empty)
        );
        assert_eq!(
            AtomicAmount::from_decimal_str("-1"),
            Err(AtomicAmountParseError::Negative)
        );
        assert_eq!(
            AtomicAmount::from_decimal_str("00"),
            Err(AtomicAmountParseError::LeadingZero)
        );
        assert_eq!(
            AtomicAmount::from_decimal_str("01"),
            Err(AtomicAmountParseError::LeadingZero)
        );
        assert_eq!(
            AtomicAmount::from_decimal_str("+1"),
            Err(AtomicAmountParseError::InvalidCharacter {
                byte_index: 0,
                character: '+',
            })
        );
        assert_eq!(
            AtomicAmount::from_decimal_str("1_0"),
            Err(AtomicAmountParseError::InvalidCharacter {
                byte_index: 1,
                character: '_',
            })
        );
        assert_eq!(
            AtomicAmount::from_decimal_str("1１"),
            Err(AtomicAmountParseError::InvalidCharacter {
                byte_index: 1,
                character: '１',
            })
        );
        assert_eq!(
            AtomicAmount::from_decimal_str(" 1"),
            Err(AtomicAmountParseError::InvalidCharacter {
                byte_index: 0,
                character: ' ',
            })
        );
    }

    #[test]
    fn decimal_parser_rejects_values_above_the_unsigned_256_bit_maximum() {
        assert_eq!(
            AtomicAmount::from_decimal_str(ABOVE_MAX_DECIMAL),
            Err(AtomicAmountParseError::Overflow)
        );
    }

    #[test]
    fn checked_add_handles_carry_and_rejects_overflow() {
        let mut low_byte_maximum = [0; 32];
        low_byte_maximum[31] = u8::MAX;
        let expected = {
            let mut bytes = [0; 32];
            bytes[30] = 1;
            AtomicAmount(bytes)
        };

        assert_eq!(
            AtomicAmount(low_byte_maximum).checked_add(&one()),
            Ok(expected)
        );
        assert_eq!(
            AtomicAmount([u8::MAX; 32]).checked_add(&one()),
            Err(AtomicAmountArithmeticError::Overflow)
        );
    }

    #[test]
    fn checked_sub_handles_borrow_and_rejects_underflow() {
        let mut high_byte = [0; 32];
        high_byte[30] = 1;
        let expected = {
            let mut bytes = [0; 32];
            bytes[31] = u8::MAX;
            AtomicAmount(bytes)
        };

        assert_eq!(AtomicAmount(high_byte).checked_sub(&one()), Ok(expected));
        assert_eq!(
            AtomicAmount::ZERO.checked_sub(&one()),
            Err(AtomicAmountArithmeticError::Underflow)
        );
    }
}
