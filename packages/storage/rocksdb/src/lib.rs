//! Serialized RocksDB implementation of the backend-independent storage API.
//!
//! Runtime reads, conditions, writes, and backup creation
//! are serialized through one dedicated RocksDB owner thread. Restores target
//! a new database directory and reject an incompatible physical format.

mod backend;
mod codec;
mod format;

pub use backend::{BackupInfo, DEFAULT_COMMAND_QUEUE_CAPACITY, RocksDb};
