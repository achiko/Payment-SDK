use std::sync::{Arc, Mutex, MutexGuard};

use futures_channel::oneshot;

use crate::{AddressFilter, BlockPosition, BlockRef, CanonicalAddress, IndexError, IndexErrorKind};

#[derive(Default)]
struct State {
    initialized: bool,
    persisted: Option<BlockRef>,
    revision: u64,
    commit: bool,
    publication: bool,
    recovery: bool,
    waiters: Vec<oneshot::Sender<()>>,
}

/// Serializes checkpoint commits with forward-only address publication.
#[derive(Default)]
pub struct ScopeAdmission {
    state: Mutex<State>,
}

impl ScopeAdmission {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Captures filters and their revision against one repository checkpoint.
    pub fn plan<F>(
        self: &Arc<Self>,
        persisted: Option<BlockRef>,
        capture: F,
    ) -> Result<SyncPlan, IndexError>
    where
        F: FnOnce() -> Result<Vec<AddressFilter>, IndexError>,
    {
        let revision = {
            let mut state = self.lock()?;
            if state.commit || state.publication {
                return Err(conflict("address admission is changing"));
            }
            if !state.initialized || state.recovery || state.persisted != persisted {
                state.persisted = persisted.clone();
                state.initialized = true;
                state.recovery = false;
            }
            state.revision
        };
        let filters = capture()?;
        let state = self.lock()?;
        if state.commit
            || state.publication
            || state.recovery
            || state.persisted != persisted
            || state.revision != revision
        {
            return Err(conflict(
                "checkpoint or address revision changed during filter capture",
            ));
        }
        Ok(SyncPlan {
            filters,
            checkpoint: persisted,
            revision,
            admission: Some(self.clone()),
        })
    }

    /// Waits without holding the state lock, then reserves publication.
    pub async fn publication(
        self: &Arc<Self>,
        persisted: Option<BlockRef>,
    ) -> Result<PublicationPermit, IndexError> {
        let mut reload = Some(persisted);
        loop {
            let wait = {
                let mut state = self.lock()?;
                if state.commit || state.publication {
                    reload = None;
                    let (send, receive) = oneshot::channel();
                    state.waiters.push(send);
                    Some(receive)
                } else {
                    if let Some(persisted) = reload.take()
                        && (!state.initialized || state.recovery || state.persisted != persisted)
                    {
                        state.persisted = persisted;
                        state.initialized = true;
                        state.recovery = false;
                    }
                    state.publication = true;
                    return Ok(PublicationPermit {
                        admission: self.clone(),
                        checkpoint: state.persisted.clone(),
                        finished: false,
                    });
                }
            };
            let wait = wait.ok_or_else(|| unavailable("busy admission did not create a waiter"))?;
            wait.await
                .map_err(|_| unavailable("address admission waiter was abandoned"))?;
        }
    }

