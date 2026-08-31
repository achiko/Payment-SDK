#[path = "interpreter_movement.rs"]
mod movement;
#[path = "interpreter_wire.rs"]
mod wire;

use std::collections::BTreeSet;

use indexing::{
    AssetId, BlockInterpreter as IndexBlockInterpreter, CanonicalAddress, ChainId, IndexError,
    IndexErrorKind, IndexScope, InterpretedBlock, NetworkFee, ObservationDraft,
    ObservationDraftStatus, OutputChanges, TransactionRef,
};

use crate::{Address, AssetKind};

use self::{movement::Movements, wire::Transactions};
use super::Block;

/// Scope-bound owner for complete native SOL block interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interpreter {
    scope: IndexScope,
    asset: AssetId,
}

impl Interpreter {
    pub fn new(scope: IndexScope) -> Result<Self, IndexError> {
        if scope.chain != ChainId(crate::CHAIN.to_owned()) {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Solana interpreter scope must use the solana chain ID",
                false,
            ));
        }
        if scope.network.trim().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Solana interpreter network slug must not be empty",
                false,
            ));
        }
        let asset = AssetKind::Native.id();
        Ok(Self { scope, asset })
    }

    #[must_use]
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }

    fn selected(&self, addresses: &[CanonicalAddress]) -> Result<BTreeSet<Address>, IndexError> {
        addresses
            .iter()
            .map(|address| {
                if !address.belongs_to(&self.scope) {
                    return Err(IndexError::new(
                        IndexErrorKind::ScopeMismatch,
                        "Solana address filter belongs to a different scope",
                        false,
                    ));
                }
                address.value.parse::<Address>().map_err(|_| {
                    IndexError::new(
                        IndexErrorKind::InvalidRequest,
                        "Solana address filter is not canonical Base58",
                        false,
                    )
                })
            })
            .collect()
    }
}

impl IndexBlockInterpreter for Interpreter {
    type Block = Block;

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[CanonicalAddress],
    ) -> Result<InterpretedBlock, IndexError> {
        let selected = self.selected(addresses)?;
        let parsed = Transactions::parse(block.raw())?;
        let mut observations = Vec::new();

        for transaction in parsed.values() {
            let movements = if transaction.succeeded() {
                Movements::decode(transaction)?
            } else {
                Movements::default()
            };

            if transaction.succeeded() {
                let affected = transaction.selected_effects(&selected, &movements);
                if !affected.is_empty() && transaction.inner().is_none() {
                    return Err(invalid_block(
                        "successful selected Solana transaction has incomplete inner instructions",
                    ));
                }
                for address in affected {
                    transaction.reconcile(address, &movements)?;
                }
            }

            let relevant = if transaction.succeeded() {
                selected.contains(transaction.fee_payer()) || movements.touches_any(&selected)
            } else {
                selected.contains(transaction.fee_payer())
            };
            if !relevant {
                continue;
            }

            observations.push(ObservationDraft {
                scope: self.scope.clone(),
                transaction_id: TransactionRef {
                    scope: self.scope.clone(),
                    value: transaction.signature().to_owned(),
                },
                status: if transaction.succeeded() {
                    ObservationDraftStatus::Included
                } else {
                    ObservationDraftStatus::Failed {
                        reason: Some("Solana transaction execution failed".to_owned()),
                    }
                },
                movements: movements.into_values(&self.scope, &self.asset),
                fee: Some(NetworkFee {
                    asset: self.asset.clone(),
                    amount: base::Decimal::from_atomic(transaction.fee().into(), 0),
                    payer: Some(canonical(transaction.fee_payer(), &self.scope)),
                }),
            });
        }

        Ok(InterpretedBlock {
            block: block.reference().clone(),
            transactions: observations,
            outputs: OutputChanges::default(),
        })
    }
}

fn canonical(address: &Address, scope: &IndexScope) -> CanonicalAddress {
    CanonicalAddress {
        scope: scope.clone(),
        value: address.to_string(),
    }
}

fn invalid_block(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidBlock, message, false)
}

#[cfg(test)]
#[path = "interpreter_test.rs"]
mod tests;
