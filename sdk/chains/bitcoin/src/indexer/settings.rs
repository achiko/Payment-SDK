//! Declarative construction for the Bitcoin block indexer.
//!
//! Collapses the transport, node preflight, source, interpreter, and
//! synchronization wiring into one value an embedding binary can fill in and
//! hand over. The repository stays a parameter: this crate implements chain
//! semantics and must not know which storage backend an application chose.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use indexing::{ChainId, IndexError, IndexErrorKind, IndexScope, Observer, SyncConfig};
use json_rpc::{Config as TransportConfig, Http, Retry};

use crate::{
    BlockInterpreter, CoreConfig, Indexer, Network, RpcClient, Source, SourceConfig,
    parse_bitcoin_block_hash,
};

/// Bitcoin Core RPC credentials, sent as HTTP basic authentication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl Credentials {
    fn header(&self) -> (String, String) {
        use base64::Engine;

        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.user, self.password));
        ("Authorization".to_owned(), format!("Basic {encoded}"))
    }
}

/// Everything required to build a Bitcoin indexer.
///
/// Every field is public, so a caller may take [`IndexerSettings::new`] and
/// override only what differs from the defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexerSettings {
    /// Bitcoin Core JSON-RPC endpoints. The first is primary; any others are
    /// tried in order when it fails.
    pub endpoints: Vec<String>,
    /// Encoded into an `Authorization` header when present.
    pub credentials: Option<Credentials>,
    /// Extra request headers, for a proxy or a pre-encoded cookie credential.
    pub headers: Vec<(String, String)>,
    /// Verified against the node before the indexer is returned.
    pub network: Network,
    /// Verified against the node before the indexer is returned.
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
    /// application: three retries, a fifteen-second request budget, six
    /// confirmations, and a hundred-block rollback journal.
    #[must_use]
    pub fn new(
        endpoint: impl Into<String>,
        network: Network,
        genesis_hash: impl Into<String>,
    ) -> Self {
        Self {
            endpoints: vec![endpoint.into()],
            credentials: None,
            headers: Vec::new(),
            network,
            genesis_hash: genesis_hash.into(),
            confirmations: 6,
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
            network: self.network.canonical_name().to_owned(),
        }
    }

    /// Opens one long-lived Bitcoin Core connection and verifies the node
    /// before any indexing happens: expected network, genesis block, supported
    /// major version, and a transaction index that has reached the tip.
    ///
    /// Hold this for the life of the process and clone it as needed — a clone
    /// is an `Arc` bump — so indexing and wallet capabilities share one
    /// connection instead of opening a second.
    pub async fn client(&self) -> Result<RpcClient<Http>, IndexError> {
        let genesis = parse_bitcoin_block_hash(&self.genesis_hash)?;
        Ok(RpcClient::connect(
            self.transport()?,
            CoreConfig {
                expected_network: self.network,
                expected_genesis_hash: genesis,
            },
        )
        .await?)
    }

    /// Builds an indexer on an already-connected client.
    ///
    /// Takes the genesis hash from the client rather than re-reading it here,
    /// so the source cannot disagree with the connection it was given.
    ///
    /// `observer`, when supplied, is called after each block this indexer
    /// commits. Pass `None` to index without notification.
    pub fn build<R>(
        &self,
        client: RpcClient<Http>,
        repository: R,
        observer: Option<Arc<dyn Observer>>,
    ) -> Result<Indexer<Http, R>, IndexError>
    where
        R: Clone,
    {
        if client.config().expected_network != self.network {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "Bitcoin client is connected to a different network than these settings",
                false,
            ));
        }
        let scope = self.scope();
        let source = Source::from_client(
            client.clone(),
            SourceConfig {
                scope: scope.clone(),
                network: self.network,
                expected_genesis_hash: client.config().expected_genesis_hash.clone(),
            },
        )?;
        let mut indexer = Indexer::new(
            source,
            BlockInterpreter::new(scope.clone(), self.network)?,
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

    fn transport(&self) -> Result<Http, IndexError> {
        let primary = self.endpoints.first().ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidRequest,
                "at least one Bitcoin RPC endpoint is required",
                false,
            )
        })?;
        let mut config = TransportConfig::new(primary.clone(), self.request_timeout);
        config.endpoints.clone_from(&self.endpoints);
        config.max_response_bytes = self.max_response_bytes;
        config.headers.clone_from(&self.headers);
        if let Some(credentials) = &self.credentials {
            config.headers.push(credentials.header());
        }
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
