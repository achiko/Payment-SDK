use indexing::{BlockRef, BoxFuture, IndexError, IndexScope, RebuildGeneration};

/// One adapter-owned record mutation applied in the canonical block commit.
///
/// Keys are relative to the repository's private scope and generation prefix.
/// This type is intentionally owned by the RocksDB indexing adapter; neither
/// chain implementations nor generic indexing contracts speak in byte keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionMutation {
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    PutIfPresent {
        required_key: Vec<u8>,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        key: Vec<u8>,
    },
}

impl ProjectionMutation {
    #[must_use]
    pub fn key(&self) -> &[u8] {
        match self {
            Self::Put { key, .. } | Self::PutIfPresent { key, .. } | Self::Delete { key } => key,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionBatch {
    pub mutations: Vec<ProjectionMutation>,
}

impl ProjectionBatch {
    #[must_use]
    pub const fn new(mutations: Vec<ProjectionMutation>) -> Self {
        Self { mutations }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mutations.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionGet {
    pub scope: IndexScope,
    pub key: Vec<u8>,
    pub expected_snapshot: Option<ProjectionSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionSnapshot {
    pub generation: RebuildGeneration,
    pub revision: u64,
    pub checkpoint: Option<BlockRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionResult {
    pub snapshot: ProjectionSnapshot,
    pub value: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionCursor {
    pub snapshot: ProjectionSnapshot,
    pub key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionScan {
    pub scope: IndexScope,
    pub prefix: Vec<u8>,
    pub after: Option<ProjectionCursor>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionPage {
    pub snapshot: ProjectionSnapshot,
    pub entries: Vec<ProjectionEntry>,
    pub next: Option<ProjectionCursor>,
}

/// Low-level access used only to build composition-owned semantic readers.
pub trait ProjectionQuery: Send + Sync {
    fn projection_get<'a>(
        &'a self,
        request: ProjectionGet,
    ) -> BoxFuture<'a, Result<ProjectionResult, IndexError>>;

    fn projection_scan<'a>(
        &'a self,
        request: ProjectionScan,
    ) -> BoxFuture<'a, Result<ProjectionPage, IndexError>>;
}
