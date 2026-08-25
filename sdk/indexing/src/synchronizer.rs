use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    AddressFilter, BlockAddition, BlockHeight, BlockInterpreter, BlockObservation, BlockOutcome,
    BlockRef, BlockSelector, BlockSource, Blocks, IndexError, IndexErrorKind, IndexedBlock,
    Observer, SyncPhase, SyncStatus,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncConfig {
    pub(crate) scope: crate::IndexScope,
    pub(crate) minimum_confirmations: u64,
    pub(crate) reorg_retention: u64,
    pub(crate) batch_size: usize,
}

impl SyncConfig {
    pub fn new(
        scope: crate::IndexScope,
        minimum_confirmations: u64,
        reorg_retention: u64,
        batch_size: usize,
    ) -> Result<Self, IndexError> {
        if minimum_confirmations == 0 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "minimum confirmations must be greater than zero",
                false,
            ));
        }
        if reorg_retention == 0 || batch_size == 0 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "rollback retention and batch size must be greater than zero",
                false,
            ));
        }
        Ok(Self {
            scope,
            minimum_confirmations,
            reorg_retention,
            batch_size,
        })
    }
}

struct RunningGuard<'a>(&'a AtomicBool);
impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) fn anchor_height(filters: &[AddressFilter], target: BlockHeight) -> Option<BlockHeight> {
    match filters.iter().map(|filter| filter.start_height).min() {
        None => Some(target),
        Some(BlockHeight(0)) => None,
        Some(BlockHeight(height)) => Some(target.min(BlockHeight(height - 1))),
    }
}

/// Synchronizes caller-selected addresses without owning their lifecycle.
pub(crate) struct Synchronizer<S, I, R> {
    source: S,
    interpreter: I,
    repository: R,
    config: SyncConfig,
    running: AtomicBool,
    observer: Option<Arc<dyn Observer>>,
}

impl<S, I, R> Synchronizer<S, I, R> {
    #[must_use]
    pub(crate) fn new(source: S, interpreter: I, repository: R, config: SyncConfig) -> Self {
        Self {
            source,
            interpreter,
            repository,
            config,
            running: AtomicBool::new(false),
            observer: None,
        }
    }

    /// Notifies `observer` after each block this synchronizer commits.
    pub(crate) fn observe(&mut self, observer: Arc<dyn Observer>) {
        self.observer = Some(observer);
    }
}

