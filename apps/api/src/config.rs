use std::{
    fmt,
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
    /// Retry durable AwaitingWatch deposits against the Indexer Service.
    ReconcileWatches(ReconcileOptions),
    /// Mirror a bounded page sequence from the IX event feed into PS storage.
    IngestEvents(IngestOptions),
    /// Report independent ingestion and business-projection progress.
    ProjectionStatus(ProjectionStatusOptions),
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
                "Indexer bearer token must be non-empty and contain no whitespace",
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
pub struct IndexerOptions {
    /// Indexer API origin. The URL is redacted from Debug output.
    #[arg(long, env = "PS_INDEXER_URL")]
    pub indexer_url: IndexerEndpoint,

    #[arg(long, env = "PS_INDEXER_NETWORK")]
    pub network: String,

    #[arg(long, env = "PS_INDEXER_BEARER_TOKEN", hide_env_values = true)]
    pub bearer_token: Option<BearerSecret>,

    #[arg(long, env = "PS_INDEXER_TIMEOUT_SECONDS", default_value_t = 15)]
    pub request_timeout_seconds: u64,

    #[arg(long, env = "PS_INDEXER_RETRY_ATTEMPTS", default_value_t = 3)]
    pub retry_attempts: u32,

    #[arg(long, env = "PS_INDEXER_RETRY_INITIAL_MILLIS", default_value_t = 100)]
    pub retry_initial_millis: u64,

    #[arg(long, env = "PS_INDEXER_RETRY_MAX_MILLIS", default_value_t = 1_000)]
    pub retry_max_millis: u64,
}

impl IndexerOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
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
    use super::*;

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
}
