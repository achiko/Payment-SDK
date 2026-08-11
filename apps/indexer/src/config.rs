use std::{net::SocketAddr, path::PathBuf, time::Duration};

use chain_bitcoin::{BitcoinNetwork, format_bitcoin_block_hash, parse_bitcoin_block_hash};
use chain_identity::ChainId;
use clap::{Args, Parser, Subcommand};
use http::{AuthenticationMode, BearerToken};
use indexing::{BlockHash, BlockHeight, ConfirmationPolicy, IndexError, IndexScope};
use url::{Host, Url};

#[derive(Parser)]
#[command(
    name = "indexer-worker",
    version,
    about = "Bitcoin and Ethereum Indexer Service"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run canonical synchronization and the public HTTP API.
    Serve(ServeOptions),
    /// Create a consistent RocksDB BackupEngine snapshot.
    Backup(BackupOptions),
    /// Apply an explicit physical-schema or semantic-policy migration.
    Migrate(MigrationOptions),
    /// Build and atomically activate a shadow indexing generation.
    Rebuild(RebuildOptions),
    /// Remove an unpublished shadow generation after a failed rebuild.
    RebuildAbort(GenerationOptions),
    /// Remove one inactive projection generation after operator verification.
    Cleanup(GenerationOptions),
    /// Operate one Bitcoin Indexer Service scope.
    Bitcoin(BitcoinOptions),
}

#[derive(Args)]
pub struct BitcoinOptions {
    #[command(subcommand)]
    pub command: BitcoinCommand,
}

#[derive(Subcommand)]
pub enum BitcoinCommand {
    /// Run Bitcoin canonical synchronization and the public HTTP API.
    Serve(BitcoinServeOptions),
    /// Create a consistent RocksDB BackupEngine snapshot.
    Backup(BackupOptions),
    /// Apply a Bitcoin physical-schema or semantic-policy migration.
    Migrate(BitcoinMigrationOptions),
    /// Build and atomically activate a Bitcoin shadow indexing generation.
    Rebuild(BitcoinRebuildOptions),
    /// Remove an unpublished Bitcoin shadow generation after a failed rebuild.
    RebuildAbort(BitcoinGenerationOptions),
    /// Remove one inactive Bitcoin projection generation after operator verification.
    Cleanup(BitcoinGenerationOptions),
}

#[derive(Args, Clone)]
pub struct DatabaseOptions {
    /// Exclusive RocksDB directory for this Indexer scope.
    #[arg(long, env = "IX_DATABASE_PATH")]
    pub database_path: PathBuf,
}

#[derive(Args, Clone)]
pub struct RepositoryOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    #[arg(long, env = "IX_NETWORK")]
    pub network: String,

    #[arg(long, env = "IX_BOOTSTRAP_HEIGHT")]
    pub bootstrap_height: u64,

    #[arg(long, env = "IX_CONFIRMATION_DEPTH", default_value_t = 12)]
    pub confirmation_depth: u64,

    #[arg(long, env = "IX_REORG_RETENTION", default_value_t = 50)]
    pub reorg_retention: u64,
}

impl RepositoryOptions {
    pub fn scope(&self) -> Result<IndexScope, ConfigError> {
        if self.network.trim().is_empty() {
            return Err(ConfigError::new("network slug must not be empty"));
        }
        Ok(IndexScope {
            chain: ChainId("ethereum".to_owned()),
            network: self.network.clone(),
        })
    }

