use std::{
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::{Arc, mpsc as std_mpsc},
    thread,
};

use rocksdb::{
    ColumnFamily, ColumnFamilyDescriptor, DB, DBCompressionType, Direction, Env, IteratorMode,
    Options, WriteBatch as RocksWriteBatch, WriteOptions,
    backup::{BackupEngine, BackupEngineOptions, RestoreOptions},
};
use storage::{
    BoxFuture, CommitResult, Condition, Key, Namespace, Operation, ScanPage, ScanRequest, Storage,
    StorageError, StorageErrorKind, StoredValue, Version, WriteBatch,
};
use tokio::sync::{mpsc, oneshot};

use crate::codec::{
    decode_global_version, decode_physical_key, decode_stored_value, encode_global_version,
    encode_physical_key, encode_stored_value, namespace_prefix,
};
use crate::schema::{
    CURRENT_SCHEMA_VERSION, MIGRATION_V0_TO_V1, MigrationReport, REGISTERED_SCHEMA_MIGRATIONS,
    SchemaMigration, SchemaVersion, decode_schema_version, encode_schema_version,
};

const META_COLUMN_FAMILY: &str = "meta";
const DATA_COLUMN_FAMILY: &str = "data";
const DEFAULT_COLUMN_FAMILY: &str = "default";
const GLOBAL_VERSION_KEY: &[u8] = b"global-version";
const SCHEMA_VERSION_KEY: &[u8] = b"schema-version";

/// Default upper bound for operations accepted but not yet handled by the DB
/// owner thread.
pub const DEFAULT_COMMAND_QUEUE_CAPACITY: usize = 256;

/// Verified RocksDB backup metadata returned after backup or restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackupInfo {
    pub backup_id: u32,
    pub timestamp: i64,
    pub size: u64,
    pub file_count: u32,
}

/// Backup evidence and schema transitions from one migration run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationOutcome {
    pub backup: BackupInfo,
    pub report: MigrationReport,
}

/// A serialized, durable [`Storage`] implementation backed by RocksDB.
///
/// All RocksDB access is routed through one bounded channel to one dedicated
/// OS thread. This makes condition evaluation and the following write batch a
/// single logical critical section without blocking the async runtime.
#[derive(Clone)]
pub struct RocksDbStorage {
    inner: Arc<Inner>,
}

struct Inner {
    command_tx: Option<mpsc::Sender<Command>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl RocksDbStorage {
    /// Opens or creates a database using the default bounded command capacity.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::Unavailable`] when the worker thread or
    /// RocksDB cannot be opened, and [`StorageErrorKind::CorruptData`] when
    /// persisted metadata cannot be decoded.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_with_queue_capacity(path, DEFAULT_COMMAND_QUEUE_CAPACITY)
    }

