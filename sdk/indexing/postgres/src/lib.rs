//! PostgreSQL persistence for canonical history, live outputs, a checkpoint,
//! and the bounded rollback journal.
//!
//! Scope is a column rather than a key prefix, so one schema serves every
//! configured chain. A repository still binds to exactly one scope and refuses
//! requests for another, matching the embedded redb implementation.
//!
//! # Round trips
//!
//! An embedded store commits a block with one batch write; a database pays a
//! network round trip per statement, so the count of statements — not the count
//! of rows — is what this backend has to keep down. Two rules hold everywhere
//! here:
//!
//!   * every statement goes through the connection's statement cache, so a
//!     repeated query costs one round trip rather than a parse plus a bind; and
//!   * a set of rows is written by one statement over parameter arrays, never
//!     by a loop of single-row statements.
//!
//! The result is a fixed statement count per block instead of one that grows
//! with the block's transactions, movements, and outputs.

mod columns;
mod read;
mod registry;
mod revert;
mod row;
mod write;

use deadpool_postgres::{Client, Manager, ManagerConfig, Pool, RecyclingMethod, Transaction};
use indexing::{
    BlockAddition, BlockOutcome, BlockRef, BlockSelector, Blocks, BoxFuture, CanonicalAddress,
    IndexError, IndexErrorKind, IndexScope,
};
use tokio_postgres::{NoTls, Statement};

/// The scope's tip. Column aliases match [`row::block`] so every block-shaped
/// row decodes through one function.
const CHECKPOINT: &str = "SELECT height, hash, parent_hash AS parent, \
                          block_timestamp AS timestamp \
                          FROM checkpoint WHERE chain = $1 AND network = $2";

/// A retained block, which is the only place a non-tip height is recorded.
const RETAINED_BLOCK: &str = "SELECT height, block_hash AS hash, block_parent AS parent, \
                              block_timestamp AS timestamp FROM journal \
                              WHERE chain = $1 AND network = $2 AND height = $3";

/// Builds a connection pool from a libpq-style URL.
///
/// TLS is not configured: this is intended for a database reached over a
/// trusted local socket or network. Wrap the manager yourself for anything else.
///
/// Recycling is [`RecyclingMethod::Fast`] deliberately: a cleaning recycle
/// discards the session, which would throw away the prepared statements this
/// backend depends on and put the parse round trip back on every query.
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

    pub(crate) async fn client(&self) -> Result<Client, IndexError> {
        self.pool.get().await.map_err(unavailable)
    }

    /// The scope's current checkpoint, read on a caller-supplied connection.
    ///
    /// A page reads the checkpoint twice to prove it did not move, and both
    /// reads must share the request's connection: acquiring a second one costs
    /// a pool round trip and can observe a different session's view.
    pub(crate) async fn checkpoint_on(
        &self,
        client: &Client,
    ) -> Result<Option<BlockRef>, IndexError> {
        let statement = prepare(client, CHECKPOINT).await?;
        let row = client
            .query_opt(&statement, &[&self.scope.chain.0, &self.scope.network])
            .await
            .map_err(store)?;
        row.as_ref().map(|row| row::block(row, "")).transpose()
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
        let client = self.client().await?;

        // Tip comes from the checkpoint; a specific height comes from the
        // retained journal, exactly as the redb repository resolves them.
        let Some(height) = height else {
            return self.checkpoint_on(&client).await;
        };
        let height = row::as_i64(height.0, "block height")?;
        let statement = prepare(&client, RETAINED_BLOCK).await?;
        let row = client
            .query_opt(&statement, &[&scope.chain.0, &scope.network, &height])
            .await
            .map_err(store)?;
        row.as_ref().map(|row| row::block(row, "")).transpose()
    }
}

/// Prepares through the connection's cache, so a repeated statement costs one
/// round trip instead of a parse and a bind.
pub(crate) async fn prepare(client: &Client, sql: &str) -> Result<Statement, IndexError> {
    client.prepare_cached(sql).await.map_err(store)
}

/// The same cache, reached from inside a transaction. The cache belongs to the
/// connection, so statements survive the transaction that first prepared them.
pub(crate) async fn prepare_in(
    transaction: &Transaction<'_>,
    sql: &str,
) -> Result<Statement, IndexError> {
    transaction.prepare_cached(sql).await.map_err(store)
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

fn invalid(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidRequest, message, false)
}

fn store(error: tokio_postgres::Error) -> IndexError {
    IndexError::new(IndexErrorKind::Store, error.to_string(), true)
}

fn unavailable(error: deadpool_postgres::PoolError) -> IndexError {
    IndexError::new(IndexErrorKind::Store, error.to_string(), true)
}
