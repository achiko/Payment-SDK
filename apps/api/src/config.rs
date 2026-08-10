use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use clap::{Args, Parser, Subcommand};
use reqwest::Url;

const MAX_PAGE_SIZE: usize = 1_000;
const MAX_WORK_UNITS: usize = 10_000;
const MAX_RETRY_ATTEMPTS: u32 = 10;
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_WORKER_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Parser)]
#[command(
    name = "payment-api",
    version,
    about = "Durable Payment Service maintenance runtime"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the Payment Service HTTP API and durable workflow workers.
    Serve(ServeOptions),
    /// Create and verify a consistent Payment Service RocksDB backup.
    Backup(BackupOptions),
    /// Back up, migrate, validate, and bind an existing Payment Service database.
    Migrate(MigrationOptions),
    /// Retry durable AwaitingWatch deposits against the Indexer Service.
    ReconcileWatches(ReconcileOptions),
    /// Mirror a bounded page sequence from the IX event feed into PS storage.
    IngestEvents(IngestOptions),
    /// Report independent ingestion and business-projection progress.
    ProjectionStatus(ProjectionStatusOptions),
    /// Operate one native-Bitcoin Payment Service scope.
    Bitcoin(BitcoinOptions),
}

#[derive(Args)]
pub struct BitcoinOptions {
    #[command(subcommand)]
    pub command: BitcoinCommand,
}

#[derive(Subcommand)]
#[expect(
    clippy::large_enum_variant,
    reason = "CLI options are constructed once and boxing would obscure command dispatch"
)]
pub enum BitcoinCommand {
    /// Run the native-Bitcoin Payment Service and durable workflow workers.
    Serve(BitcoinServeOptions),
    /// Create and verify a consistent Bitcoin Payment Service backup.
    Backup(BackupOptions),
    /// Back up, migrate, validate, and bind a Bitcoin Payment Service database.
    Migrate(MigrationOptions),
    /// Retry durable Bitcoin AwaitingWatch deposits against IX.
    ReconcileWatches(ReconcileOptions),
    /// Mirror a bounded Bitcoin IX event-feed page sequence into PS storage.
    IngestEvents(IngestOptions),
    /// Report Bitcoin ingestion and business-projection progress.
    ProjectionStatus(ProjectionStatusOptions),
}

#[derive(Args, Clone, Debug)]
pub struct BackupOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    #[arg(long, env = "PS_BACKUP_PATH")]
    pub backup_path: PathBuf,
}

impl BackupOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.database.validate()?;
        validate_backup_path(&self.backup_path)
    }
}

#[derive(Args, Clone, Debug)]
pub struct MigrationOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    /// Verified safety backup created before the first mutation.
    #[arg(long, env = "PS_BACKUP_PATH")]
    pub backup_path: PathBuf,

    /// Versioned policy that will become the database's active policy identity.
    #[arg(long, env = "PS_POLICY_PATH")]
    pub policy_path: PathBuf,

    /// Operator assertion used to bind legacy deposits that lacked network identity.
    #[arg(long, env = "PS_MIGRATION_NETWORK")]
    pub network: String,
}

impl MigrationOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.database.validate()?;
        validate_backup_path(&self.backup_path)?;
        validate_policy_path(&self.policy_path)?;
        if self.network.trim().is_empty() {
            return Err(ConfigError::new("migration network must not be empty"));
        }
        Ok(())
    }
}

#[derive(Args, Clone)]
pub struct DatabaseOptions {
    /// PS-owned RocksDB directory. Never point this at the IX database.
    #[arg(long, env = "PS_DATABASE_PATH")]
    pub database_path: PathBuf,
}

impl DatabaseOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_database_path(&self.database_path)
    }
}

impl fmt::Debug for DatabaseOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseOptions")
            .field("database_path", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct IndexerEndpoint(Url);

impl IndexerEndpoint {
    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.0
    }

    fn is_loopback(&self) -> bool {
        let Some(host) = self.0.host_str() else {
            return false;
        };
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        let ip_literal = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        ip_literal
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    }

    fn validate_transport(&self, service: &str) -> Result<(), ConfigError> {
        if self.0.scheme() == "http" && !self.is_loopback() {
            return Err(ConfigError::new(format!(
                "{service} endpoint must use HTTPS unless its host is localhost or a loopback IP"
            )));
        }
        Ok(())
    }
}

