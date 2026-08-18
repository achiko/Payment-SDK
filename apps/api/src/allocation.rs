use base::Decimal;
use deposits::{Collection, CollectionAllocation, DepositError};

use crate::collection::invalid;

pub(super) fn allocate(
    collection: &Collection,
    fee: &Decimal,
) -> Result<Vec<CollectionAllocation>, DepositError> {
    let fee = fee
        .to_atomic_u64(0)
        .map_err(|_| invalid("collection fee is not a scale-zero atomic amount"))?;
    let gross = collection
        .participants
        .iter()
        .map(|participant| {
            participant
                .reservation
                .amount
                .to_atomic_u64(0)
                .map_err(|_| invalid("collection reservation is not a scale-zero atomic amount"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let total = gross.iter().try_fold(0_u64, |sum, amount| {
        sum.checked_add(*amount)
            .ok_or_else(|| invalid("collection reservation total overflowed"))
    })?;
    if total == 0 || fee >= total {
        return Err(invalid("collection fee consumes all reserved value"));
    }

    let mut shares = Vec::with_capacity(gross.len());
    let mut allocated = 0_u64;
    for (position, amount) in gross.iter().copied().enumerate() {
        let numerator = u128::from(fee) * u128::from(amount);
        let share = u64::try_from(numerator / u128::from(total))
            .map_err(|_| invalid("collection fee allocation overflowed"))?;
        allocated = allocated
            .checked_add(share)
            .ok_or_else(|| invalid("collection fee allocation overflowed"))?;
        shares.push((position, share, numerator % u128::from(total)));
    }
    shares.sort_by(|left, right| {
        right.2.cmp(&left.2).then_with(|| {
            collection.participants[left.0]
                .reservation
                .deposit_id
                .cmp(&collection.participants[right.0].reservation.deposit_id)
        })
    });
    for item in shares.iter_mut().take((fee - allocated) as usize) {
        item.1 += 1;
    }
    shares.sort_by_key(|item| item.0);

    collection
        .participants
        .iter()
        .zip(gross)
        .zip(shares)
        .map(|((participant, gross), (_, allocated_fee, _))| {
            let master = gross
                .checked_sub(allocated_fee)
                .ok_or_else(|| invalid("allocated fee exceeds participant value"))?;
            Ok(CollectionAllocation {
                deposit_id: participant.reservation.deposit_id.clone(),
                asset: collection.asset.clone(),
                gross_debit: Decimal::from(gross),
                master_credit: Decimal::from(master),
                allocated_fee_asset: collection.asset.clone(),
                allocated_fee: Decimal::from(allocated_fee),
            })
        })
        .collect()
}
