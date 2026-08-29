use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures_executor::block_on;
use indexing::{
    AddressFilter, BlockAddition, BlockHash, BlockHeight, BlockInterpreter, BlockOutcome,
    BlockParent, BlockPosition, BlockRef, BlockSelector, BlockSource, Blocks, BoxFuture,
    CanonicalAddress, CanonicalPage, ChainId, FilterSource, HistoryQuery, IndexError,
    IndexErrorKind, IndexScope, IndexedBlock, Indexer, InterpretedBlock, OutputChanges, Service,
    SourceError, SyncConfig, SyncPhase, Transactions,
};

#[derive(Clone, Debug)]
struct TestBlock(BlockRef);

impl IndexedBlock for TestBlock {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

#[derive(Clone)]
struct Source {
    blocks: Arc<Mutex<BTreeMap<BlockPosition, TestBlock>>>,
    tip_calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<BlockHeight>>>,
    /// Runs while the tip is being observed, standing in for anything that can
    /// happen during that round trip — such as a wallet being created.
    during_tip: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Source {
    fn new(tip: u64) -> Self {
        let blocks = (0..=tip)
            .map(|height| {
                let block = TestBlock(block(BlockHeight(height)));
                (block.0.position, block)
            })
            .collect();
        Self {
            blocks: Arc::new(Mutex::new(blocks)),
            tip_calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            during_tip: None,
        }
    }

    fn sparse(blocks: impl IntoIterator<Item = BlockRef>) -> Self {
        Self {
            blocks: Arc::new(Mutex::new(
                blocks
                    .into_iter()
                    .map(|block| (block.position, TestBlock(block)))
                    .collect(),
            )),
            tip_calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            during_tip: None,
        }
    }

    fn replace(&self, blocks: impl IntoIterator<Item = BlockRef>) {
        *self.blocks.lock().expect("source blocks") = blocks
            .into_iter()
            .map(|block| (block.position, TestBlock(block)))
            .collect();
    }

    fn during_tip(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.during_tip = Some(Arc::new(hook));
        self
    }

    fn tip_calls(&self) -> usize {
        self.tip_calls.load(Ordering::Acquire)
    }

    fn requests(&self) -> Vec<BlockHeight> {
        self.requests.lock().expect("source requests").clone()
    }
}

impl BlockSource for Source {
    type Block = TestBlock;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        self.tip_calls.fetch_add(1, Ordering::AcqRel);
        if let Some(hook) = &self.during_tip {
            hook();
        }
        let result = self
            .blocks
            .lock()
            .expect("source blocks")
            .last_key_value()
            .map(|(_, block)| block.0.clone())
            .ok_or_else(|| SourceError {
                message: "source has no produced blocks".into(),
                retryable: true,
            });
        Box::pin(async move { result })
    }

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        let result = if limit == 0 || start > end {
            Err(SourceError {
                message: "invalid block range".into(),
                retryable: false,
            })
        } else {
            let blocks = self
                .blocks
                .lock()
                .expect("source blocks")
                .range(start..=end)
                .take(limit)
                .map(|(_, block)| block.clone())
                .collect::<Vec<_>>();
            let mut requests = self.requests.lock().expect("source requests");
            for block in &blocks {
                requests.push(block.0.height);
            }
            Ok(blocks)
        };
        Box::pin(async move { result })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        let block = self
            .blocks
            .lock()
            .expect("source blocks")
            .get(&position)
            .map(|block| block.0.clone());
        Box::pin(async move { Ok(block) })
    }
}

#[derive(Clone, Default)]
struct Repository {
    state: Arc<Mutex<RepositoryState>>,
}

#[derive(Default)]
struct RepositoryState {
    checkpoint: Option<BlockRef>,
    blocks: BTreeMap<BlockHeight, BlockRef>,
}

impl Blocks for Repository {
    fn get<'a>(
        &'a self,
        selector: BlockSelector,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            let state = self.state.lock().expect("repository state");
            Ok(match selector {
                BlockSelector::Tip(_) => state.checkpoint.clone(),
                BlockSelector::Height { height, .. } => state.blocks.get(&height).cloned(),
            })
        })
    }

    fn add<'a>(
        &'a self,
        addition: BlockAddition,
    ) -> BoxFuture<'a, Result<BlockOutcome, IndexError>> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("repository state");
            if state.checkpoint.as_ref() != addition.expected_checkpoint() {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "checkpoint changed",
                    true,
                ));
            }
            let block = addition.block().clone();
            state.blocks.insert(block.height, block.clone());
            state.checkpoint = Some(block);
            Ok(BlockOutcome::Applied)
        })
    }

    fn remove<'a>(
        &'a self,
        _scope: IndexScope,
        expected_tip: BlockRef,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("repository state");
            if state.checkpoint.as_ref() != Some(&expected_tip) {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "checkpoint changed",
                    true,
                ));
            }
            state.blocks.remove(&expected_tip.height);
            state.checkpoint = expected_tip
                .height
                .0
                .checked_sub(1)
                .and_then(|height| state.blocks.get(&BlockHeight(height)).cloned());
            Ok(state.checkpoint.clone())
        })
    }
}

