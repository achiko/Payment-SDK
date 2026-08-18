use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    BlockHeight, BlockInterpreter, BlockRef, BlockSource, BlockStore, CanonicalStore, CommitBlock,
    ConfirmationPolicy, IndexError, IndexErrorKind, IndexedBlock, RevertTip, StatusStore,
    SyncPhase, SyncRequest, SyncStatus, WatchStore,
};

pub const DEFAULT_CONFIRMATIONS: u64 = 12;
pub const DEFAULT_REORG_RETENTION: u64 = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncConfig {
    pub scope: crate::IndexScope,
    pub bootstrap_height: BlockHeight,
    pub confirmation_policy: ConfirmationPolicy,
    pub reorg_retention: u64,
}

impl SyncConfig {
    pub fn new(
        scope: crate::IndexScope,
        bootstrap_height: BlockHeight,
        confirmation_policy: ConfirmationPolicy,
        reorg_retention: u64,
    ) -> Result<Self, IndexError> {
        if confirmation_policy.minimum_confirmations == 0 {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "confirmation depth must be greater than zero",
                false,
            ));
        }
        if confirmation_policy.require_chain_finality {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "ordered synchronization does not consume a chain-finality source",
                false,
            ));
        }
        if reorg_retention == 0 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "reorg retention must be greater than zero",
                false,
            ));
        }

        Ok(Self {
            scope,
            bootstrap_height,
            confirmation_policy,
            reorg_retention,
        })
    }

    #[must_use]
    pub fn defaults(scope: crate::IndexScope, bootstrap_height: BlockHeight) -> Self {
        Self {
            scope,
            bootstrap_height,
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: DEFAULT_CONFIRMATIONS,
                require_chain_finality: false,
            },
            reorg_retention: DEFAULT_REORG_RETENTION,
        }
    }
}

struct RunningGuard<'a>(&'a AtomicBool);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Ordered, polling-based synchronization for one chain and network.
pub struct Synchronizer<S, I, R> {
    source: S,
    interpreter: I,
    repository: R,
    config: SyncConfig,
    running: AtomicBool,
}

