use storage::{StorageError, StorageErrorKind};

const SCHEMA_MAGIC: &[u8; 4] = b"W3SV";
const SCHEMA_FRAME_VERSION: u8 = 1;
const SCHEMA_FRAME_LEN: usize = 9;

/// Persistent physical-schema version understood by this adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchemaVersion(pub u32);

impl SchemaVersion {
    /// Legacy databases created before schema metadata was introduced.
    pub const LEGACY: Self = Self(0);

    /// Returns the numeric schema version.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Physical schema emitted by this version of `storage-rocksdb`.
pub const CURRENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion(1);

/// One ordered, supported physical-schema transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaMigration {
    pub from: SchemaVersion,
    pub to: SchemaVersion,
}

/// Result of an explicit closed-database migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationReport {
    pub previous: SchemaVersion,
    pub current: SchemaVersion,
    pub applied: Vec<SchemaMigration>,
}

pub(crate) const MIGRATION_V0_TO_V1: SchemaMigration = SchemaMigration {
    from: SchemaVersion::LEGACY,
    to: CURRENT_SCHEMA_VERSION,
};

/// Ordered physical-schema transitions implemented by this binary.
pub const REGISTERED_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[MIGRATION_V0_TO_V1];

pub(crate) fn encode_schema_version(version: SchemaVersion) -> Result<Vec<u8>, StorageError> {
    if version == SchemaVersion::LEGACY {
        return Err(invalid_request(
            "schema version zero is represented by absent legacy metadata",
        ));
    }

    let mut frame = Vec::with_capacity(SCHEMA_FRAME_LEN);
    frame.extend_from_slice(SCHEMA_MAGIC);
    frame.push(SCHEMA_FRAME_VERSION);
    frame.extend_from_slice(&version.0.to_be_bytes());
    Ok(frame)
}

pub(crate) fn decode_schema_version(frame: &[u8]) -> Result<SchemaVersion, StorageError> {
    if frame.len() != SCHEMA_FRAME_LEN {
        return Err(corrupt_data(format!(
            "schema version frame has length {}, expected {SCHEMA_FRAME_LEN}",
            frame.len()
        )));
    }
    if &frame[..4] != SCHEMA_MAGIC {
        return Err(corrupt_data("schema version frame has invalid magic bytes"));
    }
    if frame[4] != SCHEMA_FRAME_VERSION {
        return Err(corrupt_data(format!(
            "schema version frame has unsupported envelope version {}",
            frame[4]
        )));
    }

    let mut encoded_version = [0_u8; 4];
    encoded_version.copy_from_slice(&frame[5..]);
    let version = SchemaVersion(u32::from_be_bytes(encoded_version));
    if version == SchemaVersion::LEGACY {
        return Err(corrupt_data(
            "schema version zero must be represented by absent legacy metadata",
        ));
    }
    Ok(version)
}

fn invalid_request(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::InvalidRequest,
        message: message.into(),
    }
}

fn corrupt_data(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::CorruptData,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_round_trips() -> Result<(), StorageError> {
        let encoded = encode_schema_version(CURRENT_SCHEMA_VERSION)?;
        assert_eq!(decode_schema_version(&encoded)?, CURRENT_SCHEMA_VERSION);
        Ok(())
    }

    #[test]
    fn future_envelope_version_is_rejected() -> Result<(), StorageError> {
        let mut encoded = encode_schema_version(CURRENT_SCHEMA_VERSION)?;
        encoded[4] = 2;

        let error = decode_schema_version(&encoded)
            .expect_err("an unknown schema envelope must fail closed");
        assert_eq!(error.kind, StorageErrorKind::CorruptData);
        Ok(())
    }
}
