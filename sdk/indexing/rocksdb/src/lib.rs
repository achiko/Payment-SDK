mod amount_record;
mod index_record;
mod keys;
mod movement_record;
mod output;
mod projection;
mod record;
mod repository;
#[cfg(test)]
mod repository_test;

use indexing::*;

pub use output::OutputReader;
pub(crate) use projection::{
    ProjectionBatch, ProjectionCursor, ProjectionEntry, ProjectionGet, ProjectionMutation,
    ProjectionPage, ProjectionResult, ProjectionScan, ProjectionSnapshot,
};

/// Durable implementation detail shared by this adapter's repository traits.
///
/// Every semantic mutation is fenced by a scope-local compare-and-swap record,
/// then applied through one [`storage::Store::commit`] call.
///
/// Applications construct this concrete adapter from a RocksDB storage engine.
pub struct Repository {
    storage: storage_rocksdb::RocksDb,
    scope: IndexScope,
}

impl Repository {
    #[must_use]
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }

    pub fn new(storage: storage_rocksdb::RocksDb, scope: IndexScope) -> Result<Self, IndexError> {
        if scope.chain.0.trim().is_empty() || scope.network.trim().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "persistent index scope must contain a chain and network",
                false,
            ));
        }
        Ok(Self { storage, scope })
    }
}

impl Clone for Repository {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            scope: self.scope.clone(),
        }
    }
}