    pub fn confirmation_policy(&self) -> Result<ConfirmationPolicy, ConfigError> {
        if self.confirmation_depth == 0 {
            return Err(ConfigError::new(
                "confirmation depth must be greater than zero",
            ));
        }
        Ok(ConfirmationPolicy {
            minimum_confirmations: self.confirmation_depth,
            require_chain_finality: false,
        })
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.scope()?;
        self.confirmation_policy()?;
        if self.reorg_retention == 0 {
            return Err(ConfigError::new(
                "reorg retention must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// Required repository policy for one Bitcoin scope.
///
/// Bitcoin deliberately has no implicit confirmation or reorg policy. Every
/// deployment and every policy-sensitive maintenance command must provide both
/// values explicitly.
#[derive(Args, Clone)]
pub struct BitcoinRepositoryOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    /// Bitcoin Core network: mainnet, testnet3, testnet4, signet, or regtest.
    #[arg(long, env = "IX_NETWORK")]
    pub network: String,

    #[arg(long, env = "IX_BOOTSTRAP_HEIGHT")]
    pub bootstrap_height: u64,

    #[arg(long, env = "IX_CONFIRMATION_DEPTH")]
    pub confirmation_depth: u64,

    #[arg(long, env = "IX_REORG_RETENTION")]
    pub reorg_retention: u64,
}

impl BitcoinRepositoryOptions {
    pub fn network(&self) -> Result<BitcoinNetwork, ConfigError> {
        parse_bitcoin_network(&self.network)
    }

    pub fn scope(&self) -> Result<IndexScope, ConfigError> {
        let network = self.network()?;
        Ok(IndexScope {
            chain: ChainId("bitcoin".to_owned()),
            network: network.canonical_name().to_owned(),
        })
    }

    pub fn confirmation_policy(&self) -> Result<ConfirmationPolicy, ConfigError> {
        if self.confirmation_depth == 0 {
            return Err(ConfigError::new(
                "confirmation depth must be greater than zero",
            ));
        }
        Ok(ConfirmationPolicy {
            minimum_confirmations: self.confirmation_depth,
            require_chain_finality: false,
        })
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.scope()?;
        self.confirmation_policy()?;
        if self.reorg_retention == 0 {
            return Err(ConfigError::new(
                "reorg retention must be greater than zero",
            ));
        }
        Ok(())
    }
}

#[derive(Args, Clone)]
pub struct SourceOptions {
    #[arg(long, env = "IX_EXPECTED_CHAIN_ID")]
    pub expected_chain_id: u64,

    /// Canonical 32-byte block-zero hash with a 0x prefix.
    #[arg(long, env = "IX_EXPECTED_GENESIS_HASH")]
    pub expected_genesis_hash: String,

    /// Authoritative HTTP JSON-RPC provider. This value is redacted in logs.
    #[arg(long, env = "IX_RPC_HTTP_URL", hide_env_values = true)]
    pub rpc_http_url: String,

    /// Optional wake-only newHeads provider. HTTP remains authoritative.
    #[arg(long, env = "IX_RPC_WS_URL", hide_env_values = true)]
    pub rpc_ws_url: Option<String>,

    #[arg(long, env = "IX_RPC_TIMEOUT_SECONDS", default_value_t = 15)]
    pub rpc_timeout_seconds: u64,
}

impl SourceOptions {
    pub fn genesis_hash(&self) -> Result<BlockHash, ConfigError> {
        decode_hash(&self.expected_genesis_hash)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.genesis_hash()?;
        if self.rpc_http_url.trim().is_empty() {
            return Err(ConfigError::new("HTTP RPC URL must not be empty"));
        }
        if self.rpc_timeout_seconds == 0 {
            return Err(ConfigError::new(
                "HTTP RPC timeout must be greater than zero",
            ));
        }
        if let Some(url) = &self.rpc_ws_url {
            if !(url.starts_with("ws://") || url.starts_with("wss://")) {
                return Err(ConfigError::new(
                    "WebSocket RPC URL must use ws:// or wss://",
                ));
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        Duration::from_secs(self.rpc_timeout_seconds)
    }
}

#[derive(Args, Clone)]
pub struct BitcoinSourceOptions {
    /// Conventional 64-hex Bitcoin Core block-zero hash (display byte order).
    #[arg(long, env = "IX_EXPECTED_GENESIS_HASH")]
    pub expected_genesis_hash: String,

    /// Authoritative Bitcoin Core HTTP JSON-RPC endpoint. Redacted in logs.
    #[arg(long, env = "IX_RPC_HTTP_URL", hide_env_values = true)]
    pub rpc_http_url: String,

    /// Repeatable HTTP header as `name=value`. `IX_RPC_HEADERS` is a
    /// comma-delimited equivalent. Values are always redacted from help.
    #[arg(
        long = "rpc-header",
        env = "IX_RPC_HEADERS",
        hide_env_values = true,
        value_delimiter = ','
    )]
    pub rpc_headers: Vec<String>,

    #[arg(long, env = "IX_RPC_TIMEOUT_SECONDS", default_value_t = 15)]
    pub rpc_timeout_seconds: u64,

    /// Maximum decoded body for each Core verbosity-2 block or previous-transaction response.
    #[arg(long, env = "IX_RPC_MAX_RESPONSE_BYTES", default_value_t = 268_435_456)]
    pub rpc_max_response_bytes: usize,
}

impl BitcoinSourceOptions {
    pub fn genesis_hash(&self) -> Result<BlockHash, ConfigError> {
        let hash = parse_bitcoin_block_hash(&self.expected_genesis_hash)
            .map_err(|error| ConfigError::new(error.message))?;
        let canonical =
            format_bitcoin_block_hash(&hash).map_err(|error| ConfigError::new(error.message))?;
        if canonical != self.expected_genesis_hash {
            return Err(ConfigError::new(
                "Bitcoin genesis hash must use canonical lowercase display encoding",
            ));
        }
        Ok(hash)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.genesis_hash()?;
        validate_bitcoin_rpc_url(&self.rpc_http_url)?;
        let headers = self.parsed_rpc_headers()?;
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        {
            return Err(ConfigError::new(
                "Bitcoin Core RPC requires an authorization header",
            ));
        }
        if self.rpc_timeout_seconds == 0 || self.rpc_max_response_bytes == 0 {
            return Err(ConfigError::new(
                "Bitcoin Core RPC timeout and response limit must be greater than zero",
            ));
        }
        Ok(())
    }

    pub fn parsed_rpc_headers(&self) -> Result<Vec<(String, String)>, ConfigError> {
        let mut headers = Vec::with_capacity(self.rpc_headers.len());
        for header in &self.rpc_headers {
            let (name, value) = header.split_once('=').ok_or_else(|| {
                ConfigError::new("Bitcoin Core RPC header must use name=value syntax")
            })?;
            let name = name.trim();
            if name.is_empty() || value.is_empty() {
                return Err(ConfigError::new(
                    "Bitcoin Core RPC header name and value must not be empty",
                ));
            }
            axum::http::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ConfigError::new("Bitcoin Core RPC header name is invalid"))?;
            axum::http::HeaderValue::from_str(value)
                .map_err(|_| ConfigError::new("Bitcoin Core RPC header value is invalid"))?;
            if headers
                .iter()
                .any(|(existing, _): &(String, String)| existing.eq_ignore_ascii_case(name))
            {
                return Err(ConfigError::new(
                    "Bitcoin Core RPC header names must be unique",
                ));
            }
            headers.push((name.to_ascii_lowercase(), value.to_owned()));
        }
        Ok(headers)
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        Duration::from_secs(self.rpc_timeout_seconds)
    }
}

#[derive(Args, Clone)]
pub struct ServeOptions {
    #[command(flatten)]
    pub repository: RepositoryOptions,

    #[command(flatten)]
    pub source: SourceOptions,

    #[arg(long, env = "IX_HTTP_BIND", default_value = "127.0.0.1:8080")]
    pub http_bind: SocketAddr,

    #[arg(long, env = "IX_METRICS_BIND", default_value = "127.0.0.1:9090")]
    pub metrics_bind: SocketAddr,

    /// Require service bearer authentication (`true`) or trust every reachable caller (`false`).
    #[arg(
        long = "strict-authentication-mode",
        env = "STRICT_AUTHENTICATION_MODE"
    )]
    pub authentication_mode: AuthenticationMode,

    #[arg(long, env = "IX_BEARER_TOKEN", hide_env_values = true)]
    pub bearer_token: Option<String>,

    /// Assert that a trusted upstream terminates TLS for a non-loopback bind.
    #[arg(long, env = "IX_UPSTREAM_TLS_TERMINATED", default_value_t = false)]
    pub upstream_tls_terminated: bool,

    #[arg(long, env = "IX_POLL_SECONDS", default_value_t = 5)]
    pub poll_seconds: u64,

    #[arg(long, env = "IX_READY_MAX_LAG", default_value_t = 2)]
    pub ready_max_lag: u64,

    #[arg(long, env = "IX_READY_MAX_AGE_SECONDS", default_value_t = 30)]
    pub ready_max_age_seconds: u64,
}

impl ServeOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.repository.validate()?;
        self.source.validate()?;
        validate_service_options(
            self.http_bind,
            self.metrics_bind,
            self.authentication_mode,
            self.bearer_token.as_deref(),
            self.upstream_tls_terminated,
            self.poll_seconds,
            self.ready_max_age_seconds,
        )
    }

    #[must_use]
    pub const fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_seconds)
    }

    #[must_use]
    pub const fn ready_max_age(&self) -> Duration {
        Duration::from_secs(self.ready_max_age_seconds)
    }
}

