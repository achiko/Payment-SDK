//! Reorg-safe, chain-independent block synchronization contracts.

mod block;
mod changes;
mod error;
mod observation;
mod service;
mod source;
mod store;
mod watch;

pub use block::{BlockHash, BlockHeight, BlockRef, IndexedBlock};
pub use changes::{BlockChanges, IndexedEvent};
pub use error::{IndexError, IndexErrorKind, SourceError};
pub use observation::{
    ConfirmationPolicy, ConfirmationProof, EventCursor, FinalityScanPage, FinalityScanRequest,
    MovementId, MovementKind, NetworkFee, ObservationEvent, ObservationEventId,
    ObservationEventPage, ObservationEventRequest, ObservationRevision, ObservedTransaction,
    TransactionPage, TransactionPageRequest, TransactionStatus, ValueMovement, WatchReceipt,
    WatchRequest, WatchSelector,
};
pub use service::{
    IndexingWorker, ObservationEventSource, ObservationQuery, ObservationRegistry, SyncRequest,
    SyncStatus,
};
pub use source::{BlockInterpreter, BlockSource, MempoolSource};
pub use store::{IndexStore, ObservationStore, WatchStore};
pub use watch::{IndexScope, WatchId, WatchTarget};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
