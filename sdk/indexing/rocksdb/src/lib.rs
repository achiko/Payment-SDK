mod amount_record;
mod keys;
mod movement_record;
mod record;
mod repository;
#[cfg(test)]
mod repository_test;

use indexing::*;

/// RocksDB storage for canonical history, live outputs, a checkpoint, and the
/// bounded rollback journal required to reverse recent blocks.
pub struct Repository {
    storage: storage_rocksdb::RocksDb,
    scope: IndexScope,
}

impl Repository {
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
