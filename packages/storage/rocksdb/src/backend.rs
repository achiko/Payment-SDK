use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, mpsc as std_mpsc},
    thread,
};

use rocksdb::{DB, Options, backup::RestoreOptions};
use storage::{
    BoxFuture, CommitResult, Error, Key, Namespace, ScanPage, ScanRequest, Store, StoredValue,
    WriteBatch,
};
use tokio::sync::{mpsc, oneshot};

mod format;
mod path;
mod read;
mod support;
mod write;

use path::*;
use support::*;

const META_COLUMN_FAMILY: &str = "meta";
const DATA_COLUMN_FAMILY: &str = "data";
const DEFAULT_COLUMN_FAMILY: &str = "default";
const GLOBAL_VERSION_KEY: &[u8] = b"global-version";
const DATABASE_FORMAT_KEY: &[u8] = b"database-format";

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

/// A serialized, durable [`Store`] implementation backed by RocksDB.
///
/// All RocksDB access is routed through one bounded channel to one dedicated
/// OS thread. This makes condition evaluation and the following write batch a
/// single logical critical section without blocking the async runtime.
#[derive(Clone)]
pub struct RocksDb {
    inner: Arc<WorkerHandle>,
}

struct WorkerHandle {
    command_tx: Option<mpsc::Sender<Command>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl RocksDb {
    /// Opens or creates a database using the default bounded command capacity.
    ///
    /// # Errors
    ///
    /// Returns [`storage::ErrorKind::Unavailable`] when the worker thread or
    /// RocksDB cannot be opened, and [`storage::ErrorKind::CorruptData`] when
    /// persisted metadata cannot be decoded.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
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
    ) -> Result<Self, Error> {
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
                inner: Arc::new(WorkerHandle {
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
    ) -> Result<BackupInfo, Error> {
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
    ) -> Result<BackupInfo, Error> {
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

        let restored = Backend::open_existing(&destination)?;
        restored.require_format()?;
        restored.global_version()?;
        drop(restored);
        Ok(info)
    }

    fn command_sender(&self) -> Result<mpsc::Sender<Command>, Error> {
        self.inner
            .command_tx
            .as_ref()
            .cloned()
            .ok_or_else(|| unavailable("RocksDB owner thread is shutting down"))
    }

    #[cfg(test)]
    async fn hold_owner_for_test(&self) -> Result<std_mpsc::Sender<()>, Error> {
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

impl Store for RocksDb {
    fn get<'a>(
        &'a self,
        namespace: &'a Namespace,
        key: &'a Key,
    ) -> BoxFuture<'a, Result<Option<StoredValue>, Error>> {
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

    fn scan<'a>(&'a self, request: ScanRequest) -> BoxFuture<'a, Result<ScanPage, Error>> {
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

    fn commit<'a>(&'a self, batch: WriteBatch) -> BoxFuture<'a, Result<CommitResult, Error>> {
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

impl Drop for WorkerHandle {
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
        reply: oneshot::Sender<Result<Option<StoredValue>, Error>>,
    },
    Scan {
        request: ScanRequest,
        reply: oneshot::Sender<Result<ScanPage, Error>>,
    },
    Commit {
        batch: WriteBatch,
        reply: oneshot::Sender<Result<CommitResult, Error>>,
    },
    CreateBackup {
        backup_directory: PathBuf,
        reply: oneshot::Sender<Result<BackupInfo, Error>>,
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
    fn open(path: &Path) -> Result<Self, Error> {
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

        backend.ensure_format()?;
        backend.global_version()?;
        Ok(backend)
    }

    fn open_existing(path: &Path) -> Result<Self, Error> {
        let database_path = normalized_absolute_path(path)?;
        if !database_path.is_dir() {
            return Err(invalid_request(
                "database path must be an existing RocksDB directory",
            ));
        }

        let database_options = Options::default();
        let db = DB::open_cf_descriptors(
            &database_options,
            &database_path,
            column_family_descriptors(),
        )
        .map_err(|error| {
            unavailable(format!("failed to open existing RocksDB database: {error}"))
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
                #[cfg(test)]
                Command::TestHold { entered, release } => {
                    let _ignored = entered.send(());
                    let _ignored = release.recv();
                }
            }
        }
    }

    fn create_backup(&self, backup_directory: &Path) -> Result<BackupInfo, Error> {
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
}

#[cfg(test)]
#[path = "backend/store_test.rs"]
mod store_test;

#[cfg(test)]
#[path = "backend/recovery_test.rs"]
mod recovery_test;
