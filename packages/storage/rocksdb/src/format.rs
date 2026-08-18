use storage::{Error, ErrorKind};

/// Marker written into every database created by this adapter.
///
/// The marker describes the complete physical layout. A database with any
/// other marker belongs to a different format and is rejected rather than
/// interpreted speculatively.
pub(crate) const DATABASE_FORMAT: &[u8] = b"w3-storage-rocksdb";

pub(crate) fn validate_database_format(bytes: &[u8]) -> Result<(), Error> {
    if bytes == DATABASE_FORMAT {
        return Ok(());
    }

    Err(Error {
        kind: ErrorKind::CorruptData,
        message: "database format is not supported by this adapter".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_database_format_is_accepted() -> Result<(), Error> {
        validate_database_format(DATABASE_FORMAT)
    }

    #[test]
    fn another_database_format_is_rejected() {
        let error = validate_database_format(b"another-format")
            .expect_err("a different database format must fail closed");
        assert_eq!(error.kind, ErrorKind::CorruptData);
    }
}
