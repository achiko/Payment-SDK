use crate::{BlockHeight, ConfirmationPolicy, IndexError, IndexErrorKind};

pub const V1_CONFIRMATION_DEPTH: u64 = 12;
pub const V1_REORG_RETENTION: u64 = 50;

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
                "ordered depth worker does not consume a chain-finality source",
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
    pub fn default_v1(scope: crate::IndexScope, bootstrap_height: BlockHeight) -> Self {
        Self {
            scope,
            bootstrap_height,
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: V1_CONFIRMATION_DEPTH,
                require_chain_finality: false,
            },
            reorg_retention: V1_REORG_RETENTION,
        }
    }
}
