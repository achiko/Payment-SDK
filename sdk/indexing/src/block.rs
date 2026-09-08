use std::collections::BTreeSet;

use crate::{
    BlockHeight, BlockParent, BlockRef, BoxFuture, CanonicalTransaction, IndexError,
    IndexErrorKind, IndexScope, ObservationDraft, OutputChanges, TransactionRef,
};

/// Chain-neutral facts produced by inspecting one native block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterpretedBlock {
    pub block: BlockRef,
    pub transactions: Vec<ObservationDraft>,
    pub outputs: OutputChanges,
}

/// A validated canonical block transition supplied to persistent storage.
///
/// Storage compares `expected_checkpoint`, derives rollback data from its own
/// current state, and commits every fact atomically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockAddition {
    scope: IndexScope,
    expected_checkpoint: Option<BlockRef>,
    retention: u64,
    block: BlockRef,
    transactions: Vec<CanonicalTransaction>,
    outputs: OutputChanges,
}

impl BlockAddition {
    pub fn new(
        scope: IndexScope,
        expected_checkpoint: Option<BlockRef>,
        retention: u64,
        interpreted: InterpretedBlock,
    ) -> Result<Self, IndexError> {
        if retention == 0 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "rollback retention must be greater than zero",
                false,
            ));
        }
        if let Some(checkpoint) = &expected_checkpoint {
            let next = checkpoint.height.0.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "checkpoint height is exhausted",
                    false,
                )
            })?;
            if interpreted.block.height != BlockHeight(next)
                || interpreted.block.position <= checkpoint.position
                || interpreted.block.parent.as_ref()
                    != Some(&BlockParent {
                        position: checkpoint.position,
                        hash: checkpoint.hash.clone(),
                    })
            {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "block does not connect to the checkpoint",
                    true,
                ));
            }
        }

        interpreted
            .outputs
            .validate(&scope, interpreted.block.height)?;
        let mut transaction_ids = BTreeSet::<TransactionRef>::new();
        let mut transactions = Vec::with_capacity(interpreted.transactions.len());
        for draft in interpreted.transactions {
            if !transaction_ids.insert(draft.transaction_id.clone()) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains a duplicate transaction",
                    false,
                ));
            }
            transactions.push(draft.canonical(&scope, &interpreted.block)?);
        }

        Ok(Self {
            scope,
            expected_checkpoint,
            retention,
            block: interpreted.block,
            transactions,
            outputs: interpreted.outputs,
        })
    }

    #[must_use]
    pub const fn scope(&self) -> &IndexScope {
        &self.scope
    }

    #[must_use]
    pub const fn expected_checkpoint(&self) -> Option<&BlockRef> {
        self.expected_checkpoint.as_ref()
    }

    #[must_use]
    pub const fn retention(&self) -> u64 {
        self.retention
    }

    #[must_use]
    pub const fn block(&self) -> &BlockRef {
        &self.block
    }

    #[must_use]
    pub fn transactions(&self) -> &[CanonicalTransaction] {
        &self.transactions
    }

    #[must_use]
    pub const fn outputs(&self) -> &OutputChanges {
        &self.outputs
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockSelector {
    Tip(IndexScope),
    Height {
        scope: IndexScope,
        height: BlockHeight,
    },
}

/// Canonical blocks and their atomic persistence lifecycle.
pub trait Blocks: Send + Sync {
    /// Reads either the current checkpoint or a retained canonical block.
    fn get<'a>(
        &'a self,
        selector: BlockSelector,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;

    /// Atomically writes history, live outputs, rollback data, and checkpoint.
    fn add<'a>(
        &'a self,
        addition: BlockAddition,
    ) -> BoxFuture<'a, Result<BlockOutcome, IndexError>>;

    /// Removes the current tip using storage-owned rollback data and returns
    /// the restored checkpoint.
    fn remove<'a>(
        &'a self,
        scope: IndexScope,
        expected_tip: BlockRef,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockOutcome {
    Applied,
    /// The exact block is already the current checkpoint. A retained journal
    /// entry at the same height is not sufficient for this outcome.
    AlreadyApplied,
}

#[cfg(test)]
mod tests {
    use base::Decimal;

    use crate::{
        AssetId, BlockHash, CanonicalAddress, ChainId, IndexScope, IndexedOutput, MovementId,
        NetworkFee, ObservationDraftStatus, OutputChanges, OutputId, OutputKey, TransactionRef,
        ValueMovement,
    };

    use super::*;

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("test".into()),
            network: "mainnet".into(),
        }
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            position: crate::BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent: height.checked_sub(1).map(|value| crate::BlockParent {
                position: crate::BlockPosition(value),
                hash: BlockHash(vec![value as u8]),
            }),
            timestamp: None,
        }
    }

    fn output_key(scope: &IndexScope, id: &str) -> OutputKey {
        OutputKey {
            address: CanonicalAddress {
                scope: scope.clone(),
                value: "owner".into(),
            },
            output: OutputId {
                transaction: TransactionRef {
                    scope: scope.clone(),
                    value: id.into(),
                },
                index: 0,
            },
        }
    }

    fn asset(scope: &IndexScope) -> AssetId {
        AssetId {
            chain: scope.chain.clone(),
            asset: "native".into(),
        }
    }

    fn address(scope: &IndexScope) -> CanonicalAddress {
        CanonicalAddress {
            scope: scope.clone(),
            value: "owner".into(),
        }
    }

    fn draft(scope: &IndexScope, amount: Decimal) -> ObservationDraft {
        ObservationDraft {
            scope: scope.clone(),
            transaction_id: TransactionRef {
                scope: scope.clone(),
                value: "transaction".into(),
            },
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::Mint {
                id: MovementId("movement".into()),
                asset: asset(scope),
                amount,
                to: address(scope),
            }],
            fee: None,
        }
    }

    #[test]
    fn rejects_a_block_that_does_not_connect_to_the_expected_checkpoint() {
        let own_scope = scope();
        let mut next = block(2);
        next.parent = Some(crate::BlockParent {
            position: crate::BlockPosition(1),
            hash: BlockHash(vec![99]),
        });

        let error = BlockAddition::new(
            own_scope,
            Some(block(1)),
            10,
            InterpretedBlock {
                block: next,
                transactions: Vec::new(),
                outputs: OutputChanges::default(),
            },
        )
        .expect_err("disconnected block");

        assert_eq!(error.kind, IndexErrorKind::CannotConnect);
    }

    #[test]
    fn rejects_overlapping_output_changes_before_storage() {
        let own_scope = scope();
        let key = output_key(&own_scope, "spent");

        let error = BlockAddition::new(
            own_scope,
            Some(block(0)),
            10,
            InterpretedBlock {
                block: block(1),
                transactions: Vec::new(),
                outputs: OutputChanges {
                    created: Vec::new(),
                    spent: vec![key.clone()],
                    tracked_spends: vec![key],
                },
            },
        )
        .expect_err("duplicate spend");

        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    }

    #[test]
    fn rejects_negative_movement_and_fee_amounts_before_storage() {
        let own_scope = scope();
        let negative = "-1".parse::<Decimal>().expect("negative decimal");
        let movement_error = BlockAddition::new(
            own_scope.clone(),
            None,
            10,
            InterpretedBlock {
                block: block(1),
                transactions: vec![draft(&own_scope, negative.clone())],
                outputs: OutputChanges::default(),
            },
        )
        .expect_err("negative movement");

        let mut fee_draft = draft(&own_scope, Decimal::zero());
        fee_draft.fee = Some(NetworkFee {
            asset: asset(&own_scope),
            amount: negative,
            payer: Some(address(&own_scope)),
        });
        let fee_error = BlockAddition::new(
            own_scope,
            None,
            10,
            InterpretedBlock {
                block: block(1),
                transactions: vec![fee_draft],
                outputs: OutputChanges::default(),
            },
        )
        .expect_err("negative fee");

        assert_eq!(movement_error.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(fee_error.kind, IndexErrorKind::InvalidBlock);
    }

    #[test]
    fn rejects_negative_live_outputs_before_storage() {
        let own_scope = scope();
        let key = output_key(&own_scope, "created");
        let error = BlockAddition::new(
            own_scope.clone(),
            None,
            10,
            InterpretedBlock {
                block: block(1),
                transactions: Vec::new(),
                outputs: OutputChanges {
                    created: vec![IndexedOutput {
                        id: key.output,
                        address: key.address,
                        asset: asset(&own_scope),
                        amount: "-1".parse().expect("negative decimal"),
                        evidence: Vec::new(),
                        created_at: BlockHeight(1),
                        coinbase: false,
                    }],
                    spent: Vec::new(),
                    tracked_spends: Vec::new(),
                },
            },
        )
        .expect_err("negative output");

        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    }

    #[test]
    fn rejects_duplicate_output_identity_before_storage() {
        let own_scope = scope();
        let id = output_key(&own_scope, "created").output;
        let created = |owner: &str| IndexedOutput {
            id: id.clone(),
            address: CanonicalAddress {
                scope: own_scope.clone(),
                value: owner.into(),
            },
            asset: asset(&own_scope),
            amount: Decimal::from(1_u64),
            evidence: Vec::new(),
            created_at: BlockHeight(1),
            coinbase: false,
        };

        let error = BlockAddition::new(
            own_scope.clone(),
            None,
            10,
            InterpretedBlock {
                block: block(1),
                transactions: Vec::new(),
                outputs: OutputChanges {
                    created: vec![created("first"), created("second")],
                    spent: Vec::new(),
                    tracked_spends: Vec::new(),
                },
            },
        )
        .expect_err("duplicate output identity");

        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    }
}
