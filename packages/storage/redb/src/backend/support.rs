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

pub(super) fn transaction_error(error: TransactionError, context: &str) -> Error {
    match error {
        TransactionError::Storage(error) => storage_error(error, context),
        TransactionError::ReadTransactionStillInUse(_) => {
            other(format!("{context}: read transaction is still in use"))
        }
        _ => unavailable(format!("{context}: {error}")),
    }
}

pub(super) fn table_error(error: TableError, context: &str) -> Error {
    match error {
        TableError::Storage(error) => storage_error(error, context),
        TableError::TableTypeMismatch { .. }
        | TableError::TableIsMultimap(_)
        | TableError::TableIsNotMultimap(_)
        | TableError::TypeDefinitionChanged { .. }
        | TableError::TableDoesNotExist(_)
        | TableError::TableExists(_) => Error::corrupt_data(format!("{context}: {error}")),
        TableError::TableAlreadyOpen(_, _) => other(format!("{context}: {error}")),
        _ => Error::corrupt_data(format!("{context}: {error}")),
    }
}

pub(super) fn operation_error(error: StorageError, context: &str) -> Error {
    storage_error(error, context)
}

// design-lint: allow unclassified-free-function -- shared redb commit-error adapter between foreign types preserves unknown persistence outcomes for format and batch commits
pub(super) fn commit_error(error: CommitError) -> Error {
    // A failed commit can have persisted before the error became observable.
    // The caller must treat the outcome as unknown and reconcile via CAS.
    unavailable(format!("redb atomic commit outcome is unknown: {error}"))
}

fn storage_error(error: StorageError, context: &str) -> Error {
    match error {
        StorageError::Corrupted(detail) => Error::corrupt_data(format!("{context}: {detail}")),
        StorageError::ValueTooLarge(size) => invalid_request(format!(
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

pub(super) fn unavailable(message: impl Into<String>) -> Error {
    Error {
        kind: ErrorKind::Unavailable,
        message: message.into(),
    }
}

pub(super) fn invalid_request(message: impl Into<String>) -> Error {
    Error {
        kind: ErrorKind::InvalidRequest,
        message: message.into(),
    }
}

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
        let error = operation_error(
            StorageError::Corrupted("fixture damage".to_owned()),
            "read failed",
        );
        assert_eq!(error.kind, ErrorKind::CorruptData);
        assert_eq!(error.message, "read failed: fixture damage");
    }
}
