mod env_file;

use std::{env, error::Error, fmt, net::SocketAddr, path::PathBuf, str::FromStr};

use chain_bitcoin::BitcoinNetwork;
use indexer_worker::{BitcoinIndexerService, BitcoinIndexerServiceConfig, PrometheusTelemetry};

const DEFAULT_NETWORK: &str = "regtest";
const DEFAULT_DATABASE_PATH: &str = "./tmp/bitcoin-indexer-demo-db";
const DEFAULT_CORE_RPC_URL: &str = "http://127.0.0.1:18443";
const DEFAULT_HTTP_BIND: &str = "127.0.0.1:18080";
const DEFAULT_METRICS_BIND: &str = "127.0.0.1:19090";
const DEFAULT_BOOTSTRAP_HEIGHT: &str = "0";
const DEFAULT_CONFIRMATION_DEPTH: &str = "2";
const DEFAULT_REORG_RETENTION: &str = "100";
const DEFAULT_RPC_TIMEOUT_SECONDS: &str = "15";
const DEFAULT_POLL_SECONDS: &str = "5";
const DEFAULT_READY_MAX_LAG: &str = "2";
const DEFAULT_READY_MAX_AGE_SECONDS: &str = "30";

type MainResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

struct DemoConfig {
    network: BitcoinNetwork,
    database_path: PathBuf,
    bootstrap_height: u64,
    confirmation_depth: u64,
    reorg_retention: u64,
    expected_genesis_hash: String,
    core_rpc_url: String,
    core_authorization: String,
    rpc_timeout_seconds: u64,
    http_bind: SocketAddr,
    metrics_bind: SocketAddr,
    indexer_bearer_token: String,
    poll_seconds: u64,
    ready_max_lag: u64,
    ready_max_age_seconds: u64,
}

impl DemoConfig {
    fn from_env() -> Result<Self, DemoConfigError> {
        Ok(Self {
            network: parse_network(&env_or_default("DEMO_BITCOIN_NETWORK", DEFAULT_NETWORK)?)?,
            database_path: PathBuf::from(env_or_default(
                "DEMO_BITCOIN_INDEXER_DATABASE_PATH",
                DEFAULT_DATABASE_PATH,
            )?),
            bootstrap_height: read_u64_env(
                "DEMO_BITCOIN_BOOTSTRAP_HEIGHT",
                DEFAULT_BOOTSTRAP_HEIGHT,
            )?,
            confirmation_depth: read_positive_u64_env(
                "DEMO_BITCOIN_CONFIRMATION_DEPTH",
                DEFAULT_CONFIRMATION_DEPTH,
            )?,
            reorg_retention: read_positive_u64_env(
                "DEMO_BITCOIN_REORG_RETENTION",
                DEFAULT_REORG_RETENTION,
            )?,
            expected_genesis_hash: required_env("DEMO_BITCOIN_EXPECTED_GENESIS_HASH")?,
            core_rpc_url: env_or_default("DEMO_BITCOIN_CORE_RPC_URL", DEFAULT_CORE_RPC_URL)?,
            core_authorization: required_env("DEMO_BITCOIN_CORE_AUTHORIZATION")?,
            rpc_timeout_seconds: read_positive_u64_env(
                "DEMO_BITCOIN_RPC_TIMEOUT_SECONDS",
                DEFAULT_RPC_TIMEOUT_SECONDS,
            )?,
            http_bind: read_socket_addr_env("DEMO_BITCOIN_INDEXER_HTTP_BIND", DEFAULT_HTTP_BIND)?,
            metrics_bind: read_socket_addr_env(
                "DEMO_BITCOIN_INDEXER_METRICS_BIND",
                DEFAULT_METRICS_BIND,
            )?,
            indexer_bearer_token: required_env("DEMO_BITCOIN_IX_BEARER_TOKEN")?,
            poll_seconds: read_positive_u64_env("DEMO_BITCOIN_POLL_SECONDS", DEFAULT_POLL_SECONDS)?,
            ready_max_lag: read_u64_env("DEMO_BITCOIN_READY_MAX_LAG", DEFAULT_READY_MAX_LAG)?,
            ready_max_age_seconds: read_positive_u64_env(
                "DEMO_BITCOIN_READY_MAX_AGE_SECONDS",
                DEFAULT_READY_MAX_AGE_SECONDS,
            )?,
        })
    }