    fn begin(self: &Arc<Self>, plan: &SyncPlan) -> Result<CommitPermit, IndexError> {
        let mut state = self.lock()?;
        if state.recovery {
            return Err(conflict("checkpoint admission requires repository reload"));
        }
        if state.commit || state.publication {
            return Err(conflict("checkpoint admission is busy"));
        }
        if state.persisted != plan.checkpoint || state.revision != plan.revision {
            return Err(conflict(
                "checkpoint or address revision changed before commit",
            ));
        }
        state.commit = true;
        Ok(CommitPermit {
            admission: Some(self.clone()),
            started: false,
            finished: false,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, IndexError> {
        self.state
            .lock()
            .map_err(|_| unavailable("address admission lock is poisoned"))
    }
}

/// One immutable filter/checkpoint/revision snapshot used by a sync plan.
pub struct SyncPlan {
    filters: Vec<AddressFilter>,
    checkpoint: Option<BlockRef>,
    revision: u64,
    admission: Option<Arc<ScopeAdmission>>,
}

impl SyncPlan {
    #[must_use]
    pub fn detached(filters: Vec<AddressFilter>, checkpoint: Option<BlockRef>) -> Self {
        Self {
            filters,
            checkpoint,
            revision: 0,
            admission: None,
        }
    }

    #[must_use]
    pub fn filters(&self) -> &[AddressFilter] {
        &self.filters
    }

    pub(crate) fn active_addresses(&self, position: BlockPosition) -> Vec<CanonicalAddress> {
        self.filters
            .iter()
            .filter(|filter| filter.start_position <= position)
            .map(|filter| filter.address.clone())
            .collect()
    }

    #[must_use]
    pub fn checkpoint(&self) -> Option<&BlockRef> {
        self.checkpoint.as_ref()
    }

    #[must_use]
    pub fn with_filters(mut self, filters: Vec<AddressFilter>) -> Self {
        self.filters = filters;
        self
    }

    pub fn begin(&self) -> Result<CommitPermit, IndexError> {
        match &self.admission {
            Some(admission) => admission.begin(self),
            None => Ok(CommitPermit::detached()),
        }
    }

    pub fn advance(&mut self, checkpoint: BlockRef) {
        self.checkpoint = Some(checkpoint);
    }
}

/// Owns one asynchronous repository transition without holding a mutex guard.
pub struct CommitPermit {
    admission: Option<Arc<ScopeAdmission>>,
    started: bool,
    finished: bool,
}

impl CommitPermit {
    fn detached() -> Self {
        Self {
            admission: None,
            started: false,
            finished: false,
        }
    }

    pub fn start(&mut self) {
        self.started = true;
    }

    pub fn persist(&mut self, checkpoint: Option<BlockRef>) -> Result<(), IndexError> {
        if let Some(admission) = &self.admission {
            admission.lock()?.persisted = checkpoint;
        }
        Ok(())
    }

    pub fn complete(mut self, checkpoint: Option<BlockRef>) -> Result<(), IndexError> {
        if let Some(admission) = &self.admission {
            let mut state = admission.lock()?;
            state.persisted = checkpoint;
            state.commit = false;
            notify(&mut state);
        }
        self.finished = true;
        Ok(())
    }
}

impl Drop for CommitPermit {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let Some(admission) = &self.admission else {
            return;
        };
        if let Ok(mut state) = admission.state.lock() {
            state.commit = false;
            state.recovery |= self.started;
            notify(&mut state);
        }
    }
}

/// Holds publication closed against commits until wallet/filter insertion ends.
pub struct PublicationPermit {
    admission: Arc<ScopeAdmission>,
    checkpoint: Option<BlockRef>,
    finished: bool,
}

impl PublicationPermit {
    #[must_use]
    pub fn checkpoint(&self) -> Option<&BlockRef> {
        self.checkpoint.as_ref()
    }

    pub fn complete(mut self) -> Result<(), IndexError> {
        let mut state = self.admission.lock()?;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| unavailable("address filter revision is exhausted"))?;
        state.publication = false;
        notify(&mut state);
        drop(state);
        self.finished = true;
        Ok(())
    }
}

impl Drop for PublicationPermit {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(mut state) = self.admission.state.lock() {
            state.publication = false;
            notify(&mut state);
        }
    }
}

fn notify(state: &mut State) {
    for waiter in std::mem::take(&mut state.waiters) {
        let _ = waiter.send(());
    }
}

fn conflict(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Conflict, message, true)
}