    /// Opens or creates a database with an explicit bounded command capacity.
    ///
    /// # Errors
    ///
    /// A zero capacity is rejected as an invalid request. Other open failures
    /// are returned with storage context.
    pub fn open_with_queue_capacity(
        path: impl AsRef<Path>,
        command_queue_capacity: usize,
    ) -> Result<Self, StorageError> {
        if command_queue_capacity == 0 {
            return Err(invalid_request(
                "RocksDB command queue capacity must be greater than zero",
            ));
        }

        let path = path.as_ref().to_path_buf();
        let (command_tx, command_rx) = mpsc::channel(command_queue_capacity);
        let (startup_tx, startup_rx) = std_mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("storage-rocksdb-owner".to_owned())
            .spawn(move || match Backend::open(&path) {
                Ok(backend) => {
                    if startup_tx.send(Ok(())).is_ok() {
                        backend.run(command_rx);
                    }
                }
                Err(error) => {
                    drop(startup_tx.send(Err(error)));
                }
            })
            .map_err(|error| {
                unavailable(format!("failed to spawn RocksDB owner thread: {error}"))
            })?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(Inner {
                    command_tx: Some(command_tx),
                    worker: Some(worker),
                }),
            }),
            Ok(Err(error)) => {
                drop(command_tx);
                drop(worker.join());
                Err(error)
            }
            Err(error) => {
                drop(command_tx);
                let worker_result = worker.join();
                let detail = if worker_result.is_err() {
                    "the owner thread panicked during startup"
                } else {
                    "the owner thread exited during startup"
                };
                Err(unavailable(format!(
                    "failed to receive RocksDB startup result: {error}; {detail}"
                )))
            }
        }
    }

    /// Creates and verifies a consistent backup on the DB owner thread.
    ///
    /// The backup is ordered with reads and commits already accepted by this
    /// handle. RocksDB flushes memtables before capturing the snapshot. The
    /// backup directory must not equal, contain, or be contained by the live
    /// database directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the owner thread is unavailable, the paths
    /// overlap, or RocksDB cannot create or verify the backup.
    pub async fn create_backup(
        &self,
        backup_directory: impl AsRef<Path>,
    ) -> Result<BackupInfo, StorageError> {
        let sender = self.command_sender()?;
        let backup_directory = backup_directory.as_ref().to_path_buf();
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(Command::CreateBackup {
                backup_directory,
                reply: reply_tx,
            })
            .await
            .map_err(|_| unavailable("RocksDB owner thread stopped before backup was accepted"))?;
        reply_rx.await.map_err(|_| {
            unavailable("RocksDB owner thread stopped before backup returned a result")
        })?
    }

    /// Returns the physical schema version persisted by this database.
    ///
    /// # Errors
    ///
    /// Returns an error if the owner thread is unavailable or schema metadata
    /// is corrupt.
    pub async fn schema_version(&self) -> Result<SchemaVersion, StorageError> {
        let sender = self.command_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(Command::SchemaVersion { reply: reply_tx })
            .await
            .map_err(|_| {
                unavailable("RocksDB owner thread stopped before schema read was accepted")
            })?;
        reply_rx.await.map_err(|_| {
            unavailable("RocksDB owner thread stopped before schema read returned a result")
        })?
    }

    /// Backs up and applies every registered migration to a closed database.
    ///
    /// This operation acquires RocksDB's exclusive path lock. It therefore
    /// fails when this or another process still has the database open. It
    /// creates and verifies the requested backup before the first schema
    /// mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for an open database, backup failure, corrupt metadata,
    /// an unsupported migration gap, or a database newer than this binary.
    pub fn migrate(
        path: impl AsRef<Path>,
        backup_directory: impl AsRef<Path>,
    ) -> Result<MigrationOutcome, StorageError> {
        let backend = Backend::open_for_migration(path.as_ref())?;
        let backup = backend.create_backup(backup_directory.as_ref())?;
        let report = backend.migrate_to_current()?;
        Ok(MigrationOutcome { backup, report })
    }

    /// Restores the latest verified backup into a new database directory.
    ///
    /// The destination must not already exist. This deliberately prevents a
    /// restore from overwriting a live or merely forgotten database; operators
    /// must choose a fresh path and switch to it after validation.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination exists, paths overlap, no backup is
    /// present, verification fails, restore fails, or the restored database
    /// cannot be opened by this binary.
    pub fn restore_latest_backup(
        backup_directory: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<BackupInfo, StorageError> {
        let backup_directory = normalized_absolute_path(backup_directory.as_ref())?;
        let destination = normalized_absolute_path(destination.as_ref())?;
        let backup_overlap_path = resolved_path_for_overlap(&backup_directory)?;
        let destination_overlap_path = resolved_path_for_overlap(&destination)?;
        validate_separate_paths(
            &backup_overlap_path,
            &destination_overlap_path,
            "backup",
            "destination",
        )?;
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                return Err(invalid_request(
                    "restore destination already exists; restore requires a new database path",
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(unavailable(format!(
                    "failed to inspect restore destination: {error}"
                )));
            }
        }

        let mut backup_engine = open_backup_engine(&backup_directory)?;
        let info = latest_verified_backup(&backup_engine)?;
        let mut restore_options = RestoreOptions::default();
        restore_options.set_keep_log_files(false);
        backup_engine
            .restore_from_latest_backup(&destination, &destination, &restore_options)
            .map_err(|error| unavailable(format!("RocksDB restore failed: {error}")))?;

        // Validation accepts both the current schema and a registered legacy
        // migration source, so a pre-migration backup remains restorable.
        let restored = Backend::open_for_migration(&destination)?;
        restored.validate_migration_source()?;
        drop(restored);
        Ok(info)
    }

    fn command_sender(&self) -> Result<mpsc::Sender<Command>, StorageError> {
        self.inner
            .command_tx
            .as_ref()
            .cloned()
            .ok_or_else(|| unavailable("RocksDB owner thread is shutting down"))
    }

    #[cfg(test)]
    async fn hold_owner_for_test(&self) -> Result<std_mpsc::Sender<()>, StorageError> {
        let sender = self.command_sender()?;
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        sender
            .send(Command::TestHold {
                entered: entered_tx,
                release: release_rx,
            })
            .await
            .map_err(|_| {
                unavailable("RocksDB owner thread stopped before test hold was accepted")
            })?;
        entered_rx
            .await
            .map_err(|_| unavailable("RocksDB owner thread stopped before entering test hold"))?;
        Ok(release_tx)
    }
}

