mod keys;
mod record;
mod repository;
#[cfg(test)]
mod tests;

use crate::{
    BlockHeight, ConfirmationPolicy, IndexError, IndexErrorKind, IndexScope, ProjectionBatch,
    RebuildGeneration,
};

/// Explicit serialization boundary for chain-owned watch targets and undo data.
///
/// Implementations belong with the concrete chain and must define a stable,
/// versioned byte format. The persistent repository never serializes an
/// arbitrary chain-owned Rust enum or struct layout.
pub trait IndexRecordCodec: Send + Sync {
    type Target: Clone + Send + Sync + 'static;
    type Undo: Clone + Send + Sync + 'static;

    fn encode_target(&self, target: &Self::Target) -> Result<Vec<u8>, IndexError>;

    fn decode_target(&self, encoded: &[u8]) -> Result<Self::Target, IndexError>;

    fn encode_undo(&self, undo: &Self::Undo) -> Result<Vec<u8>, IndexError>;

    fn decode_undo(&self, encoded: &[u8]) -> Result<Self::Undo, IndexError>;

    /// Converts chain-owned undo data into inverse opaque projection changes.
    ///
    /// Chains without a durable projection retain the empty default. The
    /// repository invokes this only after decoding the retained undo bundle
    /// and before changing canonical state.
    fn rollback_projection(&self, _undo: &Self::Undo) -> Result<ProjectionBatch, IndexError> {
        Ok(ProjectionBatch::default())
    }
}

/// Strict pass-through codec for chain adapters that already own a versioned
/// byte representation.
#[derive(Clone, Copy, Debug, Default)]
pub struct RawBytesIndexCodec;

impl IndexRecordCodec for RawBytesIndexCodec {
    type Target = Vec<u8>;
    type Undo = Vec<u8>;

    fn encode_target(&self, target: &Self::Target) -> Result<Vec<u8>, IndexError> {
        Ok(target.clone())
    }

    fn decode_target(&self, encoded: &[u8]) -> Result<Self::Target, IndexError> {
        Ok(encoded.to_vec())
    }

    fn encode_undo(&self, undo: &Self::Undo) -> Result<Vec<u8>, IndexError> {
        Ok(undo.clone())
    }

    fn decode_undo(&self, encoded: &[u8]) -> Result<Self::Undo, IndexError> {
        Ok(encoded.to_vec())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistentIndexConfig {
    pub scope: IndexScope,
    pub bootstrap_height: BlockHeight,
    pub confirmation_policy: ConfirmationPolicy,
    pub reorg_retention: u64,
}

impl PersistentIndexConfig {
    pub fn new(
        scope: IndexScope,
        bootstrap_height: BlockHeight,
        confirmation_policy: ConfirmationPolicy,
        reorg_retention: u64,
    ) -> Result<Self, IndexError> {
        if scope.chain.0.trim().is_empty() || scope.network.trim().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "persistent index scope must contain a chain and network",
                false,
            ));
        }
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
                "the v1 persistent repository has no chain-finality source",
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
}

/// Backend-independent durable implementation of [`crate::IndexRepository`].
///
/// Every semantic mutation is fenced by a scope-local compare-and-swap record,
/// then applied through one [`storage::Storage::commit`] call.
pub struct PersistentIndexRepository<S, C> {
    storage: S,
    codec: C,
    config: PersistentIndexConfig,
}

impl<S, C> PersistentIndexRepository<S, C> {
    #[must_use]
    pub fn new(storage: S, codec: C, config: PersistentIndexConfig) -> Self {
        Self {
            storage,
            codec,
            config,
        }
    }

    #[must_use]
    pub fn config(&self) -> &PersistentIndexConfig {
        &self.config
    }

    #[must_use]
    pub fn storage(&self) -> &S {
        &self.storage
    }

    #[must_use]
    pub fn codec(&self) -> &C {
        &self.codec
    }
}

impl<S: Clone, C: Clone> Clone for PersistentIndexRepository<S, C> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            codec: self.codec.clone(),
            config: self.config.clone(),
        }
    }
}

const BASE_GENERATION: RebuildGeneration = RebuildGeneration(0);