#[derive(Args, Clone)]
pub struct BitcoinServeOptions {
    #[command(flatten)]
    pub repository: BitcoinRepositoryOptions,

    #[command(flatten)]
    pub source: BitcoinSourceOptions,

    #[arg(long, env = "IX_HTTP_BIND", default_value = "127.0.0.1:8080")]
    pub http_bind: SocketAddr,

    #[arg(long, env = "IX_METRICS_BIND", default_value = "127.0.0.1:9090")]
    pub metrics_bind: SocketAddr,

    /// Require service bearer authentication (`true`) or trust every reachable caller (`false`).
    #[arg(
        long = "strict-authentication-mode",
        env = "STRICT_AUTHENTICATION_MODE"
    )]
    pub authentication_mode: AuthenticationMode,

    #[arg(long, env = "IX_BEARER_TOKEN", hide_env_values = true)]
    pub bearer_token: Option<String>,

    /// Assert that a trusted upstream terminates TLS for a non-loopback bind.
    #[arg(long, env = "IX_UPSTREAM_TLS_TERMINATED", default_value_t = false)]
    pub upstream_tls_terminated: bool,

    #[arg(long, env = "IX_POLL_SECONDS", default_value_t = 5)]
    pub poll_seconds: u64,

    #[arg(long, env = "IX_READY_MAX_LAG", default_value_t = 2)]
    pub ready_max_lag: u64,

    #[arg(long, env = "IX_READY_MAX_AGE_SECONDS", default_value_t = 30)]
    pub ready_max_age_seconds: u64,
}

