use base::Decimal;

use std::collections::BTreeSet;

use crate::{
    AssetId, BlockHeight, BlockRef, BoxFuture, CanonicalAddress, IndexError, IndexErrorKind,
    IndexScope, TransactionRef,
};

/// Stable position in a snapshot-consistent output listing.
///
/// `position` is opaque to callers. It may only be returned to the same
/// [`Outputs`] implementation in a later request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputCursor {
    pub checkpoint: Option<BlockRef>,
    pub position: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputRequest {
    pub scope: IndexScope,
    pub address: CanonicalAddress,
    pub after: Option<OutputCursor>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputPage {
    pub checkpoint: Option<BlockRef>,
    pub outputs: Vec<IndexedOutput>,
    pub next: Option<OutputCursor>,
}

/// Current live outputs indexed for canonical addresses.
pub trait Outputs: Send + Sync {
    /// Lists one checkpoint-consistent page of spendable outputs.
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>>;
}

/// Chain-neutral identity of one transaction output.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputId {
    pub transaction: TransactionRef,
    pub index: u32,
}

/// Address-qualified output identity used by materialized spendable-output indexes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputKey {
    pub address: CanonicalAddress,
    pub output: OutputId,
}

/// A spendable output discovered in a canonical block.
///
/// `evidence` is opaque chain-owned data required by a transaction builder,
/// such as a locking script. Indexing stores it without interpreting it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedOutput {
    pub id: OutputId,
    pub address: CanonicalAddress,
    pub asset: AssetId,
    pub amount: Decimal,
    pub evidence: Vec<u8>,
    pub created_at: BlockHeight,
    pub coinbase: bool,
}

impl IndexedOutput {
    #[must_use]
    pub fn key(&self) -> OutputKey {
        OutputKey {
            address: self.address.clone(),
            output: self.id.clone(),
        }
    }
}

/// Canonical output-index changes produced while interpreting one block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputChanges {
    pub created: Vec<IndexedOutput>,
    pub spent: Vec<OutputKey>,
    /// Spends outside the current address filter are applied only if the output
    /// is already materialized. This preserves previously indexed outputs.
    pub tracked_spends: Vec<OutputKey>,
}

impl OutputChanges {
    pub(crate) fn validate(
        &self,
        scope: &IndexScope,
        block_height: BlockHeight,
    ) -> Result<(), IndexError> {
        let mut created_ids = BTreeSet::new();
        let mut created = BTreeSet::new();
        for output in &self.created {
            if !output.address.belongs_to(scope)
                || !output.id.transaction.belongs_to(scope)
                || output.asset.chain != scope.chain
                || output.amount.validate_amount().is_err()
                || output.created_at != block_height
                || !created_ids.insert(output.id.clone())
                || !created.insert(output.key())
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains an invalid output",
                    false,
                ));
            }
        }
        let mut spent = BTreeSet::new();
        for key in self.spent.iter().chain(&self.tracked_spends) {
            if !key.address.belongs_to(scope)
                || !key.output.transaction.belongs_to(scope)
                || created.contains(key)
                || !spent.insert(key)
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains overlapping or duplicate output changes",
                    false,
                ));
            }
        }
        Ok(())
    }
}
