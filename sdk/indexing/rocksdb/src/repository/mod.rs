mod blocks;
mod outputs;
mod storage;
mod transactions;

use ::storage::{Condition, Key, Operation, ScanRequest, Value, Version, WriteBatch};
use bincode::{Decode, Encode, config};

use super::{Repository, keys, record};
use indexing::{
    BlockAddition, BlockOutcome, BlockRef, BlockSelector, Blocks, CanonicalPage, HistoryCursor,
    HistoryQuery, IndexError, IndexErrorKind, IndexScope, OutputCursor, OutputPage, OutputRequest,
    Outputs, Transactions,
};

const MAX_PAGE: usize = 1_000;

pub(super) struct Stored<T> {
    value: T,
    version: Version,
}
