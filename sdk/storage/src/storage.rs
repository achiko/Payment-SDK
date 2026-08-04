use crate::{BoxFuture, Key, Namespace, StorageError, StoredValue, Version, WriteBatch};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRequest {
    pub namespace: Namespace,
    pub prefix: Vec<u8>,
    pub after: Option<Key>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanPage {
    pub entries: Vec<(Key, StoredValue)>,
    pub next: Option<Key>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitResult {
    pub version: Version,
}

pub trait Storage: Send + Sync {
    fn get<'a>(
        &'a self,
        namespace: &'a Namespace,
        key: &'a Key,
    ) -> BoxFuture<'a, Result<Option<StoredValue>, StorageError>>;

    fn scan<'a>(&'a self, request: ScanRequest) -> BoxFuture<'a, Result<ScanPage, StorageError>>;

    /// Commits the complete batch atomically or makes no changes.
    fn commit<'a>(&'a self, batch: WriteBatch)
    -> BoxFuture<'a, Result<CommitResult, StorageError>>;
}