impl Storage for RocksDbStorage {
    fn get<'a>(
        &'a self,
        namespace: &'a Namespace,
        key: &'a Key,
    ) -> BoxFuture<'a, Result<Option<StoredValue>, StorageError>> {
        let sender = self.command_sender();
        let namespace = namespace.clone();
        let key = key.clone();
        Box::pin(async move {
            let sender = sender?;
            let (reply_tx, reply_rx) = oneshot::channel();
            sender
                .send(Command::Get {
                    namespace,
                    key,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| unavailable("RocksDB owner thread stopped before get was accepted"))?;
            reply_rx.await.map_err(|_| {
                unavailable("RocksDB owner thread stopped before get returned a result")
            })?
        })
    }

    fn scan<'a>(&'a self, request: ScanRequest) -> BoxFuture<'a, Result<ScanPage, StorageError>> {
        let sender = self.command_sender();
        Box::pin(async move {
            let sender = sender?;
            let (reply_tx, reply_rx) = oneshot::channel();
            sender
                .send(Command::Scan {
                    request,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| {
                    unavailable("RocksDB owner thread stopped before scan was accepted")
                })?;
            reply_rx.await.map_err(|_| {
                unavailable("RocksDB owner thread stopped before scan returned a result")
            })?
        })
    }

    fn commit<'a>(
        &'a self,
        batch: WriteBatch,
    ) -> BoxFuture<'a, Result<CommitResult, StorageError>> {
        let sender = self.command_sender();
        Box::pin(async move {
            let sender = sender?;
            let (reply_tx, reply_rx) = oneshot::channel();
            sender
                .send(Command::Commit {
                    batch,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| {
                    unavailable("RocksDB owner thread stopped before commit was accepted")
                })?;
            reply_rx.await.map_err(|_| {
                unavailable("RocksDB owner thread stopped before commit returned a result")
            })?
        })
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Closing the last command sender lets the owner finish every accepted
        // command before dropping RocksDB. This is deliberately synchronous at
        // final ownership teardown so a subsequent reopen cannot race the old
        // database handle.
        drop(self.command_tx.take());
        if let Some(worker) = self.worker.take() {
            drop(worker.join());
        }
    }
}

enum Command {
    Get {
        namespace: Namespace,
        key: Key,
        reply: oneshot::Sender<Result<Option<StoredValue>, StorageError>>,
    },
    Scan {
        request: ScanRequest,
        reply: oneshot::Sender<Result<ScanPage, StorageError>>,
    },
    Commit {
        batch: WriteBatch,
        reply: oneshot::Sender<Result<CommitResult, StorageError>>,
    },
    CreateBackup {
        backup_directory: PathBuf,
        reply: oneshot::Sender<Result<BackupInfo, StorageError>>,
    },
    SchemaVersion {
        reply: oneshot::Sender<Result<SchemaVersion, StorageError>>,
    },
    #[cfg(test)]
    TestHold {
        entered: oneshot::Sender<()>,
        release: std_mpsc::Receiver<()>,
    },
}

struct Backend {
    db: DB,
    database_path: PathBuf,
}

impl Backend {
    fn open(path: &Path) -> Result<Self, StorageError> {
        let database_path = normalized_absolute_path(path)?;
        let mut database_options = Options::default();
        database_options.create_if_missing(true);
        database_options.create_missing_column_families(true);
        let db = DB::open_cf_descriptors(
            &database_options,
            &database_path,
            column_family_descriptors(),
        )
        .map_err(|error| unavailable(format!("failed to open RocksDB database: {error}")))?;
        let database_path = fs::canonicalize(&database_path).map_err(|error| {
            unavailable(format!(
                "failed to resolve the opened RocksDB database path: {error}"
            ))
        })?;
        let backend = Self { db, database_path };

        backend.ensure_schema_ready()?;
        backend.global_version()?;
        Ok(backend)
    }

    fn open_for_migration(path: &Path) -> Result<Self, StorageError> {
        let database_path = normalized_absolute_path(path)?;
        if !database_path.is_dir() {
            return Err(invalid_request(
                "migration requires an existing RocksDB database directory",
            ));
        }

        let database_options = Options::default();
        let db = DB::open_cf_descriptors(
            &database_options,
            &database_path,
            column_family_descriptors(),
        )
        .map_err(|error| {
            unavailable(format!(
                "failed to open RocksDB database exclusively for migration: {error}"
            ))
        })?;
        let database_path = fs::canonicalize(&database_path).map_err(|error| {
            unavailable(format!(
                "failed to resolve the opened RocksDB database path: {error}"
            ))
        })?;
        Ok(Self { db, database_path })
    }

    fn run(self, mut command_rx: mpsc::Receiver<Command>) {
        while let Some(command) = command_rx.blocking_recv() {
            match command {
                Command::Get {
                    namespace,
                    key,
                    reply,
                } => {
                    drop(reply.send(self.get(&namespace, &key)));
                }
                Command::Scan { request, reply } => {
                    drop(reply.send(self.scan(request)));
                }
                Command::Commit { batch, reply } => {
                    // The write remains accepted even if the requester has
                    // cancelled and the oneshot receiver is already gone.
                    drop(reply.send(self.commit(batch)));
                }
                Command::CreateBackup {
                    backup_directory,
                    reply,
                } => {
                    drop(reply.send(self.create_backup(&backup_directory)));
                }
                Command::SchemaVersion { reply } => {
                    drop(reply.send(self.required_schema_version()));
                }
                #[cfg(test)]
                Command::TestHold { entered, release } => {
                    let _ignored = entered.send(());
                    let _ignored = release.recv();
                }
            }
        }
    }

