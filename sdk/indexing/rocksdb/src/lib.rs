mod amount_record;
mod codec;
mod consumer;
mod index_record;
mod keys;
mod movement_record;
mod output;
mod projection;
mod record;
mod repository;
#[cfg(test)]
mod repository_test;
mod runtime;

use indexing::*;

#[cfg(test)]
pub(crate) use codec::RawCodec;
pub(crate) use codec::{Projector, RecordCodec, RecordTypes, TargetCodec, UndoCodec};
#[doc(hidden)]
pub use index_record::IndexRecords;
pub use output::OutputReader;
pub(crate) use projection::{
    ProjectionBatch, ProjectionCursor, ProjectionEntry, ProjectionGet, ProjectionMutation,
    ProjectionPage, ProjectionQuery, ProjectionResult, ProjectionScan, ProjectionSnapshot,
};
pub use runtime::{Handle, OpenError, Runtime, SyncOutcome};

use RecordCodec as IndexRecordCodec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub scope: IndexScope,
    pub bootstrap_height: BlockHeight,
    pub confirmation_policy: ConfirmationPolicy,
    pub reorg_retention: u64,
}

impl Config {
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
                "the persistent repository has no chain-finality source",
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

/// Durable implementation detail shared by this adapter's repository traits.
///
/// Every semantic mutation is fenced by a scope-local compare-and-swap record,
/// then applied through one [`storage::Store::commit`] call.
///
/// Applications should use [`RocksRepository`]. The storage and record type
/// parameters remain visible only because Rust type aliases expose their
/// underlying type; construction and codec selection are adapter-private.
pub struct Repository<S, C> {
    storage: S,
    records: C,
    config: Config,
}

impl<S, C> Repository<S, C> {
    #[must_use]
    pub(crate) fn with_codec(storage: S, records: C, config: Config) -> Self {
        Self {
            storage,
            records,
            config,
        }
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }
}

impl Repository<storage_rocksdb::RocksDb, IndexRecords> {
    #[must_use]
    pub fn new(storage: storage_rocksdb::RocksDb, config: Config) -> Self {
        Self::with_codec(storage, IndexRecords::default(), config)
    }
}

impl<S: Clone, C: Clone> Clone for Repository<S, C> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            records: self.records.clone(),
            config: self.config.clone(),
        }
    }
}

const BASE_GENERATION: RebuildGeneration = RebuildGeneration(0);

pub type RocksRepository = Repository<storage_rocksdb::RocksDb, IndexRecords>;
