use std::path::Path;

use rocksdb::{
    ColumnFamilyDescriptor, DBCompressionType, Env, Options,
    backup::{BackupEngine, BackupEngineOptions},
};
use storage::{Error, ErrorKind};

use super::{BackupInfo, DATA_COLUMN_FAMILY, DEFAULT_COLUMN_FAMILY, META_COLUMN_FAMILY};

pub(super) fn column_family_descriptors() -> [ColumnFamilyDescriptor; 3] {
    let compressed = || {
        let mut options = Options::default();
        options.set_compression_type(DBCompressionType::Lz4);
        options
    };
    [
        ColumnFamilyDescriptor::new(DEFAULT_COLUMN_FAMILY, compressed()),
        ColumnFamilyDescriptor::new(META_COLUMN_FAMILY, compressed()),
        ColumnFamilyDescriptor::new(DATA_COLUMN_FAMILY, compressed()),
    ]
}

pub(super) fn open_backup_engine(backup_directory: &Path) -> Result<BackupEngine, Error> {
    let mut options = BackupEngineOptions::new(backup_directory).map_err(|error| {
        unavailable(format!(
            "failed to configure RocksDB backup directory: {error}"
        ))
    })?;
    options.set_sync(true);
    let environment = Env::new()
        .map_err(|error| unavailable(format!("failed to create RocksDB environment: {error}")))?;
    BackupEngine::open(&options, &environment)
        .map_err(|error| unavailable(format!("failed to open RocksDB backup engine: {error}")))
}

pub(super) fn latest_verified_backup(backup_engine: &BackupEngine) -> Result<BackupInfo, Error> {
    let latest = backup_engine
        .get_backup_info()
        .into_iter()
        .max_by_key(|info| info.backup_id)
        .ok_or_else(|| invalid_request("backup directory contains no RocksDB backups"))?;
    backup_engine
        .verify_backup(latest.backup_id)
        .map_err(|error| unavailable(format!("RocksDB backup verification failed: {error}")))?;
    Ok(BackupInfo {
        backup_id: latest.backup_id,
        timestamp: latest.timestamp,
        size: latest.size,
        file_count: latest.num_files,
    })
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
