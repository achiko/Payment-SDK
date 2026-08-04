//! Serialized RocksDB implementation of the backend-independent storage API.
//!
//! Runtime reads, conditions, writes, schema inspection, and backup creation
//! are serialized through one dedicated RocksDB owner thread. Explicit
//! migration and restore entry points require the live database to be closed.

mod backend;
mod codec;
mod schema;

pub use backend::{BackupInfo, DEFAULT_COMMAND_QUEUE_CAPACITY, MigrationOutcome, RocksDbStorage};
pub use schema::{
    CURRENT_SCHEMA_VERSION, MigrationReport, REGISTERED_SCHEMA_MIGRATIONS, SchemaMigration,
    SchemaVersion,
};
