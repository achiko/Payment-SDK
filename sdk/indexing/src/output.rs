use base::Decimal;

use crate::{
    AssetId, BlockHeight, BlockRef, BoxFuture, CanonicalAddress, IndexError, IndexScope,
    TransactionRef,
};

/// Stable position in a snapshot-consistent output listing.
///
/// `position` is opaque to callers. It may only be returned to the same
/// [`OutputQuery`] implementation in a later request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputCursor {
    pub snapshot: OutputSnapshot,
    pub position: Vec<u8>,
}

/// Canonical state against which an output page was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSnapshot {
    pub revision: u64,
    pub checkpoint: Option<BlockRef>,
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
    pub snapshot: OutputSnapshot,
    pub outputs: Vec<IndexedOutput>,
    pub next: Option<OutputCursor>,
}

/// Reads currently spendable outputs for canonical addresses.
pub trait OutputQuery: Send + Sync {
    fn outputs<'a>(
        &'a self,
        request: OutputRequest,
    ) -> BoxFuture<'a, Result<OutputPage, IndexError>>;
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
    /// Spends for inactive watches are applied only if the output is already
    /// materialized. This preserves correctness across watch lifetimes.
    pub tracked_spends: Vec<OutputKey>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexChanges {
    pub outputs: OutputChanges,
}

/// Projection identities removed when one canonical block is reverted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexUndo {
    pub created: Vec<OutputKey>,
    pub spent: Vec<OutputKey>,
}