    fn create_backup(&self, backup_directory: &Path) -> Result<BackupInfo, StorageError> {
        let backup_directory = normalized_absolute_path(backup_directory)?;
        let backup_overlap_path = resolved_path_for_overlap(&backup_directory)?;
        validate_separate_paths(
            &self.database_path,
            &backup_overlap_path,
            "database",
            "backup",
        )?;

        let mut backup_engine = open_backup_engine(&backup_directory)?;
        backup_engine
            .create_new_backup_flush(&self.db, true)
            .map_err(|error| unavailable(format!("RocksDB backup failed: {error}")))?;
        latest_verified_backup(&backup_engine)
    }

    fn ensure_schema_ready(&self) -> Result<(), StorageError> {
        match self.persisted_schema_version()? {
            Some(version) if version == CURRENT_SCHEMA_VERSION => Ok(()),
            Some(version) if version > CURRENT_SCHEMA_VERSION => Err(corrupt_data(format!(
                "database schema version {} is newer than supported version {}",
                version.get(),
                CURRENT_SCHEMA_VERSION.get()
            ))),
            Some(version) => Err(invalid_request(format!(
                "database schema version {} requires an explicit migration to version {}",
                version.get(),
                CURRENT_SCHEMA_VERSION.get()
            ))),
            None if self.is_uninitialized()? => self.write_schema_version(CURRENT_SCHEMA_VERSION),
            None => Err(invalid_request(format!(
                "legacy database schema version 0 requires an explicit migration to version {}",
                CURRENT_SCHEMA_VERSION.get()
            ))),
        }
    }

    fn required_schema_version(&self) -> Result<SchemaVersion, StorageError> {
        let version = self.persisted_schema_version()?.ok_or_else(|| {
            corrupt_data("an open RocksDB database is missing physical schema metadata")
        })?;
        if version != CURRENT_SCHEMA_VERSION {
            return Err(corrupt_data(format!(
                "open RocksDB database has schema version {}, expected {}",
                version.get(),
                CURRENT_SCHEMA_VERSION.get()
            )));
        }
        Ok(version)
    }

    fn persisted_schema_version(&self) -> Result<Option<SchemaVersion>, StorageError> {
        let raw = self
            .db
            .get_cf(self.meta_cf()?, SCHEMA_VERSION_KEY)
            .map_err(|error| unavailable(format!("failed to read RocksDB schema: {error}")))?;
        raw.as_deref().map(decode_schema_version).transpose()
    }

    fn write_schema_version(&self, version: SchemaVersion) -> Result<(), StorageError> {
        let mut batch = RocksWriteBatch::default();
        batch.put_cf(
            self.meta_cf()?,
            SCHEMA_VERSION_KEY,
            encode_schema_version(version)?,
        );
        self.write_sync(batch, "schema metadata")
    }

    fn is_uninitialized(&self) -> Result<bool, StorageError> {
        let global_version = self
            .db
            .get_cf(self.meta_cf()?, GLOBAL_VERSION_KEY)
            .map_err(|error| unavailable(format!("failed to inspect RocksDB metadata: {error}")))?;
        if global_version.is_some() {
            return Ok(false);
        }

        match self
            .db
            .iterator_cf(self.data_cf()?, IteratorMode::Start)
            .next()
        {
            None => Ok(true),
            Some(Ok(_)) => Ok(false),
            Some(Err(error)) => Err(unavailable(format!(
                "failed to inspect RocksDB data during schema initialization: {error}"
            ))),
        }
    }

    fn migrate_to_current(self) -> Result<MigrationReport, StorageError> {
        let previous = self
            .persisted_schema_version()?
            .unwrap_or(SchemaVersion::LEGACY);
        self.validate_migration_source()?;
        let mut current = previous;
        let mut applied = Vec::new();
        while current < CURRENT_SCHEMA_VERSION {
            let migration = migration_from(current).ok_or_else(|| {
                invalid_request(format!(
                    "no registered migration from schema version {} to {}",
                    current.get(),
                    CURRENT_SCHEMA_VERSION.get()
                ))
            })?;
            self.apply_migration(migration)?;
            current = migration.to;
            applied.push(migration);
        }

        let persisted = self
            .persisted_schema_version()?
            .ok_or_else(|| corrupt_data("migration completed without persisted schema metadata"))?;
        if persisted != current {
            return Err(corrupt_data(format!(
                "migration reported schema version {}, but persisted version is {}",
                current.get(),
                persisted.get()
            )));
        }

        Ok(MigrationReport {
            previous,
            current,
            applied,
        })
    }

