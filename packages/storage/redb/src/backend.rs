use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc as std_mpsc},
    thread,
};

use redb::{Database, TableDefinition};
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

const DATA_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("data");
const META_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("meta");
const GLOBAL_VERSION_KEY: &[u8] = b"global-version";
const DATABASE_FORMAT_KEY: &[u8] = b"database-format";
const DATABASE_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Default upper bound for operations accepted but not yet handled by the DB
/// owner thread.
pub const DEFAULT_COMMAND_QUEUE_CAPACITY: usize = 256;

/// A serialized, durable [`Store`] implementation backed by redb.
///
/// All database access is routed through one bounded channel to one dedicated
/// OS thread. This keeps synchronous file I/O off async executors and makes
/// condition evaluation plus the following write one logical critical section.
#[derive(Clone)]
pub struct Redb {
    inner: Arc<WorkerHandle>,
}

struct WorkerHandle {
    command_tx: Option<mpsc::Sender<Command>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Redb {
    /// Opens or creates one redb database file using the default command bound.
    ///
    /// The path must be absolute, its parent directory must already exist, and
    /// an existing path must be a compatible redb database file.
    ///
    /// # Errors
    ///
    /// Returns an error when path validation, file locking, physical format
    /// validation, database opening, or owner-thread startup fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with_queue_capacity(path, DEFAULT_COMMAND_QUEUE_CAPACITY)
    }

    /// Opens or creates a database with an explicit bounded command capacity.
    ///
    /// # Errors
    ///
    /// A zero capacity is rejected as an invalid request. Other failures are
    /// returned with storage context.
    pub fn open_with_queue_capacity(
        path: impl AsRef<Path>,
        command_queue_capacity: usize,
    ) -> Result<Self, Error> {
        if command_queue_capacity == 0 {
            return Err(invalid_request(
                "redb command queue capacity must be greater than zero",
            ));
        }

        let path = path.as_ref().to_path_buf();
        let (command_tx, command_rx) = mpsc::channel(command_queue_capacity);
        let (startup_tx, startup_rx) = std_mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("storage-redb-owner".to_owned())
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
            .map_err(|error| unavailable(format!("failed to spawn redb owner thread: {error}")))?;

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
                let detail = if worker.join().is_err() {
                    "the owner thread panicked during startup"
                } else {
                    "the owner thread exited during startup"
                };
                Err(unavailable(format!(
                    "failed to receive redb startup result: {error}; {detail}"
                )))
            }
        }
    }

    fn command_sender(&self) -> Result<mpsc::Sender<Command>, Error> {
        self.inner
            .command_tx
            .as_ref()
            .cloned()
            .ok_or_else(|| unavailable("redb owner thread is shutting down"))
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
            .map_err(|_| unavailable("redb owner thread stopped before test hold was accepted"))?;
        entered_rx
            .await
            .map_err(|_| unavailable("redb owner thread stopped before entering test hold"))?;
        Ok(release_tx)
    }

    #[cfg(test)]
    async fn fail_after_next_commit_for_test(&self) -> Result<(), Error> {
        let sender = self.command_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(Command::TestFailAfterNextCommit { reply: reply_tx })
            .await
            .map_err(|_| unavailable("redb owner thread stopped before arming commit failure"))?;
        reply_rx
            .await
            .map_err(|_| unavailable("redb owner thread stopped before commit failure was armed"))
    }

    #[cfg(test)]
    async fn fail_next_reopen_for_test(&self) -> Result<(), Error> {
        let sender = self.command_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(Command::TestFailNextReopen { reply: reply_tx })
            .await
            .map_err(|_| unavailable("redb owner thread stopped before arming reopen failure"))?;
        reply_rx
            .await
            .map_err(|_| unavailable("redb owner thread stopped before reopen failure was armed"))
    }

    #[cfg(test)]
    async fn reopen_count_for_test(&self) -> Result<usize, Error> {
        let sender = self.command_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(Command::TestReopenCount { reply: reply_tx })
            .await
            .map_err(|_| unavailable("redb owner thread stopped before reopen count request"))?;
        reply_rx
            .await
            .map_err(|_| unavailable("redb owner thread stopped before returning the reopen count"))
    }
}

