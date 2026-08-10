use std::{error::Error, fmt};

use chain_bitcoin::Satoshi;
use chain_identity::AtomicAmount;
use deposits::DepositId;

/// One deposit's gross contribution to a native-Bitcoin collection batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinFeeAllocationInput {
    pub deposit_id: DepositId,
    pub gross: AtomicAmount,
}

/// Deterministic fee attribution for one deposit in a Bitcoin collection batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinFeeAllocation {
    pub deposit_id: DepositId,
    pub gross: Satoshi,
    pub allocated_fee: Satoshi,
    pub master_credit: Satoshi,
}

/// Failure to allocate a Bitcoin collection fee without losing value or
/// producing an invalid per-deposit attribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitcoinFeeAllocationError {
    EmptyBatch,
    DuplicateDeposit {
        deposit_id: DepositId,
    },
    ZeroGross {
        deposit_id: DepositId,
    },
    GrossExceedsBitcoinRange {
        deposit_id: DepositId,
    },
    TotalGrossOverflow,
    FeeExceedsTotalGross {
        fee: Satoshi,
        total_gross: Satoshi,
    },
    NoMasterCredit {
        deposit_id: DepositId,
        gross: Satoshi,
        allocated_fee: Satoshi,
    },
    ArithmeticOverflow,
}

impl fmt::Display for BitcoinFeeAllocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBatch => formatter.write_str("Bitcoin fee allocation batch is empty"),
            Self::DuplicateDeposit { deposit_id } => write!(
                formatter,
                "Bitcoin fee allocation contains duplicate deposit `{}`",
                deposit_id.0
            ),
            Self::ZeroGross { deposit_id } => write!(
                formatter,
                "Bitcoin fee allocation gross amount is zero for deposit `{}`",
                deposit_id.0
            ),
            Self::GrossExceedsBitcoinRange { deposit_id } => write!(
                formatter,
                "Bitcoin fee allocation gross amount exceeds u64 for deposit `{}`",
                deposit_id.0
            ),
            Self::TotalGrossOverflow => formatter.write_str(
                "Bitcoin fee allocation total gross amount exceeds the native u64 range",
            ),
            Self::FeeExceedsTotalGross { fee, total_gross } => write!(
                formatter,
                "Bitcoin collection fee {} exceeds total gross input {}",
                fee.0, total_gross.0
            ),
            Self::NoMasterCredit {
                deposit_id,
                gross,
                allocated_fee,
            } => write!(
                formatter,
                "Bitcoin fee allocation {} consumes gross input {} for deposit `{}`",
                allocated_fee.0, gross.0, deposit_id.0
            ),
            Self::ArithmeticOverflow => {
                formatter.write_str("Bitcoin fee allocation arithmetic overflowed")
            }
        }
    }
}

impl Error for BitcoinFeeAllocationError {}

#[derive(Clone, Debug)]
struct WorkingAllocation {
    deposit_id: DepositId,
    gross: u64,
    allocated_fee: u64,
    remainder: u128,
}