impl Transactions for Repository {
    fn list<'a>(
        &'a self,
        _request: HistoryQuery,
    ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        Box::pin(async move {
            Ok(CanonicalPage {
                checkpoint: self
                    .state
                    .lock()
                    .expect("repository state")
                    .checkpoint
                    .clone(),
                transactions: Vec::new(),
                next: None,
            })
        })
    }
}

type Inspections = Arc<Mutex<Vec<(BlockHeight, Vec<CanonicalAddress>)>>>;

#[derive(Clone, Default)]
struct Interpreter {
    inspections: Inspections,
}

impl Interpreter {
    fn inspections(&self) -> Vec<(BlockHeight, Vec<CanonicalAddress>)> {
        self.inspections
            .lock()
            .expect("interpreter inspections")
            .clone()
    }
}

impl BlockInterpreter for Interpreter {
    type Block = TestBlock;

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[CanonicalAddress],
    ) -> Result<InterpretedBlock, IndexError> {
        self.inspections
            .lock()
            .expect("interpreter inspections")
            .push((block.0.height, addresses.to_vec()));
        Ok(InterpretedBlock {
            block: block.0.clone(),
            transactions: Vec::new(),
            outputs: OutputChanges::default(),
        })
    }
}

/// An address selection that can grow while a pass is running, the way a
/// registry does when a wallet is created.
#[derive(Clone, Default)]
struct Selection(Arc<Mutex<Vec<AddressFilter>>>);

impl Selection {
    fn register(&self, filter: AddressFilter) {
        self.0.lock().expect("selection").push(filter);
    }
}

impl FilterSource for Selection {
    fn filters(&self) -> Result<Vec<AddressFilter>, IndexError> {
        Ok(self.0.lock().expect("selection").clone())
    }
}

fn scope(chain: &str) -> IndexScope {
    IndexScope {
        chain: ChainId(chain.into()),
        network: "testnet".into(),
    }
}

fn address(scope: &IndexScope, value: &str) -> CanonicalAddress {
    CanonicalAddress {
        scope: scope.clone(),
        value: value.into(),
    }
}

fn hash(height: BlockHeight) -> BlockHash {
    BlockHash(height.0.to_be_bytes().to_vec())
}

fn block(height: BlockHeight) -> BlockRef {
    BlockRef {
        position: BlockPosition(height.0),
        height,
        hash: hash(height),
        parent: height.0.checked_sub(1).map(|parent| BlockParent {
            position: BlockPosition(parent),
            hash: hash(BlockHeight(parent)),
        }),
        timestamp: None,
    }
}

fn sparse_block(
    position: u64,
    height: u64,
    hash_byte: u8,
    parent_position: u64,
    parent_hash: u8,
) -> BlockRef {
    BlockRef {
        position: BlockPosition(position),
        height: BlockHeight(height),
        hash: BlockHash(vec![hash_byte]),
        parent: Some(BlockParent {
            position: BlockPosition(parent_position),
            hash: BlockHash(vec![parent_hash]),
        }),
        timestamp: Some(1_000 + position),
    }
}

fn sparse_chain() -> Vec<BlockRef> {
    vec![
        sparse_block(97, 49, 0, 94, 9),
        sparse_block(100, 50, 1, 97, 0),
        sparse_block(103, 51, 2, 100, 1),
        sparse_block(107, 52, 3, 103, 2),
    ]
}

fn replacement_chain() -> Vec<BlockRef> {
    vec![
        sparse_block(97, 49, 0, 94, 9),
        sparse_block(100, 50, 1, 97, 0),
        sparse_block(104, 51, 12, 100, 1),
        sparse_block(108, 52, 13, 104, 12),
    ]
}

fn config(scope: IndexScope) -> SyncConfig {
    SyncConfig::new(scope, 1, 4, 100).expect("sync config")
}

fn bounded_config(scope: IndexScope, retention: u64, batch_size: usize) -> SyncConfig {
    SyncConfig::new(scope, 1, retention, batch_size).expect("bounded sync config")
}

