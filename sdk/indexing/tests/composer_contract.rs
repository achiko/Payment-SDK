use std::sync::{Arc, Mutex};

use futures_executor::block_on;
use indexing::{
    AddressFilter, BlockHash, BlockHeight, BlockRef, BoxFuture, CanonicalAddress, ChainId,
    Checkpoint, Composer, History, HistoryQuery, IndexError, IndexErrorKind, IndexScope, Indexer,
    SyncPhase, SyncStatus, TransactionPage,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Checkpoint(IndexScope),
    History(Box<HistoryQuery>),
    Sync(Vec<AddressFilter>),
}

struct Probe {
    scopes: Vec<IndexScope>,
    checkpoint: BlockRef,
    calls: Mutex<Vec<Call>>,
}

impl Probe {
    fn new(scope: IndexScope, height: u64) -> Self {
        Self {
            scopes: vec![scope],
            checkpoint: block(height),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("probe calls").clone()
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("probe calls").push(call);
    }
}

impl Checkpoint for Probe {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.record(Call::Checkpoint(scope.clone()));
        let checkpoint = self.checkpoint.clone();
        Box::pin(async move { Ok(Some(checkpoint)) })
    }
}

impl History for Probe {
    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        self.record(Call::History(Box::new(request)));
        let checkpoint = self.checkpoint.clone();
        Box::pin(async move {
            Ok(TransactionPage {
                checkpoint: Some(checkpoint),
                transactions: Vec::new(),
                next: None,
            })
        })
    }
}

impl Indexer for Probe {
    fn scopes(&self) -> &[IndexScope] {
        &self.scopes
    }

    fn sync<'a>(
        &'a self,
        filters: Vec<AddressFilter>,
    ) -> BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>> {
        self.record(Call::Sync(filters));
        let statuses = self
            .scopes
            .iter()
            .cloned()
            .map(|scope| SyncStatus {
                scope,
                checkpoint: Some(self.checkpoint.clone()),
                observed_tip: Some(self.checkpoint.clone()),
                phase: SyncPhase::Ready,
            })
            .collect();
        Box::pin(async move { Ok(statuses) })
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

fn block(height: u64) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(vec![height as u8]),
        parent_hash: height
            .checked_sub(1)
            .map(|value| BlockHash(vec![value as u8])),
        timestamp: None,
    }
}

fn exercise_indexer(indexer: &dyn Indexer, scope: &IndexScope, height: u64) {
    assert_eq!(indexer.scopes(), std::slice::from_ref(scope));
    assert_eq!(
        block_on(indexer.checkpoint(scope)).expect("checkpoint through Indexer"),
        Some(block(height))
    );

    let owner = address(scope, "owner");
    let filter = AddressFilter {
        address: owner.clone(),
        start_height: BlockHeight(height),
    };
    let page = block_on(indexer.history(HistoryQuery {
        scope: scope.clone(),
        address: owner,
        after: None,
        limit: 10,
    }))
    .expect("history through Indexer");
    assert_eq!(page.checkpoint, Some(block(height)));

    let statuses = block_on(indexer.sync(vec![filter])).expect("sync through Indexer");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].scope, *scope);
}

#[test]
fn one_chain_and_composer_share_the_indexer_contract() {
    let single_scope = scope("single-chain");
    let single = Probe::new(single_scope.clone(), 4);
    exercise_indexer(&single, &single_scope, 4);

    let composed_scope = scope("composed-chain");
    let composer = Composer::new(vec![
        Arc::new(Probe::new(composed_scope.clone(), 7)) as Arc<dyn Indexer>
    ])
    .expect("one chain composes");
    exercise_indexer(&composer, &composed_scope, 7);
}