    fn validate_migration_source(&self) -> Result<(), StorageError> {
        let version = self
            .persisted_schema_version()?
            .unwrap_or(SchemaVersion::LEGACY);
        if version > CURRENT_SCHEMA_VERSION {
            return Err(corrupt_data(format!(
                "database schema version {} is newer than supported version {}",
                version.get(),
                CURRENT_SCHEMA_VERSION.get()
            )));
        }
        if version < CURRENT_SCHEMA_VERSION && migration_from(version).is_none() {
            return Err(invalid_request(format!(
                "no registered migration from schema version {} to {}",
                version.get(),
                CURRENT_SCHEMA_VERSION.get()
            )));
        }
        self.global_version().map(|_| ())
    }

    fn apply_migration(&self, migration: SchemaMigration) -> Result<(), StorageError> {
        match migration {
            MIGRATION_V0_TO_V1 => self.write_schema_version(migration.to),
            _ => Err(invalid_request(format!(
                "schema migration {} -> {} is registered without an implementation",
                migration.from.get(),
                migration.to.get()
            ))),
        }
    }

    fn write_sync(&self, batch: RocksWriteBatch, description: &str) -> Result<(), StorageError> {
        let mut write_options = WriteOptions::default();
        write_options.disable_wal(false);
        write_options.set_sync(true);
        self.db
            .write_opt(batch, &write_options)
            .map_err(|error| unavailable(format!("failed to persist {description}: {error}")))
    }

    fn get(&self, namespace: &Namespace, key: &Key) -> Result<Option<StoredValue>, StorageError> {
        let physical_key = encode_physical_key(namespace, key)?;
        let raw = self
            .db
            .get_cf(self.data_cf()?, physical_key)
            .map_err(|error| unavailable(format!("RocksDB point read failed: {error}")))?;
        raw.as_deref().map(decode_stored_value).transpose()
    }

    fn scan(&self, request: ScanRequest) -> Result<ScanPage, StorageError> {
        if request.limit == 0 {
            return Err(invalid_request("scan limit must be greater than zero"));
        }
        let read_limit = request
            .limit
            .checked_add(1)
            .ok_or_else(|| invalid_request("scan limit is too large"))?;
        if request
            .after
            .as_ref()
            .is_some_and(|after| !after.0.starts_with(&request.prefix))
        {
            return Err(invalid_request(
                "scan continuation key does not match the requested prefix",
            ));
        }

        let namespace_prefix = namespace_prefix(&request.namespace)?;
        let mut physical_prefix = namespace_prefix.clone();
        physical_prefix.extend_from_slice(&request.prefix);
        let start = match &request.after {
            Some(after) => encode_physical_key(&request.namespace, after)?,
            None => physical_prefix.clone(),
        };

        let mut entries = Vec::with_capacity(request.limit.min(256));
        let iterator = self.db.full_iterator_cf(
            self.data_cf()?,
            IteratorMode::From(&start, Direction::Forward),
        );
        for item in iterator {
            let (physical_key, raw_value) =
                item.map_err(|error| unavailable(format!("RocksDB prefix scan failed: {error}")))?;
            if !physical_key.starts_with(&physical_prefix) {
                break;
            }

            let logical_key = decode_physical_key(&physical_key, &request.namespace)?;
            if request
                .after
                .as_ref()
                .is_some_and(|after| logical_key <= *after)
            {
                continue;
            }
            entries.push((logical_key, decode_stored_value(&raw_value)?));
            if entries.len() == read_limit {
                break;
            }
        }

        let next = if entries.len() > request.limit {
            entries.pop();
            entries.last().map(|(key, _)| key.clone())
        } else {
            None
        };
        Ok(ScanPage { entries, next })
    }

    fn commit(&self, batch: WriteBatch) -> Result<CommitResult, StorageError> {
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

    fn evaluate_condition(&self, condition: &Condition) -> Result<(), StorageError> {
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

    fn global_version(&self) -> Result<Version, StorageError> {
        let raw = self
            .db
            .get_cf(self.meta_cf()?, GLOBAL_VERSION_KEY)
            .map_err(|error| unavailable(format!("failed to read RocksDB metadata: {error}")))?;
        raw.as_deref()
            .map(decode_global_version)
            .transpose()
            .map(|version| version.unwrap_or(Version(0)))
    }

    fn data_cf(&self) -> Result<&ColumnFamily, StorageError> {
        self.db
            .cf_handle(DATA_COLUMN_FAMILY)
            .ok_or_else(|| corrupt_data("RocksDB data column family is missing"))
    }

    fn meta_cf(&self) -> Result<&ColumnFamily, StorageError> {
        self.db
            .cf_handle(META_COLUMN_FAMILY)
            .ok_or_else(|| corrupt_data("RocksDB meta column family is missing"))
    }
}

fn column_family_descriptors() -> [ColumnFamilyDescriptor; 3] {
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

fn migration_from(version: SchemaVersion) -> Option<SchemaMigration> {
    REGISTERED_SCHEMA_MIGRATIONS
        .iter()
        .copied()
        .find(|migration| migration.from == version)
}

fn open_backup_engine(backup_directory: &Path) -> Result<BackupEngine, StorageError> {
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

fn latest_verified_backup(backup_engine: &BackupEngine) -> Result<BackupInfo, StorageError> {
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

fn normalized_absolute_path(path: &Path) -> Result<PathBuf, StorageError> {
    if path.as_os_str().is_empty() {
        return Err(invalid_request("RocksDB path must not be empty"));
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| unavailable(format!("failed to resolve current directory: {error}")))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid_request("RocksDB path escapes the filesystem root"));
                }
            }
        }
    }
    Ok(normalized)
}

