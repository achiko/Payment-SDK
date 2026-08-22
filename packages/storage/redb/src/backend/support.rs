use redb::{
    CommitError, DatabaseError, SetDurabilityError, StorageError, TableError, TransactionError,
};
use storage::{Error, ErrorKind};

pub(super) fn database_error(error: DatabaseError) -> Error {
    match error {
        DatabaseError::DatabaseAlreadyOpen => {
            unavailable("redb database file is already open for writing")
        }
        DatabaseError::UpgradeRequired(version) => corrupt_data(format!(
            "redb database file requires an unsupported format upgrade from version {version}"
        )),
        DatabaseError::Storage(error) => storage_error(error, "redb database open failed"),
        DatabaseError::RepairAborted => {
            corrupt_data("redb database repair was aborted while opening the file")
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
        | TableError::TableExists(_) => corrupt_data(format!("{context}: {error}")),
        TableError::TableAlreadyOpen(_, _) => other(format!("{context}: {error}")),
        _ => corrupt_data(format!("{context}: {error}")),
    }
}

pub(super) fn operation_error(error: StorageError, context: &str) -> Error {
    storage_error(error, context)
}

pub(super) fn durability_error(error: SetDurabilityError, context: &str) -> Error {
    other(format!("{context}: {error}"))
}

pub(super) fn commit_error(error: CommitError) -> Error {
    // A failed commit can have persisted before the error became observable.
    // The caller must treat the outcome as unknown and reconcile via CAS.
    unavailable(format!("redb atomic commit outcome is unknown: {error}"))
}

fn storage_error(error: StorageError, context: &str) -> Error {
    match error {
        StorageError::Corrupted(detail) => corrupt_data(format!("{context}: {detail}")),
        StorageError::ValueTooLarge(size) => invalid_request(format!(
            "{context}: redb rejected a key or value with {size} bytes"
        )),
        StorageError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            corrupt_data(format!("{context}: {error}"))
        }
        StorageError::Io(error) => unavailable(format!("{context}: {error}")),
        StorageError::PreviousIo | StorageError::DatabaseClosed | StorageError::LockPoisoned(_) => {
            unavailable(format!("{context}: {error}"))
        }
        _ => unavailable(format!("{context}: {error}")),
    }
}

pub(super) fn conflict(message: impl Into<String>) -> Error {
    Error {
        kind: ErrorKind::Conflict,
        message: message.into(),
    }
}

pub(super) fn unavailable(message: impl Into<String>) -> Error {
    Error {
        kind: ErrorKind::Unavailable,
        message: message.into(),
    }
}

pub(super) fn corrupt_data(message: impl Into<String>) -> Error {
    Error {
        kind: ErrorKind::CorruptData,
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
