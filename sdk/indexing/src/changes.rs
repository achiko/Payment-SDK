use crate::{BlockRef, ConfirmationPolicy, IndexScope, ObservationDraft, WatchId, WatchVersion};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedEvent<E> {
    pub watch_id: WatchId,
    pub transaction_id: Vec<u8>,
    pub payload: E,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockChanges<E, U> {
    pub block: BlockRef,
    pub events: Vec<IndexedEvent<E>>,
    /// Chain-owned information required to reverse this block atomically.
    pub undo: U,
}

/// Exact source payloads retained with a reversible canonical block bundle.
/// Decoded chain-native fields remain owned by the concrete chain crate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawBlockData {
    pub block: Vec<u8>,
    pub receipts: Vec<Vec<u8>>,
}

/// Complete semantic input to one atomic repository commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterpretedBlock<U> {
    pub block: BlockRef,
    pub drafts: Vec<ObservationDraft>,
    /// Chain-owned information required to reverse the block.
    pub undo: U,
    pub raw: RawBlockData,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitBlockCommand<U> {
    pub scope: IndexScope,
    pub expected_checkpoint: Option<BlockRef>,
    pub expected_watch_version: WatchVersion,
    pub confirmation_policy: ConfirmationPolicy,
    pub reorg_retention: u64,
    pub block: InterpretedBlock<U>,
}
