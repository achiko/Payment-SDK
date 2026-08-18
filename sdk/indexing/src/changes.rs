use crate::{BlockRef, ConfirmationPolicy, IndexScope, ObservationDraft, WatchVersion};

/// Exact source payloads retained with a reversible canonical block bundle.
/// Decoded chain-native fields remain owned by the concrete chain crate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawBlock {
    pub block: Vec<u8>,
    pub receipts: Vec<Vec<u8>>,
}

/// Complete semantic input to one atomic repository commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterpretedBlock<E, U> {
    pub block: BlockRef,
    pub drafts: Vec<ObservationDraft>,
    /// Typed chain semantics; only a repository adapter may encode them.
    pub effect: E,
    /// Chain-owned information required to reverse the block.
    pub undo: U,
    pub raw: RawBlock,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitBlock<E, U> {
    pub scope: IndexScope,
    pub expected_checkpoint: Option<BlockRef>,
    pub expected_watch_version: WatchVersion,
    pub confirmation_policy: ConfirmationPolicy,
    pub reorg_retention: u64,
    pub block: InterpretedBlock<E, U>,
}
