use std::cmp::Ordering;

use num_bigint::BigInt;

use super::{Decimal, DecimalErrorKind, DecimalParts, DecimalSign};

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

#[test]
fn ordering_compares_numeric_values_instead_of_structural_parts() {
    let smaller = "0.00059859".parse::<Decimal>().unwrap();
    let larger = "0.0006".parse::<Decimal>().unwrap();

    assert!(smaller < larger);
    assert!(larger > smaller);
}

#[test]
fn ordering_handles_unbounded_magnitudes_and_distant_scales() {
    let huge = "99999999999999999999999999999999999999999999999999"
        .parse::<Decimal>()
        .unwrap();
    let tiny = Decimal::new(BigInt::from(1_u8), 1_000_000);

    assert!(huge > tiny);
    assert!(tiny > Decimal::zero());
}

#[test]
fn ordering_agrees_with_canonical_equality() {
    let parsed = "1.2300".parse::<Decimal>().unwrap();
    let constructed = Decimal::new(BigInt::from(123_u8), 2);

    assert_eq!(parsed, constructed);
    assert_eq!(parsed.cmp(&constructed), Ordering::Equal);
}

#[test]
fn ordering_handles_negative_values_across_scales() {
    let farther_from_zero = "-1000000000000000000000000000000.1"
        .parse::<Decimal>()
        .unwrap();
    let closer_to_zero = "-0.00000000000000000001".parse::<Decimal>().unwrap();

    assert!(farther_from_zero < closer_to_zero);
    assert!(closer_to_zero < Decimal::zero());
}
