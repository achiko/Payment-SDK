use std::{fs, path::Path, process::Command};

use super::*;
use redb::{Durability, TableDefinition};
use storage::{ErrorKind, Operation, Version};
use tempfile::TempDir;

use crate::codec::{encode_global_version, encode_physical_key, encode_stored_value};

const CRASH_CHILD_PATH_ENV: &str = "STORAGE_REDB_CRASH_CHILD_PATH";

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

fn create_raw(path: &Path) -> Result<Database, Error> {
    Database::create(path).map_err(|error| other(format!("test database create failed: {error}")))
}

fn open_raw(path: &Path) -> Result<Database, Error> {
    Database::open(path).map_err(|error| other(format!("test database open failed: {error}")))
}

#[tokio::test]
async fn closed_database_file_can_be_copied_and_reopened() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    let backup_path = directory.path().join("backup.redb");
    let records = namespace("records");
    let snapshot_key = key("snapshot");
    {
        let storage = Redb::open(&database_path)?;
        storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &snapshot_key, "captured")],
            })
            .await?;
    }

    let copied = fs::copy(&database_path, &backup_path)
        .map_err(|error| other(format!("cold database copy failed: {error}")))?;
    assert!(copied > 0);
    let backup = Redb::open(&backup_path)?;
    assert_eq!(
        backup.get(&records, &snapshot_key).await?,
        Some(StoredValue {
            value: value("captured"),
            version: Version(1),
        })
    );
    Ok(())
}

#[tokio::test]
async fn second_open_is_rejected_by_file_lock() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    let storage = Redb::open(&database_path)?;

    let second_open = match Redb::open(&database_path) {
        Ok(_) => panic!("redb must enforce one writable owner per file"),
        Err(error) => error,
    };
    assert_eq!(second_open.kind, ErrorKind::Unavailable);

    drop(storage);
    let _reopened = Redb::open(&database_path)?;
    Ok(())
}

#[tokio::test]
async fn populated_database_without_format_marker_is_rejected() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    let records = namespace("records");
    let primary = key("primary");
    {
        let db = create_raw(&database_path)?;
        let mut transaction = db
            .begin_write()
            .map_err(|error| other(format!("test transaction failed: {error}")))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| other(format!("test durability failed: {error}")))?;
        {
            let mut data = transaction
                .open_table(DATA_TABLE)
                .map_err(|error| other(format!("test data table failed: {error}")))?;
            let mut meta = transaction
                .open_table(META_TABLE)
                .map_err(|error| other(format!("test metadata table failed: {error}")))?;
            let physical_key = encode_physical_key(&records, &primary)?;
            let stored = encode_stored_value(&value("stored-value"), Version(1))?;
            drop(
                data.insert(physical_key.as_slice(), stored.as_slice())
                    .map_err(|error| other(format!("test data write failed: {error}")))?,
            );
            let version = encode_global_version(Version(1))?;
            drop(
                meta.insert(GLOBAL_VERSION_KEY, version.as_slice())
                    .map_err(|error| other(format!("test metadata write failed: {error}")))?,
            );
        }
        transaction
            .commit()
            .map_err(|error| other(format!("test commit failed: {error}")))?;
    }

    let open_error = match Redb::open(&database_path) {
        Ok(_) => panic!("a populated database without a format marker must fail closed"),
        Err(error) => error,
    };
    assert_eq!(open_error.kind, ErrorKind::CorruptData);
    Ok(())
}

#[test]
fn populated_database_without_global_version_is_rejected() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    let records = namespace("records");
    let primary = key("primary");
    {
        let db = create_raw(&database_path)?;
        let mut transaction = db
            .begin_write()
            .map_err(|error| other(format!("test transaction failed: {error}")))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| other(format!("test durability failed: {error}")))?;
        {
            let mut data = transaction
                .open_table(DATA_TABLE)
                .map_err(|error| other(format!("test data table failed: {error}")))?;
            let mut meta = transaction
                .open_table(META_TABLE)
                .map_err(|error| other(format!("test metadata table failed: {error}")))?;
            let physical_key = encode_physical_key(&records, &primary)?;
            let stored = encode_stored_value(&value("stored-value"), Version(1))?;
            drop(
                data.insert(physical_key.as_slice(), stored.as_slice())
                    .map_err(|error| other(format!("test data write failed: {error}")))?,
            );
            drop(
                meta.insert(DATABASE_FORMAT_KEY, crate::format::DATABASE_FORMAT)
                    .map_err(|error| other(format!("test marker write failed: {error}")))?,
            );
        }
        transaction
            .commit()
            .map_err(|error| other(format!("test commit failed: {error}")))?;
    }

    let error = match Redb::open(&database_path) {
        Ok(_) => panic!("populated data without a global version must fail closed"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::CorruptData);
    Ok(())
}

