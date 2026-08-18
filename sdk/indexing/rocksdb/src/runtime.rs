use std::{error::Error as StdError, fmt, path::Path};

use indexing::{
    BackfillReader, BackfillWriter, BlockInterpreter, BlockSource, Checkpoint, CommitBackfill,
    EventPage, EventQuery, History, HistoryQuery, IndexError, IndexErrorKind, IndexScope,
    ObservedTransaction, Observer, SyncPhase, SyncRequest, SyncStatus, TransactionPage,
    TransactionQuery, UnwatchOutcome, UnwatchRequest, WatchReceipt, WatchRequest, Watcher, Worker,
};

use crate::{Config, OutputReader, RocksRepository};

/// Cloneable consumer surface for one durable index scope.
///
/// The handle deliberately hides RocksDB and the synchronization machinery.
/// Business code can retain it as `Arc<dyn indexing::Indexer>` while the
/// composition root owns and drives the corresponding [`Runtime`].
#[derive(Clone)]
pub struct Handle {
    repository: RocksRepository,
}

impl Handle {
    #[must_use]
    pub fn outputs(&self) -> OutputReader<RocksRepository> {
        OutputReader::new(self.repository.clone())
    }

    pub async fn status(&self, scope: &IndexScope) -> Result<SyncStatus, IndexError> {
        indexing::StatusStore::status(&self.repository, scope).await
    }
}

impl Checkpoint for Handle {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> indexing::BoxFuture<'a, Result<Option<indexing::BlockRef>, IndexError>> {
        Checkpoint::checkpoint(&self.repository, scope)
    }
}

impl Watcher for Handle {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> indexing::BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Watcher::watch(&self.repository, request)
    }

    fn unwatch<'a>(
        &'a self,
        request: UnwatchRequest,
    ) -> indexing::BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Watcher::unwatch(&self.repository, request)
    }
}

impl History for Handle {
    fn transaction<'a>(
        &'a self,
        request: TransactionQuery,
    ) -> indexing::BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
        History::transaction(&self.repository, request)
    }

    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> indexing::BoxFuture<'a, Result<TransactionPage, IndexError>> {
        History::history(&self.repository, request)
    }
}

impl Observer for Handle {
    fn events<'a>(
        &'a self,
        request: EventQuery,
    ) -> indexing::BoxFuture<'a, Result<EventPage, IndexError>> {
        Observer::events(&self.repository, request)
    }
}

/// Embedded durable indexer for one chain and network.
///
/// A composition root supplies chain-owned source and interpreter values. The
/// runtime owns synchronization and backfills; storage details remain here.
pub struct Runtime<S, I> {
    source: S,
    interpreter: I,
    worker: indexing::SyncWorker<S, I, RocksRepository>,
    handle: Handle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOutcome {
    pub status: SyncStatus,
    pub backfills: usize,
}

#[derive(Debug)]
pub enum OpenError {
    Storage(storage::Error),
    Index(IndexError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "failed to open index storage: {error}"),
            Self::Index(error) => write!(formatter, "invalid index runtime configuration: {error}"),
        }
    }
}

impl StdError for OpenError {}

impl<S, I> Runtime<S, I>
where
    S: BlockSource<Block = I::Block> + Clone,
    I: BlockInterpreter + Clone,
{
    pub fn open(
        path: impl AsRef<Path>,
        config: Config,
        source: S,
        interpreter: I,
    ) -> Result<Self, OpenError> {
        let storage = storage_rocksdb::RocksDb::open(path).map_err(OpenError::Storage)?;
        Self::new(config, storage, source, interpreter).map_err(OpenError::Index)
    }

    pub fn new(
        config: Config,
        storage: storage_rocksdb::RocksDb,
        source: S,
        interpreter: I,
    ) -> Result<Self, IndexError> {
        let sync = indexing::SyncConfig::new(
            config.scope.clone(),
            config.bootstrap_height,
            config.confirmation_policy,
            config.reorg_retention,
        )?;
        let repository = RocksRepository::new(storage, config);
        let handle = Handle {
            repository: repository.clone(),
        };
        Ok(Self {
            worker: indexing::SyncWorker::new(
                source.clone(),
                interpreter.clone(),
                repository,
                sync,
            ),
            source,
            interpreter,
            handle,
        })
    }

    #[must_use]
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }
}

impl<S, I> Runtime<S, I>
where
    S: BlockSource<Block = I::Block> + Clone,
    I: BlockInterpreter + Clone,
    RocksRepository: indexing::CanonicalReader<Target = I::Target, Effect = I::Effect, Undo = I::Undo>
        + indexing::WatchReader
        + indexing::ChainWriter
        + indexing::StatusStore
        + BackfillReader
        + BackfillWriter,
{
    pub async fn sync(&self, max_blocks: usize) -> Result<SyncOutcome, IndexError> {
        if max_blocks == 0 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "synchronization batch size must be greater than zero",
                false,
            ));
        }
        let scope = &self.handle.repository.config().scope;
        let status = self
            .worker
            .sync(SyncRequest {
                scope: scope.clone(),
                through: None,
                max_blocks: Some(max_blocks),
            })
            .await?;
        let backfills = if status.phase == SyncPhase::Ready {
            self.backfill(max_blocks).await?
        } else {
            0
        };
        Ok(SyncOutcome { status, backfills })
    }

    pub async fn status(&self) -> Result<SyncStatus, IndexError> {
        self.worker
            .status(&self.handle.repository.config().scope)
            .await
    }

    async fn backfill(&self, max_blocks: usize) -> Result<usize, IndexError> {
        let repository = &self.handle.repository;
        let scope = &repository.config().scope;
        let jobs = repository
            .pending_watch_backfills(scope, max_blocks)
            .await?;
        let mut applied = 0_usize;
        for job in jobs {
            let checkpoint = indexing::CanonicalReader::checkpoint(repository, scope)
                .await?
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Conflict,
                        "historical watch backfill requires a canonical checkpoint",
                        true,
                    )
                })?;
            let watches =
                indexing::WatchReader::watches_at(repository, scope, job.next_height).await?;
            let watch = watches
                .watches
                .into_iter()
                .find(|watch| watch.id == job.watch_id)
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidWatch,
                        "historical watch is inactive at its backfill cursor",
                        false,
                    )
                })?;
            let block = self.source.block_at(job.next_height).await?;
            let interpreted = self
                .interpreter
                .inspect(&block, std::slice::from_ref(&watch))?;
            if self.source.canonical_hash(job.next_height).await?.as_ref()
                != Some(&interpreted.block.hash)
            {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "canonical block changed before its backfill commit",
                    true,
                ));
            }
            repository
                .commit_watch_backfill_effect(
                    CommitBackfill {
                        scope: scope.clone(),
                        watch_id: job.watch_id,
                        expected_next_height: job.next_height,
                        expected_checkpoint: checkpoint,
                        block: interpreted.block,
                        drafts: interpreted.drafts,
                    },
                    self.interpreter.backfill_effect(interpreted.effect)?,
                )
                .await?;
            applied = applied.saturating_add(1);
        }
        Ok(applied)
    }
}
