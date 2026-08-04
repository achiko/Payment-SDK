use crate::{BlockRef, WatchId};

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
