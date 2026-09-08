use redb::{Durability, ReadableDatabase};
use storage::{Error, ErrorKind};

use super::{
    Backend, DATA_TABLE, DATABASE_FORMAT_KEY, META_TABLE, commit_error, table_error,
    transaction_error,
};

/// Marker written into every database created by this adapter.
///
/// The marker describes the complete physical layout. A database with any
/// other marker belongs to a different format and is rejected rather than
/// interpreted speculatively.
pub(super) const DATABASE_FORMAT: &[u8] = b"w3-storage-redb";

impl Backend {
    pub(super) fn initialize_format(&mut self) -> Result<(), Error> {
        let mut transaction = self
            .database()?
            .begin_write()
            .map_err(|error| transaction_error(error, "failed to initialize redb format"))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| Error {
                kind: ErrorKind::Other,
                message: format!("failed to configure redb initialization: {error}"),
            })?;
        {
            let _data = transaction
                .open_table(DATA_TABLE)
                .map_err(|error| table_error(error, "failed to create redb data table"))?;
            let mut meta = transaction
                .open_table(META_TABLE)
                .map_err(|error| table_error(error, "failed to create redb metadata table"))?;
            meta.insert(DATABASE_FORMAT_KEY, DATABASE_FORMAT)
                .map_err(|error| {
                    super::storage_error(error, "failed to write redb format marker")
                })?;
        }
        transaction.commit().map_err(commit_error)
    }

    pub(super) fn require_format(&self) -> Result<(), Error> {
        let transaction = self
            .database()?
            .begin_read()
            .map_err(|error| transaction_error(error, "failed to read redb format"))?;
        let _data = transaction
            .open_table(DATA_TABLE)
            .map_err(|error| table_error(error, "redb data table is incompatible"))?;
        let meta = transaction
            .open_table(META_TABLE)
            .map_err(|error| table_error(error, "redb metadata table is incompatible"))?;
        let marker = meta
            .get(DATABASE_FORMAT_KEY)
            .map_err(|error| super::storage_error(error, "failed to read redb format marker"))?
            .ok_or_else(|| Error::corrupt_data("redb database has no physical format marker"))?;
        if marker.value() != DATABASE_FORMAT {
            return Err(Error::corrupt_data(
                "database format is not supported by this adapter",
            ));
        }
        Ok(())
    }
}