fn resolved_path_for_overlap(path: &Path) -> Result<PathBuf, StorageError> {
    let ancestor = path
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .ok_or_else(|| invalid_request("RocksDB path has no existing filesystem ancestor"))?;
    let canonical_ancestor = fs::canonicalize(ancestor).map_err(|error| {
        unavailable(format!(
            "failed to resolve RocksDB path for overlap validation: {error}"
        ))
    })?;
    let suffix = path.strip_prefix(ancestor).map_err(|error| {
        invalid_request(format!(
            "failed to separate RocksDB path from its existing ancestor: {error}"
        ))
    })?;
    Ok(canonical_ancestor.join(suffix))
}

fn validate_separate_paths(
    first: &Path,
    second: &Path,
    first_label: &str,
    second_label: &str,
) -> Result<(), StorageError> {
    if first == second || first.starts_with(second) || second.starts_with(first) {
        return Err(invalid_request(format!(
            "RocksDB {first_label} and {second_label} paths must not overlap"
        )));
    }
    Ok(())
}

fn conflict(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::Conflict,
        message: message.into(),
    }
}

fn unavailable(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::Unavailable,
        message: message.into(),
    }
}

fn corrupt_data(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::CorruptData,
        message: message.into(),
    }
}

fn invalid_request(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::InvalidRequest,
        message: message.into(),
    }
}

