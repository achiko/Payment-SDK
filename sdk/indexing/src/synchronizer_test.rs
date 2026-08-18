use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use futures_executor::block_on;

use crate::{
    BlockHash, BlockHeight, BlockInterpreter, BlockOutcome, BlockRef, BlockSource, BlockStore,
    BoxFuture, CanonicalStore, ChainId, CommitBlock, CommitContext, CommitPlan, ConfirmationPolicy,
    IndexChanges, IndexError, IndexErrorKind, IndexScope, IndexUndo, IndexedBlock,
    InterpretedBlock, RegisterWatch, RevertBlock, RevertContext, RevertPlan, RevertTip,
    SourceError, StatusStore, SyncConfig, SyncPhase, SyncRequest, SyncStatus, Synchronizer,
    WatchContext, WatchPlan, WatchSnapshot, WatchStore, WatchVersion,
};

#[derive(Clone)]
struct TestBlock(BlockRef);

impl IndexedBlock for TestBlock {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

#[derive(Clone)]
struct Source(Arc<Mutex<Vec<TestBlock>>>);

impl Source {
    fn replace(&self, blocks: Vec<TestBlock>) {
        *self.0.lock().expect("source lock") = blocks;
    }
}

impl BlockSource for Source {
    type Block = TestBlock;

    fn tip(&self) -> BoxFuture<'_, Result<BlockRef, SourceError>> {
        Box::pin(async {
            self.0
                .lock()
                .expect("source lock")
                .last()
                .map(IndexedBlock::block_ref)
                .ok_or_else(|| SourceError {
                    message: "empty test chain".into(),
                    retryable: false,
                })
        })
    }

    fn block_at(&self, height: BlockHeight) -> BoxFuture<'_, Result<Self::Block, SourceError>> {
        Box::pin(async move {
            self.0
                .lock()
                .expect("source lock")
                .iter()
                .find(|block| block.0.height == height)
                .cloned()
                .ok_or_else(|| SourceError {
                    message: "missing test block".into(),
                    retryable: false,
                })
        })
    }

    fn canonical_hash(
        &self,
        height: BlockHeight,
    ) -> BoxFuture<'_, Result<Option<BlockHash>, SourceError>> {
        Box::pin(async move {
            Ok(self
                .0
                .lock()
                .expect("source lock")
                .iter()
                .find(|block| block.0.height == height)
                .map(|block| block.0.hash.clone()))
        })
    }
}

struct Interpreter;

impl BlockInterpreter for Interpreter {
    type Block = TestBlock;
    type Target = crate::WatchSelector;
    type Effect = IndexChanges;
    type Undo = IndexUndo;

    fn inspect(
        &self,
        block: &Self::Block,
        _watches: &[crate::WatchTarget<Self::Target>],
    ) -> Result<InterpretedBlock<Self::Effect, Self::Undo>, IndexError> {
        Ok(InterpretedBlock {
            block: block.block_ref(),
            drafts: Vec::new(),
            effect: IndexChanges::default(),
            undo: IndexUndo::default(),
        })
    }
}

#[derive(Default)]
struct State {
    canonical: BTreeMap<BlockHeight, BlockRef>,
    status: Option<SyncStatus>,
    commits: Vec<BlockRef>,
    reverts: Vec<BlockRef>,
    conflict_once: bool,
    commit_attempts: usize,
}

#[derive(Default)]
struct Repository(Mutex<State>);

impl Repository {
    fn conflict_once() -> Self {
        Self(Mutex::new(State {
            conflict_once: true,
            ..State::default()
        }))
    }
}

impl CanonicalStore for Repository {
    fn checkpoint(
        &self,
        _scope: &IndexScope,
    ) -> BoxFuture<'_, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async {
            Ok(self
                .0
                .lock()
                .expect("repository lock")
                .canonical
                .last_key_value()
                .map(|(_, block)| block.clone()))
        })
    }

    fn canonical_block(
        &self,
        _scope: &IndexScope,
        height: BlockHeight,
    ) -> BoxFuture<'_, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            Ok(self
                .0
                .lock()
                .expect("repository lock")
                .canonical
                .get(&height)
                .cloned())
        })
    }

    fn load_commit<'a>(
        &'a self,
        command: &'a CommitBlock<IndexChanges, IndexUndo>,
    ) -> BoxFuture<'a, Result<CommitContext, IndexError>> {
        Box::pin(async move {
            Ok(CommitContext {
                checkpoint: self.checkpoint(&command.scope).await?,
                watch_version: command.expected_watch_version,
                active_watches: Default::default(),
                observations: Default::default(),
                pending_confirmations: Default::default(),
            })
        })
    }
}

