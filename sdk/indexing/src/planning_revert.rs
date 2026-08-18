use std::collections::BTreeMap;

use crate::{
    IndexError, IndexErrorKind, ObservationRevision, ObservationTransition, PendingChange,
    RevertContext, RevertDecision, RevertPlan, RevertTip, StoredObservation, TransactionStatus,
};

pub fn revert<U: Clone>(
    command: &RevertTip,
    context: &RevertContext<U>,
) -> Result<RevertDecision<U>, IndexError> {
    if context.checkpoint.as_ref() != Some(&command.expected_tip) {
        if context
            .checkpoint
            .as_ref()
            .is_none_or(|checkpoint| checkpoint.height < command.expected_tip.height)
        {
            return Ok(RevertDecision {
                checkpoint: context.checkpoint.clone(),
                plan: None,
            });
        }
        return Err(IndexError::new(
            IndexErrorKind::Conflict,
            "revert must target the exact newest canonical tip",
            true,
        ));
    }
    let block = context.block.as_ref().ok_or_else(|| {
        IndexError::new(
            IndexErrorKind::ReorgBeyondRetention,
            "tip undo data is outside the retained rollback window",
            false,
        )
    })?;
    if block.block != command.expected_tip {
        return Err(IndexError::new(
            IndexErrorKind::Store,
            "undo data does not match the canonical tip",
            false,
        ));
    }
    let observed_at = block
        .prior_checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.timestamp)
        .unwrap_or(command.expected_tip.height.0);
    let mut transitions = BTreeMap::new();
    for change in &block.observations {
        let transaction_id = change.current.transaction.transaction_id.clone();
        let next = if change.included_here {
            let (transaction, watch_ids) = crate::planning::observation(
                Some(&change.current.transaction),
                &change.current.watch_ids,
                &command.scope,
                &transaction_id,
                TransactionStatus::Reorged {
                    previous_block: command.expected_tip.clone(),
                },
                None,
                observed_at,
            )?;
            StoredObservation {
                transaction,
                watch_ids,
            }
        } else {
            restored(change, observed_at)?
        };
        let pending = pending(change, &next);
        if transitions
            .insert(
                transaction_id,
                ObservationTransition {
                    prior: Some(change.current.clone()),
                    prior_addresses: crate::planning::addresses(&change.current.transaction),
                    next_addresses: crate::planning::addresses(&next.transaction),
                    next,
                    included_here: change.included_here,
                    pending,
                },
            )
            .is_some()
        {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "undo data contains duplicate observations",
                false,
            ));
        }
    }
    let plan = RevertPlan {
        scope: command.scope.clone(),
        expected_tip: command.expected_tip.clone(),
        checkpoint: block.prior_checkpoint.clone(),
        undo: block.undo.clone(),
        transitions,
    };
    Ok(RevertDecision {
        checkpoint: plan.checkpoint.clone(),
        plan: Some(plan),
    })
}

fn pending(change: &crate::RevertObservation, next: &StoredObservation) -> PendingChange {
    if change.included_here
        && let TransactionStatus::Included { block, .. }
        | TransactionStatus::Confirmed { block, .. } = &change.current.transaction.status
    {
        return PendingChange::Remove {
            inclusion: block.height,
        };
    }
    if !change.included_here
        && let TransactionStatus::Included { block, .. } = &next.transaction.status
    {
        return PendingChange::Add {
            inclusion: block.height,
        };
    }
    PendingChange::None
}

fn restored(
    change: &crate::RevertObservation,
    observed_at: u64,
) -> Result<StoredObservation, IndexError> {
    let mut prior = change.prior.clone().ok_or_else(|| {
        IndexError::new(
            IndexErrorKind::Store,
            "confirmation rollback is missing its prior observation",
            false,
        )
    })?;
    prior.transaction.revision = ObservationRevision(
        change
            .current
            .transaction
            .revision
            .0
            .checked_add(1)
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "observation revision is exhausted",
                    false,
                )
            })?,
    );
    prior.transaction.observed_at = observed_at;
    Ok(prior)
}
