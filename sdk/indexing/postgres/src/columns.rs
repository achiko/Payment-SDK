//! Block facts laid out as the column arrays the batched inserts bind.
//!
//! Each insert in `write` sends one array per column and lets PostgreSQL
//! expand them into rows, so a block costs one statement per table rather than
//! one per row. This module owns that transposition — the facts arrive
//! row-shaped and leave column-shaped — and nothing else.

use indexing::{
    BlockAddition, CanonicalAddress, CanonicalStatus, CanonicalTransaction, IndexError,
    IndexedOutput, OutputKey, ValueMovement,
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

    fn extend(
        &mut self,
        canonical: &CanonicalTransaction,
        address: &CanonicalAddress,
    ) -> Result<(), IndexError> {
        for (ordinal, movement) in canonical.movements.iter().enumerate() {
            let ordinal = i32::try_from(ordinal)
                .map_err(|_| row::store("transaction has too many movements"))?;
            self.address.push(address.value.clone());
            self.transaction_id
                .push(canonical.transaction_id.value.clone());
            self.ordinal.push(ordinal);
            self.kind.push(
                match movement {
                    ValueMovement::Transfer { .. } => "transfer",
                    ValueMovement::Input { .. } => "input",
                    ValueMovement::Output { .. } => "output",
                    ValueMovement::Mint { .. } => "mint",
                    ValueMovement::Burn { .. } => "burn",
                }
                .to_owned(),
            );
            self.movement_id.push(movement.id().0.clone());
            self.asset_chain.push(movement.asset().chain.0.clone());
            self.asset.push(movement.asset().asset.clone());
            self.amount.push(movement.amount().to_string());
            self.from_address
                .push(movement.from().map(|value| value.value.clone()));
            self.to_address
                .push(movement.to().map(|value| value.value.clone()));
        }
        Ok(())
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

                movements.extend(canonical, &address)?;
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

impl TryFrom<&[OutputKey]> for SpendKeys {
    type Error = IndexError;

    fn try_from(keys: &[OutputKey]) -> Result<Self, Self::Error> {
        let mut spends = Self::default();
        for key in keys {
            spends.push(key)?;
        }
        Ok(spends)
    }
}

// design-lint: allow unclassified-free-function -- checked PostgreSQL INT4 output-index conversion is shared by created and spent column arrays and preserves the same storage-range error
fn index(value: u32) -> Result<i32, IndexError> {
    i32::try_from(value).map_err(|_| row::store("output index exceeds the storage range"))
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
    fn movement_rows_keep_all_tags_and_restart_ordinals_for_each_address() {
        let output = output("tx", 0, "0.5");
        let owner = output.address.clone();
        let other = CanonicalAddress {
            value: "other".into(),
            ..owner.clone()
        };
        let asset = output.asset;
        let amount = output.amount;
        let id = indexing::MovementId("movement".into());
        let canonical = CanonicalTransaction {
            scope: owner.scope.clone(),
            transaction_id: output.id.transaction,
            status: CanonicalStatus::Included {
                block: indexing::BlockRef {
                    position: indexing::BlockPosition(0),
                    height: BlockHeight(0),
                    hash: indexing::BlockHash(vec![0]),
                    parent: None,
                    timestamp: None,
                },
            },
            movements: vec![
                ValueMovement::Transfer {
                    id: id.clone(),
                    asset: asset.clone(),
                    amount: amount.clone(),
                    from: owner.clone(),
                    to: other.clone(),
                },
                ValueMovement::Input {
                    id: id.clone(),
                    asset: asset.clone(),
                    amount: amount.clone(),
                    owner: Some(owner.clone()),
                },
                ValueMovement::Output {
                    id: id.clone(),
                    asset: asset.clone(),
                    amount: amount.clone(),
                    owner: None,
                },
                ValueMovement::Mint {
                    id: id.clone(),
                    asset: asset.clone(),
                    amount: amount.clone(),
                    to: other.clone(),
                },
                ValueMovement::Burn {
                    id,
                    asset,
                    amount,
                    from: owner.clone(),
                },
            ],
            fee: None,
        };
        let mut rows = MovementRows::default();
        rows.extend(&canonical, &owner)
            .expect("first address movements");
        rows.extend(&canonical, &other)
            .expect("second address movements");
        assert_eq!(rows.ordinal, [0, 1, 2, 3, 4, 0, 1, 2, 3, 4]);
        assert_eq!(
            rows.kind,
            ["transfer", "input", "output", "mint", "burn"].repeat(2)
        );
        assert_eq!(
            rows.address,
            [vec![owner.value.clone(); 5], vec![other.value.clone(); 5]].concat()
        );
        assert_eq!(rows.transaction_id, vec!["tx"; 10]);
        assert_eq!(rows.amount, vec!["0.5"; 10]);
        assert_eq!(
            rows.from_address,
            [
                Some(owner.value.clone()),
                Some(owner.value.clone()),
                None,
                None,
                Some(owner.value)
            ]
            .into_iter()
            .cycle()
            .take(10)
            .collect::<Vec<_>>()
        );
        assert_eq!(
            rows.to_address,
            [
                Some(other.value.clone()),
                None,
                None,
                Some(other.value),
                None
            ]
            .into_iter()
            .cycle()
            .take(10)
            .collect::<Vec<_>>()
        );
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

    #[test]
    fn spend_keys_preserve_sql_index_boundaries_and_reject_overflow() {
        assert!(SpendKeys::try_from([].as_slice()).unwrap().is_empty());
        let keys = [
            output("zero", 0, "1").key(),
            output("max", i32::MAX as u32, "1").key(),
            output("zero", 0, "1").key(),
        ];
        let rows = SpendKeys::try_from(keys.as_slice()).expect("valid spent output identities");
        assert_eq!(rows.transaction_id, ["zero", "max", "zero"]);
        assert_eq!(rows.output_index, [0, i32::MAX, 0]);
        assert_eq!(
            rows.address,
            ["address-zero", "address-max", "address-zero"]
        );
        assert_eq!(rows.len(), 3);

        for index in [i32::MAX as u32 + 1, u32::MAX] {
            let keys = [
                output("valid", 0, "1").key(),
                output("invalid", index, "1").key(),
            ];
            let error = SpendKeys::try_from(keys.as_slice())
                .err()
                .expect("unrepresentable spent index");
            assert_eq!(error.kind, IndexErrorKind::Store);
            assert_eq!(error.message, "output index exceeds the storage range");
            assert!(!error.retryable);
        }
    }
}
