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

/// One row per address a transaction touched, together with its ordered movements.
#[derive(Default)]
pub(crate) struct HistoryRows {
    pub(crate) movements: MovementRows,
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

/// The address-qualified output identities one spend statement removes.
#[derive(Default)]
pub(crate) struct SpendKeys {
    pub(crate) address: Vec<String>,
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
        self.address.push(key.address.value.clone());
        self.transaction_id
            .push(key.output.transaction.value.clone());
        self.output_index.push(index(key.output.index)?);
        Ok(())
    }
}

impl TryFrom<&BlockAddition> for HistoryRows {
    type Error = IndexError;

    /// Transposes canonical transactions into address-primary history and
    /// movement columns, duplicating each transaction's movements per address.
    fn try_from(addition: &BlockAddition) -> Result<Self, Self::Error> {
        let mut history = Self::default();
        let movements = &mut history.movements;

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
        Ok(history)
    }
}

impl TryFrom<&[IndexedOutput]> for OutputRows {
    type Error = IndexError;

    /// Transposes created outputs into the ordered columns of one batched insert.
    fn try_from(outputs: &[IndexedOutput]) -> Result<Self, Self::Error> {
        let mut rows = Self::default();
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

#[cfg(test)]
mod tests {
    use indexing::{
        AssetId, BlockHeight, CanonicalAddress, ChainId, IndexErrorKind, IndexScope, OutputId,
        TransactionRef,
    };

    use super::*;

    fn output(transaction: &str, index: u32, amount: &str) -> IndexedOutput {
        let scope = IndexScope {
            chain: ChainId("chain".to_owned()),
            network: "network".to_owned(),
        };
        IndexedOutput {
            id: OutputId {
                transaction: TransactionRef {
                    scope: scope.clone(),
                    value: transaction.to_owned(),
                },
                index,
            },
            address: CanonicalAddress {
                scope,
                value: format!("address-{transaction}"),
            },
            asset: AssetId {
                chain: ChainId("chain".to_owned()),
                asset: "native".to_owned(),
            },
            amount: amount.parse().expect("test amount"),
            evidence: vec![1, 2],
            created_at: BlockHeight(42),
            coinbase: false,
        }
    }

    #[test]
    fn output_rows_preserve_column_alignment_order_and_exact_amounts() {
        let outputs = [
            output("b", 0, "123456789012345678901234567890.000000000000000001"),
            IndexedOutput {
                evidence: vec![3, 4],
                coinbase: true,
                ..output("a", i32::MAX as u32, "0.5")
            },
        ];
        let rows = OutputRows::try_from(outputs.as_slice()).expect("output columns");
        assert_eq!(rows.transaction_id, ["b", "a"]);
        assert_eq!(rows.output_index, [0, i32::MAX]);
        assert_eq!(rows.address, ["address-b", "address-a"]);
        assert_eq!(rows.asset_chain, ["chain", "chain"]);
        assert_eq!(rows.asset, ["native", "native"]);
        assert_eq!(
            rows.amount,
            ["123456789012345678901234567890.000000000000000001", "0.5"]
        );
        assert_eq!(rows.evidence, [vec![1, 2], vec![3, 4]]);
        assert_eq!(rows.coinbase, [false, true]);
    }

    #[test]
    fn output_rows_accept_empty_batches_and_reject_unstorable_indexes() {
        assert!(
            OutputRows::try_from([].as_slice())
                .expect("empty batch")
                .is_empty()
        );
        for index in [i32::MAX as u32 + 1, u32::MAX] {
            let outputs = [output("a", index, "1")];
            let error = OutputRows::try_from(outputs.as_slice())
                .err()
                .expect("out-of-range output index");
            assert_eq!(error.kind, IndexErrorKind::Store);
            assert_eq!(error.message, "output index exceeds the storage range");
            assert!(!error.retryable);
        }
    }
}