impl BitcoinServeOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.repository.validate()?;
        self.source.validate()?;
        validate_service_options(
            self.http_bind,
            self.metrics_bind,
            self.authentication_mode,
            self.bearer_token.as_deref(),
            self.upstream_tls_terminated,
            self.poll_seconds,
            self.ready_max_age_seconds,
        )
    }

    #[must_use]
    pub const fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_seconds)
    }

    #[must_use]
    pub const fn ready_max_age(&self) -> Duration {
        Duration::from_secs(self.ready_max_age_seconds)
    }
}

#[derive(Args, Clone)]
pub struct BackupOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    #[arg(long, env = "IX_BACKUP_PATH")]
    pub backup_path: PathBuf,
}

#[derive(Args)]
pub struct MigrationOptions {
    #[command(subcommand)]
    pub command: MigrationCommand,
}

#[derive(Subcommand)]
pub enum MigrationCommand {
    /// Apply registered physical RocksDB schema migrations.
    Schema(SchemaMigrationOptions),
    /// Atomically change confirmation/retention policy; checkpointed state must be Ready and is rebuilt afterward.
    Policy(PolicyMigrationOptions),
}

#[derive(Args)]
pub struct BitcoinMigrationOptions {
    #[command(subcommand)]
    pub command: BitcoinMigrationCommand,
}

#[derive(Subcommand)]
pub enum BitcoinMigrationCommand {
    /// Apply registered physical RocksDB schema migrations.
    Schema(SchemaMigrationOptions),
    /// Atomically change Bitcoin confirmation/retention policy.
    Policy(BitcoinPolicyMigrationOptions),
}

#[derive(Args, Clone)]
pub struct SchemaMigrationOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    /// Verified safety backup created before the first schema mutation.
    #[arg(long, env = "IX_BACKUP_PATH")]
    pub backup_path: PathBuf,
}

