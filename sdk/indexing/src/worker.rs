use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use crate::{
    BlockHeight, BlockInterpreter, BlockRef, BlockSource, CanonicalReader, ChainWriter,
    CommitBlock, CommitObservation, CommitStatus, IndexError, IndexErrorKind, IndexedBlock,
    NoopWorkerObserver, RebuildReason, ReorgDepth, ReorgObservation, RevertOutcome, RevertTip,
    StatusStore, SyncPhase, SyncRequest, SyncStatus, WatchReader, WorkerObserver,
};

mod config;
mod contract;
mod validation;

pub use config::{SyncConfig, V1_CONFIRMATION_DEPTH, V1_REORG_RETENTION};
use contract::RunningGuard;
/// HTTP-authoritative, one-scope ordered synchronization worker.
///
/// WebSocket notifications intentionally do not enter this type. They may only
/// wake a caller that invokes `sync`; every decision is made from `BlockSource`
/// tip, canonical-hash, and full-block reads.
pub struct SyncWorker<S, I, R> {
    source: S,
    interpreter: I,
    repository: R,
    config: SyncConfig,
    observer: Arc<dyn WorkerObserver>,
    running: AtomicBool,
}

impl<S, I, R> SyncWorker<S, I, R> {
    #[must_use]
    pub fn new(source: S, interpreter: I, repository: R, config: SyncConfig) -> Self {
        Self {
            source,
            interpreter,
            repository,
            config,
            observer: Arc::new(NoopWorkerObserver::Ignore),
            running: AtomicBool::new(false),
        }
    }

    /// Installs the process-owned adapter for ordered-sync observations.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn WorkerObserver>) -> Self {
        self.observer = observer;
        self
    }

    #[must_use]
    pub fn repository(&self) -> &R {
        &self.repository
    }
}

