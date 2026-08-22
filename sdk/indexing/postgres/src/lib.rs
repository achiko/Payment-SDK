//! PostgreSQL persistence for canonical history, live outputs, a checkpoint,
//! and the bounded rollback journal.
//!
//! Scope is a column rather than a key prefix, so one schema serves every
//! configured chain. A repository still binds to exactly one scope and refuses
//! requests for another, matching the embedded redb implementation.

mod read;
mod registry;
mod revert;
mod row;
mod write;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use indexing::{
    BlockAddition, BlockHash, BlockHeight, BlockOutcome, BlockRef, BlockSelector, Blocks,
    BoxFuture, CanonicalAddress, IndexError, IndexErrorKind, IndexScope,
};
use tokio_postgres::{NoTls, Row};

/// Builds a connection pool from a libpq-style URL.
///
/// TLS is not configured: this is intended for a database reached over a
/// trusted local socket or network. Wrap the manager yourself for anything else.
pub fn pool(url: &str, max_size: usize) -> Result<Pool, IndexError> {
    let config = url
        .parse::<tokio_postgres::Config>()
        .map_err(|error| invalid(format!("invalid PostgreSQL URL: {error}")))?;
    let manager = Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(max_size)
        .build()
        .map_err(|error| invalid(format!("could not build a connection pool: {error}")))
}

/// One chain's indexing store.
pub struct Repository {
    pool: Pool,
    scope: IndexScope,
}

impl Repository {
    pub fn new(pool: Pool, scope: IndexScope) -> Result<Self, IndexError> {
        if scope.chain.0.trim().is_empty() || scope.network.trim().is_empty() {
            return Err(invalid(
                "persistent index scope must contain a chain and network",
            ));
        }
        Ok(Self { pool, scope })
    }

    /// Rejects a request aimed at a different chain rather than silently
    /// reading another scope's rows.
    fn check_scope(&self, scope: &IndexScope) -> Result<(), IndexError> {
        if scope == &self.scope {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::ScopeMismatch,
            "request belongs to another index scope",
            false,
        ))
    }

    /// The scope's current checkpoint, used to keep a page consistent.
    pub(crate) async fn read_checkpoint(&self) -> Result<Option<BlockRef>, IndexError> {
        self.read_block(BlockSelector::Tip(self.scope.clone()))
            .await
    }

    /// Rejects an address from another chain before it reaches a query.
    pub(crate) fn check_address(&self, address: &CanonicalAddress) -> Result<(), IndexError> {
        if address.belongs_to(&self.scope) {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::ScopeMismatch,
            "address belongs to another index scope",
            false,
        ))
    }

    async fn read_block(&self, selector: BlockSelector) -> Result<Option<BlockRef>, IndexError> {
        let (scope, height) = match selector {
            BlockSelector::Tip(scope) => (scope, None),
            BlockSelector::Height { scope, height } => (scope, Some(height)),
        };
        self.check_scope(&scope)?;
        let client = self.pool.get().await.map_err(unavailable)?;

        // Tip comes from the checkpoint; a specific height comes from the
        // retained journal, exactly as the redb repository resolves them.
        let row = match height {
            None => client
                .query_opt(
                    "SELECT height, hash, parent_hash, block_timestamp \
                     FROM checkpoint WHERE chain = $1 AND network = $2",
                    &[&scope.chain.0, &scope.network],
                )
                .await
                .map_err(store)?,
            Some(height) => {
                let height = i64::try_from(height.0)
                    .map_err(|_| invalid("block height exceeds the storage range"))?;
                client
                    .query_opt(
                        "SELECT height, block_hash AS hash, block_parent AS parent_hash, \
                         block_timestamp FROM journal \
                         WHERE chain = $1 AND network = $2 AND height = $3",
                        &[&scope.chain.0, &scope.network, &height],
                    )
                    .await
                    .map_err(store)?
            }
        };
        row.map(block_ref).transpose()
    }
}

impl Clone for Repository {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            scope: self.scope.clone(),
        }
    }
}

impl Blocks for Repository {
    fn get<'a>(
        &'a self,
        selector: BlockSelector,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { self.read_block(selector).await })
    }

    fn add<'a>(
        &'a self,
        addition: BlockAddition,
    ) -> BoxFuture<'a, Result<BlockOutcome, IndexError>> {
        Box::pin(async move { self.write_block(addition).await })
    }

    fn remove<'a>(
        &'a self,
        scope: IndexScope,
        expected_tip: BlockRef,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { self.remove_tip(&scope, &expected_tip).await })
    }
}

fn block_ref(row: Row) -> Result<BlockRef, IndexError> {
    let height: i64 = row.try_get("height").map_err(store)?;
    let hash: Vec<u8> = row.try_get("hash").map_err(store)?;
    let parent_hash: Option<Vec<u8>> = row.try_get("parent_hash").map_err(store)?;
    let timestamp: Option<i64> = row.try_get("block_timestamp").map_err(store)?;
    Ok(BlockRef {
        height: BlockHeight(u64::try_from(height).map_err(|_| store_message("negative height"))?),
        hash: BlockHash(hash),
        parent_hash: parent_hash.map(BlockHash),
        timestamp: timestamp
            .map(u64::try_from)
            .transpose()
            .map_err(|_| store_message("negative block timestamp"))?,
    })
}

fn invalid(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidRequest, message, false)
}

fn store_message(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}

fn store(error: tokio_postgres::Error) -> IndexError {
    IndexError::new(IndexErrorKind::Store, error.to_string(), true)
}

fn unavailable(error: deadpool_postgres::PoolError) -> IndexError {
    IndexError::new(IndexErrorKind::Store, error.to_string(), true)
}