#[derive(Args, Clone)]
pub struct PolicyMigrationOptions {
    /// Target repository configuration. Scope and bootstrap height are
    /// immutable; confirmation depth and retention are the new values.
    #[command(flatten)]
    pub repository: RepositoryOptions,

    #[arg(long, env = "IX_FROM_CONFIRMATION_DEPTH")]
    pub from_confirmation_depth: u64,

    #[arg(long, env = "IX_FROM_REORG_RETENTION")]
    pub from_reorg_retention: u64,

    /// Stable operator-supplied idempotency key for safe retries.
    #[arg(long, env = "IX_POLICY_MIGRATION_ID")]
    pub migration_id: String,

    /// Human-readable audit reason persisted with the immutable migration row.
    #[arg(long, env = "IX_POLICY_MIGRATION_REASON")]
    pub reason: String,

    /// Verified safety backup created before any physical or semantic mutation.
    #[arg(long, env = "IX_BACKUP_PATH")]
    pub backup_path: PathBuf,
}

impl PolicyMigrationOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.repository.validate()?;
        if self.from_confirmation_depth == 0 || self.from_reorg_retention == 0 {
            return Err(ConfigError::new(
                "source confirmation depth and reorg retention must be greater than zero",
            ));
        }
        if self.migration_id.trim().is_empty() || self.migration_id.len() > 256 {
            return Err(ConfigError::new(
                "policy migration ID must contain 1 through 256 bytes",
            ));
        }
        if self.reason.trim().is_empty() || self.reason.len() > 4_096 {
            return Err(ConfigError::new(
                "policy migration reason must contain 1 through 4096 bytes",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn expected_confirmation_policy(&self) -> ConfirmationPolicy {
        ConfirmationPolicy {
            minimum_confirmations: self.from_confirmation_depth,
            require_chain_finality: false,
        }
    }
}

#[derive(Args, Clone)]
pub struct BitcoinPolicyMigrationOptions {
    #[command(flatten)]
    pub repository: BitcoinRepositoryOptions,

    #[arg(long, env = "IX_FROM_CONFIRMATION_DEPTH")]
    pub from_confirmation_depth: u64,

    #[arg(long, env = "IX_FROM_REORG_RETENTION")]
    pub from_reorg_retention: u64,

    #[arg(long, env = "IX_POLICY_MIGRATION_ID")]
    pub migration_id: String,

    #[arg(long, env = "IX_POLICY_MIGRATION_REASON")]
    pub reason: String,

    #[arg(long, env = "IX_BACKUP_PATH")]
    pub backup_path: PathBuf,
}

impl BitcoinPolicyMigrationOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.repository.validate()?;
        validate_policy_migration(
            self.from_confirmation_depth,
            self.from_reorg_retention,
            &self.migration_id,
            &self.reason,
        )
    }

    #[must_use]
    pub const fn expected_confirmation_policy(&self) -> ConfirmationPolicy {
        ConfirmationPolicy {
            minimum_confirmations: self.from_confirmation_depth,
            require_chain_finality: false,
        }
    }
}

#[derive(Args, Clone)]
pub struct RebuildOptions {
    #[command(flatten)]
    pub repository: RepositoryOptions,

    #[command(flatten)]
    pub source: SourceOptions,

    /// Verified safety backup created before staging the rebuild.
    #[arg(long, env = "IX_BACKUP_PATH")]
    pub backup_path: PathBuf,
}

#[derive(Args, Clone)]
pub struct BitcoinRebuildOptions {
    #[command(flatten)]
    pub repository: BitcoinRepositoryOptions,

    #[command(flatten)]
    pub source: BitcoinSourceOptions,

    /// Verified safety backup created before staging the rebuild.
    #[arg(long, env = "IX_BACKUP_PATH")]
    pub backup_path: PathBuf,
}

#[derive(Args, Clone)]
pub struct GenerationOptions {
    #[command(flatten)]
    pub repository: RepositoryOptions,

    #[arg(long)]
    pub generation: u64,

    /// Verified safety backup created before abort or cleanup.
    #[arg(long, env = "IX_BACKUP_PATH")]
    pub backup_path: PathBuf,
}

