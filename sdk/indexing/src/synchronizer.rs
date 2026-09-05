use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    AddressFilter, BlockAddition, BlockHeight, BlockInterpreter, BlockObservation, BlockOutcome,
    BlockParent, BlockPosition, BlockRef, BlockSelector, BlockSource, Blocks, IndexError,
    IndexErrorKind, IndexedBlock, Observer, SyncPhase, SyncStatus,
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

pub(super) fn earliest_position(filters: &[AddressFilter]) -> Option<BlockPosition> {
    filters.iter().map(|filter| filter.start_position).min()
}

fn cannot_connect(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::CannotConnect, message, true)
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
        let mut checkpoint = self
            .repository
            .get(BlockSelector::Tip(self.config.scope.clone()))
            .await?;
        let mut plan = selection.plan(&self.config.scope, checkpoint.clone())?;
        self.validate(plan.filters())?;
        if let Some(local) = checkpoint.clone()
            && self
                .source
                .canonical_at(local.position)
                .await
                .map_err(IndexError::from)?
                .as_ref()
                != Some(&local)
        {
            checkpoint = self.reconcile(local, &mut plan).await?;
        }
        let mut applied = 0_usize;
        if checkpoint.is_none() {
            let birthday = earliest_position(plan.filters());
            if birthday.is_none_or(|position| position > observed_tip.position) {
                let anchor = self
                    .one_block(observed_tip.position, observed_tip.position)
                    .await?;
                checkpoint = Some(self.apply(anchor, &[], None, &mut plan).await?);
                applied += 1;
            } else if let Some(start) = birthday {
                let first = self.one_block(start, observed_tip.position).await?;
                let first_ref = first.block_ref();
                if let Some(parent) = &first_ref.parent {
                    let anchor = self.one_block(parent.position, parent.position).await?;
                    let anchor_ref = anchor.block_ref();
                    if anchor_ref.position != parent.position || anchor_ref.hash != parent.hash {
                        return Err(cannot_connect(
                            "birthday anchor does not match the first block parent",
                        ));
                    }
                    checkpoint = Some(self.apply(anchor, &[], None, &mut plan).await?);
                    applied += 1;
                }
                if applied < self.config.batch_size {
                    let addresses = plan.active_addresses(first_ref.position);
                    checkpoint = Some(self.apply(first, &addresses, checkpoint, &mut plan).await?);
                    applied += 1;
                }
            }
        }

        if applied < self.config.batch_size
            && let Some(tip) = &checkpoint
            && tip.position < observed_tip.position
        {
            let start = tip.position.checked_successor().ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "checkpoint position is exhausted",
                    false,
                )
            })?;
            let remaining = self.config.batch_size - applied;
            let blocks = self
                .fetch_blocks(start, observed_tip.position, remaining)
                .await?;
            if blocks.is_empty() {
                return Err(cannot_connect(
                    "source returned no produced block before its observed tip",
                ));
            }
            for source_block in blocks {
                let position = source_block.block_ref().position;
                let addresses = plan.active_addresses(position);
                checkpoint = Some(
                    self.apply(source_block, &addresses, checkpoint, &mut plan)
                        .await?,
                );
            }
        }
        let caught_up = checkpoint
            .as_ref()
            .is_some_and(|value| value.position >= observed_tip.position);
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

    async fn one_block(
        &self,
        start: BlockPosition,
        end: BlockPosition,
    ) -> Result<I::Block, IndexError> {
        let blocks = self.fetch_blocks(start, end, 1).await?;
        let [block]: [I::Block; 1] = blocks
            .try_into()
            .map_err(|_| cannot_connect("source did not return the required produced block"))?;
        Ok(block)
    }

    async fn fetch_blocks(
        &self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> Result<Vec<I::Block>, IndexError> {
        let blocks = self
            .source
            .blocks(start, end, limit)
            .await
            .map_err(IndexError::from)?;
        if blocks.len() > limit {
            return Err(cannot_connect("source exceeded the returned-block limit"));
        }
        let mut previous = None;
        for block in &blocks {
            let position = block.block_ref().position;
            if position < start
                || position > end
                || previous.is_some_and(|previous| position <= previous)
            {
                return Err(cannot_connect(
                    "source blocks are outside the range or not strictly increasing",
                ));
            }
            previous = Some(position);
        }
        Ok(blocks)
    }

    async fn apply(
        &self,
        source_block: I::Block,
        addresses: &[crate::CanonicalAddress],
        checkpoint: Option<BlockRef>,
        plan: &mut crate::SyncPlan,
    ) -> Result<BlockRef, IndexError> {
        let block = source_block.block_ref();
        if block.position == BlockPosition(0) {
            if block.parent.is_some() {
                return Err(cannot_connect("genesis block must not have a parent"));
            }
        } else if block.parent.is_none() {
            return Err(cannot_connect("non-genesis block is missing its parent"));
        }
        if let Some(tip) = &checkpoint {
            let expected_height = tip.height.checked_successor().ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "checkpoint height is exhausted",
                    false,
                )
            })?;
            if block.position <= tip.position
                || block.height != expected_height
                || block.parent.as_ref()
                    != Some(&BlockParent {
                        position: tip.position,
                        hash: tip.hash.clone(),
                    })
            {
                return Err(cannot_connect(
                    "source block does not connect to the checkpoint",
                ));
            }
        }
        let interpreted = self.interpreter.inspect(&source_block, addresses)?;
        self.validate_block(&block, &interpreted)?;
        if self
            .source
            .canonical_at(block.position)
            .await
            .map_err(IndexError::from)?
            .as_ref()
            != Some(&block)
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
        let mut permit = plan.begin()?;
        permit.start();
        let outcome = self.repository.add(addition).await?;
        permit.complete(Some(block.clone()))?;
        plan.advance(block.clone());
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

    async fn reconcile(
        &self,
        tip: BlockRef,
        plan: &mut crate::SyncPlan,
    ) -> Result<Option<BlockRef>, IndexError> {
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
            let remote = match &local {
                Some(block) => self
                    .source
                    .canonical_at(block.position)
                    .await
                    .map_err(IndexError::from)?,
                None => None,
            };
            if let (Some(local), Some(remote)) = (local, remote)
                && local == remote
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

        let mut permit = plan.begin()?;
        permit.start();
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
            permit.persist(current.clone())?;
        }
        permit.complete(current.clone())?;
        if let Some(checkpoint) = &current {
            plan.advance(checkpoint.clone());
        }
        Ok(current)
    }
}
