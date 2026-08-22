//! Serialized redb implementation of the backend-independent storage API.
//!
//! Runtime reads, conditions, and writes are serialized through one dedicated
//! redb owner thread. The database is one file. A cold backup therefore means
//! dropping the final [`Redb`] handle, copying that closed file, and verifying
//! that the copy can be opened before relying on it for recovery.

mod backend;
mod codec;
mod format;

pub use backend::{DEFAULT_COMMAND_QUEUE_CAPACITY, Redb};
