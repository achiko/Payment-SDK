use rocksdb::{ColumnFamily, WriteBatch as RocksWriteBatch, WriteOptions};
use storage::{CommitResult, Condition, Error, Operation, Version, WriteBatch};

use crate::codec::{
    decode_global_version, encode_global_version, encode_physical_key, encode_stored_value,
};

use super::{
    Backend, DATA_COLUMN_FAMILY, GLOBAL_VERSION_KEY, META_COLUMN_FAMILY, conflict, corrupt_data,
    other, unavailable,
};

impl Backend {
    pub(super) fn commit(&self, batch: WriteBatch) -> Result<CommitResult, Error> {
        for condition in &batch.conditions {
            self.evaluate_condition(condition)?;
        }

        let current_version = self.global_version()?;
        let next_version = Version(
            current_version
                .0
                .checked_add(1)
                .ok_or_else(|| other("global storage version is exhausted"))?,
        );
        let data_cf = self.data_cf()?;
        let meta_cf = self.meta_cf()?;
        let mut rocks_batch = RocksWriteBatch::default();
        for operation in batch.operations {
            match operation {
                Operation::Put {
                    namespace,
                    key,
                    value,
                } => {
                    let physical_key = encode_physical_key(&namespace, &key)?;
                    let frame = encode_stored_value(&value, next_version)?;
                    rocks_batch.put_cf(data_cf, physical_key, frame);
                }
                Operation::Delete { namespace, key } => {
                    let physical_key = encode_physical_key(&namespace, &key)?;
                    rocks_batch.delete_cf(data_cf, physical_key);
                }
            }
        }
        rocks_batch.put_cf(
            meta_cf,
            GLOBAL_VERSION_KEY,
            encode_global_version(next_version)?,
        );

        let mut write_options = WriteOptions::default();
        write_options.disable_wal(false);
        write_options.set_sync(true);
        self.db
            .write_opt(rocks_batch, &write_options)
            .map_err(|error| unavailable(format!("RocksDB atomic commit failed: {error}")))?;

        Ok(CommitResult {
            version: next_version,
        })
    }

    fn evaluate_condition(&self, condition: &Condition) -> Result<(), Error> {
        match condition {
            Condition::Missing { namespace, key } => {
                if self.get(namespace, key)?.is_some() {
                    return Err(conflict(format!(
                        "missing condition failed in namespace `{}` because the key exists",
                        namespace.0
                    )));
                }
            }
            Condition::Version {
                namespace,
                key,
                expected,
            } => {
                let actual = self.get(namespace, key)?.ok_or_else(|| {
                    conflict(format!(
                        "version condition failed in namespace `{}` because the key is missing",
                        namespace.0
                    ))
                })?;
                if actual.version != *expected {
                    return Err(conflict(format!(
                        "version condition failed in namespace `{}`: expected {}, found {}",
                        namespace.0, expected.0, actual.version.0
                    )));
                }
            }
        }
        Ok(())
    }

    pub(super) fn global_version(&self) -> Result<Version, Error> {
        let raw = self
            .db
            .get_cf(self.meta_cf()?, GLOBAL_VERSION_KEY)
            .map_err(|error| unavailable(format!("failed to read RocksDB metadata: {error}")))?;
        raw.as_deref()
            .map(decode_global_version)
            .transpose()
            .map(|version| version.unwrap_or(Version(0)))
    }

    pub(super) fn data_cf(&self) -> Result<&ColumnFamily, Error> {
        self.db
            .cf_handle(DATA_COLUMN_FAMILY)
            .ok_or_else(|| corrupt_data("RocksDB data column family is missing"))
    }

    pub(super) fn meta_cf(&self) -> Result<&ColumnFamily, Error> {
        self.db
            .cf_handle(META_COLUMN_FAMILY)
            .ok_or_else(|| corrupt_data("RocksDB meta column family is missing"))
    }
}