#[derive(Args, Clone)]
pub struct BitcoinGenerationOptions {
    #[command(flatten)]
    pub repository: BitcoinRepositoryOptions,

    #[arg(long)]
    pub generation: u64,

    /// Verified safety backup created before abort or cleanup.
    #[arg(long, env = "IX_BACKUP_PATH")]
    pub backup_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

impl From<IndexError> for ConfigError {
    fn from(error: IndexError) -> Self {
        Self::new(error.message)
    }
}

#[must_use]
pub const fn bootstrap_height(options: &RepositoryOptions) -> BlockHeight {
    BlockHeight(options.bootstrap_height)
}

#[must_use]
pub const fn bitcoin_bootstrap_height(options: &BitcoinRepositoryOptions) -> BlockHeight {
    BlockHeight(options.bootstrap_height)
}

fn parse_bitcoin_network(input: &str) -> Result<BitcoinNetwork, ConfigError> {
    match input {
        "mainnet" => Ok(BitcoinNetwork::Mainnet),
        "testnet3" => Ok(BitcoinNetwork::Testnet3),
        "testnet4" => Ok(BitcoinNetwork::Testnet4),
        "signet" => Ok(BitcoinNetwork::Signet),
        "regtest" => Ok(BitcoinNetwork::Regtest),
        _ => Err(ConfigError::new(
            "Bitcoin network must be mainnet, testnet3, testnet4, signet, or regtest",
        )),
    }
}

fn validate_service_options(
    http_bind: SocketAddr,
    metrics_bind: SocketAddr,
    authentication_mode: AuthenticationMode,
    bearer_token: Option<&str>,
    upstream_tls_terminated: bool,
    poll_seconds: u64,
    ready_max_age_seconds: u64,
) -> Result<(), ConfigError> {
    if poll_seconds == 0 || ready_max_age_seconds == 0 {
        return Err(ConfigError::new(
            "poll and readiness-age intervals must be greater than zero",
        ));
    }
    if !metrics_bind.ip().is_loopback() {
        return Err(ConfigError::new(
            "the Prometheus listener must bind to loopback",
        ));
    }
    if authentication_mode.is_strict() {
        let token = bearer_token.ok_or_else(|| {
            ConfigError::new(
                "Indexer Service bearer token is required in strict authentication mode",
            )
        })?;
        BearerToken::new(token).map_err(|error| ConfigError::new(error.to_string()))?;
    }
    if !http_bind.ip().is_loopback() && !upstream_tls_terminated {
        return Err(ConfigError::new(
            "a non-loopback API bind requires trusted upstream TLS",
        ));
    }
    Ok(())
}

fn validate_policy_migration(
    from_confirmation_depth: u64,
    from_reorg_retention: u64,
    migration_id: &str,
    reason: &str,
) -> Result<(), ConfigError> {
    if from_confirmation_depth == 0 || from_reorg_retention == 0 {
        return Err(ConfigError::new(
            "source confirmation depth and reorg retention must be greater than zero",
        ));
    }
    if migration_id.trim().is_empty() || migration_id.len() > 256 {
        return Err(ConfigError::new(
            "policy migration ID must contain 1 through 256 bytes",
        ));
    }
    if reason.trim().is_empty() || reason.len() > 4_096 {
        return Err(ConfigError::new(
            "policy migration reason must contain 1 through 4096 bytes",
        ));
    }
    Ok(())
}

fn validate_bitcoin_rpc_url(input: &str) -> Result<(), ConfigError> {
    let url = Url::parse(input).map_err(|_| ConfigError::new("Bitcoin Core RPC URL is invalid"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::new(
            "Bitcoin Core RPC credentials must be supplied through a redacted header",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigError::new(
            "Bitcoin Core RPC URL must not contain a query or fragment",
        ));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let loopback = match url.host() {
                Some(Host::Ipv4(address)) => address.is_loopback(),
                Some(Host::Ipv6(address)) => address.is_loopback(),
                Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
                None => false,
            };
            if loopback {
                Ok(())
            } else {
                Err(ConfigError::new(
                    "plain HTTP Bitcoin Core RPC is restricted to loopback",
                ))
            }
        }
        _ => Err(ConfigError::new(
            "Bitcoin Core RPC URL must use http:// or https://",
        )),
    }
}

fn decode_hash(input: &str) -> Result<BlockHash, ConfigError> {
    let hex = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .ok_or_else(|| ConfigError::new("genesis hash must have a 0x prefix"))?;
    if hex.len() != 64 {
        return Err(ConfigError::new(
            "genesis hash must encode exactly 32 bytes",
        ));
    }
    let mut bytes = Vec::with_capacity(32);
    for index in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[index..index + 2], 16)
            .map_err(|_| ConfigError::new("genesis hash contains non-hex characters"))?;
        bytes.push(byte);
    }
    Ok(BlockHash(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn provider_and_bearer_environment_values_are_hidden_from_help() {
        let command = Cli::command();
        let serve = command
            .find_subcommand("serve")
            .expect("serve subcommand must exist");

        for id in ["rpc_http_url", "rpc_ws_url", "bearer_token"] {
            let argument = serve
                .get_arguments()
                .find(|argument| argument.get_id().as_str() == id)
                .expect("sensitive serve argument must exist");
            assert!(
                argument.is_hide_env_values_set(),
                "{id} must hide its environment value from help output"
            );
        }
    }

    #[test]
    fn parses_exact_genesis_hash() {
        let parsed = decode_hash(&format!("0x{}", "ab".repeat(32)))
            .expect("an exact 32-byte hash must parse");
        assert_eq!(parsed, BlockHash(vec![0xab; 32]));
        assert!(decode_hash("ab").is_err());
        assert!(decode_hash(&format!("0x{}", "ab".repeat(31))).is_err());
    }

    #[test]
    fn parses_an_explicit_policy_migration() {
        let cli = Cli::try_parse_from([
            "indexer-worker",
            "migrate",
            "policy",
            "--database-path",
            "ix.db",
            "--network",
            "devnet",
            "--bootstrap-height",
            "1",
            "--confirmation-depth",
            "24",
            "--reorg-retention",
            "75",
            "--from-confirmation-depth",
            "12",
            "--from-reorg-retention",
            "50",
            "--migration-id",
            "depth-24-v1",
            "--reason",
            "increase the accounting safety margin",
            "--backup-path",
            "ix.backup",
        ])
        .expect("the complete policy migration command must parse");

        let Command::Migrate(MigrationOptions {
            command: MigrationCommand::Policy(options),
        }) = cli.command
        else {
            panic!("the policy migration subcommand must be selected");
        };
        options
            .validate()
            .expect("the parsed policy migration must validate");
        assert_eq!(options.from_confirmation_depth, 12);
        assert_eq!(options.from_reorg_retention, 50);
        assert_eq!(options.repository.confirmation_depth, 24);
        assert_eq!(options.repository.reorg_retention, 75);
    }

    #[test]
    fn parses_bitcoin_serve_with_explicit_policy_and_redacted_headers() {
        let cli = Cli::try_parse_from([
            "indexer-worker",
            "bitcoin",
            "serve",
            "--database-path",
            "bitcoin.db",
            "--network",
            "regtest",
            "--bootstrap-height",
            "0",
            "--confirmation-depth",
            "2",
            "--reorg-retention",
            "100",
            "--expected-genesis-hash",
            &"11".repeat(32),
            "--rpc-http-url",
            "http://127.0.0.1:18443",
            "--rpc-header",
            "authorization=Basic hidden",
            "--strict-authentication-mode",
            "true",
            "--bearer-token",
            "indexer-hidden",
        ])
        .expect("complete Bitcoin serve command must parse");

        let Command::Bitcoin(BitcoinOptions {
            command: BitcoinCommand::Serve(options),
        }) = cli.command
        else {
            panic!("Bitcoin serve command must be selected");
        };
        options
            .validate()
            .expect("complete Bitcoin serve options must validate");
        assert_eq!(options.repository.confirmation_depth, 2);
        assert_eq!(options.repository.reorg_retention, 100);
        assert_eq!(options.authentication_mode, AuthenticationMode::Strict);
        assert_eq!(
            options
                .source
                .parsed_rpc_headers()
                .expect("header must parse")
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["authorization"]
        );

        let command = Cli::command();
        let bitcoin = command
            .find_subcommand("bitcoin")
            .expect("Bitcoin subcommand must exist");
        let serve = bitcoin
            .find_subcommand("serve")
            .expect("Bitcoin serve subcommand must exist");
        let rpc_headers = serve
            .get_arguments()
            .find(|argument| argument.get_id().as_str() == "rpc_headers")
            .expect("Bitcoin RPC header argument must exist");
        assert!(rpc_headers.is_hide_env_values_set());
    }

    #[test]
    fn bitcoin_serve_has_no_hidden_policy_defaults() {
        let error = match Cli::try_parse_from([
            "indexer-worker",
            "bitcoin",
            "serve",
            "--database-path",
            "bitcoin.db",
            "--network",
            "regtest",
            "--bootstrap-height",
            "0",
            "--expected-genesis-hash",
            &"11".repeat(32),
            "--rpc-http-url",
            "http://127.0.0.1:18443",
            "--strict-authentication-mode",
            "true",
        ]) {
            Ok(_) => panic!("Bitcoin policy fields must be required"),
            Err(error) => error,
        };

        let message = error.to_string();
        assert!(message.contains("--confirmation-depth"));
        assert!(message.contains("--reorg-retention"));
    }

    #[test]
    fn bitcoin_rpc_transport_rejects_credential_urls_and_remote_plaintext() {
        assert!(validate_bitcoin_rpc_url("http://127.0.0.1:18443").is_ok());
        assert!(validate_bitcoin_rpc_url("https://bitcoin.example.test").is_ok());
        assert!(validate_bitcoin_rpc_url("http://bitcoin.example.test").is_err());
        assert!(validate_bitcoin_rpc_url("http://user:secret@127.0.0.1:18443").is_err());
        assert!(validate_bitcoin_rpc_url("http://127.0.0.1:18443?token=secret").is_err());
    }

    #[test]
    fn bitcoin_rpc_headers_reject_injection_and_duplicates() {
        let options = BitcoinSourceOptions {
            expected_genesis_hash: "11".repeat(32),
            rpc_http_url: "http://127.0.0.1:18443".to_owned(),
            rpc_headers: vec!["authorization=Basic hidden".to_owned()],
            rpc_timeout_seconds: 15,
            rpc_max_response_bytes: 268_435_456,
        };
        assert!(options.parsed_rpc_headers().is_ok());

        let mut injected = options.clone();
        injected.rpc_headers = vec!["authorization=Basic hidden\r\nx-evil: yes".to_owned()];
        assert!(injected.parsed_rpc_headers().is_err());

        let mut duplicate = options;
        duplicate.rpc_headers = vec![
            "Authorization=one".to_owned(),
            "authorization=two".to_owned(),
        ];
        assert!(duplicate.parsed_rpc_headers().is_err());

        let missing = BitcoinSourceOptions {
            expected_genesis_hash: "11".repeat(32),
            rpc_http_url: "http://127.0.0.1:18443".to_owned(),
            rpc_headers: Vec::new(),
            rpc_timeout_seconds: 15,
            rpc_max_response_bytes: 268_435_456,
        };
        assert!(missing.validate().is_err());
    }

    #[test]
    fn bitcoin_identity_configuration_requires_canonical_wire_values() {
        assert_eq!(
            parse_bitcoin_network("regtest").expect("canonical network must parse"),
            BitcoinNetwork::Regtest
        );
        for noncanonical in ["Regtest", " regtest", "regtest ", "main", "test"] {
            assert!(parse_bitcoin_network(noncanonical).is_err());
        }

        let options = BitcoinSourceOptions {
            expected_genesis_hash: "AB".repeat(32),
            rpc_http_url: "http://127.0.0.1:18443".to_owned(),
            rpc_headers: vec!["authorization=Basic hidden".to_owned()],
            rpc_timeout_seconds: 15,
            rpc_max_response_bytes: 268_435_456,
        };
        assert!(options.genesis_hash().is_err());
    }
}