fn unavailable(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    use futures_executor::block_on;

    use super::*;
    use crate::{BlockHash, BlockHeight, BlockParent, ChainId, IndexScope};

    fn block(position: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(position),
            height: BlockHeight(position),
            hash: BlockHash(vec![position as u8]),
            parent: position.checked_sub(1).map(|parent| BlockParent {
                position: BlockPosition(parent),
                hash: BlockHash(vec![parent as u8]),
            }),
            timestamp: None,
        }
    }

    fn commit_error(result: Result<CommitPermit, IndexError>, message: &str) -> IndexError {
        match result {
            Ok(_) => panic!("{message}"),
            Err(error) => error,
        }
    }

    fn filter(value: &str, position: u64) -> AddressFilter {
        AddressFilter {
            address: CanonicalAddress {
                scope: IndexScope {
                    chain: ChainId("test".into()),
                    network: "testing".into(),
                },
                value: value.into(),
            },
            start_position: BlockPosition(position),
        }
    }

    #[test]
    fn empty_plan_has_no_active_addresses() {
        let plan = SyncPlan::detached(Vec::new(), None);

        assert!(plan.active_addresses(BlockPosition(0)).is_empty());
        assert!(plan.active_addresses(BlockPosition(u64::MAX)).is_empty());
    }

    #[test]
    fn active_addresses_use_inclusive_native_birthdays_and_preserve_input_order() {
        let filters = vec![
            filter("future", 107),
            filter("birthday", 103),
            filter("earlier", 100),
            filter("skipped", 102),
        ];
        let checkpoint = BlockRef {
            height: BlockHeight(2),
            ..block(103)
        };
        let plan = SyncPlan::detached(filters.clone(), Some(checkpoint));

        assert!(plan.active_addresses(BlockPosition(99)).is_empty());
        assert_eq!(
            plan.active_addresses(BlockPosition(103)),
            vec![
                filters[1].address.clone(),
                filters[2].address.clone(),
                filters[3].address.clone(),
            ]
        );
        assert_eq!(
            plan.active_addresses(BlockPosition(107)),
            filters
                .iter()
                .map(|filter| filter.address.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(plan.filters(), filters);
    }

    #[test]
    fn checkpoint_movement_keeps_the_captured_address_selection() {
        let admission = Arc::new(ScopeAdmission::new());
        let mut filters = vec![filter("selected", 103), filter("future", 107)];
        let selected = filters[0].address.clone();
        let mut plan = admission
            .plan(Some(block(100)), || Ok(filters.clone()))
            .expect("captured plan");
        filters[0].start_position = BlockPosition(0);
        filters.push(filter("registered-later", 101));

        for position in [107, 100] {
            plan.advance(block(position));
            assert_eq!(plan.checkpoint(), Some(&block(position)));
            assert!(plan.active_addresses(BlockPosition(102)).is_empty());
            assert_eq!(
                plan.active_addresses(BlockPosition(103)),
                vec![selected.clone()]
            );
        }
    }

    #[test]
    fn commit_wins_and_publication_uses_the_committed_checkpoint() {
        let admission = Arc::new(ScopeAdmission::new());
        let plan = admission
            .plan(None, || Ok(Vec::new()))
            .expect("initial plan");
        let mut commit = plan.begin().expect("commit permit");
        commit.start();

        let mut publication = Box::pin(admission.publication(None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            publication.as_mut().poll(&mut context),
            Poll::Pending
        ));

        commit
            .complete(Some(block(7)))
            .expect("checkpoint publication");
        let publication = block_on(publication).expect("waiting publication");
        assert_eq!(publication.checkpoint(), Some(&block(7)));
    }

    #[test]
    fn publication_wins_and_invalidates_the_older_sync_plan() {
        let admission = Arc::new(ScopeAdmission::new());
        let plan = admission
            .plan(Some(block(7)), || Ok(Vec::new()))
            .expect("initial plan");
        let publication =
            block_on(admission.publication(Some(block(7)))).expect("publication permit");

        assert_eq!(
            commit_error(plan.begin(), "publication must block commit").kind,
            IndexErrorKind::Conflict
        );
        publication.complete().expect("publish filter revision");
        assert_eq!(
            commit_error(plan.begin(), "old revision must not commit").kind,
            IndexErrorKind::Conflict
        );
    }

    #[test]
    fn dropped_started_commit_requires_checkpoint_reload() {
        let admission = Arc::new(ScopeAdmission::new());
        let stale = admission
            .plan(Some(block(7)), || Ok(Vec::new()))
            .expect("initial plan");
        let mut commit = stale.begin().expect("commit permit");
        commit.start();
        drop(commit);

        assert_eq!(
            commit_error(stale.begin(), "recovery must block stale plan").kind,
            IndexErrorKind::Conflict
        );
        let reloaded = admission
            .plan(Some(block(8)), || Ok(Vec::new()))
            .expect("repository reload repairs admission");
        reloaded
            .begin()
            .expect("reloaded plan can commit")
            .complete(Some(block(8)))
            .expect("complete reloaded plan");
    }

    #[test]
    fn dropped_publication_releases_waiters_without_changing_revision() {
        let admission = Arc::new(ScopeAdmission::new());
        let first = block_on(admission.publication(None)).expect("first publication");
        let mut second = Box::pin(admission.publication(None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));

        drop(first);
        let second = block_on(second).expect("drop wakes next publication");
        second.complete().expect("second publication completes");
        admission
            .plan(None, || Ok(Vec::new()))
            .expect("admission remains usable");
    }

    #[test]
    fn plan_captures_filters_without_holding_the_admission_lock() {
        let admission = Arc::new(ScopeAdmission::new());
        let inspected = admission.clone();

        admission
            .plan(None, || {
                assert!(inspected.state.try_lock().is_ok());
                Ok(Vec::new())
            })
            .expect("unlocked filter capture");
    }
}
