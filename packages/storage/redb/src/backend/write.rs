use redb::{Durability, ReadableDatabase, ReadableTable};
use storage::{CommitResult, Condition, Error, ErrorKind, Operation, Version, WriteBatch};

use crate::codec::{
    decode_global_version, decode_stored_value, encode_global_version, encode_physical_key,
    encode_stored_value,
};

use super::{
    Backend, DATA_TABLE, GLOBAL_VERSION_KEY, META_TABLE, commit_error, conflict, corrupt_data,
    durability_error, operation_error, other, table_error, transaction_error,
};

impl Backend {
    pub(super) fn commit(&mut self, batch: WriteBatch) -> Result<CommitResult, Error> {
        self.ensure_open()?;
        let result = self.commit_once(batch);
        match result {
            Err(mut error) if error.kind == ErrorKind::Unavailable => {
                // A redb I/O/commit failure latches the handle and a failed
                // commit may already be durable. Close and reopen, never replay.
                if let Err(reopen_error) = self.reopen() {
                    error.message = format!(
                        "{}; database reopen before the next write failed: {}",
                        error.message, reopen_error.message
                    );
                }
                Err(error)
            }
            other => other,
        }
    }

    fn commit_once(&mut self, batch: WriteBatch) -> Result<CommitResult, Error> {
        let mut transaction = self
            .database()?
            .begin_write()
            .map_err(|error| transaction_error(error, "failed to begin redb write transaction"))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| durability_error(error, "failed to configure redb commit"))?;

        let next_version;
        {
            let mut data = transaction
                .open_table(DATA_TABLE)
                .map_err(|error| table_error(error, "failed to open redb data table"))?;
            let mut meta = transaction
                .open_table(META_TABLE)
                .map_err(|error| table_error(error, "failed to open redb metadata table"))?;

            for condition in &batch.conditions {
                evaluate_condition(&data, condition)?;
            }

            let current_version = match meta
                .get(GLOBAL_VERSION_KEY)
                .map_err(|error| operation_error(error, "failed to read redb global version"))?
            {
                Some(raw) => decode_global_version(raw.value())?,
                None => Version(0),
            };
            next_version = Version(
                current_version
                    .0
                    .checked_add(1)
                    .ok_or_else(|| other("global storage version is exhausted"))?,
            );

            for operation in batch.operations {
                match operation {
                    Operation::Put {
                        namespace,
                        key,
                        value,
                    } => {
                        let physical_key = encode_physical_key(&namespace, &key)?;
                        let frame = encode_stored_value(&value, next_version)?;
                        drop(
                            data.insert(physical_key.as_slice(), frame.as_slice())
                                .map_err(|error| {
                                    operation_error(error, "failed to write redb data record")
                                })?,
                        );
                    }
                    Operation::Delete { namespace, key } => {
                        let physical_key = encode_physical_key(&namespace, &key)?;
                        drop(data.remove(physical_key.as_slice()).map_err(|error| {
                            operation_error(error, "failed to delete redb data record")
                        })?);
                    }
                }
            }
            let encoded_version = encode_global_version(next_version)?;
            drop(
                meta.insert(GLOBAL_VERSION_KEY, encoded_version.as_slice())
                    .map_err(|error| {
                        operation_error(error, "failed to write redb global version")
                    })?,
            );
        }

        transaction.commit().map_err(commit_error)?;

        #[cfg(test)]
        if std::mem::take(&mut self.fail_after_next_commit) {
            return Err(super::unavailable(
                "injected ambiguous redb commit result after persistence",
            ));
        }

        Ok(CommitResult {
            version: next_version,
        })
    }

    pub(super) fn global_version(&self) -> Result<Version, Error> {
        let transaction = self
            .database()?
            .begin_read()
            .map_err(|error| transaction_error(error, "failed to read redb metadata"))?;
        let meta = transaction
            .open_table(META_TABLE)
            .map_err(|error| table_error(error, "redb metadata table is incompatible"))?;
        if let Some(raw) = meta
            .get(GLOBAL_VERSION_KEY)
            .map_err(|error| operation_error(error, "failed to read redb global version"))?
        {
            return decode_global_version(raw.value());
        }
        drop(meta);

        let data = transaction
            .open_table(DATA_TABLE)
            .map_err(|error| table_error(error, "redb data table is incompatible"))?;
        if data
            .first()
            .map_err(|error| operation_error(error, "failed to inspect redb data table"))?
            .is_some()
        {
            return Err(corrupt_data(
                "redb database contains data records but has no global version",
            ));
        }
        Ok(Version(0))
    }
}

fn evaluate_condition(
    data: &impl ReadableTable<&'static [u8], &'static [u8]>,
    condition: &Condition,
) -> Result<(), Error> {
    match condition {
        Condition::Missing { namespace, key } => {
            let physical_key = encode_physical_key(namespace, key)?;
            if data
                .get(physical_key.as_slice())
                .map_err(|error| operation_error(error, "failed to evaluate redb condition"))?
                .is_some()
            {
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
            let physical_key = encode_physical_key(namespace, key)?;
            let raw = data
                .get(physical_key.as_slice())
                .map_err(|error| operation_error(error, "failed to evaluate redb condition"))?
                .ok_or_else(|| {
                    conflict(format!(
                        "version condition failed in namespace `{}` because the key is missing",
                        namespace.0
                    ))
                })?;
            let actual = decode_stored_value(raw.value())?;
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
