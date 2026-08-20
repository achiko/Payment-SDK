//! Declarative construction for the Ethereum block indexer.
//!
//! Collapses the transport, source, interpreter, and synchronization wiring
//! into one value an embedding binary can fill in and hand over. The repository
//! stays a parameter: this crate implements chain semantics and must not know
//! which storage backend an application chose.
//!
//! An Ethereum client carries no chain identity of its own, so nothing can be
//! verified until a source exists. Chain ID and genesis are therefore checked
//! in [`IndexerSettings::build`], which is why that call is the asynchronous one
//! here and [`IndexerSettings::client`] is not.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use indexing::{BlockHash, ChainId, IndexError, IndexErrorKind, IndexScope, Observer, SyncConfig};
use json_rpc::{Config as TransportConfig, Http, Retry};

use crate::{BlockInterpreter, Indexer, RpcClient, Source, SourceConfig};

/// Which EVM chain an indexer follows.
///
/// Both halves are explicit on purpose. The numeric ID is what the node is
/// verified against; the slug names the scope and becomes part of the storage
/// keyspace, so it must stay stable across restarts. Deriving one from the
/// other would let a renamed default silently orphan an existing index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub chain_id: u64,
    pub slug: String,
}

impl Network {
    #[must_use]
    pub fn new(chain_id: u64, slug: impl Into<String>) -> Self {
        Self {
            chain_id,
            slug: slug.into(),
        }
    }
}

/// Everything required to build an Ethereum indexer.
///
/// Every field is public, so a caller may take [`IndexerSettings::new`] and
/// override only what differs from the defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexerSettings {
    /// Ethereum JSON-RPC endpoints. The first is primary; any others are tried
    /// in order when it fails.
    pub endpoints: Vec<String>,
    /// Request headers, for provider API keys or a proxy.
    pub headers: Vec<(String, String)>,
    /// Verified against `eth_chainId` when the indexer is built; its slug
    /// becomes the scope's network and part of the storage keyspace.
    pub network: Network,
    /// Verified against block zero when the indexer is built.
    pub genesis_hash: String,
    /// Depth at which history reports a transaction confirmed.
    pub confirmations: u64,
    /// How far back a reorg can be reversed from the rollback journal.
    pub reorg_retention: u64,
    /// Blocks indexed per `sync` call.
    pub batch_size: usize,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
    pub retry_attempts: NonZeroU32,
    pub retry_initial_backoff: Duration,
    pub retry_max_backoff: Duration,
}

impl IndexerSettings {
    /// Settings for one endpoint, with defaults matching the reference
    /// application: three retries, a fifteen-second request budget, twelve
    /// confirmations, and a hundred-block rollback journal.
    #[must_use]
    pub fn new(
        endpoint: impl Into<String>,
        network: Network,
        genesis_hash: impl Into<String>,
    ) -> Self {
        Self {
            endpoints: vec![endpoint.into()],
            headers: Vec::new(),
            network,
            genesis_hash: genesis_hash.into(),
            confirmations: 12,
            reorg_retention: 100,
            batch_size: 100,
            request_timeout: Duration::from_secs(15),
            max_response_bytes: 64 * 1024 * 1024,
            retry_attempts: NonZeroU32::new(3).expect("three is not zero"),
            retry_initial_backoff: Duration::from_millis(250),
            retry_max_backoff: Duration::from_secs(2),
        }
    }

    /// The scope this indexer owns. Open the repository with it, so the store
    /// and the indexer cannot disagree about which chain they hold.
    #[must_use]
    pub fn scope(&self) -> IndexScope {
        IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: self.network.slug.clone(),
        }
    }

    /// Builds one long-lived Ethereum client.
    ///
    /// No network round-trip happens here: an Ethereum client is a transport
    /// wrapper with no chain identity of its own. Hold it for the life of the
    /// process and clone it as needed — a clone is an `Arc` bump — so indexing
    /// and wallet capabilities share one connection instead of opening a
    /// second.
    pub fn client(&self) -> Result<RpcClient<Http>, IndexError> {
        Ok(RpcClient::new(self.transport()?))
    }

    /// Builds an indexer on an existing client, verifying that the node really
    /// serves the configured chain ID and genesis block before returning.
    ///
    /// `observer`, when supplied, is called after each block this indexer
    /// commits. Pass `None` to index without notification.
    pub async fn build<R>(
        &self,
        client: RpcClient<Http>,
        repository: R,
        observer: Option<Arc<dyn Observer>>,
    ) -> Result<Indexer<Http, R>, IndexError>
    where
        R: Clone,
    {
        let scope = self.scope();
        let source = Source::from_rpc(
            client,
            SourceConfig {
                scope: scope.clone(),
                expected_chain_id: self.network.chain_id,
                expected_genesis_hash: self.genesis()?,
            },
        )
        .await?;
        let mut indexer = Indexer::new(
            source,
            BlockInterpreter::new(scope.clone())?,
            repository,
            SyncConfig::new(
                scope,
                self.confirmations,
                self.reorg_retention,
                self.batch_size,
            )?,
        );
        if let Some(observer) = observer {
            indexer.observe(observer);
        }
        Ok(indexer)
    }

    fn genesis(&self) -> Result<BlockHash, IndexError> {
        let digits = self
            .genesis_hash
            .strip_prefix("0x")
            .unwrap_or(&self.genesis_hash);
        let bytes = alloy_primitives::hex::decode(digits).map_err(|_| {
            IndexError::new(
                IndexErrorKind::InvalidRequest,
                "Ethereum genesis hash must be hexadecimal",
                false,
            )
        })?;
        if bytes.len() != 32 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "Ethereum genesis hash must contain exactly 32 bytes",
                false,
            ));
        }
        Ok(BlockHash(bytes))
    }

    fn transport(&self) -> Result<Http, IndexError> {
        let primary = self.endpoints.first().ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidRequest,
                "at least one Ethereum RPC endpoint is required",
                false,
            )
        })?;
        let mut config = TransportConfig::new(primary.clone(), self.request_timeout);
        config.endpoints.clone_from(&self.endpoints);
        config.max_response_bytes = self.max_response_bytes;
        config.headers.clone_from(&self.headers);
        config.retry = Retry::new(
            self.retry_attempts,
            self.retry_initial_backoff,
            self.retry_max_backoff,
        )
        .map_err(transport_error)?;
        Http::new(config).map_err(transport_error)
    }
}

fn transport_error(error: json_rpc::Error) -> IndexError {
    IndexError::new(IndexErrorKind::Source, error.to_string(), false)
}
