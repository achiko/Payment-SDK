use redb::{CommitError, DatabaseError, StorageError, TableError, TransactionError};
use storage::{Error, ErrorKind};

// design-lint: allow unclassified-free-function -- redb database-open error translation between foreign types keeps backend-specific corruption and availability policy in this adapter
pub(super) fn database_error(error: DatabaseError) -> Error {
    match error {
        DatabaseError::DatabaseAlreadyOpen => {
            unavailable("redb database file is already open for writing")
        }
        DatabaseError::UpgradeRequired(version) => Error::corrupt_data(format!(
            "redb database file requires an unsupported format upgrade from version {version}"
        )),
        DatabaseError::Storage(error) => storage_error(error, "redb database open failed"),
        DatabaseError::RepairAborted => {
            Error::corrupt_data("redb database repair was aborted while opening the file")
        }
        DatabaseError::TransactionInProgress => {
            unavailable("redb database cannot open while a transaction is in progress")
        }
        _ => unavailable(format!("redb database open failed: {error}")),
    }
}

// design-lint: allow unclassified-free-function -- redb transaction-error translation between foreign types preserves local borrow conflicts and nested storage classification with caller context
pub(super) fn transaction_error(error: TransactionError, context: &str) -> Error {
    match error {
        TransactionError::Storage(error) => storage_error(error, context),
        TransactionError::ReadTransactionStillInUse(_) => Error {
            kind: ErrorKind::Other,
            message: format!("{context}: read transaction is still in use"),
        },
        _ => unavailable(format!("{context}: {error}")),
    }
}

// design-lint: allow unclassified-free-function -- redb table-error translation between foreign types distinguishes stored schema incompatibility from local borrow conflicts and preserves operation context
pub(super) fn table_error(error: TableError, context: &str) -> Error {
    match error {
        TableError::Storage(error) => storage_error(error, context),
        TableError::TableTypeMismatch { .. }
        | TableError::TableIsMultimap(_)
        | TableError::TableIsNotMultimap(_)
        | TableError::TypeDefinitionChanged { .. }
        | TableError::TableDoesNotExist(_)
        | TableError::TableExists(_) => Error::corrupt_data(format!("{context}: {error}")),
        TableError::TableAlreadyOpen(_, _) => Error {
            kind: ErrorKind::Other,
            message: format!("{context}: {error}"),
        },
        _ => Error::corrupt_data(format!("{context}: {error}")),
    }
}

// design-lint: allow unclassified-free-function -- shared redb commit-error adapter between foreign types preserves unknown persistence outcomes for format and batch commits
pub(super) fn commit_error(error: CommitError) -> Error {
    // A failed commit can have persisted before the error became observable.
    // The caller must treat the outcome as unknown and reconcile via CAS.
    unavailable(format!("redb atomic commit outcome is unknown: {error}"))
}

// design-lint: allow unclassified-free-function -- redb operation-error translation between foreign types preserves caller context and corruption, size, and availability policy separately from ambiguous commits
pub(super) fn storage_error(error: StorageError, context: &str) -> Error {
    match error {
        StorageError::Corrupted(detail) => Error::corrupt_data(format!("{context}: {detail}")),
        StorageError::ValueTooLarge(size) => Error::invalid_request(format!(
            "{context}: redb rejected a key or value with {size} bytes"
        )),
        StorageError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            Error::corrupt_data(format!("{context}: {error}"))
        }
        StorageError::Io(error) => unavailable(format!("{context}: {error}")),
        StorageError::PreviousIo | StorageError::DatabaseClosed | StorageError::LockPoisoned(_) => {
            unavailable(format!("{context}: {error}"))
        }
        _ => unavailable(format!("{context}: {error}")),
    }
}

// design-lint: allow unclassified-free-function -- redb owner-thread, filesystem and native-error boundaries construct foreign availability errors while preserving caller context and unknown commit outcomes
pub(super) fn unavailable(message: impl Into<String>) -> Error {
    Error {
        kind: ErrorKind::Unavailable,
        message: message.into(),
    }
}

