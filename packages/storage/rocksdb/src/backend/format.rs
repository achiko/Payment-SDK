use rocksdb::{IteratorMode, WriteBatch as RocksWriteBatch, WriteOptions};
use storage::Error;

use crate::format::{DATABASE_FORMAT, validate_database_format};

use super::{Backend, DATABASE_FORMAT_KEY, GLOBAL_VERSION_KEY, corrupt_data, unavailable};

impl Backend {
    pub(super) fn ensure_format(&self) -> Result<(), Error> {
        match self.persisted_format()? {
            Some(bytes) => validate_database_format(&bytes),
            None if self.is_empty()? => self.write_format(),
            None => Err(corrupt_data(
                "database contains records but has no physical format marker",
            )),
        }
    }

    pub(super) fn require_format(&self) -> Result<(), Error> {
        let bytes = self
            .persisted_format()?
            .ok_or_else(|| corrupt_data("database has no physical format marker"))?;
        validate_database_format(&bytes)
    }

    fn persisted_format(&self) -> Result<Option<Vec<u8>>, Error> {
        self.db
            .get_cf(self.meta_cf()?, DATABASE_FORMAT_KEY)
            .map_err(|error| unavailable(format!("failed to read RocksDB format: {error}")))
    }

    fn write_format(&self) -> Result<(), Error> {
        let mut batch = RocksWriteBatch::default();
        batch.put_cf(self.meta_cf()?, DATABASE_FORMAT_KEY, DATABASE_FORMAT);
        self.write_sync(batch, "database format marker")
    }

    fn is_empty(&self) -> Result<bool, Error> {
        let global_version = self
            .db
            .get_cf(self.meta_cf()?, GLOBAL_VERSION_KEY)
            .map_err(|error| unavailable(format!("failed to inspect RocksDB metadata: {error}")))?;
        if global_version.is_some() {
            return Ok(false);
        }

        match self
            .db
            .iterator_cf(self.data_cf()?, IteratorMode::Start)
            .next()
        {
            None => Ok(true),
            Some(Ok(_)) => Ok(false),
            Some(Err(error)) => Err(unavailable(format!(
                "failed to inspect RocksDB data during format initialization: {error}"
            ))),
        }
    }

    fn write_sync(&self, batch: RocksWriteBatch, description: &str) -> Result<(), Error> {
        let mut write_options = WriteOptions::default();
        write_options.disable_wal(false);
        write_options.set_sync(true);
        self.db
            .write_opt(batch, &write_options)
            .map_err(|error| unavailable(format!("failed to persist {description}: {error}")))
    }
}