#[test]
fn empty_initialized_database_without_global_version_reopens() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    drop(Redb::open(&database_path)?);

    let reopened = Redb::open(&database_path)?;
    drop(reopened);
    Ok(())
}

#[test]
fn database_with_only_a_foreign_table_is_rejected() -> Result<(), Error> {
    const FOREIGN_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("foreign");
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    {
        let db = create_raw(&database_path)?;
        let transaction = db
            .begin_write()
            .map_err(|error| other(format!("test transaction failed: {error}")))?;
        {
            let mut foreign = transaction
                .open_table(FOREIGN_TABLE)
                .map_err(|error| other(format!("test foreign table failed: {error}")))?;
            drop(
                foreign
                    .insert(b"key".as_slice(), b"value".as_slice())
                    .map_err(|error| other(format!("test foreign write failed: {error}")))?,
            );
        }
        transaction
            .commit()
            .map_err(|error| other(format!("test commit failed: {error}")))?;
    }

    let error = match Redb::open(&database_path) {
        Ok(_) => panic!("a redb file without this adapter's tables must not be adopted"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::CorruptData, "{error}");
    Ok(())
}

#[test]
fn incompatible_table_type_is_rejected() -> Result<(), Error> {
    const WRONG_DATA_TABLE: TableDefinition<u64, u64> = TableDefinition::new("data");
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    {
        let db = create_raw(&database_path)?;
        let transaction = db
            .begin_write()
            .map_err(|error| other(format!("test transaction failed: {error}")))?;
        {
            let mut data = transaction
                .open_table(WRONG_DATA_TABLE)
                .map_err(|error| other(format!("test wrong table failed: {error}")))?;
            drop(
                data.insert(1, 2)
                    .map_err(|error| other(format!("test wrong write failed: {error}")))?,
            );
            let mut meta = transaction
                .open_table(META_TABLE)
                .map_err(|error| other(format!("test metadata table failed: {error}")))?;
            drop(
                meta.insert(DATABASE_FORMAT_KEY, crate::format::DATABASE_FORMAT)
                    .map_err(|error| other(format!("test marker write failed: {error}")))?,
            );
        }
        transaction
            .commit()
            .map_err(|error| other(format!("test commit failed: {error}")))?;
    }

    let error = match Redb::open(&database_path) {
        Ok(_) => panic!("an incompatible data table type must fail closed"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::CorruptData);
    Ok(())
}

#[test]
fn different_database_format_fails_closed() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    {
        let storage = Redb::open(&database_path)?;
        drop(storage);
    }
    {
        let db = open_raw(&database_path)?;
        let transaction = db
            .begin_write()
            .map_err(|error| other(format!("test transaction failed: {error}")))?;
        {
            let mut meta = transaction
                .open_table(META_TABLE)
                .map_err(|error| other(format!("test metadata table failed: {error}")))?;
            drop(
                meta.insert(DATABASE_FORMAT_KEY, b"another-database-format".as_slice())
                    .map_err(|error| other(format!("test marker write failed: {error}")))?,
            );
        }
        transaction
            .commit()
            .map_err(|error| other(format!("test commit failed: {error}")))?;
    }

    let error = match Redb::open(&database_path) {
        Ok(_) => panic!("a different database format must fail closed"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::CorruptData);
    Ok(())
}

#[tokio::test]
async fn malformed_persisted_frame_is_reported_as_corruption() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    let records = namespace("records");
    let primary = key("primary");
    {
        let storage = Redb::open(&database_path)?;
        drop(storage);
    }
    {
        let db = open_raw(&database_path)?;
        let transaction = db
            .begin_write()
            .map_err(|error| other(format!("test transaction failed: {error}")))?;
        {
            let mut data = transaction
                .open_table(DATA_TABLE)
                .map_err(|error| other(format!("test data table failed: {error}")))?;
            let mut meta = transaction
                .open_table(META_TABLE)
                .map_err(|error| other(format!("test metadata table failed: {error}")))?;
            let physical_key = encode_physical_key(&records, &primary)?;
            drop(
                data.insert(physical_key.as_slice(), b"invalid".as_slice())
                    .map_err(|error| other(format!("test malformed write failed: {error}")))?,
            );
            let version = encode_global_version(Version(1))?;
            drop(
                meta.insert(GLOBAL_VERSION_KEY, version.as_slice())
                    .map_err(|error| other(format!("test metadata write failed: {error}")))?,
            );
        }
        transaction
            .commit()
            .map_err(|error| other(format!("test commit failed: {error}")))?;
    }

    let reopened = Redb::open(&database_path)?;
    let error = reopened
        .get(&records, &primary)
        .await
        .expect_err("a malformed value frame must be rejected");
    assert_eq!(error.kind, ErrorKind::CorruptData);
    Ok(())
}

#[test]
fn relative_path_is_rejected() {
    let error = match Redb::open("relative-index.redb") {
        Ok(_) => panic!("storage must not resolve ambient process paths"),
        Err(error) => error,
    };

    assert_eq!(error.kind, ErrorKind::InvalidRequest);
}

#[test]
fn directory_path_is_rejected() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let error = match Redb::open(directory.path()) {
        Ok(_) => panic!("a database directory must not be treated as a redb file"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    Ok(())
}

#[test]
fn missing_parent_directory_is_rejected() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("missing").join("database.redb");
    let error = match Redb::open(database_path) {
        Ok(_) => panic!("the adapter must not create database parent directories"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    Ok(())
}

#[test]
fn foreign_file_is_rejected() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    fs::write(&database_path, b"this is not a redb database")
        .map_err(|error| other(format!("test foreign file write failed: {error}")))?;

    let error = match Redb::open(&database_path) {
        Ok(_) => panic!("an arbitrary existing file must not be adopted"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::CorruptData, "{error}");
    Ok(())
}

#[test]
fn zero_queue_capacity_is_rejected() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");

    let error = match Redb::open_with_queue_capacity(database_path, 0) {
        Ok(_) => panic!("a zero-capacity queue cannot provide bounded command delivery"),
        Err(error) => error,
    };

    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    Ok(())
}

#[tokio::test]
async fn forced_process_exit_preserves_immediate_commit() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database_path = directory.path().join("database.redb");
    let output = Command::new(
        std::env::current_exe()
            .map_err(|error| other(format!("test executable failed: {error}")))?,
    )
    .arg("--exact")
    .arg("backend::recovery_test::forced_exit_writer_helper")
    .arg("--nocapture")
    .env(CRASH_CHILD_PATH_ENV, &database_path)
    .output()
    .map_err(|error| other(format!("failed to start forced-exit child: {error}")))?;
    if !output.status.success() {
        return Err(other(format!(
            "forced-exit child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let storage = Redb::open(&database_path)?;
    assert_eq!(
        storage
            .get(&namespace("crash-recovery"), &key("committed"))
            .await?,
        Some(StoredValue {
            value: value("durable"),
            version: Version(1),
        })
    );
    Ok(())
}

#[test]
fn forced_exit_writer_helper() {
    let Some(path) = std::env::var_os(CRASH_CHILD_PATH_ENV) else {
        return;
    };
    let storage = Redb::open(path).expect("forced-exit child must open redb");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("forced-exit child must create a runtime");
    runtime
        .block_on(storage.commit(WriteBatch {
            conditions: vec![],
            operations: vec![put(
                &namespace("crash-recovery"),
                &key("committed"),
                "durable",
            )],
        }))
        .expect("forced-exit child commit must return");
    std::process::exit(0);
}