impl WatchStore for Repository {
    fn watches_at(
        &self,
        _scope: &IndexScope,
        _height: BlockHeight,
    ) -> BoxFuture<'_, Result<WatchSnapshot<crate::WatchSelector>, IndexError>> {
        Box::pin(async {
            Ok(WatchSnapshot {
                version: WatchVersion(0),
                watches: Vec::new(),
            })
        })
    }

    fn load_watch(
        &self,
        _command: &RegisterWatch<crate::WatchSelector>,
    ) -> BoxFuture<'_, Result<WatchContext<crate::WatchSelector>, IndexError>> {
        Box::pin(async {
            Err(IndexError::new(
                IndexErrorKind::Other,
                "watch registration is outside this synchronizer test",
                false,
            ))
        })
    }

    fn save_watch(
        &self,
        _plan: WatchPlan<crate::WatchSelector>,
    ) -> BoxFuture<'_, Result<(), IndexError>> {
        Box::pin(async { Ok(()) })
    }
}

impl BlockStore for Repository {
    fn commit_block(
        &self,
        plan: CommitPlan<IndexChanges, IndexUndo>,
    ) -> BoxFuture<'_, Result<BlockOutcome, IndexError>> {
        Box::pin(async move {
            let mut state = self.0.lock().expect("repository lock");
            state.commit_attempts += 1;
            if state.conflict_once {
                state.conflict_once = false;
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "simulated watch-version conflict",
                    true,
                ));
            }
            let checkpoint = state
                .canonical
                .last_key_value()
                .map(|(_, block)| block.clone());
            if checkpoint != plan.expected_checkpoint {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "checkpoint changed",
                    true,
                ));
            }
            let block = plan.block;
            state.canonical.insert(block.height, block.clone());
            state.commits.push(block);
            Ok(BlockOutcome::Applied)
        })
    }

    fn load_revert<'a>(
        &'a self,
        command: &'a RevertTip,
    ) -> BoxFuture<'a, Result<RevertContext<IndexUndo>, IndexError>> {
        Box::pin(async move {
            let state = self.0.lock().expect("repository lock");
            let checkpoint = state
                .canonical
                .last_key_value()
                .map(|(_, block)| block.clone());
            let block = (checkpoint.as_ref() == Some(&command.expected_tip)).then(|| RevertBlock {
                block: command.expected_tip.clone(),
                prior_checkpoint: state
                    .canonical
                    .range(..command.expected_tip.height)
                    .next_back()
                    .map(|(_, block)| block.clone()),
                undo: IndexUndo::default(),
                observations: Vec::new(),
            });
            Ok(RevertContext { checkpoint, block })
        })
    }

    fn save_revert(&self, plan: RevertPlan<IndexUndo>) -> BoxFuture<'_, Result<(), IndexError>> {
        Box::pin(async move {
            let mut state = self.0.lock().expect("repository lock");
            let current = state
                .canonical
                .last_key_value()
                .map(|(_, block)| block.clone());
            if current.as_ref() != Some(&plan.expected_tip) {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "tip changed",
                    true,
                ));
            }
            state.canonical.remove(&plan.expected_tip.height);
            state.reverts.push(plan.expected_tip);
            Ok(())
        })
    }
}

impl StatusStore for Repository {
    fn status(&self, _scope: &IndexScope) -> BoxFuture<'_, Result<Option<SyncStatus>, IndexError>> {
        Box::pin(async { Ok(self.0.lock().expect("repository lock").status.clone()) })
    }

    fn set_status(&self, status: SyncStatus) -> BoxFuture<'_, Result<(), IndexError>> {
        Box::pin(async move {
            self.0.lock().expect("repository lock").status = Some(status);
            Ok(())
        })
    }
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("test".into()),
        network: "local".into(),
    }
}