impl Store for Redb {
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
                .map_err(|_| unavailable("redb owner thread stopped before get was accepted"))?;
            reply_rx.await.map_err(|_| {
                unavailable("redb owner thread stopped before get returned a result")
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
                .map_err(|_| unavailable("redb owner thread stopped before scan was accepted"))?;
            reply_rx.await.map_err(|_| {
                unavailable("redb owner thread stopped before scan returned a result")
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
                .map_err(|_| unavailable("redb owner thread stopped before commit was accepted"))?;
            reply_rx.await.map_err(|_| {
                unavailable("redb owner thread stopped before commit returned a result")
            })?
        })
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // Closing the final sender lets the owner finish every accepted command
        // before closing the file. A cold copy after final-handle teardown is
        // therefore ordered after all accepted commits.
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
    #[cfg(test)]
    TestHold {
        entered: oneshot::Sender<()>,
        release: std_mpsc::Receiver<()>,
    },
    #[cfg(test)]
    TestFailAfterNextCommit { reply: oneshot::Sender<()> },
    #[cfg(test)]
    TestFailNextReopen { reply: oneshot::Sender<()> },
    #[cfg(test)]
    TestReopenCount { reply: oneshot::Sender<usize> },
}

struct Backend {
    db: Option<Database>,
    database_path: PathBuf,
    #[cfg(test)]
    fail_after_next_commit: bool,
    #[cfg(test)]
    fail_next_reopen: bool,
    #[cfg(test)]
    reopen_count: usize,
}

impl Backend {
    fn open(path: &Path) -> Result<Self, Error> {
        let database_path = validated_database_path(path)?;
        let db = open_database(&database_path.path, database_path.initialize)?;
        let mut backend = Self {
            db: Some(db),
            database_path: database_path.path,
            #[cfg(test)]
            fail_after_next_commit: false,
            #[cfg(test)]
            fail_next_reopen: false,
            #[cfg(test)]
            reopen_count: 0,
        };

        if database_path.initialize {
            backend.initialize_format()?;
        } else {
            backend.require_format()?;
            backend.global_version()?;
        }
        Ok(backend)
    }

    fn database(&self) -> Result<&Database, Error> {
        self.db
            .as_ref()
            .ok_or_else(|| unavailable("redb database is closed after a storage failure"))
    }

    fn ensure_open(&mut self) -> Result<(), Error> {
        if self.db.is_some() {
            return Ok(());
        }
        self.reopen()
    }

    fn reopen(&mut self) -> Result<(), Error> {
        self.db = None;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_reopen) {
            return Err(unavailable("injected redb database reopen failure"));
        }
        let db = open_database(&self.database_path, false)?;
        self.db = Some(db);
        if let Err(error) = self
            .require_format()
            .and_then(|()| self.global_version().map(drop))
        {
            self.db = None;
            return Err(error);
        }
        #[cfg(test)]
        {
            self.reopen_count += 1;
        }
        Ok(())
    }

    fn run(mut self, mut command_rx: mpsc::Receiver<Command>) {
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
                    // cancelled and its oneshot receiver is already gone.
                    drop(reply.send(self.commit(batch)));
                }
                #[cfg(test)]
                Command::TestHold { entered, release } => {
                    let _ignored = entered.send(());
                    let _ignored = release.recv();
                }
                #[cfg(test)]
                Command::TestFailAfterNextCommit { reply } => {
                    self.fail_after_next_commit = true;
                    let _ignored = reply.send(());
                }
                #[cfg(test)]
                Command::TestFailNextReopen { reply } => {
                    self.fail_next_reopen = true;
                    let _ignored = reply.send(());
                }
                #[cfg(test)]
                Command::TestReopenCount { reply } => {
                    let _ignored = reply.send(self.reopen_count);
                }
            }
        }
    }
}

fn open_database(path: &Path, initialize: bool) -> Result<Database, Error> {
    let mut builder = Database::builder();
    builder.set_cache_size(DATABASE_CACHE_BYTES);
    let result = if initialize {
        builder.create(path)
    } else {
        builder.open(path)
    };
    result.map_err(database_error)
}

#[cfg(test)]
#[path = "backend/store_test.rs"]
mod store_test;

#[cfg(test)]
#[path = "backend/recovery_test.rs"]
mod recovery_test;