#[test]
fn routes_each_scoped_operation_to_its_indexer() {
    let first_scope = scope("first-chain");
    let second_scope = scope("second-chain");
    let first = Arc::new(Probe::new(first_scope.clone(), 4));
    let second = Arc::new(Probe::new(second_scope.clone(), 7));
    let composer = Composer::new(vec![
        first.clone() as Arc<dyn Indexer>,
        second.clone() as Arc<dyn Indexer>,
    ])
    .expect("disjoint scopes compose");

    let checkpoint = block_on(composer.checkpoint(&first_scope)).expect("checkpoint");
    assert_eq!(checkpoint, Some(block(4)));

    let query = HistoryQuery {
        scope: second_scope.clone(),
        address: address(&second_scope, "owner"),
        after: None,
        limit: 25,
    };
    let page = block_on(composer.history(query.clone())).expect("history");
    assert_eq!(page.checkpoint, Some(block(7)));

    assert_eq!(first.calls(), vec![Call::Checkpoint(first_scope)]);
    assert_eq!(second.calls(), vec![Call::History(Box::new(query))]);
}

#[test]
fn rejects_an_empty_composer() {
    let result = Composer::new(Vec::new());
    let error = match result {
        Ok(_) => panic!("empty composer must not report readiness"),
        Err(error) => error,
    };

    assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
}

#[test]
fn rejects_duplicate_scopes() {
    let shared_scope = scope("same-chain");
    let result = Composer::new(vec![
        Arc::new(Probe::new(shared_scope.clone(), 1)) as Arc<dyn Indexer>,
        Arc::new(Probe::new(shared_scope, 2)) as Arc<dyn Indexer>,
    ]);

    let error = match result {
        Ok(_) => panic!("duplicate scopes must not compose"),
        Err(error) => error,
    };
    assert_eq!(error.kind, IndexErrorKind::Conflict);
}

#[test]
fn rejects_operations_for_an_unconfigured_scope() {
    let configured = scope("configured-chain");
    let missing = scope("missing-chain");
    let composer = Composer::new(vec![Arc::new(Probe::new(configured, 1)) as Arc<dyn Indexer>])
        .expect("one scope composes");

    let checkpoint_error = block_on(composer.checkpoint(&missing)).expect_err("missing checkpoint");
    let history_error = block_on(composer.history(HistoryQuery {
        scope: missing.clone(),
        address: address(&missing, "owner"),
        after: None,
        limit: 1,
    }))
    .expect_err("missing history");
    let sync_error = block_on(composer.sync(vec![AddressFilter {
        address: address(&missing, "owner"),
        start_height: BlockHeight(0),
    }]))
    .expect_err("missing sync scope");

    assert_eq!(checkpoint_error.kind, IndexErrorKind::ScopeMismatch);
    assert_eq!(history_error.kind, IndexErrorKind::ScopeMismatch);
    assert_eq!(sync_error.kind, IndexErrorKind::ScopeMismatch);
}

#[test]
fn partitions_filters_and_combines_statuses_from_every_indexer() {
    let first_scope = scope("first-chain");
    let second_scope = scope("second-chain");
    let idle_scope = scope("idle-chain");
    let first = Arc::new(Probe::new(first_scope.clone(), 4));
    let second = Arc::new(Probe::new(second_scope.clone(), 7));
    let idle = Arc::new(Probe::new(idle_scope.clone(), 9));
    let composer = Composer::new(vec![
        first.clone() as Arc<dyn Indexer>,
        second.clone() as Arc<dyn Indexer>,
        idle.clone() as Arc<dyn Indexer>,
    ])
    .expect("disjoint scopes compose");

    let first_filter = AddressFilter {
        address: address(&first_scope, "first-owner"),
        start_height: BlockHeight(2),
    };
    let second_filter = AddressFilter {
        address: address(&second_scope, "second-owner"),
        start_height: BlockHeight(5),
    };
    let statuses = block_on(composer.sync(vec![second_filter.clone(), first_filter.clone()]))
        .expect("composed sync");

    assert_eq!(
        statuses
            .into_iter()
            .map(|status| status.scope)
            .collect::<Vec<_>>(),
        vec![first_scope, second_scope, idle_scope]
    );
    assert_eq!(first.calls(), vec![Call::Sync(vec![first_filter])]);
    assert_eq!(second.calls(), vec![Call::Sync(vec![second_filter])]);
    assert_eq!(idle.calls(), vec![Call::Sync(Vec::new())]);
}