impl FromStr for IndexerEndpoint {
    type Err = ConfigError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut url = Url::parse(input)
            .map_err(|_| ConfigError::new("Indexer endpoint is not a valid URL"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ConfigError::new(
                "Indexer endpoint must use http:// or https://",
            ));
        }
        if url.host_str().is_none() {
            return Err(ConfigError::new("Indexer endpoint must contain a host"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ConfigError::new(
                "Indexer endpoint must not contain embedded credentials",
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ConfigError::new(
                "Indexer endpoint must not contain a query or fragment",
            ));
        }
        if !matches!(url.path(), "" | "/") {
            return Err(ConfigError::new(
                "Indexer endpoint must not contain a path prefix",
            ));
        }
        url.set_path("/");
        Ok(Self(url))
    }
}

impl fmt::Debug for IndexerEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IndexerEndpoint([REDACTED])")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BearerSecret(String);

impl BearerSecret {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl FromStr for BearerSecret {
    type Err = ConfigError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty()
            || input
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(ConfigError::new(
                "bearer token must be non-empty and contain no whitespace",
            ));
        }
        Ok(Self(input.to_owned()))
    }
}

impl fmt::Debug for BearerSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerSecret([REDACTED])")
    }
}

#[derive(Args, Clone)]
pub struct WalletOptions {
    /// Stateless Wallet Service API origin. The URL is redacted from Debug output.
    #[arg(long, env = "PS_WALLET_URL")]
    pub wallet_url: IndexerEndpoint,

    #[arg(
        id = "wallet_bearer_token",
        long = "wallet-bearer-token",
        env = "PS_WALLET_BEARER_TOKEN",
        hide_env_values = true
    )]
    pub bearer_token: BearerSecret,

    #[arg(
        id = "wallet_request_timeout_seconds",
        long = "wallet-timeout-seconds",
        env = "PS_WALLET_TIMEOUT_SECONDS",
        default_value_t = 15
    )]
    pub request_timeout_seconds: u64,

    #[arg(
        id = "wallet_retry_attempts",
        long = "wallet-retry-attempts",
        env = "PS_WALLET_RETRY_ATTEMPTS",
        default_value_t = 3
    )]
    pub retry_attempts: u32,

    #[arg(
        id = "wallet_retry_initial_millis",
        long = "wallet-retry-initial-millis",
        env = "PS_WALLET_RETRY_INITIAL_MILLIS",
        default_value_t = 100
    )]
    pub retry_initial_millis: u64,

    #[arg(
        id = "wallet_retry_max_millis",
        long = "wallet-retry-max-millis",
        env = "PS_WALLET_RETRY_MAX_MILLIS",
        default_value_t = 1_000
    )]
    pub retry_max_millis: u64,
}

impl WalletOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.wallet_url.validate_transport("Wallet Service")?;
        let timeout = self.request_timeout();
        if timeout.is_zero() || timeout > MAX_REQUEST_TIMEOUT {
            return Err(ConfigError::new(
                "Wallet Service request timeout must be between 1 and 300 seconds",
            ));
        }
        if self.retry_attempts == 0 || self.retry_attempts > MAX_RETRY_ATTEMPTS {
            return Err(ConfigError::new(
                "Wallet Service retry attempts must be between 1 and 10",
            ));
        }
        if self.retry_initial_backoff() > self.retry_max_backoff() {
            return Err(ConfigError::new(
                "Wallet Service initial retry backoff must not exceed maximum retry backoff",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_seconds)
    }

    #[must_use]
    pub const fn retry_initial_backoff(&self) -> Duration {
        Duration::from_millis(self.retry_initial_millis)
    }

    #[must_use]
    pub const fn retry_max_backoff(&self) -> Duration {
        Duration::from_millis(self.retry_max_millis)
    }

    pub fn retry_attempts(&self) -> Result<NonZeroU32, ConfigError> {
        NonZeroU32::new(self.retry_attempts).ok_or_else(|| {
            ConfigError::new("Wallet Service retry attempts must be greater than zero")
        })
    }
}