fn other(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::Other,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use super::*;
    use storage::Version;
    use tempfile::TempDir;

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
    async fn cancellation_after_enqueue_does_not_cancel_the_accepted_commit()
    -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = RocksDbStorage::open(directory.path())?;
        let records = namespace("cancelled-request");
        let record_key = key("accepted-write");
        let release = storage.hold_owner_for_test().await?;
        let mut commit = storage.commit(WriteBatch {
            conditions: Vec::new(),
            operations: vec![put(&records, &record_key, "durable")],
        });

        std::future::poll_fn(|context| match commit.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("owner hold must keep the accepted commit pending"),
        })
        .await;
        drop(commit);
        release
            .send(())
            .map_err(|_| other("failed to release RocksDB owner test hold"))?;

        let stored = storage.get(&records, &record_key).await?;
        assert_eq!(stored.map(|stored| stored.value), Some(value("durable")));
        Ok(())
    }

    #[tokio::test]
    async fn put_get_and_paginated_scan_are_ordered() -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = RocksDbStorage::open(directory.path())?;
        let observations = namespace("observations");
        let other_namespace = namespace("other");

        let result = storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![
                    put(&observations, &key("tx/a"), "a"),
                    put(&observations, &key("tx/b"), "b"),
                    put(&observations, &key("tx/c"), "c"),
                    put(&other_namespace, &key("tx/a"), "isolated"),
                ],
            })
            .await?;
        assert_eq!(result.version, Version(1));
        assert_eq!(
            storage.get(&observations, &key("tx/b")).await?,
            Some(StoredValue {
                value: value("b"),
                version: Version(1),
            })
        );

        let first = storage
            .scan(ScanRequest {
                namespace: observations.clone(),
                prefix: b"tx/".to_vec(),
                after: None,
                limit: 2,
            })
            .await?;
        assert_eq!(
            first
                .entries
                .iter()
                .map(|(entry_key, _)| entry_key.clone())
                .collect::<Vec<_>>(),
            vec![key("tx/a"), key("tx/b")]
        );
        assert_eq!(first.next, Some(key("tx/b")));

        let second = storage
            .scan(ScanRequest {
                namespace: observations,
                prefix: b"tx/".to_vec(),
                after: first.next,
                limit: 2,
            })
            .await?;
        assert_eq!(
            second
                .entries
                .iter()
                .map(|(entry_key, _)| entry_key.clone())
                .collect::<Vec<_>>(),
            vec![key("tx/c")]
        );
        assert_eq!(second.next, None);
        Ok(())
    }

    #[tokio::test]
    async fn stale_condition_rejects_the_complete_batch() -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = RocksDbStorage::open(directory.path())?;
        let records = namespace("records");
        let primary = key("primary");
        let side_effect = key("side-effect");

        storage
            .commit(WriteBatch {
                conditions: vec![Condition::Missing {
                    namespace: records.clone(),
                    key: primary.clone(),
                }],
                operations: vec![put(&records, &primary, "v1")],
            })
            .await?;
        storage
            .commit(WriteBatch {
                conditions: vec![Condition::Version {
                    namespace: records.clone(),
                    key: primary.clone(),
                    expected: Version(1),
                }],
                operations: vec![put(&records, &primary, "v2")],
            })
            .await?;

        let error = storage
            .commit(WriteBatch {
                conditions: vec![Condition::Version {
                    namespace: records.clone(),
                    key: primary.clone(),
                    expected: Version(1),
                }],
                operations: vec![
                    put(&records, &primary, "stale"),
                    put(&records, &side_effect, "must-not-commit"),
                ],
            })
            .await
            .expect_err("a stale compare-and-swap must fail");
        assert_eq!(error.kind, StorageErrorKind::Conflict);
        assert_eq!(storage.get(&records, &side_effect).await?, None);
        assert_eq!(
            storage.get(&records, &primary).await?,
            Some(StoredValue {
                value: value("v2"),
                version: Version(2),
            })
        );

        let next = storage
            .commit(WriteBatch {
                conditions: vec![Condition::Missing {
                    namespace: records.clone(),
                    key: side_effect.clone(),
                }],
                operations: vec![put(&records, &side_effect, "committed")],
            })
            .await?;
        assert_eq!(next.version, Version(3));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_compare_and_swap_has_one_winner() -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = RocksDbStorage::open(directory.path())?;
        let records = namespace("records");
        let primary = key("primary");
        storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &primary, "v1")],
            })
            .await?;

        let first = storage.clone();
        let second = storage.clone();
        let first_batch = WriteBatch {
            conditions: vec![Condition::Version {
                namespace: records.clone(),
                key: primary.clone(),
                expected: Version(1),
            }],
            operations: vec![put(&records, &primary, "first")],
        };
        let second_batch = WriteBatch {
            conditions: vec![Condition::Version {
                namespace: records.clone(),
                key: primary.clone(),
                expected: Version(1),
            }],
            operations: vec![put(&records, &primary, "second")],
        };

        let (first_result, second_result) =
            tokio::join!(first.commit(first_batch), second.commit(second_batch));
        let successes = usize::from(first_result.is_ok()) + usize::from(second_result.is_ok());
        let conflicts = usize::from(matches!(
            first_result,
            Err(StorageError {
                kind: StorageErrorKind::Conflict,
                ..
            })
        )) + usize::from(matches!(
            second_result,
            Err(StorageError {
                kind: StorageErrorKind::Conflict,
                ..
            })
        ));
        assert_eq!(successes, 1);
        assert_eq!(conflicts, 1);
        Ok(())
    }

    #[tokio::test]
    async fn delete_removes_the_value() -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = RocksDbStorage::open(directory.path())?;
        let records = namespace("records");
        let primary = key("primary");
        storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &primary, "v1")],
            })
            .await?;

        let result = storage
            .commit(WriteBatch {
                conditions: vec![Condition::Version {
                    namespace: records.clone(),
                    key: primary.clone(),
                    expected: Version(1),
                }],
                operations: vec![Operation::Delete {
                    namespace: records.clone(),
                    key: primary.clone(),
                }],
            })
            .await?;

        assert_eq!(result.version, Version(2));
        assert_eq!(storage.get(&records, &primary).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn persisted_values_and_global_version_survive_reopen() -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let records = namespace("records");
        let primary = key("primary");
        {
            let storage = RocksDbStorage::open(directory.path())?;
            storage
                .commit(WriteBatch {
                    conditions: vec![],
                    operations: vec![put(&records, &primary, "v1")],
                })
                .await?;
        }

        let reopened = RocksDbStorage::open(directory.path())?;
        assert_eq!(
            reopened.get(&records, &primary).await?,
            Some(StoredValue {
                value: value("v1"),
                version: Version(1),
            })
        );
        let result = reopened
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &key("second"), "v2")],
            })
            .await?;
        assert_eq!(result.version, Version(2));
        Ok(())
    }

    #[tokio::test]
    async fn backup_restore_recovers_the_verified_snapshot() -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database");
        let backup_path = directory.path().join("backup");
        let restore_path = directory.path().join("restored");
        let records = namespace("records");
        let snapshot_key = key("snapshot");
        let later_key = key("after-backup");
        let storage = RocksDbStorage::open(&database_path)?;
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
        assert_eq!(overlap_error.kind, StorageErrorKind::InvalidRequest);

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
        let overwrite_error = RocksDbStorage::restore_latest_backup(&backup_path, &database_path)
            .expect_err("restore must never overwrite an existing database path");
        assert_eq!(overwrite_error.kind, StorageErrorKind::InvalidRequest);
        drop(storage);

        let restored = RocksDbStorage::restore_latest_backup(&backup_path, &restore_path)?;
        assert_eq!(restored, backup);
        let storage = RocksDbStorage::open(&restore_path)?;
        assert_eq!(storage.schema_version().await?, CURRENT_SCHEMA_VERSION);
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
    async fn second_open_and_live_migration_are_rejected_by_path_lock() -> Result<(), StorageError>
    {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let backup_directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = RocksDbStorage::open(directory.path())?;

        let second_open = match RocksDbStorage::open(directory.path()) {
            Ok(_) => panic!("RocksDB must enforce one open owner per path"),
            Err(error) => error,
        };
        assert_eq!(second_open.kind, StorageErrorKind::Unavailable);
        let live_migration = RocksDbStorage::migrate(directory.path(), backup_directory.path())
            .expect_err("migration must require the live owner to close first");
        assert_eq!(live_migration.kind, StorageErrorKind::Unavailable);

        drop(storage);
        let reopened = RocksDbStorage::open(directory.path())?;
        assert_eq!(reopened.schema_version().await?, CURRENT_SCHEMA_VERSION);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_schema_fixture_requires_and_accepts_registered_migration()
    -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let backup_directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let records = namespace("records");
        let primary = key("legacy");
        {
            let mut options = Options::default();
            options.create_if_missing(true);
            options.create_missing_column_families(true);
            let db =
                DB::open_cf_descriptors(&options, directory.path(), column_family_descriptors())
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
                encode_stored_value(&value("legacy-value"), Version(1))?,
            )
            .map_err(|error| other(error.to_string()))?;
            db.put_cf(
                meta_cf,
                GLOBAL_VERSION_KEY,
                encode_global_version(Version(1))?,
            )
            .map_err(|error| other(error.to_string()))?;
        }

        let open_error = match RocksDbStorage::open(directory.path()) {
            Ok(_) => panic!("legacy schema must not be migrated as a serve side effect"),
            Err(error) => error,
        };
        assert_eq!(open_error.kind, StorageErrorKind::InvalidRequest);

        let outcome = RocksDbStorage::migrate(directory.path(), backup_directory.path())?;
        assert!(outcome.backup.backup_id > 0);
        assert_eq!(outcome.report.previous, SchemaVersion::LEGACY);
        assert_eq!(outcome.report.current, CURRENT_SCHEMA_VERSION);
        assert_eq!(outcome.report.applied, vec![MIGRATION_V0_TO_V1]);

        let storage = RocksDbStorage::open(directory.path())?;
        assert_eq!(storage.schema_version().await?, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            storage.get(&records, &primary).await?,
            Some(StoredValue {
                value: value("legacy-value"),
                version: Version(1),
            })
        );
        Ok(())
    }

    #[test]
    fn future_schema_fixture_fails_closed() -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let backup_directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        {
            let storage = RocksDbStorage::open(directory.path())?;
            drop(storage);
        }
        {
            let options = Options::default();
            let db =
                DB::open_cf_descriptors(&options, directory.path(), column_family_descriptors())
                    .map_err(|error| other(error.to_string()))?;
            let meta_cf = db
                .cf_handle(META_COLUMN_FAMILY)
                .ok_or_else(|| other("test meta column family is missing"))?;
            let future = SchemaVersion(
                CURRENT_SCHEMA_VERSION
                    .get()
                    .checked_add(1)
                    .ok_or_else(|| other("test schema version overflowed"))?,
            );
            db.put_cf(meta_cf, SCHEMA_VERSION_KEY, encode_schema_version(future)?)
                .map_err(|error| other(error.to_string()))?;
        }

        let error = match RocksDbStorage::open(directory.path()) {
            Ok(_) => panic!("a future schema must not be opened by an older binary"),
            Err(error) => error,
        };
        assert_eq!(error.kind, StorageErrorKind::CorruptData);
        let migration_error = RocksDbStorage::migrate(directory.path(), backup_directory.path())
            .expect_err("a future schema must not be migrated by an older binary");
        assert_eq!(migration_error.kind, StorageErrorKind::CorruptData);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_persisted_frame_is_reported_as_corruption() -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let records = namespace("records");
        let primary = key("primary");
        {
            let storage = RocksDbStorage::open(directory.path())?;
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

        let reopened = RocksDbStorage::open(directory.path())?;
        let error = reopened
            .get(&records, &primary)
            .await
            .expect_err("a malformed value frame must be rejected");
        assert_eq!(error.kind, StorageErrorKind::CorruptData);
        Ok(())
    }

    #[test]
    fn zero_queue_capacity_is_rejected() -> Result<(), StorageError> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;

        let error = match RocksDbStorage::open_with_queue_capacity(directory.path(), 0) {
            Ok(_) => panic!("a zero-capacity queue cannot provide bounded command delivery"),
            Err(error) => error,
        };

        assert_eq!(error.kind, StorageErrorKind::InvalidRequest);
        Ok(())
    }
}
