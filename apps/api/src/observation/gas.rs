use deposits::{Collection, CollectionLeg, Deposit, DepositError, MirroredObservation};

use super::facts::invalid;

pub(super) fn validate(
    observation: &MirroredObservation,
    collection: &Collection,
    leg: &CollectionLeg,
    deposit: &Deposit,
) -> Result<(), DepositError> {
    let planned = leg
        .planned_amount
        .as_ref()
        .ok_or_else(|| invalid("confirmed gas-funding leg has no planned amount"))?;
    let movements = observation
        .event
        .transaction
        .movements
        .iter()
        .filter(|movement| {
            movement.to() == Some(&deposit.address)
                && movement.asset().chain == collection.asset.chain
                && movement.asset().asset == "native"
        })
        .collect::<Vec<_>>();
    if movements.is_empty() {
        return Err(invalid(
            "confirmed gas-funding transaction has no native deposit credit",
        ));
    }
    let amount = movements
        .iter()
        .try_fold(base::Decimal::zero(), |total, movement| {
            total
                .checked_add(movement.amount())
                .map_err(|error| invalid(format!("gas-funding amount overflowed: {error}")))
        })?;
    if &amount != planned {
        return Err(invalid(
            "confirmed gas-funding credit differs from its planned amount",
        ));
    }
    Ok(())
}