impl fmt::Debug for WalletOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WalletOptions")
            .field("wallet_url", &self.wallet_url)
            .field("bearer_token", &self.bearer_token)
            .field("request_timeout_seconds", &self.request_timeout_seconds)
            .field("retry_attempts", &self.retry_attempts)
            .field("retry_initial_millis", &self.retry_initial_millis)
            .field("retry_max_millis", &self.retry_max_millis)
            .finish()
    }
}

#[derive(Args, Clone)]
pub struct IndexerOptions {
    /// Indexer API origin. The URL is redacted from Debug output.
    #[arg(long, env = "PS_INDEXER_URL")]
    pub indexer_url: IndexerEndpoint,

    #[arg(long, env = "PS_INDEXER_NETWORK")]
    pub network: String,

    #[arg(
        id = "indexer_bearer_token",
        long = "indexer-bearer-token",
        env = "PS_INDEXER_BEARER_TOKEN",
        hide_env_values = true
    )]
    pub bearer_token: Option<BearerSecret>,

    #[arg(
        id = "indexer_request_timeout_seconds",
        long = "indexer-timeout-seconds",
        env = "PS_INDEXER_TIMEOUT_SECONDS",
        default_value_t = 15
    )]
    pub request_timeout_seconds: u64,

    #[arg(
        id = "indexer_retry_attempts",
        long = "indexer-retry-attempts",
        env = "PS_INDEXER_RETRY_ATTEMPTS",
        default_value_t = 3
    )]
    pub retry_attempts: u32,

    #[arg(
        id = "indexer_retry_initial_millis",
        long = "indexer-retry-initial-millis",
        env = "PS_INDEXER_RETRY_INITIAL_MILLIS",
        default_value_t = 100
    )]
    pub retry_initial_millis: u64,

    #[arg(
        id = "indexer_retry_max_millis",
        long = "indexer-retry-max-millis",
        env = "PS_INDEXER_RETRY_MAX_MILLIS",
        default_value_t = 1_000
    )]
    pub retry_max_millis: u64,
}

impl IndexerOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.indexer_url.validate_transport("Indexer Service")?;
        if !self.indexer_url.is_loopback() && self.bearer_token.is_none() {
            return Err(ConfigError::new(
                "a non-loopback Indexer Service endpoint requires a bearer token",
            ));
        }
        if self.network.trim().is_empty() {
            return Err(ConfigError::new("Indexer network must not be empty"));
        }
        let timeout = self.request_timeout();
        if timeout.is_zero() || timeout > MAX_REQUEST_TIMEOUT {
            return Err(ConfigError::new(
                "Indexer request timeout must be between 1 and 300 seconds",
            ));
        }
        if self.retry_attempts == 0 || self.retry_attempts > MAX_RETRY_ATTEMPTS {
            return Err(ConfigError::new(
                "Indexer retry attempts must be between 1 and 10",
            ));
        }
        if self.retry_initial_backoff() > self.retry_max_backoff() {
            return Err(ConfigError::new(
                "initial retry backoff must not exceed maximum retry backoff",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_seconds)
    }

    #[must_use]
    pub const fn retry_initial_backoff(&self) -> Duration {
        Duration::from_millis(self.retry_initial_millis)
    }

    #[must_use]
    pub const fn retry_max_backoff(&self) -> Duration {
        Duration::from_millis(self.retry_max_millis)
    }

    pub fn retry_attempts(&self) -> Result<NonZeroU32, ConfigError> {
        NonZeroU32::new(self.retry_attempts)
            .ok_or_else(|| ConfigError::new("Indexer retry attempts must be greater than zero"))
    }
}

impl fmt::Debug for IndexerOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IndexerOptions")
            .field("indexer_url", &self.indexer_url)
            .field("network", &self.network)
            .field("bearer_token", &self.bearer_token)
            .field("request_timeout_seconds", &self.request_timeout_seconds)
            .field("retry_attempts", &self.retry_attempts)
            .field("retry_initial_millis", &self.retry_initial_millis)
            .field("retry_max_millis", &self.retry_max_millis)
            .finish()
    }
}