#[test]
fn source_contract_is_complete_bounded_and_position_addressed() {
    let source = Source::new(4);

    assert_eq!(block_on(source.tip()).expect("tip"), block(BlockHeight(4)));
    let blocks = block_on(source.blocks(BlockPosition(1), BlockPosition(4), 2))
        .expect("bounded inclusive range");
    assert_eq!(
        blocks
            .iter()
            .map(IndexedBlock::block_ref)
            .collect::<Vec<_>>(),
        [block(BlockHeight(1)), block(BlockHeight(2))]
    );
    assert_eq!(
        block_on(source.canonical_at(BlockPosition(3))).expect("canonical lookup"),
        Some(block(BlockHeight(3)))
    );
    assert_eq!(
        block_on(source.canonical_at(BlockPosition(5))).expect("omitted position"),
        None
    );
    let error = block_on(source.blocks(BlockPosition(1), BlockPosition(4), 0))
        .expect_err("zero returned-block limit");
    assert!(!error.retryable);
}

#[test]
fn sparse_sync_uses_actual_blocks_and_resumes_a_bounded_prefix() {
    let own_scope = scope("sparse-chain");
    let owner = address(&own_scope, "owner");
    let source = Source::sparse(sparse_chain());
    let interpreter = Interpreter::default();
    let service = Service::new(
        source,
        interpreter.clone(),
        Repository::default(),
        bounded_config(own_scope, 4, 2),
    );
    let filters = vec![AddressFilter {
        address: owner.clone(),
        start_position: BlockPosition(102),
    }];

    let first = block_on(service.sync(&filters)).expect("first sparse prefix");
    assert_eq!(first[0].phase, SyncPhase::CatchingUp);
    assert_eq!(first[0].checkpoint, Some(sparse_chain()[2].clone()));

    let second = block_on(service.sync(&filters)).expect("second sparse prefix");
    assert_eq!(second[0].phase, SyncPhase::Ready);
    assert_eq!(second[0].checkpoint, Some(sparse_chain()[3].clone()));
    assert_eq!(
        interpreter.inspections(),
        [
            (BlockHeight(50), Vec::new()),
            (BlockHeight(51), vec![owner.clone()]),
            (BlockHeight(52), vec![owner]),
        ]
    );
}

#[test]
fn sparse_sync_reconciles_a_retained_reorg_by_native_position() {
    let own_scope = scope("sparse-reorg");
    let owner = address(&own_scope, "owner");
    let source = Source::sparse(sparse_chain());
    let repository = Repository::default();
    let service = Service::new(
        source.clone(),
        Interpreter::default(),
        repository,
        config(own_scope.clone()),
    );
    let filters = vec![AddressFilter {
        address: owner,
        start_position: BlockPosition(100),
    }];
    block_on(service.sync(&filters)).expect("initial sparse chain");

    source.replace(replacement_chain());
    let status = block_on(service.sync(&filters)).expect("retained sparse reorg");

    assert_eq!(status[0].phase, SyncPhase::Ready);
    assert_eq!(status[0].checkpoint, Some(replacement_chain()[3].clone()));
}

#[test]
fn sparse_sync_reports_reorg_beyond_retention() {
    let own_scope = scope("sparse-deep-reorg");
    let owner = address(&own_scope, "owner");
    let source = Source::sparse(sparse_chain());
    let service = Service::new(
        source.clone(),
        Interpreter::default(),
        Repository::default(),
        bounded_config(own_scope, 1, 100),
    );
    let filters = vec![AddressFilter {
        address: owner,
        start_position: BlockPosition(100),
    }];
    block_on(service.sync(&filters)).expect("initial sparse chain");

    source.replace(replacement_chain());
    let error = block_on(service.sync(&filters)).expect_err("deep sparse reorg");

    assert_eq!(error.kind, IndexErrorKind::ReorgTooDeep);
}

#[test]
fn fresh_wallet_sync_starts_at_its_earliest_birthday_not_genesis() {
    let own_scope = scope("any-chain");
    let first = address(&own_scope, "first");
    let second = address(&own_scope, "second");
    let interpreter = Interpreter::default();
    let source = Source::new(4);
    let service = Service::new(
        source.clone(),
        interpreter.clone(),
        Repository::default(),
        config(own_scope),
    );

    let statuses = block_on(service.sync(&vec![
        AddressFilter {
            address: first.clone(),
            start_position: BlockPosition(3),
        },
        AddressFilter {
            address: second.clone(),
            start_position: BlockPosition(4),
        },
    ]))
    .expect("sync selected addresses");

    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].phase, SyncPhase::Ready);
    assert_eq!(statuses[0].checkpoint, Some(block(BlockHeight(4))));
    assert_eq!(
        source.requests(),
        vec![BlockHeight(3), BlockHeight(2), BlockHeight(4)]
    );
    assert_eq!(
        interpreter.inspections(),
        vec![
            (BlockHeight(2), Vec::new()),
            (BlockHeight(3), vec![first.clone()]),
            (BlockHeight(4), vec![first, second]),
        ]
    );
}