impl<S, I, R> SyncWorker<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: CanonicalReader<Target = I::Target, Effect = I::Effect, Undo = I::Undo>
        + WatchReader
        + ChainWriter
        + StatusStore,
{
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

        // Recovery-required phases are durable, externally observable service
        // states. Normal polling must not try to overwrite them with
        // `Reconciling`; only an explicit rebuild may clear them.
        let durable_status = self.repository.status(&self.config.scope).await?;
        if matches!(
            durable_status.phase,
            SyncPhase::RebuildRequired | SyncPhase::Halted
        ) {
            return Ok(durable_status);
        }

        let mut checkpoint = self.repository.checkpoint(&self.config.scope).await?;
        // Fail closed before the first external read. If the source is down,
        // a previously persisted Ready phase must not remain queryable while
        // this reconciliation attempt waits to retry.
        self.publish_status(SyncPhase::Reconciling, checkpoint.clone(), None, None, None)
            .await?;
        let observed_tip = self.source.tip().await.map_err(IndexError::from)?;
        self.publish_status(
            SyncPhase::Reconciling,
            checkpoint.clone(),
            Some(observed_tip.clone()),
            None,
            None,
        )
        .await?;

        let mut replaying = false;
        if let Some(local_tip) = checkpoint.clone() {
            match self
                .source
                .canonical_hash(local_tip.height)
                .await
                .map_err(IndexError::from)?
            {
                Some(remote_hash) if remote_hash == local_tip.hash => {}
                Some(_) => {
                    let reconciliation =
                        self.recover_reorg(local_tip, observed_tip.clone()).await?;
                    if let Some(status) = reconciliation.rebuild_required {
                        return Ok(status);
                    }
                    checkpoint = reconciliation.checkpoint;
                    replaying = true;
                }
                None => {
                    return Err(IndexError::new(
                        IndexErrorKind::Source,
                        "authoritative source does not currently expose the persisted checkpoint height",
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
                    IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "checkpoint height overflow",
                        false,
                    )
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
                        "source returned a block at an unexpected height",
                    )
                    .await;
            }
            if checkpoint
                .as_ref()
                .is_some_and(|current| source_ref.parent_hash.as_ref() != Some(&current.hash))
            {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "candidate block does not connect to the persisted checkpoint",
                    true,
                ));
            }

            let interpreted = match self.interpreter.inspect(&source_block, &watches.watches) {
                Ok(interpreted) => interpreted,
                Err(error) if !error.retryable => {
                    return self
                        .halt_with_error(checkpoint, Some(observed_tip), error)
                        .await;
                }
                Err(error) => return Err(error),
            };
            if let Err(error) = self.validate_interpreted(&source_ref, &interpreted) {
                return self
                    .halt_with_error(checkpoint, Some(observed_tip), error)
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
                    "candidate block changed before its atomic commit",
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
            let started_at = Instant::now();
            let result = self.repository.commit_block(command).await;
            let elapsed = started_at.elapsed();
            let outcome = match &result {
                Ok(outcome) => CommitStatus::Success(*outcome),
                Err(error) => CommitStatus::Failure {
                    kind: error.kind,
                    retryable: error.retryable,
                },
            };
            self.observer.block_commit(CommitObservation {
                scope: self.config.scope.clone(),
                block: source_ref.clone(),
                elapsed,
                outcome,
            });
            match result {
                Ok(_) => {
                    checkpoint = Some(source_ref);
                    applied += 1;
                    watch_conflicts = 0;
                    let phase = if replaying {
                        SyncPhase::Replaying
                    } else {
                        SyncPhase::CatchingUp
                    };
                    self.publish_status(
                        phase,
                        checkpoint.clone(),
                        Some(observed_tip.clone()),
                        None,
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
        let phase = if caught_up {
            SyncPhase::Ready
        } else {
            SyncPhase::CatchingUp
        };
        self.publish_status(phase, checkpoint, Some(observed_tip), None, None)
            .await
    }

    async fn recover_reorg(
        &self,
        checkpoint: BlockRef,
        observed_tip: BlockRef,
    ) -> Result<Reconciliation, IndexError> {
        let oldest_retained = BlockHeight(
            checkpoint
                .height
                .0
                .saturating_sub(self.config.reorg_retention),
        );
        let mut candidate_height = checkpoint.height;
        let ancestor = loop {
            let local = self
                .repository
                .canonical_block(&self.config.scope, candidate_height)
                .await?;
            let remote = self
                .source
                .canonical_hash(candidate_height)
                .await
                .map_err(IndexError::from)?;
            if let (Some(local), Some(remote)) = (local, remote) {
                if local.hash == remote {
                    break Some(local);
                }
            }
            if candidate_height == oldest_retained {
                break None;
            }
            candidate_height = BlockHeight(candidate_height.0.saturating_sub(1));
        };

        let Some(ancestor) = ancestor else {
            self.observer.reorg_detected(ReorgObservation {
                scope: self.config.scope.clone(),
                previous_tip: checkpoint.clone(),
                depth: ReorgDepth::BeyondRetention {
                    minimum_depth: checkpoint
                        .height
                        .0
                        .saturating_sub(oldest_retained.0)
                        .saturating_add(1),
                    oldest_retained,
                },
            });
            let reason = RebuildReason {
                checkpoint: checkpoint.clone(),
                oldest_retained,
                message: format!(
                    "no common ancestor was found in the retained {}-block window",
                    self.config.reorg_retention
                ),
            };
            let status = self
                .publish_status(
                    SyncPhase::RebuildRequired,
                    Some(checkpoint),
                    Some(observed_tip),
                    Some(reason),
                    None,
                )
                .await?;
            return Ok(Reconciliation {
                checkpoint: status.checkpoint.clone(),
                rebuild_required: Some(status),
            });
        };

        let depth = checkpoint
            .height
            .0
            .checked_sub(ancestor.height.0)
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "common ancestor is above the persisted checkpoint",
                    false,
                )
            })?;
        self.observer.reorg_detected(ReorgObservation {
            scope: self.config.scope.clone(),
            previous_tip: checkpoint.clone(),
            depth: ReorgDepth::Exact {
                depth,
                common_ancestor: ancestor.clone(),
            },
        });

        let mut current = Some(checkpoint);
        while current
            .as_ref()
            .is_some_and(|tip| tip.height > ancestor.height)
        {
            self.publish_status(
                SyncPhase::Reverting,
                current.clone(),
                Some(observed_tip.clone()),
                None,
                None,
            )
            .await?;
            let expected_tip = current.clone().ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "repository lost its checkpoint during reorg recovery",
                    false,
                )
            })?;
            current = match self
                .repository
                .revert_tip(RevertTip {
                    scope: self.config.scope.clone(),
                    expected_tip,
                })
                .await?
            {
                RevertOutcome::Reverted { checkpoint }
                | RevertOutcome::AlreadyReverted { checkpoint } => checkpoint,
            };
        }
        self.publish_status(
            SyncPhase::Replaying,
            current.clone(),
            Some(observed_tip),
            None,
            None,
        )
        .await?;
        Ok(Reconciliation {
            checkpoint: current,
            rebuild_required: None,
        })
    }

    async fn publish_status(
        &self,
        phase: SyncPhase,
        checkpoint: Option<BlockRef>,
        observed_tip: Option<BlockRef>,
        rebuild_reason: Option<RebuildReason>,
        halted_reason: Option<String>,
    ) -> Result<SyncStatus, IndexError> {
        let status = SyncStatus {
            scope: self.config.scope.clone(),
            checkpoint,
            observed_tip,
            confirmation_policy: self.config.confirmation_policy,
            phase,
            rebuild_reason,
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
        self.halt_with_error(
            checkpoint,
            observed_tip,
            IndexError::new(IndexErrorKind::InvalidBlock, message, false),
        )
        .await
    }

    async fn halt_with_error(
        &self,
        checkpoint: Option<BlockRef>,
        observed_tip: Option<BlockRef>,
        error: IndexError,
    ) -> Result<SyncStatus, IndexError> {
        self.publish_status(
            SyncPhase::Halted,
            checkpoint,
            observed_tip,
            None,
            Some(error.message.clone()),
        )
        .await
    }
}

struct Reconciliation {
    checkpoint: Option<BlockRef>,
    rebuild_required: Option<SyncStatus>,
}