    fn into_service_config(self) -> BitcoinIndexerServiceConfig {
        let mut config = BitcoinIndexerServiceConfig::new(
            self.database_path,
            self.network,
            self.bootstrap_height,
            self.confirmation_depth,
            self.reorg_retention,
            self.expected_genesis_hash,
            self.core_rpc_url,
        );
        config.rpc_headers = vec![format!("authorization={}", self.core_authorization)];
        config.rpc_timeout_seconds = self.rpc_timeout_seconds;
        config.http_bind = self.http_bind;
        config.metrics_bind = self.metrics_bind;
        config.bearer_token = Some(self.indexer_bearer_token);
        config.poll_seconds = self.poll_seconds;
        config.ready_max_lag = self.ready_max_lag;
        config.ready_max_age_seconds = self.ready_max_age_seconds;
        config
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DemoConfigError {
    message: String,
}

impl DemoConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for DemoConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DemoConfigError {}

fn required_env(name: &'static str) -> Result<String, DemoConfigError> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) | Err(env::VarError::NotPresent) => Err(DemoConfigError::new(format!(
            "required environment variable {name} is missing or empty"
        ))),
        Err(env::VarError::NotUnicode(_)) => Err(DemoConfigError::new(format!(
            "environment variable {name} must contain valid Unicode"
        ))),
    }
}

fn env_or_default(name: &'static str, default: &'static str) -> Result<String, DemoConfigError> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) => Err(DemoConfigError::new(format!(
            "environment variable {name} must not be empty"
        ))),
        Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(env::VarError::NotUnicode(_)) => Err(DemoConfigError::new(format!(
            "environment variable {name} must contain valid Unicode"
        ))),
    }
}

fn parse_network(value: &str) -> Result<BitcoinNetwork, DemoConfigError> {
    match value {
        "mainnet" => Ok(BitcoinNetwork::Mainnet),
        "testnet3" => Ok(BitcoinNetwork::Testnet3),
        "testnet4" => Ok(BitcoinNetwork::Testnet4),
        "signet" => Ok(BitcoinNetwork::Signet),
        "regtest" => Ok(BitcoinNetwork::Regtest),
        _ => Err(DemoConfigError::new(
            "DEMO_BITCOIN_NETWORK must be mainnet, testnet3, testnet4, signet, or regtest",
        )),
    }
}

fn read_u64_env(name: &'static str, default: &'static str) -> Result<u64, DemoConfigError> {
    parse_u64(name, &env_or_default(name, default)?)
}

fn read_positive_u64_env(
    name: &'static str,
    default: &'static str,
) -> Result<u64, DemoConfigError> {
    let value = read_u64_env(name, default)?;
    if value == 0 {
        return Err(DemoConfigError::new(format!(
            "environment variable {name} must be greater than zero"
        )));
    }
    Ok(value)
}

fn parse_u64(name: &'static str, value: &str) -> Result<u64, DemoConfigError> {
    value.parse::<u64>().map_err(|_| {
        DemoConfigError::new(format!(
            "environment variable {name} must be an unsigned decimal integer"
        ))
    })
}

fn read_socket_addr_env(
    name: &'static str,
    default: &'static str,
) -> Result<SocketAddr, DemoConfigError> {
    let value = env_or_default(name, default)?;
    SocketAddr::from_str(&value).map_err(|_| {
        DemoConfigError::new(format!(
            "environment variable {name} must use IP:port syntax"
        ))
    })
}

#[tokio::main]
async fn main() -> MainResult<()> {
    env_file::load_demo_env()?;

    let demo = DemoConfig::from_env()?;
    println!(
        "Starting block-only Bitcoin indexer sample for {} (API {}, metrics {})",
        demo.network.canonical_name(),
        demo.http_bind,
        demo.metrics_bind
    );

    let service = BitcoinIndexerService::new(demo.into_service_config())?;
    let telemetry = PrometheusTelemetry::install()?;
    service.run(telemetry).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_canonical_bitcoin_network_names() {
        assert_eq!(parse_network("mainnet"), Ok(BitcoinNetwork::Mainnet));
        assert_eq!(parse_network("testnet3"), Ok(BitcoinNetwork::Testnet3));
        assert_eq!(parse_network("testnet4"), Ok(BitcoinNetwork::Testnet4));
        assert_eq!(parse_network("signet"), Ok(BitcoinNetwork::Signet));
        assert_eq!(parse_network("regtest"), Ok(BitcoinNetwork::Regtest));

        for noncanonical in ["main", "test", "Regtest", " regtest", "regtest "] {
            assert!(parse_network(noncanonical).is_err());
        }
    }

    #[test]
    fn parses_unsigned_decimal_configuration_without_echoing_input() {
        assert_eq!(parse_u64("DEMO_VALUE", "0"), Ok(0));
        assert_eq!(parse_u64("DEMO_VALUE", "42"), Ok(42));

        let error = parse_u64("DEMO_VALUE", "not-a-number")
            .expect_err("non-decimal configuration must be rejected");
        assert_eq!(
            error.to_string(),
            "environment variable DEMO_VALUE must be an unsigned decimal integer"
        );
    }
}
