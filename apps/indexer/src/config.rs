use std::{net::SocketAddr, path::PathBuf, time::Duration};

use chain_identity::ChainId;
use clap::{Args, Parser, Subcommand};
use indexing::{BlockHash, BlockHeight, ConfirmationPolicy, IndexError, IndexScope};

#[derive(Parser)]
#[command(name = "indexer-worker", version, about = "Ethereum Indexer Service")]
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
pub struct ServeOptions {
    #[command(flatten)]
    pub repository: RepositoryOptions,

    #[command(flatten)]
    pub source: SourceOptions,

    #[arg(long, env = "IX_HTTP_BIND", default_value = "127.0.0.1:8080")]
    pub http_bind: SocketAddr,

    #[arg(long, env = "IX_METRICS_BIND", default_value = "127.0.0.1:9090")]
    pub metrics_bind: SocketAddr,

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
        if self.poll_seconds == 0 || self.ready_max_age_seconds == 0 {
            return Err(ConfigError::new(
                "poll and readiness-age intervals must be greater than zero",
            ));
        }
        if !self.metrics_bind.ip().is_loopback() {
            return Err(ConfigError::new(
                "the Prometheus listener must bind to loopback",
            ));
        }
        if !self.http_bind.ip().is_loopback()
            && (self.bearer_token.is_none() || !self.upstream_tls_terminated)
        {
            return Err(ConfigError::new(
                "a non-loopback API bind requires a bearer token and trusted upstream TLS",
            ));
        }
        Ok(())
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
pub struct GenerationOptions {
    #[command(flatten)]
    pub repository: RepositoryOptions,

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
}
