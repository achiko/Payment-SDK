use crate::{BlockInterpreter, BlockRef, IndexError, IndexErrorKind};

use super::SyncWorker;

impl<S, I, R> SyncWorker<S, I, R>
where
    I: BlockInterpreter,
{
    pub(super) fn validate_scope(&self, scope: &crate::IndexScope) -> Result<(), IndexError> {
        if scope != &self.config.scope {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "request scope does not match the worker scope",
                false,
            ));
        }
        Ok(())
    }

    pub(super) fn validate_interpreted(
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
}
