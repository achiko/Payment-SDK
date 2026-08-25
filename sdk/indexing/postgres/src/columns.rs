//! Block facts laid out as the column arrays the batched inserts bind.
//!
//! Each insert in `write` sends one array per column and lets PostgreSQL
//! expand them into rows, so a block costs one statement per table rather than
//! one per row. This module owns that transposition — the facts arrive
//! row-shaped and leave column-shaped — and nothing else.

use indexing::{
    BlockAddition, CanonicalStatus, IndexError, IndexedOutput, OutputKey, ValueMovement,
};

use crate::row;

/// One row per address a transaction touched.
#[derive(Default)]
pub(crate) struct HistoryRows {
    pub(crate) address: Vec<String>,
    pub(crate) transaction_id: Vec<String>,
    pub(crate) status: Vec<String>,
    pub(crate) failure_reason: Vec<Option<String>>,
    pub(crate) fee_asset: Vec<Option<String>>,
    pub(crate) fee_amount: Vec<Option<String>>,
    pub(crate) fee_payer: Vec<Option<String>>,
}

impl HistoryRows {
    pub(crate) fn is_empty(&self) -> bool {
        self.address.is_empty()
    }
}

/// The movements of those rows, duplicated per address for the same reason the
/// history rows are.
#[derive(Default)]
pub(crate) struct MovementRows {
    pub(crate) address: Vec<String>,
    pub(crate) transaction_id: Vec<String>,
    pub(crate) ordinal: Vec<i32>,
    pub(crate) kind: Vec<String>,
    pub(crate) movement_id: Vec<String>,
    pub(crate) asset_chain: Vec<String>,
    pub(crate) asset: Vec<String>,
    pub(crate) amount: Vec<String>,
    pub(crate) from_address: Vec<Option<String>>,
    pub(crate) to_address: Vec<Option<String>>,
}

impl MovementRows {
    pub(crate) fn is_empty(&self) -> bool {
        self.address.is_empty()
    }
}

/// Outputs a block created, minus the height every one of them shares.
#[derive(Default)]
pub(crate) struct OutputRows {
    pub(crate) transaction_id: Vec<String>,
    pub(crate) output_index: Vec<i32>,
    pub(crate) address: Vec<String>,
    pub(crate) asset_chain: Vec<String>,
    pub(crate) asset: Vec<String>,
    pub(crate) amount: Vec<String>,
    pub(crate) evidence: Vec<Vec<u8>>,
    pub(crate) coinbase: Vec<bool>,
}

impl OutputRows {
    pub(crate) fn is_empty(&self) -> bool {
        self.transaction_id.is_empty()
    }
}

/// The `(transaction_id, output_index)` pairs one spend statement removes.
#[derive(Default)]
pub(crate) struct SpendKeys {
    pub(crate) transaction_id: Vec<String>,
    pub(crate) output_index: Vec<i32>,
}

impl SpendKeys {
    pub(crate) fn len(&self) -> usize {
        self.transaction_id.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.transaction_id.is_empty()
    }

    fn push(&mut self, key: &OutputKey) -> Result<(), IndexError> {
        self.transaction_id
            .push(key.output.transaction.value.clone());
        self.output_index.push(index(key.output.index)?);
        Ok(())
    }
}

/// Transposes the block's canonical transactions into history and movement
/// columns.
///
/// History is address-primary, so a transaction paying two watched addresses
/// contributes one history row and one copy of its movements under each.
pub(crate) fn canonical(
    addition: &BlockAddition,
) -> Result<(HistoryRows, MovementRows), IndexError> {
    let mut history = HistoryRows::default();
    let mut movements = MovementRows::default();

    for canonical in addition.transactions() {
        let (status, reason) = match &canonical.status {
            CanonicalStatus::Included { .. } => ("included", None),
            CanonicalStatus::Failed { reason, .. } => ("failed", reason.clone()),
        };
        let fee = canonical.fee.as_ref();
        for address in canonical.addresses() {
            history.address.push(address.value.clone());
            history
                .transaction_id
                .push(canonical.transaction_id.value.clone());
            history.status.push(status.to_owned());
            history.failure_reason.push(reason.clone());
            history
                .fee_asset
                .push(fee.map(|fee| fee.asset.asset.clone()));
            history
                .fee_amount
                .push(fee.map(|fee| fee.amount.to_string()));
            history
                .fee_payer
                .push(fee.and_then(|fee| fee.payer.as_ref().map(|payer| payer.value.clone())));

            for (ordinal, movement) in canonical.movements.iter().enumerate() {
                let ordinal = i32::try_from(ordinal)
                    .map_err(|_| row::store("transaction has too many movements"))?;
                movements.address.push(address.value.clone());
                movements
                    .transaction_id
                    .push(canonical.transaction_id.value.clone());
                movements.ordinal.push(ordinal);
                movements.kind.push(kind(movement).to_owned());
                movements.movement_id.push(movement.id().0.clone());
                movements.asset_chain.push(movement.asset().chain.0.clone());
                movements.asset.push(movement.asset().asset.clone());
                movements.amount.push(movement.amount().to_string());
                movements
                    .from_address
                    .push(movement.from().map(|value| value.value.clone()));
                movements
                    .to_address
                    .push(movement.to().map(|value| value.value.clone()));
            }
        }
    }
    Ok((history, movements))
}

pub(crate) fn created(outputs: &[IndexedOutput]) -> Result<OutputRows, IndexError> {
    let mut rows = OutputRows::default();
    for output in outputs {
        rows.transaction_id
            .push(output.id.transaction.value.clone());
        rows.output_index.push(index(output.id.index)?);
        rows.address.push(output.address.value.clone());
        rows.asset_chain.push(output.asset.chain.0.clone());
        rows.asset.push(output.asset.asset.clone());
        rows.amount.push(output.amount.to_string());
        rows.evidence.push(output.evidence.clone());
        rows.coinbase.push(output.coinbase);
    }
    Ok(rows)
}

pub(crate) fn spends(keys: &[OutputKey]) -> Result<SpendKeys, IndexError> {
    let mut spends = SpendKeys::default();
    for key in keys {
        spends.push(key)?;
    }
    Ok(spends)
}

fn index(value: u32) -> Result<i32, IndexError> {
    i32::try_from(value).map_err(|_| row::store("output index exceeds the storage range"))
}

const fn kind(movement: &ValueMovement) -> &'static str {
    match movement {
        ValueMovement::Transfer { .. } => "transfer",
        ValueMovement::Input { .. } => "input",
        ValueMovement::Output { .. } => "output",
        ValueMovement::Mint { .. } => "mint",
        ValueMovement::Burn { .. } => "burn",
    }
}