#[derive(Args, Clone)]
pub struct ServeOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    #[command(flatten)]
    pub indexer: IndexerOptions,

    #[command(flatten)]
    pub wallet: WalletOptions,

    /// Versioned Payment Service policy JSON.
    #[arg(long, env = "PS_POLICY_PATH")]
    pub policy_path: PathBuf,

    #[arg(long, env = "PS_HTTP_BIND", default_value = "127.0.0.1:8081")]
    pub http_bind: SocketAddr,

    #[arg(long, env = "PS_METRICS_BIND", default_value = "127.0.0.1:9091")]
    pub metrics_bind: SocketAddr,

    /// Assert that a trusted upstream proxy terminates TLS for the HTTP listener.
    #[arg(long, env = "PS_TLS_TERMINATED_UPSTREAM", default_value_t = false)]
    pub tls_terminated_upstream: bool,

    #[arg(long, env = "PS_API_BEARER_TOKEN", hide_env_values = true)]
    pub ordinary_bearer_token: BearerSecret,

    #[arg(long, env = "PS_ADMIN_BEARER_TOKEN", hide_env_values = true)]
    pub admin_bearer_token: BearerSecret,

    #[arg(long, env = "PS_WORKER_INTERVAL_MILLIS", default_value_t = 1_000)]
    pub worker_interval_millis: u64,

    #[arg(long, env = "PS_WORKER_PAGE_SIZE", default_value_t = 100)]
    pub worker_page_size: usize,

    #[arg(long, env = "PS_SHUTDOWN_GRACE_SECONDS", default_value_t = 10)]
    pub shutdown_grace_seconds: u64,
}

impl ServeOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.database.validate()?;
        self.indexer.validate()?;
        self.wallet.validate()?;
        validate_policy_path(&self.policy_path)?;
        validate_page_size(self.worker_page_size)?;

        if !self.metrics_bind.ip().is_loopback() {
            return Err(ConfigError::new(
                "Payment Service metrics listener must bind to loopback",
            ));
        }
        if !self.http_bind.ip().is_loopback() && !self.tls_terminated_upstream {
            return Err(ConfigError::new(
                "a non-loopback Payment Service listener requires trusted upstream TLS termination",
            ));
        }
        if self.ordinary_bearer_token == self.admin_bearer_token {
            return Err(ConfigError::new(
                "ordinary and administrator bearer tokens must be different",
            ));
        }
        if self.worker_interval().is_zero() || self.worker_interval() > MAX_WORKER_INTERVAL {
            return Err(ConfigError::new(
                "worker interval must be between 1 millisecond and 300 seconds",
            ));
        }
        if self.shutdown_grace().is_zero() || self.shutdown_grace() > MAX_REQUEST_TIMEOUT {
            return Err(ConfigError::new(
                "shutdown grace must be between 1 and 300 seconds",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn worker_interval(&self) -> Duration {
        Duration::from_millis(self.worker_interval_millis)
    }

    #[must_use]
    pub const fn shutdown_grace(&self) -> Duration {
        Duration::from_secs(self.shutdown_grace_seconds)
    }
}

impl fmt::Debug for ServeOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServeOptions")
            .field("database", &self.database)
            .field("indexer", &self.indexer)
            .field("wallet", &self.wallet)
            .field("policy_path", &"[REDACTED]")
            .field("http_bind", &self.http_bind)
            .field("metrics_bind", &self.metrics_bind)
            .field("tls_terminated_upstream", &self.tls_terminated_upstream)
            .field("ordinary_bearer_token", &self.ordinary_bearer_token)
            .field("admin_bearer_token", &self.admin_bearer_token)
            .field("worker_interval_millis", &self.worker_interval_millis)
            .field("worker_page_size", &self.worker_page_size)
            .field("shutdown_grace_seconds", &self.shutdown_grace_seconds)
            .finish()
    }
}

/// Bitcoin uses the common PS transport and worker controls, with stricter IX
/// authentication and canonical-network requirements.
#[derive(Args, Clone)]
pub struct BitcoinServeOptions {
    #[command(flatten)]
    pub common: ServeOptions,
}