#[test]
fn fresh_index_without_wallets_anchors_directly_at_the_tip() {
    let own_scope = scope("any-chain");
    let interpreter = Interpreter::default();
    let source = Source::new(500);
    let service = Service::new(
        source.clone(),
        interpreter.clone(),
        Repository::default(),
        config(own_scope),
    );

    let statuses = block_on(service.sync(&Vec::new())).expect("sync without wallets");

    assert_eq!(statuses[0].phase, SyncPhase::Ready);
    assert_eq!(statuses[0].checkpoint, Some(block(BlockHeight(500))));
    assert_eq!(source.requests(), vec![BlockHeight(500)]);
    assert_eq!(
        interpreter.inspections(),
        vec![(BlockHeight(500), Vec::new())]
    );
}

#[test]
fn caller_can_restore_the_same_filter_selection_after_restart() {
    let own_scope = scope("any-chain");
    let owner = address(&own_scope, "owner");
    let repository = Repository::default();
    let initial = Service::new(
        Source::new(4),
        Interpreter::default(),
        repository.clone(),
        config(own_scope.clone()),
    );
    let filters = vec![AddressFilter {
        address: owner.clone(),
        start_position: BlockPosition(3),
    }];
    block_on(initial.sync(&filters)).expect("initial sync");
    drop(initial);

    let restarted_source = Source::new(4);
    let restarted = Service::new(
        restarted_source.clone(),
        Interpreter::default(),
        repository.clone(),
        config(own_scope.clone()),
    );
    block_on(restarted.sync(&filters)).expect("restart sync");
    assert!(restarted_source.requests().is_empty());
}

#[test]
fn sync_rejects_an_address_from_another_scope() {
    let own_scope = scope("own-chain");
    let service = Service::new(
        Source::new(0),
        Interpreter::default(),
        Repository::default(),
        config(own_scope),
    );

    let error = block_on(service.sync(&vec![AddressFilter {
        address: address(&scope("other-chain"), "owner"),
        start_position: BlockPosition(0),
    }]))
    .expect_err("foreign address");

    assert_eq!(error.kind, IndexErrorKind::ScopeMismatch);
}

#[test]
fn sync_rejects_empty_and_duplicate_addresses_before_source_io() {
    let own_scope = scope("own-chain");
    let duplicate = address(&own_scope, "duplicate");
    let cases = [
        vec![AddressFilter {
            address: address(&own_scope, ""),
            start_position: BlockPosition(0),
        }],
        vec![
            AddressFilter {
                address: duplicate.clone(),
                start_position: BlockPosition(1),
            },
            AddressFilter {
                address: duplicate,
                start_position: BlockPosition(2),
            },
        ],
    ];

    for filters in cases {
        let source = Source::new(10);
        let service = Service::new(
            source.clone(),
            Interpreter::default(),
            Repository::default(),
            config(own_scope.clone()),
        );

        let error = block_on(service.sync(&filters)).expect_err("invalid address selection");

        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
        assert_eq!(source.tip_calls(), 0);
        assert!(source.requests().is_empty());
    }
}

/// An address registered while the tip is being observed is still inspected for
/// in the blocks its birthday covers.
///
/// A wallet is anchored at the checkpoint that was current when it was created,
/// which promises every later block is inspected for it. If the pass indexed
/// against a selection read before it observed the tip, the blocks that tip
/// admits would be applied without the new address — and nothing rescans them
/// once the checkpoint moves past, so the miss is permanent rather than late.
#[test]
fn an_address_registered_while_the_tip_is_read_still_covers_its_birthday() {
    let own_scope = scope("late-chain");
    let late = address(&own_scope, "late");
    let selection = Selection::default();
    let source = Source::new(2).during_tip({
        let selection = selection.clone();
        let late = late.clone();
        move || {
            selection.register(AddressFilter {
                address: late.clone(),
                start_position: BlockPosition(1),
            });
        }
    });
    let interpreter = Interpreter::default();
    let service = Service::new(
        source,
        interpreter.clone(),
        Repository::default(),
        config(own_scope),
    );

    block_on(service.sync(&selection)).expect("sync");

    let covered = interpreter
        .inspections()
        .into_iter()
        .filter(|(height, _)| *height >= BlockHeight(1))
        .collect::<Vec<_>>();
    assert!(
        !covered.is_empty(),
        "blocks at or above the address birthday must be indexed"
    );
    for (height, addresses) in covered {
        assert!(
            addresses.contains(&late),
            "block {height:?} was indexed without the address its birthday covers"
        );
    }
}
