use std::collections::BTreeSet;

use crate::{
    BlockHeight, BlockRef, CanonicalAddress, CommitBlock, CommitContext, CommitPlan,
    ConfirmationPolicy, ConfirmationProof, IndexError, IndexErrorKind, IndexScope,
    ObservationDraft, ObservationDraftStatus, ObservationRevision, ObservationTransition,
    ObservedTransaction, PendingChange, StoredObservation, TransactionRef, TransactionStatus,
    WatchId,
};

#[cfg(test)]
#[path = "planning_test.rs"]
mod tests;

#[doc(hidden)]
pub fn validate_draft(
    draft: &ObservationDraft,
    scope: &IndexScope,
    active: &BTreeSet<WatchId>,
) -> Result<(), IndexError> {
    if draft.scope != *scope || draft.transaction_id.scope != *scope {
        return Err(IndexError::new(
            IndexErrorKind::ScopeMismatch,
            "observation belongs to another scope",
            false,
        ));
    }
    if matches!(draft.status, ObservationDraftStatus::Failed { .. }) && !draft.movements.is_empty()
    {
        return Err(IndexError::new(
            IndexErrorKind::InvalidBlock,
            "failed observation contains movements",
            false,
        ));
    }
    let mut movements = BTreeSet::new();
    for movement in &draft.movements {
        if movement.id().0.is_empty()
            || !movements.insert(movement.id().clone())
            || movement.asset().chain != scope.chain
            || movement.from().is_some_and(|value| value.scope != *scope)
            || movement.to().is_some_and(|value| value.scope != *scope)
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "observation contains an invalid movement",
                false,
            ));
        }
    }
    let mut watches = BTreeSet::new();
    if draft
        .watch_ids
        .iter()
        .any(|id| !watches.insert(id.clone()) || !active.contains(id))
    {
        return Err(IndexError::new(
            IndexErrorKind::InvalidWatch,
            "observation references an unknown watch",
            false,
        ));
    }
    Ok(())
}

#[doc(hidden)]
pub fn observation(
    prior: Option<&ObservedTransaction>,
    prior_watches: &[WatchId],
    scope: &IndexScope,
    transaction_id: &TransactionRef,
    status: TransactionStatus,
    draft: Option<&ObservationDraft>,
    observed_at: u64,
) -> Result<(ObservedTransaction, Vec<WatchId>), IndexError> {
    let revision = prior.map_or(Ok(1), |value| {
        value.revision.0.checked_add(1).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::Store,
                "observation revision is exhausted",
                false,
            )
        })
    })?;
    let mut watches = prior_watches.to_vec();
    if let Some(draft) = draft {
        watches.extend(draft.watch_ids.iter().cloned());
    }
    watches.sort();
    watches.dedup();
    Ok((
        ObservedTransaction {
            scope: scope.clone(),
            transaction_id: transaction_id.clone(),
            revision: ObservationRevision(revision),
            status,
            movements: draft.map_or_else(
                || prior.map_or_else(Vec::new, |v| v.movements.clone()),
                |value| value.movements.clone(),
            ),
            fee: draft.map_or_else(
                || prior.and_then(|v| v.fee.clone()),
                |value| value.fee.clone(),
            ),
            first_seen_at: prior.map_or_else(
                || draft.map_or(observed_at, |v| v.first_seen_at),
                |value| value.first_seen_at,
            ),
            observed_at,
        },
        watches,
    ))
}

#[doc(hidden)]
pub fn addresses(observation: &ObservedTransaction) -> BTreeSet<CanonicalAddress> {
    observation
        .movements
        .iter()
        .flat_map(|movement| {
            movement
                .from()
                .cloned()
                .into_iter()
                .chain(movement.to().cloned())
        })
        .collect()
}

#[doc(hidden)]
pub fn confirmation(
    inclusion: &BlockRef,
    observed: u64,
    tip: &BlockRef,
    policy: ConfirmationPolicy,
) -> Result<Option<TransactionStatus>, IndexError> {
    let depth = tip
        .height
        .0
        .checked_sub(inclusion.height.0)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block tip cannot prove the indexed inclusion",
                false,
            )
        })?;
    if depth <= observed {
        return Ok(None);
    }
    Ok(Some(if depth >= policy.minimum_confirmations {
        TransactionStatus::Confirmed {
            block: inclusion.clone(),
            proof: ConfirmationProof::Depth {
                required: policy.minimum_confirmations,
                observed: depth,
            },
        }
    } else {
        TransactionStatus::Included {
            block: inclusion.clone(),
            confirmations: depth,
        }
    }))
}

