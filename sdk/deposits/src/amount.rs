use base::{Decimal, DecimalError, DecimalParts, DecimalSign};

use crate::{DepositError, DepositErrorKind};

pub(crate) fn checked_add(left: &Decimal, right: &Decimal) -> Result<Decimal, DecimalError> {
    let sum = left.checked_add(right)?;
    sum.to_atomic_be_bytes::<32>(0)?;
    Ok(sum)
}

pub(crate) fn checked_sub(left: &Decimal, right: &Decimal) -> Result<Decimal, DecimalError> {
    let difference = left.checked_sub(right)?;
    difference.validate_amount()?;
    Ok(difference)
}

pub(crate) fn from_bytes(bytes: [u8; 32]) -> Decimal {
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len());
    Decimal::from_parts(DecimalParts {
        sign: DecimalSign::Positive,
        magnitude: bytes[first..].to_vec(),
        scale: 0,
    })
    .expect("a positive canonical 256-bit integer is a valid decimal")
}

pub(crate) fn to_bytes(amount: &Decimal) -> Result<[u8; 32], DepositError> {
    amount
        .to_atomic_be_bytes::<32>(0)
        .map_err(|error| DepositError {
            kind: DepositErrorKind::InvariantViolation,
            message: format!("deposit amount must be a non-negative scale-0 u256: {error}"),
        })
}

pub(crate) fn record_bytes(amount: &Decimal) -> [u8; 32] {
    to_bytes(amount).expect("validated deposit amount must fit the scale-0 u256 record")
}