/// Allocates a shared Bitcoin transaction fee proportionally to gross input.
///
/// The Hamilton/largest-remainder method is used: each deposit first receives
/// the floor of its exact proportional share, then remaining satoshis go to
/// the largest fractional remainders. Equal remainders are resolved by
/// ascending [`DepositId`]. Both the calculation and returned allocations are
/// independent of caller input order; results are returned by ascending
/// deposit ID.
///
/// # Errors
///
/// Returns [`BitcoinFeeAllocationError`] for an empty batch, duplicate
/// deposits, zero or non-`u64` gross amounts, an overflowing aggregate, a fee
/// greater than the aggregate, or an allocation that leaves no positive
/// master credit for a participant.
pub fn allocate_bitcoin_fee(
    inputs: &[BitcoinFeeAllocationInput],
    total_fee: Satoshi,
) -> Result<Vec<BitcoinFeeAllocation>, BitcoinFeeAllocationError> {
    if inputs.is_empty() {
        return Err(BitcoinFeeAllocationError::EmptyBatch);
    }

    let mut canonical_inputs = inputs.to_vec();
    canonical_inputs.sort_by(|left, right| left.deposit_id.cmp(&right.deposit_id));

    if let Some(duplicate) = canonical_inputs
        .windows(2)
        .find(|pair| pair[0].deposit_id == pair[1].deposit_id)
    {
        return Err(BitcoinFeeAllocationError::DuplicateDeposit {
            deposit_id: duplicate[0].deposit_id.clone(),
        });
    }

    let mut total_gross = 0_u64;
    let mut allocations = Vec::with_capacity(canonical_inputs.len());
    for input in canonical_inputs {
        let gross = atomic_amount_to_u64(&input.gross).ok_or_else(|| {
            BitcoinFeeAllocationError::GrossExceedsBitcoinRange {
                deposit_id: input.deposit_id.clone(),
            }
        })?;
        if gross == 0 {
            return Err(BitcoinFeeAllocationError::ZeroGross {
                deposit_id: input.deposit_id,
            });
        }
        total_gross = total_gross
            .checked_add(gross)
            .ok_or(BitcoinFeeAllocationError::TotalGrossOverflow)?;
        allocations.push(WorkingAllocation {
            deposit_id: input.deposit_id,
            gross,
            allocated_fee: 0,
            remainder: 0,
        });
    }

    if total_fee.0 > total_gross {
        return Err(BitcoinFeeAllocationError::FeeExceedsTotalGross {
            fee: total_fee,
            total_gross: Satoshi(total_gross),
        });
    }

    let total_gross_u128 = u128::from(total_gross);
    let mut allocated_floor = 0_u64;
    for allocation in &mut allocations {
        let numerator = u128::from(total_fee.0)
            .checked_mul(u128::from(allocation.gross))
            .ok_or(BitcoinFeeAllocationError::ArithmeticOverflow)?;
        allocation.allocated_fee = u64::try_from(numerator / total_gross_u128)
            .map_err(|_| BitcoinFeeAllocationError::ArithmeticOverflow)?;
        allocation.remainder = numerator % total_gross_u128;
        allocated_floor = allocated_floor
            .checked_add(allocation.allocated_fee)
            .ok_or(BitcoinFeeAllocationError::ArithmeticOverflow)?;
    }

    let mut remaining = total_fee
        .0
        .checked_sub(allocated_floor)
        .ok_or(BitcoinFeeAllocationError::ArithmeticOverflow)?;
    let mut remainder_order = (0..allocations.len()).collect::<Vec<_>>();
    remainder_order.sort_by(|left, right| {
        allocations[*right]
            .remainder
            .cmp(&allocations[*left].remainder)
            .then_with(|| {
                allocations[*left]
                    .deposit_id
                    .cmp(&allocations[*right].deposit_id)
            })
    });

    for index in remainder_order {
        if remaining == 0 {
            break;
        }
        allocations[index].allocated_fee = allocations[index]
            .allocated_fee
            .checked_add(1)
            .ok_or(BitcoinFeeAllocationError::ArithmeticOverflow)?;
        remaining -= 1;
    }
    if remaining != 0 {
        return Err(BitcoinFeeAllocationError::ArithmeticOverflow);
    }

    allocations
        .into_iter()
        .map(|allocation| {
            let master_credit = allocation
                .gross
                .checked_sub(allocation.allocated_fee)
                .ok_or(BitcoinFeeAllocationError::ArithmeticOverflow)?;
            if master_credit == 0 {
                return Err(BitcoinFeeAllocationError::NoMasterCredit {
                    deposit_id: allocation.deposit_id,
                    gross: Satoshi(allocation.gross),
                    allocated_fee: Satoshi(allocation.allocated_fee),
                });
            }
            Ok(BitcoinFeeAllocation {
                deposit_id: allocation.deposit_id,
                gross: Satoshi(allocation.gross),
                allocated_fee: Satoshi(allocation.allocated_fee),
                master_credit: Satoshi(master_credit),
            })
        })
        .collect()
}