impl BitcoinServeOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.common.validate()?;
        if self.common.indexer.bearer_token.is_none() {
            return Err(ConfigError::new(
                "Bitcoin Indexer Service authentication is required even on loopback",
            ));
        }
        match self.common.indexer.network.as_str() {
            "mainnet" | "testnet3" | "testnet4" | "signet" | "regtest" => Ok(()),
            _ => Err(ConfigError::new(
                "Bitcoin network must be mainnet, testnet3, testnet4, signet, or regtest",
            )),
        }
    }
}

impl fmt::Debug for BitcoinServeOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitcoinServeOptions")
            .field("common", &self.common)
            .finish()
    }
}

#[derive(Args, Clone, Debug)]
pub struct ReconcileOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    #[command(flatten)]
    pub indexer: IndexerOptions,

    #[arg(long, default_value_t = 100)]
    pub page_size: usize,

    #[arg(long, default_value_t = 100)]
    pub max_batches: usize,
}

impl ReconcileOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.database.validate()?;
        self.indexer.validate()?;
        validate_page_size(self.page_size)?;
        validate_bound(self.max_batches, "maximum reconcile batches")
    }
}

#[derive(Args, Clone, Debug)]
pub struct IngestOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    #[command(flatten)]
    pub indexer: IndexerOptions,

    #[arg(long, default_value_t = 100)]
    pub page_size: usize,

    #[arg(long, default_value_t = 100)]
    pub max_pages: usize,
}

impl IngestOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.database.validate()?;
        self.indexer.validate()?;
        validate_page_size(self.page_size)?;
        validate_bound(self.max_pages, "maximum ingestion pages")
    }
}

#[derive(Args, Clone, Debug)]
pub struct ProjectionStatusOptions {
    #[command(flatten)]
    pub database: DatabaseOptions,

    /// Maximum mirrored events inspected after the projection cursor.
    #[arg(long, default_value_t = 100)]
    pub sample_limit: usize,
}

impl ProjectionStatusOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.database.validate()?;
        validate_page_size(self.sample_limit)
    }
}

fn validate_database_path(path: &Path) -> Result<(), ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::new("PS database path must not be empty"));
    }
    if path == Path::new("/") {
        return Err(ConfigError::new(
            "PS database path must not be filesystem root",
        ));
    }
    Ok(())
}

fn validate_policy_path(path: &Path) -> Result<(), ConfigError> {
    if path.as_os_str().is_empty() || path == Path::new("/") {
        return Err(ConfigError::new(
            "Payment Service policy path must identify a file",
        ));
    }
    Ok(())
}

fn validate_backup_path(path: &Path) -> Result<(), ConfigError> {
    if path.as_os_str().is_empty() || path == Path::new("/") {
        return Err(ConfigError::new(
            "Payment Service backup path must identify a dedicated directory",
        ));
    }
    Ok(())
}

fn validate_page_size(value: usize) -> Result<(), ConfigError> {
    if value == 0 || value > MAX_PAGE_SIZE {
        return Err(ConfigError::new("page size must be between 1 and 1000"));
    }
    Ok(())
}

