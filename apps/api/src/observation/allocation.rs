use deposits::{Collection, CollectionAllocation, Deposit, DepositError, MirroredObservation};

use super::{classification::sum, facts::invalid};

pub(super) fn collection(
    observation: &MirroredObservation,
    collection: &Collection,
    deposit: &Deposit,
) -> Result<CollectionAllocation, DepositError> {
    let event = &observation.event;
    let debit = sum(event, |movement| {
        movement.asset() == &collection.asset && movement.from() == Some(&deposit.address)
    })?;
    let credit = sum(event, |movement| {
        movement.asset() == &collection.asset && movement.to() == Some(&collection.destination)
    })?;
    if debit.is_zero() || credit.is_zero() {
        return Err(invalid(
            "confirmed collection is missing debit or destination credit",
        ));
    }
    let (fee_asset, fee) = event.transaction.fee.as_ref().map_or_else(
        || {
            (
                indexing::AssetId {
                    chain: collection.asset.chain.clone(),
                    asset: "native".to_owned(),
                },
                base::Decimal::zero(),
            )
        },
        |fee| (fee.asset.clone(), fee.amount.clone()),
    );
    let gross = if fee_asset == collection.asset {
        debit
            .checked_add(&fee)
            .map_err(|error| invalid(format!("collection amount overflowed: {error}")))?
    } else {
        debit
    };
    Ok(CollectionAllocation {
        deposit_id: deposit.id.clone(),
        asset: collection.asset.clone(),
        gross_debit: gross,
        master_credit: credit,
        allocated_fee_asset: fee_asset,
        allocated_fee: fee,
    })
}