/// Decides every semantic consequence of committing a block. Persistence adapters
/// only load [`CommitContext`] and atomically apply the returned plan.
pub fn commit<E: Clone, U: Clone>(
    command: &CommitBlock<E, U>,
    context: &CommitContext,
) -> Result<CommitPlan<E, U>, IndexError> {
    if context.checkpoint != command.expected_checkpoint
        || context.watch_version != command.expected_watch_version
    {
        return Err(IndexError::new(
            IndexErrorKind::Conflict,
            "commit context changed while the block was interpreted",
            true,
        ));
    }
    if let Some(checkpoint) = &context.checkpoint {
        let height = checkpoint.height.0.checked_add(1).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "checkpoint height is exhausted",
                false,
            )
        })?;
        if command.block.block.height != BlockHeight(height)
            || command.block.block.parent_hash.as_ref() != Some(&checkpoint.hash)
        {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "block does not immediately connect to the checkpoint",
                true,
            ));
        }
    }

    let observed_at = command
        .block
        .block
        .timestamp
        .unwrap_or(command.block.block.height.0);
    let mut transitions = std::collections::BTreeMap::new();
    for transaction_id in &context.pending_confirmations {
        let prior = context.observations.get(transaction_id).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::Store,
                "pending confirmation has no observation",
                false,
            )
        })?;
        let (block, confirmations) = match &prior.transaction.status {
            TransactionStatus::Included {
                block,
                confirmations,
            } => (block, *confirmations),
            _ => {
                return Err(IndexError::new(
                    IndexErrorKind::Store,
                    "pending confirmation is not included",
                    false,
                ));
            }
        };
        if let Some(status) = confirmation(
            block,
            confirmations,
            &command.block.block,
            command.confirmation_policy,
        )? {
            let (transaction, watch_ids) = observation(
                Some(&prior.transaction),
                &prior.watch_ids,
                &command.scope,
                transaction_id,
                status,
                None,
                observed_at,
            )?;
            let next = StoredObservation {
                transaction,
                watch_ids,
            };
            transitions.insert(
                transaction_id.clone(),
                ObservationTransition {
                    prior: Some(prior.clone()),
                    prior_addresses: addresses(&prior.transaction),
                    next_addresses: addresses(&next.transaction),
                    next,
                    included_here: false,
                    pending: PendingChange::Remove {
                        inclusion: block.height,
                    },
                },
            );
        }
    }

    let mut drafts = BTreeSet::new();
    for draft in &command.block.drafts {
        validate_draft(draft, &command.scope, &context.active_watches)?;
        if !drafts.insert(draft.transaction_id.clone())
            || transitions.contains_key(&draft.transaction_id)
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block contains a duplicate transaction observation",
                false,
            ));
        }
        let prior = context.observations.get(&draft.transaction_id);
        if prior.is_some_and(|value| {
            matches!(
                value.transaction.status,
                TransactionStatus::Included { .. }
                    | TransactionStatus::Confirmed { .. }
                    | TransactionStatus::Failed { block: Some(_), .. }
            )
        }) {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "a canonical transaction is already included",
                false,
            ));
        }
        let status = match &draft.status {
            ObservationDraftStatus::Included => TransactionStatus::Included {
                block: command.block.block.clone(),
                confirmations: 1,
            },
            ObservationDraftStatus::Failed { reason } => TransactionStatus::Failed {
                block: Some(command.block.block.clone()),
                reason: reason.clone(),
            },
        };
        let (transaction, watch_ids) = observation(
            prior.map(|v| &v.transaction),
            prior.map_or(&[][..], |v| v.watch_ids.as_slice()),
            &command.scope,
            &draft.transaction_id,
            status,
            Some(draft),
            draft.observed_at,
        )?;
        let next = StoredObservation {
            transaction,
            watch_ids,
        };
        let pending = if matches!(next.transaction.status, TransactionStatus::Included { .. }) {
            PendingChange::Add {
                inclusion: command.block.block.height,
            }
        } else {
            PendingChange::None
        };
        transitions.insert(
            draft.transaction_id.clone(),
            ObservationTransition {
                prior: prior.cloned(),
                prior_addresses: prior.map_or_else(BTreeSet::new, |v| addresses(&v.transaction)),
                next_addresses: addresses(&next.transaction),
                next,
                included_here: true,
                pending,
            },
        );
    }

    let prune_before = (command.block.block.height.0 >= command.reorg_retention).then(|| {
        BlockHeight(
            command
                .block
                .block
                .height
                .0
                .saturating_sub(command.reorg_retention),
        )
    });
    Ok(CommitPlan {
        scope: command.scope.clone(),
        expected_checkpoint: command.expected_checkpoint.clone(),
        expected_watch_version: command.expected_watch_version,
        block: command.block.block.clone(),
        transitions,
        effect: command.block.effect.clone(),
        undo: command.block.undo.clone(),
        prune_before,
    })
}