fn block(height: u64, hash: u8, parent: Option<u8>) -> TestBlock {
    TestBlock(BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(vec![hash]),
        parent_hash: parent.map(|value| BlockHash(vec![value])),
        timestamp: Some(height),
    })
}

fn request() -> SyncRequest {
    SyncRequest {
        scope: scope(),
        through: None,
        max_blocks: None,
    }
}

fn synchronizer(
    source: Source,
    repository: Repository,
) -> Synchronizer<Source, Interpreter, Repository> {
    Synchronizer::new(
        source,
        Interpreter,
        repository,
        SyncConfig::new(
            scope(),
            BlockHeight(0),
            ConfirmationPolicy {
                minimum_confirmations: 1,
                require_chain_finality: false,
            },
            8,
        )
        .expect("valid config"),
    )
}

#[test]
fn initial_sync_commits_through_tip_and_publishes_ready_checkpoint() {
    let synchronizer = synchronizer(
        Source(Arc::new(Mutex::new(vec![
            block(0, 10, None),
            block(1, 11, Some(10)),
        ]))),
        Repository::default(),
    );

    let status = block_on(synchronizer.sync(request())).expect("sync succeeds");

    assert_eq!(status.phase, SyncPhase::Ready);
    assert_eq!(status.checkpoint, Some(block(1, 11, Some(10)).0));
    let state = synchronizer.repository().0.lock().expect("repository lock");
    assert_eq!(state.commits.len(), 2);
}

#[test]
fn later_sync_continues_after_the_durable_checkpoint() {
    let source = Source(Arc::new(Mutex::new(vec![block(0, 10, None)])));
    let synchronizer = synchronizer(source.clone(), Repository::default());
    block_on(synchronizer.sync(request())).expect("initial sync succeeds");
    source.replace(vec![
        block(0, 10, None),
        block(1, 11, Some(10)),
        block(2, 12, Some(11)),
    ]);

    let status = block_on(synchronizer.sync(request())).expect("continuation succeeds");

    assert_eq!(status.checkpoint, Some(block(2, 12, Some(11)).0));
    let state = synchronizer.repository().0.lock().expect("repository lock");
    assert_eq!(
        state
            .commits
            .iter()
            .map(|block| block.height.0)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn shallow_reorg_reverts_old_tip_then_commits_replacement_branch() {
    let source = Source(Arc::new(Mutex::new(vec![
        block(0, 10, None),
        block(1, 11, Some(10)),
        block(2, 12, Some(11)),
    ])));
    let synchronizer = synchronizer(source.clone(), Repository::default());
    block_on(synchronizer.sync(request())).expect("initial sync succeeds");
    source.replace(vec![
        block(0, 10, None),
        block(1, 21, Some(10)),
        block(2, 22, Some(21)),
    ]);

    let status = block_on(synchronizer.sync(request())).expect("reorg sync succeeds");

    assert_eq!(status.phase, SyncPhase::Ready);
    assert_eq!(status.checkpoint, Some(block(2, 22, Some(21)).0));
    let state = synchronizer.repository().0.lock().expect("repository lock");
    assert_eq!(
        state.reverts,
        vec![block(2, 12, Some(11)).0, block(1, 11, Some(10)).0]
    );
}

#[test]
fn retryable_commit_conflict_reloads_watches_and_finishes_ready() {
    let synchronizer = synchronizer(
        Source(Arc::new(Mutex::new(vec![block(0, 10, None)]))),
        Repository::conflict_once(),
    );

    let status = block_on(synchronizer.sync(request())).expect("conflict is retried");

    assert_eq!(status.phase, SyncPhase::Ready);
    assert_eq!(status.checkpoint, Some(block(0, 10, None).0));
    let state = synchronizer.repository().0.lock().expect("repository lock");
    assert_eq!(state.commit_attempts, 2);
    assert_eq!(
        state.status.as_ref().map(|status| status.phase),
        Some(SyncPhase::Ready)
    );
}