fn atomic_amount_to_u64(amount: &AtomicAmount) -> Option<u64> {
    if amount.0[..24].iter().any(|byte| *byte != 0) {
        return None;
    }

    Some(u64::from_be_bytes([
        amount.0[24],
        amount.0[25],
        amount.0[26],
        amount.0[27],
        amount.0[28],
        amount.0[29],
        amount.0[30],
        amount.0[31],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(id: &str, gross: u64) -> BitcoinFeeAllocationInput {
        BitcoinFeeAllocationInput {
            deposit_id: DepositId(id.to_owned()),
            gross: gross.to_string().parse().expect("test amount is canonical"),
        }
    }

    fn allocation_tuples(allocations: &[BitcoinFeeAllocation]) -> Vec<(&str, u64, u64, u64)> {
        allocations
            .iter()
            .map(|allocation| {
                (
                    allocation.deposit_id.0.as_str(),
                    allocation.gross.0,
                    allocation.allocated_fee.0,
                    allocation.master_credit.0,
                )
            })
            .collect()
    }

    #[test]
    fn equal_remainders_use_deposit_id_as_the_stable_tie_breaker() {
        let allocations = allocate_bitcoin_fee(
            &[
                input("deposit-c", 10),
                input("deposit-a", 10),
                input("deposit-b", 10),
            ],
            Satoshi(2),
        )
        .expect("positive credits remain");

        assert_eq!(
            allocation_tuples(&allocations),
            vec![
                ("deposit-a", 10, 1, 9),
                ("deposit-b", 10, 1, 9),
                ("deposit-c", 10, 0, 10),
            ]
        );
    }

    #[test]
    fn uneven_remainders_receive_the_unallocated_satoshis() {
        let allocations = allocate_bitcoin_fee(
            &[
                input("deposit-a", 5),
                input("deposit-b", 3),
                input("deposit-c", 2),
            ],
            Satoshi(4),
        )
        .expect("positive credits remain");

        assert_eq!(
            allocation_tuples(&allocations),
            vec![
                ("deposit-a", 5, 2, 3),
                ("deposit-b", 3, 1, 2),
                ("deposit-c", 2, 1, 1),
            ]
        );
    }

    #[test]
    fn input_order_does_not_change_output_order_or_allocations() {
        let forward =
            allocate_bitcoin_fee(&[input("a", 11), input("b", 7), input("c", 5)], Satoshi(7))
                .expect("forward allocation succeeds");
        let reverse =
            allocate_bitcoin_fee(&[input("c", 5), input("b", 7), input("a", 11)], Satoshi(7))
                .expect("reverse allocation succeeds");

        assert_eq!(forward, reverse);
    }

    #[test]
    fn rejects_empty_zero_duplicate_and_out_of_range_inputs() {
        assert_eq!(
            allocate_bitcoin_fee(&[], Satoshi(0)),
            Err(BitcoinFeeAllocationError::EmptyBatch)
        );
        assert_eq!(
            allocate_bitcoin_fee(&[input("zero", 0)], Satoshi(0)),
            Err(BitcoinFeeAllocationError::ZeroGross {
                deposit_id: DepositId("zero".to_owned()),
            })
        );
        assert_eq!(
            allocate_bitcoin_fee(&[input("same", 2), input("same", 3)], Satoshi(1)),
            Err(BitcoinFeeAllocationError::DuplicateDeposit {
                deposit_id: DepositId("same".to_owned()),
            })
        );

        let mut too_large = AtomicAmount::zero();
        too_large.0[23] = 1;
        assert_eq!(
            allocate_bitcoin_fee(
                &[BitcoinFeeAllocationInput {
                    deposit_id: DepositId("large".to_owned()),
                    gross: too_large,
                }],
                Satoshi(1),
            ),
            Err(BitcoinFeeAllocationError::GrossExceedsBitcoinRange {
                deposit_id: DepositId("large".to_owned()),
            })
        );
    }

    #[test]
    fn rejects_total_overflow_and_fee_above_total() {
        assert_eq!(
            allocate_bitcoin_fee(&[input("a", u64::MAX), input("b", 1)], Satoshi(1),),
            Err(BitcoinFeeAllocationError::TotalGrossOverflow)
        );
        assert_eq!(
            allocate_bitcoin_fee(&[input("a", 3), input("b", 2)], Satoshi(6)),
            Err(BitcoinFeeAllocationError::FeeExceedsTotalGross {
                fee: Satoshi(6),
                total_gross: Satoshi(5),
            })
        );
    }

    #[test]
    fn rejects_an_allocation_that_consumes_a_participant() {
        assert_eq!(
            allocate_bitcoin_fee(&[input("a", 1), input("b", 9)], Satoshi(9)),
            Err(BitcoinFeeAllocationError::NoMasterCredit {
                deposit_id: DepositId("a".to_owned()),
                gross: Satoshi(1),
                allocated_fee: Satoshi(1),
            })
        );
        assert!(matches!(
            allocate_bitcoin_fee(&[input("a", 2), input("b", 3)], Satoshi(5)),
            Err(BitcoinFeeAllocationError::NoMasterCredit { .. })
        ));
    }

    #[test]
    fn exhaustive_small_batches_conserve_fee_and_gross_value() {
        for gross_a in 1..=6_u64 {
            for gross_b in 1..=6_u64 {
                for gross_c in 1..=6_u64 {
                    let total_gross = gross_a + gross_b + gross_c;
                    for fee in 0..=total_gross {
                        let result = allocate_bitcoin_fee(
                            &[
                                input("c", gross_c),
                                input("a", gross_a),
                                input("b", gross_b),
                            ],
                            Satoshi(fee),
                        );

                        match result {
                            Ok(allocations) => {
                                assert_eq!(
                                    allocations
                                        .iter()
                                        .map(|allocation| allocation.allocated_fee.0)
                                        .sum::<u64>(),
                                    fee
                                );
                                assert_eq!(
                                    allocations
                                        .iter()
                                        .map(|allocation| allocation.gross.0)
                                        .sum::<u64>(),
                                    total_gross
                                );
                                for allocation in allocations {
                                    assert_eq!(
                                        allocation.gross.0,
                                        allocation.allocated_fee.0 + allocation.master_credit.0
                                    );
                                    assert!(allocation.master_credit.0 > 0);
                                }
                            }
                            Err(BitcoinFeeAllocationError::NoMasterCredit { .. }) => {}
                            Err(error) => panic!("unexpected allocation error: {error}"),
                        }
                    }
                }
            }
        }
    }
}
