use crate::{BlockRef, BoxFuture, IndexError, IndexScope, RebuildGeneration};

/// One opaque chain-owned mutation applied inside the canonical block commit.
///
/// Keys are relative to an internal scope-and-generation prefix. Concrete
/// chains own their encoding and must keep it stable across process restarts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionMutation {
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    /// Writes `key` only when `required_key` exists in the canonical
    /// projection snapshot immediately before this atomic commit.
    ///
    /// Repositories must fence both keys so a concurrent creation, deletion,
    /// or target update turns the commit into a retryable conflict rather than
    /// applying the decision against a mixed snapshot.
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

/// Opaque chain-owned projection changes for one block transition.
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
pub struct ProjectionGetRequest {
    pub scope: IndexScope,
    pub key: Vec<u8>,
    /// When present, the lookup succeeds only against this exact canonical
    /// projection snapshot.
    ///
    /// This lets callers combine a scan with dependent point lookups without
    /// joining facts across a same-generation block commit, backfill, or
    /// reorg.
    pub expected_snapshot: Option<ProjectionSnapshot>,
}

/// One immutable view of the active chain-owned projection.
///
/// `revision` is a scope-wide monotonic fence. Persistent repositories advance
/// it atomically with every canonical checkpoint or projection mutation,
/// including staged-generation work and generation activation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionSnapshot {
    pub generation: RebuildGeneration,
    pub revision: u64,
    pub checkpoint: Option<BlockRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionGetResponse {
    pub snapshot: ProjectionSnapshot,
    pub value: Option<Vec<u8>>,
}

/// Exclusive continuation for an ordered opaque projection scan.
///
/// Binding the cursor to the complete snapshot prevents pagination from
/// silently joining results across rebuild activation, ordinary block commits,
/// historical backfill, or reorg.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionCursor {
    pub snapshot: ProjectionSnapshot,
    pub key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionScanRequest {
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

/// Read-only access to the active chain-owned projection.
///
/// Implementations must not expose their physical scope or generation key
/// prefixes. A scan cursor is valid only while its complete projection
/// snapshot remains current.
pub trait ProjectionQuery: Send + Sync {
    fn projection_get<'a>(
        &'a self,
        request: ProjectionGetRequest,
    ) -> BoxFuture<'a, Result<ProjectionGetResponse, IndexError>>;

    fn projection_scan<'a>(
        &'a self,
        request: ProjectionScanRequest,
    ) -> BoxFuture<'a, Result<ProjectionPage, IndexError>>;
}