impl<S, I, R> Synchronizer<S, I, R> {
    #[must_use]
    pub fn new(source: S, interpreter: I, repository: R, config: SyncConfig) -> Self {
        Self {
            source,
            interpreter,
            repository,
            config,
            running: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn repository(&self) -> &R {
        &self.repository
    }
}

impl<S, I, R> Synchronizer<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter<
            Target = crate::WatchSelector,
            Effect = crate::IndexChanges,
            Undo = crate::IndexUndo,
        >,
    R: CanonicalStore + WatchStore + BlockStore + StatusStore,
{
    pub async fn sync(&self, request: SyncRequest) -> Result<SyncStatus, IndexError> {
        let _guard = self.enter()?;
        self.sync_inner(request).await
    }

    pub async fn status(&self, scope: &crate::IndexScope) -> Result<SyncStatus, IndexError> {
        self.validate_scope(scope)?;
        Ok(self.repository.status(scope).await?.unwrap_or_else(|| {
            SyncStatus::starting(scope.clone(), self.config.confirmation_policy)
        }))
    }

    fn enter(&self) -> Result<RunningGuard<'_>, IndexError> {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                IndexError::new(
                    IndexErrorKind::Conflict,
                    "a synchronization run is already active",
                    true,
                )
            })?;
        Ok(RunningGuard(&self.running))
    }

    fn validate_scope(&self, scope: &crate::IndexScope) -> Result<(), IndexError> {
        if scope != &self.config.scope {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "request scope does not match the synchronizer scope",
                false,
            ));
        }
        Ok(())
    }

    fn validate_interpreted(
        &self,
        source_ref: &BlockRef,
        interpreted: &crate::InterpretedBlock<I::Effect, I::Undo>,
    ) -> Result<(), IndexError> {
        if &interpreted.block != source_ref {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "interpreter changed the source block reference",
                false,
            ));
        }
        for draft in &interpreted.drafts {
            if draft.scope != self.config.scope
                || !draft.transaction_id.belongs_to(&self.config.scope)
                || draft.movements.iter().any(|movement| {
                    movement
                        .from()
                        .is_some_and(|address| !address.belongs_to(&self.config.scope))
                        || movement
                            .to()
                            .is_some_and(|address| !address.belongs_to(&self.config.scope))
                })
                || draft
                    .fee
                    .as_ref()
                    .and_then(|fee| fee.payer.as_ref())
                    .is_some_and(|address| !address.belongs_to(&self.config.scope))
            {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "interpreted observation belongs to a different scope",
                    false,
                ));
            }
            if matches!(draft.status, crate::ObservationDraftStatus::Failed { .. })
                && !draft.movements.is_empty()
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "failed receipt draft contains value movements",
                    false,
                ));
            }
        }
        Ok(())
    }

    async fn sync_inner(&self, request: SyncRequest) -> Result<SyncStatus, IndexError> {
        self.validate_scope(&request.scope)?;
        if request
            .through
            .is_some_and(|through| through < self.config.bootstrap_height)
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "requested terminal height is below the configured bootstrap height",
                false,
            ));
        }

        let durable = self
            .repository
            .status(&self.config.scope)
            .await?
            .unwrap_or_else(|| {
                SyncStatus::starting(self.config.scope.clone(), self.config.confirmation_policy)
            });
        if durable.phase == SyncPhase::Halted {
            return Ok(durable);
        }

        let mut checkpoint = self.repository.checkpoint(&self.config.scope).await?;
        self.publish(SyncPhase::Reconciling, checkpoint.clone(), None, None)
            .await?;
        let observed_tip = self.source.tip().await.map_err(IndexError::from)?;
        self.publish(
            SyncPhase::Reconciling,
            checkpoint.clone(),
            Some(observed_tip.clone()),
            None,
        )
        .await?;

        if let Some(local_tip) = checkpoint.clone() {
            match self
                .source
                .canonical_hash(local_tip.height)
                .await
                .map_err(IndexError::from)?
            {
                Some(remote_hash) if remote_hash == local_tip.hash => {}
                Some(_) => {
                    checkpoint = self
                        .revert_to_common_ancestor(local_tip, &observed_tip)
                        .await?;
                }
                None => {
                    return Err(IndexError::new(
                        IndexErrorKind::Source,
                        "source does not expose the persisted checkpoint height",
                        true,
                    ));
                }
            }
        }

        let target = request.through.map_or(observed_tip.height, |height| {
            height.min(observed_tip.height)
        });
        let max_blocks = request.max_blocks.unwrap_or(usize::MAX);
        let mut applied = 0_usize;
        let mut watch_conflicts = 0_u8;

        while applied < max_blocks {
            let next_height = match checkpoint.as_ref() {
                Some(current) => BlockHeight(current.height.0.checked_add(1).ok_or_else(|| {
                    IndexError::new(IndexErrorKind::InvalidBlock, "checkpoint overflow", false)
                })?),
                None => self.config.bootstrap_height,
            };
            if next_height > target {
                break;
            }

            let watches = self
                .repository
                .watches_at(&self.config.scope, next_height)
                .await?;
            let source_block = self
                .source
                .block_at(next_height)
                .await
                .map_err(IndexError::from)?;
            let source_ref = source_block.block_ref();
            if source_ref.height != next_height {
                return self
                    .halt(
                        checkpoint,
                        Some(observed_tip),
                        "source returned the wrong height",
                    )
                    .await;
            }
            if checkpoint
                .as_ref()
                .is_some_and(|current| source_ref.parent_hash.as_ref() != Some(&current.hash))
            {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "candidate block does not connect to the checkpoint",
                    true,
                ));
            }

            let interpreted = self.interpreter.inspect(&source_block, &watches.watches)?;
            if let Err(error) = self.validate_interpreted(&source_ref, &interpreted) {
                return self
                    .halt(checkpoint, Some(observed_tip), &error.message)
                    .await;
            }
            let canonical_hash = self
                .source
                .canonical_hash(next_height)
                .await
                .map_err(IndexError::from)?;
            if canonical_hash.as_ref() != Some(&source_ref.hash) {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "candidate block changed before commit",
                    true,
                ));
            }

            let command = CommitBlock {
                scope: self.config.scope.clone(),
                expected_checkpoint: checkpoint.clone(),
                expected_watch_version: watches.version,
                confirmation_policy: self.config.confirmation_policy,
                reorg_retention: self.config.reorg_retention,
                block: interpreted,
            };
            let result = match self.repository.load_commit(&command).await {
                Ok(context) => match crate::plan_commit(&command, &context) {
                    Ok(plan) => self.repository.commit_block(plan).await,
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            match result {
                Ok(_) => {
                    checkpoint = Some(source_ref);
                    applied += 1;
                    watch_conflicts = 0;
                    self.publish(
                        SyncPhase::CatchingUp,
                        checkpoint.clone(),
                        Some(observed_tip.clone()),
                        None,
                    )
                    .await?;
                }
                Err(error) if error.kind == IndexErrorKind::Conflict && watch_conflicts < 4 => {
                    watch_conflicts += 1;
                }
                Err(error) => return Err(error),
            }
        }

        let caught_up = checkpoint
            .as_ref()
            .is_some_and(|current| current.height >= observed_tip.height)
            || (checkpoint.is_none() && observed_tip.height < self.config.bootstrap_height);
        self.publish(
            if caught_up {
                SyncPhase::Ready
            } else {
                SyncPhase::CatchingUp
            },
            checkpoint,
            Some(observed_tip),
            None,
        )
        .await
    }

    async fn revert_to_common_ancestor(
        &self,
        checkpoint: BlockRef,
        observed_tip: &BlockRef,
    ) -> Result<Option<BlockRef>, IndexError> {
        let oldest = BlockHeight(
            checkpoint
                .height
                .0
                .saturating_sub(self.config.reorg_retention),
        );
        let mut height = checkpoint.height;
        let ancestor = loop {
            let local = self
                .repository
                .canonical_block(&self.config.scope, height)
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
        };

        let Some(ancestor) = ancestor else {
            let message =
                "reorg exceeds retained undo history; delete the index database and resync";
            self.publish(
                SyncPhase::Halted,
                Some(checkpoint),
                Some(observed_tip.clone()),
                Some(message.to_owned()),
            )
            .await?;
            return Err(IndexError::new(
                IndexErrorKind::ReorgBeyondRetention,
                message,
                false,
            ));
        };

        let mut current = Some(checkpoint);
        while current
            .as_ref()
            .is_some_and(|tip| tip.height > ancestor.height)
        {
            self.publish(
                SyncPhase::Reverting,
                current.clone(),
                Some(observed_tip.clone()),
                None,
            )
            .await?;
            let expected_tip = current.clone().ok_or_else(|| {
                IndexError::new(IndexErrorKind::Store, "checkpoint disappeared", false)
            })?;
            let command = RevertTip {
                scope: self.config.scope.clone(),
                expected_tip,
            };
            let context = self.repository.load_revert(&command).await?;
            let decision = crate::plan_revert(&command, &context)?;
            if let Some(plan) = decision.plan {
                self.repository.save_revert(plan).await?;
            }
            current = decision.checkpoint;
        }
        Ok(current)
    }

    async fn publish(
        &self,
        phase: SyncPhase,
        checkpoint: Option<BlockRef>,
        observed_tip: Option<BlockRef>,
        halted_reason: Option<String>,
    ) -> Result<SyncStatus, IndexError> {
        let status = SyncStatus {
            scope: self.config.scope.clone(),
            checkpoint,
            observed_tip,
            confirmation_policy: self.config.confirmation_policy,
            phase,
            halted_reason,
        };
        self.repository.set_status(status.clone()).await?;
        Ok(status)
    }

    async fn halt(
        &self,
        checkpoint: Option<BlockRef>,
        observed_tip: Option<BlockRef>,
        message: &str,
    ) -> Result<SyncStatus, IndexError> {
        self.publish(
            SyncPhase::Halted,
            checkpoint,
            observed_tip,
            Some(message.to_owned()),
        )
        .await
    }
}