#[cfg(test)]
pub(super) fn other(message: impl Into<String>) -> Error {
    Error {
        kind: ErrorKind::Other,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_borrow_conflicts_remain_other_errors() {
        use redb::ReadableDatabase;

        let directory = tempfile::tempdir().expect("temporary directory");
        let database = redb::Database::builder()
            .create(directory.path().join("borrow.redb"))
            .expect("test database");
        let read = database.begin_read().expect("read transaction");
        let error = transaction_error(
            TransactionError::ReadTransactionStillInUse(Box::new(read)),
            "close failed",
        );
        assert_eq!(error.kind, ErrorKind::Other);
        assert_eq!(
            error.message,
            "close failed: read transaction is still in use"
        );

        let native = TableError::TableAlreadyOpen("data".into(), std::panic::Location::caller());
        let expected = format!("open failed: {native}");
        let error = table_error(native, "open failed");
        assert_eq!(error.kind, ErrorKind::Other);
        assert_eq!(error.message, expected);
    }

    #[test]
    fn native_table_shape_errors_remain_corrupt_data_with_exact_context() {
        use redb::Value;

        for native in [
            TableError::TableTypeMismatch {
                table: "data".into(),
                key: u64::type_name(),
                value: u32::type_name(),
            },
            TableError::TableIsMultimap("data".into()),
            TableError::TableIsNotMultimap("data".into()),
            TableError::TypeDefinitionChanged {
                name: u64::type_name(),
                alignment: 8,
                width: Some(8),
            },
            TableError::TableDoesNotExist("data".into()),
            TableError::TableExists("data".into()),
        ] {
            let expected = format!("inspect table: {native}");
            let error = table_error(native, "inspect table");
            assert_eq!(error.kind, ErrorKind::CorruptData);
            assert_eq!(error.message, expected);
        }
    }

    #[test]
    fn wrapped_storage_errors_preserve_classification_and_operation_context() {
        for (native, kind, message) in [
            (
                StorageError::Corrupted("fixture damage".into()),
                ErrorKind::CorruptData,
                "operation failed: fixture damage",
            ),
            (
                StorageError::ValueTooLarge(42),
                ErrorKind::InvalidRequest,
                "operation failed: redb rejected a key or value with 42 bytes",
            ),
            (
                StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "fixture invalid data",
                )),
                ErrorKind::CorruptData,
                "operation failed: fixture invalid data",
            ),
            (
                StorageError::Io(std::io::Error::other("fixture outage")),
                ErrorKind::Unavailable,
                "operation failed: fixture outage",
            ),
        ] {
            let error = transaction_error(TransactionError::Storage(native), "operation failed");
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, message);
        }
        let error = table_error(
            TableError::Storage(StorageError::Corrupted("fixture damage".into())),
            "open table",
        );
        assert_eq!(error.kind, ErrorKind::CorruptData);
        assert_eq!(error.message, "open table: fixture damage");
        let error = table_error(
            TableError::Storage(StorageError::Io(std::io::Error::other("fixture outage"))),
            "open table",
        );
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "open table: fixture outage");
    }

    #[test]
    fn latched_closed_and_poisoned_storage_errors_remain_unavailable() {
        for native in [
            StorageError::PreviousIo,
            StorageError::DatabaseClosed,
            StorageError::LockPoisoned(std::panic::Location::caller()),
        ] {
            let expected = format!("read failed: {native}");
            let error = storage_error(native, "read failed");
            assert_eq!(error.kind, ErrorKind::Unavailable);
            assert_eq!(error.message, expected);
        }
    }

    #[test]
    fn oversized_native_values_keep_invalid_request_classification_and_context() {
        let error = storage_error(StorageError::ValueTooLarge(42), "write failed");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(
            error.message,
            "write failed: redb rejected a key or value with 42 bytes"
        );
    }

    #[test]
    fn database_open_errors_preserve_classification_and_context() {
        for (native, kind, message) in [
            (
                DatabaseError::DatabaseAlreadyOpen,
                ErrorKind::Unavailable,
                "redb database file is already open for writing",
            ),
            (
                DatabaseError::UpgradeRequired(1),
                ErrorKind::CorruptData,
                "redb database file requires an unsupported format upgrade from version 1",
            ),
            (
                DatabaseError::RepairAborted,
                ErrorKind::CorruptData,
                "redb database repair was aborted while opening the file",
            ),
            (
                DatabaseError::TransactionInProgress,
                ErrorKind::Unavailable,
                "redb database cannot open while a transaction is in progress",
            ),
            (
                DatabaseError::Storage(StorageError::Corrupted("fixture damage".to_owned())),
                ErrorKind::CorruptData,
                "redb database open failed: fixture damage",
            ),
            (
                DatabaseError::Storage(StorageError::Io(std::io::Error::other(
                    "fixture I/O failure",
                ))),
                ErrorKind::Unavailable,
                "redb database open failed: fixture I/O failure",
            ),
        ] {
            let error = database_error(native);
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn commit_errors_keep_unknown_outcome_classification_and_exact_context() {
        for (native, expected) in [
            (
                CommitError::Storage(StorageError::Io(std::io::Error::other(
                    "fixture I/O failure",
                ))),
                "redb atomic commit outcome is unknown: I/O error: fixture I/O failure",
            ),
            (
                CommitError::Storage(StorageError::Corrupted("fixture damage".to_owned())),
                "redb atomic commit outcome is unknown: DB corrupted: fixture damage",
            ),
            (
                CommitError::TransactionPoisoned,
                "redb atomic commit outcome is unknown: Transaction was poisoned by a panic",
            ),
        ] {
            let error = commit_error(native);
            assert_eq!(error.kind, ErrorKind::Unavailable);
            assert_eq!(error.message, expected);
        }
    }

    #[test]
    fn storage_corruption_outside_commit_remains_definite_corrupt_data() {
        let error = storage_error(
            StorageError::Corrupted("fixture damage".to_owned()),
            "read failed",
        );
        assert_eq!(error.kind, ErrorKind::CorruptData);
        assert_eq!(error.message, "read failed: fixture damage");
    }

    #[test]
    fn native_io_classification_preserves_corruption_and_unavailability() {
        let corruption = storage_error(
            StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "fixture damage",
            )),
            "read failed",
        );
        assert_eq!(corruption.kind, ErrorKind::CorruptData);
        assert_eq!(corruption.message, "read failed: fixture damage");
        let outage = storage_error(
            StorageError::Io(std::io::Error::other("fixture outage")),
            "read failed",
        );
        assert_eq!(outage.kind, ErrorKind::Unavailable);
        assert_eq!(outage.message, "read failed: fixture outage");
    }
}
