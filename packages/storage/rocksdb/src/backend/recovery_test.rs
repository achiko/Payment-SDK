use super::*;
use rocksdb::ColumnFamilyDescriptor;
use storage::{ErrorKind, Operation, Version};
use tempfile::TempDir;

use crate::codec::{encode_global_version, encode_physical_key, encode_stored_value};

fn namespace(name: &str) -> Namespace {
    Namespace(name.to_owned())
}

fn key(value: &str) -> Key {
    Key(value.as_bytes().to_vec())
}

fn value(value: &str) -> storage::Value {
    storage::Value(value.as_bytes().to_vec())
}

fn put(namespace: &Namespace, key: &Key, value: &str) -> Operation {
    Operation::Put {
        namespace: namespace.clone(),
        key: key.clone(),
        value: self::value(value),
    }
}

#[tokio::test]
async fn backup_restore_recovers_the_verified_snapshot() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database");
    let backup_path = directory.path().join("backup");
    let restore_path = directory.path().join("restored");
    let records = namespace("records");
    let snapshot_key = key("snapshot");
    let later_key = key("after-backup");
    let storage = RocksDb::open(&database_path)?;
    storage
        .commit(WriteBatch {
            conditions: vec![],
            operations: vec![put(&records, &snapshot_key, "captured")],
        })
        .await?;

    let overlap_error = storage
        .create_backup(database_path.join("nested-backup"))
        .await
        .expect_err("backup data must not be placed inside the live database");
    assert_eq!(overlap_error.kind, ErrorKind::InvalidRequest);

    let backup = storage.create_backup(&backup_path).await?;
    assert!(backup.backup_id > 0);
    assert!(backup.size > 0);
    assert!(backup.file_count > 0);

    storage
        .commit(WriteBatch {
            conditions: vec![],
            operations: vec![put(&records, &later_key, "not-captured")],
        })
        .await?;
    let overwrite_error = RocksDb::restore_latest_backup(&backup_path, &database_path)
        .expect_err("restore must never overwrite an existing database path");
    assert_eq!(overwrite_error.kind, ErrorKind::InvalidRequest);
    drop(storage);

    let restored = RocksDb::restore_latest_backup(&backup_path, &restore_path)?;
    assert_eq!(restored, backup);
    let storage = RocksDb::open(&restore_path)?;
    assert_eq!(
        storage.get(&records, &snapshot_key).await?,
        Some(StoredValue {
            value: value("captured"),
            version: Version(1),
        })
    );
    assert_eq!(storage.get(&records, &later_key).await?, None);
    Ok(())
}

#[tokio::test]
async fn second_open_is_rejected_by_path_lock() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let storage = RocksDb::open(directory.path())?;

    let second_open = match RocksDb::open(directory.path()) {
        Ok(_) => panic!("RocksDB must enforce one open owner per path"),
        Err(error) => error,
    };
    assert_eq!(second_open.kind, ErrorKind::Unavailable);

    drop(storage);
    let _reopened = RocksDb::open(directory.path())?;
    Ok(())
}

#[tokio::test]
async fn populated_database_without_format_marker_is_rejected() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let records = namespace("records");
    let primary = key("primary");
    {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let db = DB::open_cf_descriptors(&options, directory.path(), column_family_descriptors())
            .map_err(|error| other(error.to_string()))?;
        let data_cf = db
            .cf_handle(DATA_COLUMN_FAMILY)
            .ok_or_else(|| other("test data column family is missing"))?;
        let meta_cf = db
            .cf_handle(META_COLUMN_FAMILY)
            .ok_or_else(|| other("test meta column family is missing"))?;
        db.put_cf(
            data_cf,
            encode_physical_key(&records, &primary)?,
            encode_stored_value(&value("stored-value"), Version(1))?,
        )
        .map_err(|error| other(error.to_string()))?;
        db.put_cf(
            meta_cf,
            GLOBAL_VERSION_KEY,
            encode_global_version(Version(1))?,
        )
        .map_err(|error| other(error.to_string()))?;
    }

    let open_error = match RocksDb::open(directory.path()) {
        Ok(_) => panic!("a populated database without a format marker must fail closed"),
        Err(error) => error,
    };
    assert_eq!(open_error.kind, ErrorKind::CorruptData);
    Ok(())
}

#[test]
fn different_database_format_fails_closed() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    {
        let storage = RocksDb::open(directory.path())?;
        drop(storage);
    }
    {
        let options = Options::default();
        let db = DB::open_cf_descriptors(&options, directory.path(), column_family_descriptors())
            .map_err(|error| other(error.to_string()))?;
        let meta_cf = db
            .cf_handle(META_COLUMN_FAMILY)
            .ok_or_else(|| other("test meta column family is missing"))?;
        db.put_cf(meta_cf, DATABASE_FORMAT_KEY, b"another-database-format")
            .map_err(|error| other(error.to_string()))?;
    }

    let error = match RocksDb::open(directory.path()) {
        Ok(_) => panic!("a different database format must fail closed"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::CorruptData);
    Ok(())
}

#[tokio::test]
async fn malformed_persisted_frame_is_reported_as_corruption() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let records = namespace("records");
    let primary = key("primary");
    {
        let storage = RocksDb::open(directory.path())?;
        drop(storage);
    }
    {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let db = DB::open_cf_descriptors(
            &options,
            directory.path(),
            [
                ColumnFamilyDescriptor::new(DEFAULT_COLUMN_FAMILY, Options::default()),
                ColumnFamilyDescriptor::new(META_COLUMN_FAMILY, Options::default()),
                ColumnFamilyDescriptor::new(DATA_COLUMN_FAMILY, Options::default()),
            ],
        )
        .map_err(|error| other(error.to_string()))?;
        let data_cf = db
            .cf_handle(DATA_COLUMN_FAMILY)
            .ok_or_else(|| other("test data column family is missing"))?;
        db.put_cf(
            data_cf,
            encode_physical_key(&records, &primary)?,
            b"invalid",
        )
        .map_err(|error| other(error.to_string()))?;
    }

    let reopened = RocksDb::open(directory.path())?;
    let error = reopened
        .get(&records, &primary)
        .await
        .expect_err("a malformed value frame must be rejected");
    assert_eq!(error.kind, ErrorKind::CorruptData);
    Ok(())
}

#[test]
fn relative_path_is_rejected() {
    let error = match RocksDb::open("relative-index") {
        Ok(_) => panic!("storage must not resolve ambient process paths"),
        Err(error) => error,
    };

    assert_eq!(error.kind, ErrorKind::InvalidRequest);
}

#[test]
fn zero_queue_capacity_is_rejected() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;

    let error = match RocksDb::open_with_queue_capacity(directory.path(), 0) {
        Ok(_) => panic!("a zero-capacity queue cannot provide bounded command delivery"),
        Err(error) => error,
    };

    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    Ok(())
}