impl<S, I, R> Synchronizer<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: Blocks,
{
    pub(crate) async fn sync(
        &self,
        selection: &dyn crate::FilterSource,
    ) -> Result<SyncStatus, IndexError> {
        let _guard = self.enter()?;
        // A malformed selection is a programming error, so it is rejected
        // before any source I/O: the caller should not need a reachable node to
        // find out.
        self.validate(&selection.filters()?)?;

        let observed_tip = self.source.tip().await.map_err(IndexError::from)?;

        // Read the selection again now that the tip is known, and index against
        // this newer set. An address registered between the two reads has a
        // birthday of the current checkpoint plus one, which covers the blocks
        // this pass is about to apply; indexing them against the earlier set
        // would skip that address for blocks it was registered to cover, and
        // the checkpoint moves past them for good.
        let filters = selection.filters()?;
        self.validate(&filters)?;
        let mut checkpoint = self
            .repository
            .get(BlockSelector::Tip(self.config.scope.clone()))
            .await?;
        if let Some(local) = checkpoint.clone()
            && self
                .source
                .canonical_hash(local.height)
                .await
                .map_err(IndexError::from)?
                .as_ref()
                != Some(&local.hash)
        {
            checkpoint = self.reconcile(local).await?;
        }

        let target = observed_tip.height;
        let mut applied = 0_usize;
        if checkpoint.is_none()
            && let Some(height) = anchor_height(&filters, target)
            && applied < self.config.batch_size
        {
            checkpoint = Some(self.apply(height, &[], None).await?);
            applied += 1;
        }
        while applied < self.config.batch_size {
            let height = match &checkpoint {
                Some(value) => BlockHeight(value.height.0.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "checkpoint height is exhausted",
                        false,
                    )
                })?),
                None => BlockHeight(0),
            };
            if height > target {
                break;
            }
            let addresses = filters
                .iter()
                .filter(|item| item.start_height <= height)
                .map(|item| item.address.clone())
                .collect::<Vec<_>>();
            checkpoint = Some(self.apply(height, &addresses, checkpoint).await?);
            applied += 1;
        }
        let caught_up = checkpoint
            .as_ref()
            .is_some_and(|value| value.height >= observed_tip.height)
            || checkpoint.is_none()
                && filters
                    .iter()
                    .map(|filter| filter.start_height)
                    .min()
                    .is_some_and(|height| observed_tip.height < height);
        Ok(SyncStatus {
            scope: self.config.scope.clone(),
            checkpoint,
            observed_tip: Some(observed_tip),
            phase: if caught_up {
                SyncPhase::Ready
            } else {
                SyncPhase::CatchingUp
            },
        })
    }

    fn enter(&self) -> Result<RunningGuard<'_>, IndexError> {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                IndexError::new(
                    IndexErrorKind::Conflict,
                    "synchronization is already running",
                    true,
                )
            })?;
        Ok(RunningGuard(&self.running))
    }

    fn validate(&self, filters: &[AddressFilter]) -> Result<(), IndexError> {
        if filters
            .iter()
            .any(|item| !item.address.belongs_to(&self.config.scope))
        {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "address filter belongs to another scope",
                false,
            ));
        }
        let mut addresses = BTreeSet::new();
        if filters
            .iter()
            .any(|item| item.address.value.is_empty() || !addresses.insert(&item.address))
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "address filters must be non-empty and unique",
                false,
            ));
        }
        Ok(())
    }

    async fn apply(
        &self,
        height: BlockHeight,
        addresses: &[crate::CanonicalAddress],
        checkpoint: Option<BlockRef>,
    ) -> Result<BlockRef, IndexError> {
        let source_block = self
            .source
            .block_at(height)
            .await
            .map_err(IndexError::from)?;
        let block = source_block.block_ref();
        if block.height != height
            || checkpoint
                .as_ref()
                .is_some_and(|tip| block.parent_hash.as_ref() != Some(&tip.hash))
        {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "source block does not connect to the checkpoint",
                true,
            ));
        }
        let interpreted = self.interpreter.inspect(&source_block, addresses)?;
        self.validate_block(&block, &interpreted)?;
        if self
            .source
            .canonical_hash(height)
            .await
            .map_err(IndexError::from)?
            .as_ref()
            != Some(&block.hash)
        {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "source block changed before commit",
                true,
            ));
        }
        let addition = BlockAddition::new(
            self.config.scope.clone(),
            checkpoint,
            self.config.reorg_retention,
            interpreted,
        )?;
        // Storage consumes the addition, so keep the facts an observer needs —
        // but only when one is actually listening.
        let observed = self
            .observer
            .as_ref()
            .map(|_| addition.transactions().to_vec());
        let outcome = self.repository.add(addition).await?;
        if outcome == BlockOutcome::Applied
            && let (Some(observer), Some(transactions)) = (&self.observer, observed)
        {
            observer
                .observed(BlockObservation {
                    scope: self.config.scope.clone(),
                    block: block.clone(),
                    transactions,
                })
                .await;
        }
        Ok(block)
    }

    fn validate_block(
        &self,
        source: &BlockRef,
        block: &crate::InterpretedBlock,
    ) -> Result<(), IndexError> {
        if &block.block != source {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "interpreter changed the block reference",
                false,
            ));
        }
        Ok(())
    }

    async fn reconcile(&self, tip: BlockRef) -> Result<Option<BlockRef>, IndexError> {
        let oldest = BlockHeight(tip.height.0.saturating_sub(self.config.reorg_retention));
        let mut height = tip.height;
        let ancestor = loop {
            let local = self
                .repository
                .get(BlockSelector::Height {
                    scope: self.config.scope.clone(),
                    height,
                })
                .await?;
            let remote = self
                .source
                .canonical_hash(height)
                .await
                .map_err(IndexError::from)?;
            if let (Some(local), Some(remote)) = (local, remote)
                && local.hash == remote
            {
                break Some(local);
            }
            if height == oldest {
                break None;
            }
            height = BlockHeight(height.0.saturating_sub(1));
        }
        .ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::ReorgTooDeep,
                "reorg exceeds rollback journal; rescan required",
                false,
            )
        })?;

        let mut current = Some(tip);
        while current
            .as_ref()
            .is_some_and(|value| value.height > ancestor.height)
        {
            let expected_tip = current.clone().ok_or_else(|| {
                IndexError::new(IndexErrorKind::Store, "checkpoint disappeared", false)
            })?;
            current = self
                .repository
                .remove(self.config.scope.clone(), expected_tip)
                .await?;
        }
        Ok(current)
    }
}