fn validate_bound(value: usize, name: &str) -> Result<(), ConfigError> {
    if value == 0 || value > MAX_WORK_UNITS {
        Err(ConfigError::new(format!(
            "{name} must be between 1 and {MAX_WORK_UNITS}"
        )))
    } else {
        Ok(())
    }
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

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn clap_command_has_no_duplicate_argument_ids() {
        Cli::command().debug_assert();
    }

    fn indexer_options(endpoint: &str, bearer_token: Option<&str>) -> IndexerOptions {
        IndexerOptions {
            indexer_url: endpoint.parse().expect("test URL must parse"),
            network: "test".to_owned(),
            bearer_token: bearer_token.map(|token| token.parse().expect("test token must parse")),
            request_timeout_seconds: 15,
            retry_attempts: 3,
            retry_initial_millis: 100,
            retry_max_millis: 1_000,
        }
    }

    fn wallet_options(endpoint: &str) -> WalletOptions {
        WalletOptions {
            wallet_url: endpoint.parse().expect("test URL must parse"),
            bearer_token: "wallet-secret".parse().expect("test token must parse"),
            request_timeout_seconds: 15,
            retry_attempts: 3,
            retry_initial_millis: 100,
            retry_max_millis: 1_000,
        }
    }

    #[test]
    fn endpoint_and_token_are_redacted_from_debug_output() {
        let options = IndexerOptions {
            indexer_url: "https://private.example.invalid"
                .parse()
                .expect("test URL must parse"),
            network: "sepolia".to_owned(),
            bearer_token: Some("very-secret-token".parse().expect("token must parse")),
            request_timeout_seconds: 15,
            retry_attempts: 3,
            retry_initial_millis: 100,
            retry_max_millis: 1_000,
        };

        let output = format!("{options:?}");
        assert!(!output.contains("private.example.invalid"));
        assert!(!output.contains("very-secret-token"));
        assert!(output.contains("[REDACTED]"));
    }

    #[test]
    fn endpoint_rejects_credentials_paths_queries_and_non_http_schemes() {
        assert!("ftp://example.invalid".parse::<IndexerEndpoint>().is_err());
        assert!(
            "https://user:pass@example.invalid"
                .parse::<IndexerEndpoint>()
                .is_err()
        );
        assert!(
            "https://example.invalid/prefix"
                .parse::<IndexerEndpoint>()
                .is_err()
        );
        assert!(
            "https://example.invalid?token=secret"
                .parse::<IndexerEndpoint>()
                .is_err()
        );
    }

    #[test]
    fn plaintext_service_endpoints_are_limited_to_loopback_hosts() {
        for endpoint in [
            "http://localhost:8080",
            "http://127.0.0.2:8080",
            "http://[::1]:8080",
        ] {
            indexer_options(endpoint, None)
                .validate()
                .expect("loopback Indexer endpoint may use plaintext without authentication");
            wallet_options(endpoint)
                .validate()
                .expect("loopback Wallet endpoint may use plaintext");
        }

        let indexer_error = indexer_options(
            "http://indexer.example.invalid:8080",
            Some("indexer-secret"),
        )
        .validate()
        .expect_err("non-loopback Indexer plaintext must be rejected");
        assert!(indexer_error.to_string().contains("HTTPS"));

        let wallet_error = wallet_options("http://wallet.example.invalid:8080")
            .validate()
            .expect_err("non-loopback Wallet plaintext must be rejected");
        assert!(wallet_error.to_string().contains("HTTPS"));

        let deceptive_host = indexer_options(
            "http://localhost.example.invalid:8080",
            Some("indexer-secret"),
        )
        .validate()
        .expect_err("a domain containing localhost is not itself a loopback host");
        assert!(deceptive_host.to_string().contains("HTTPS"));
    }

    #[test]
    fn non_loopback_indexer_requires_bearer_authentication() {
        let missing = indexer_options("https://indexer.example.invalid", None)
            .validate()
            .expect_err("remote Indexer endpoint without authentication must fail");
        assert!(missing.to_string().contains("requires a bearer token"));

        indexer_options("https://indexer.example.invalid", Some("indexer-secret"))
            .validate()
            .expect("remote HTTPS Indexer endpoint with authentication must validate");
        wallet_options("https://wallet.example.invalid")
            .validate()
            .expect("remote HTTPS Wallet endpoint must validate");
    }

    #[test]
    fn retry_and_paging_bounds_fail_closed() {
        let options = IndexerOptions {
            indexer_url: "http://127.0.0.1:8080"
                .parse()
                .expect("test URL must parse"),
            network: "test".to_owned(),
            bearer_token: None,
            request_timeout_seconds: 0,
            retry_attempts: 0,
            retry_initial_millis: 200,
            retry_max_millis: 100,
        };
        assert!(options.validate().is_err());
        assert!(validate_page_size(0).is_err());
        assert!(validate_page_size(1_001).is_err());
        assert!(validate_bound(0, "test bound").is_err());
    }

    #[test]
    fn serve_configuration_requires_distinct_tokens_and_safe_bindings() {
        let mut options = ServeOptions {
            database: DatabaseOptions {
                database_path: PathBuf::from("/tmp/payment-service-test"),
            },
            indexer: IndexerOptions {
                indexer_url: "http://127.0.0.1:8080"
                    .parse()
                    .expect("test URL must parse"),
                network: "test".to_owned(),
                bearer_token: None,
                request_timeout_seconds: 15,
                retry_attempts: 3,
                retry_initial_millis: 100,
                retry_max_millis: 1_000,
            },
            wallet: WalletOptions {
                wallet_url: "http://127.0.0.1:8082"
                    .parse()
                    .expect("test URL must parse"),
                bearer_token: "wallet-secret".parse().expect("token must parse"),
                request_timeout_seconds: 15,
                retry_attempts: 3,
                retry_initial_millis: 100,
                retry_max_millis: 1_000,
            },
            policy_path: PathBuf::from("policy.json"),
            http_bind: "127.0.0.1:8081".parse().expect("bind must parse"),
            metrics_bind: "127.0.0.1:9091".parse().expect("bind must parse"),
            tls_terminated_upstream: false,
            ordinary_bearer_token: "ordinary-secret".parse().expect("token must parse"),
            admin_bearer_token: "admin-secret".parse().expect("token must parse"),
            worker_interval_millis: 1_000,
            worker_page_size: 100,
            shutdown_grace_seconds: 10,
        };
        options
            .validate()
            .expect("safe configuration must validate");

        options.admin_bearer_token = options.ordinary_bearer_token.clone();
        assert!(options.validate().is_err());
        options.admin_bearer_token = "admin-secret".parse().expect("token must parse");
        options.http_bind = "0.0.0.0:8081".parse().expect("bind must parse");
        assert!(options.validate().is_err());
        options.tls_terminated_upstream = true;
        options.metrics_bind = "0.0.0.0:9091".parse().expect("bind must parse");
        assert!(options.validate().is_err());
    }

    #[test]
    fn bitcoin_serve_requires_authenticated_ix_and_canonical_network() {
        let common = ServeOptions {
            database: DatabaseOptions {
                database_path: PathBuf::from("/tmp/bitcoin-payment-service-test"),
            },
            indexer: IndexerOptions {
                indexer_url: "http://127.0.0.1:18080"
                    .parse()
                    .expect("test URL must parse"),
                network: "regtest".to_owned(),
                bearer_token: Some("indexer-secret".parse().expect("token must parse")),
                request_timeout_seconds: 15,
                retry_attempts: 3,
                retry_initial_millis: 100,
                retry_max_millis: 1_000,
            },
            wallet: WalletOptions {
                wallet_url: "http://127.0.0.1:18082"
                    .parse()
                    .expect("test URL must parse"),
                bearer_token: "wallet-secret".parse().expect("token must parse"),
                request_timeout_seconds: 15,
                retry_attempts: 3,
                retry_initial_millis: 100,
                retry_max_millis: 1_000,
            },
            policy_path: PathBuf::from("bitcoin-policy.json"),
            http_bind: "127.0.0.1:18081".parse().expect("bind must parse"),
            metrics_bind: "127.0.0.1:19091".parse().expect("bind must parse"),
            tls_terminated_upstream: false,
            ordinary_bearer_token: "ordinary-secret".parse().expect("token must parse"),
            admin_bearer_token: "admin-secret".parse().expect("token must parse"),
            worker_interval_millis: 1_000,
            worker_page_size: 100,
            shutdown_grace_seconds: 10,
        };
        BitcoinServeOptions {
            common: common.clone(),
        }
        .validate()
        .expect("canonical authenticated Bitcoin configuration must validate");

        let mut missing_auth = common.clone();
        missing_auth.indexer.bearer_token = None;
        let error = BitcoinServeOptions {
            common: missing_auth,
        }
        .validate()
        .expect_err("Bitcoin IX authentication is mandatory even on loopback");
        assert!(error.to_string().contains("authentication is required"));

        let mut noncanonical = common;
        noncanonical.indexer.network = "test".to_owned();
        let error = BitcoinServeOptions {
            common: noncanonical,
        }
        .validate()
        .expect_err("Bitcoin aliases must not cross the PS boundary");
        assert!(error.to_string().contains("mainnet"));
    }
}
