use std::{
    error::Error,
    fmt,
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    time::Duration,
};

use chain_ethereum::{EthereumHttpRpcBuildError, EthereumHttpRpcConfig, EthereumRpcLimits, Wei};
use chain_identity::AtomicAmount;
use clap::{Args, Parser, Subcommand};
use http_support::{
    BearerToken, HttpServerConfig, HttpServerConfigError, HttpTransportBuildError, RequestLimits,
    RetryPolicy, TransportSecurity,
};
use signer_remote::{
    BearerSecret, RemoteRetryPolicy, RemoteSignerConfig, RemoteSignerConfigError,
    RemoteSignerEndpoint,
};

#[derive(Parser)]
#[command(
    name = "wallet-worker",
    version,
    about = "Stateless Ethereum Wallet Service"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Serve authenticated stateless Ethereum wallet operations.
    Serve(ServeOptions),
}

#[derive(Args, Clone)]
pub struct ServeOptions {
    #[arg(long, env = "WS_ETHEREUM_CHAIN_ID")]
    pub chain_id: u64,

    /// Ethereum HTTP JSON-RPC endpoint. Redacted from diagnostics.
    #[arg(long, env = "WS_ETHEREUM_RPC_URL", hide_env_values = true)]
    pub rpc_url: String,

    /// Repeatable `name=value` RPC header. Values are always redacted.
    #[arg(long = "rpc-header", hide_env_values = true)]
    pub rpc_headers: Vec<String>,

    #[arg(long, env = "WS_RPC_TIMEOUT_SECONDS", default_value_t = 15)]
    pub rpc_timeout_seconds: u64,

    #[arg(long, env = "WS_RPC_MAX_RESPONSE_BYTES", default_value_t = 4_194_304)]
    pub rpc_max_response_bytes: usize,

    #[arg(long, env = "WS_RPC_RETRY_ATTEMPTS", default_value_t = 3)]
    pub rpc_retry_attempts: u32,

    #[arg(long, env = "WS_RPC_RETRY_INITIAL_MILLIS", default_value_t = 100)]
    pub rpc_retry_initial_millis: u64,

    #[arg(long, env = "WS_RPC_RETRY_MAX_MILLIS", default_value_t = 2_000)]
    pub rpc_retry_max_millis: u64,

    #[arg(long, env = "WS_MAX_INPUT_BYTES", default_value_t = 4_096)]
    pub max_input_bytes: usize,

    #[arg(long, env = "WS_GAS_MARGIN_BPS", default_value_t = 2_000)]
    pub gas_margin_basis_points: u32,

    #[arg(long, env = "WS_MAX_GAS_LIMIT", default_value_t = 500_000)]
    pub max_gas_limit: u64,

    #[arg(long, env = "WS_MAX_FEE_PER_GAS_WEI", default_value = "500000000000")]
    pub max_fee_per_gas: String,

    #[arg(
        long,
        env = "WS_MAX_PRIORITY_FEE_PER_GAS_WEI",
        default_value = "100000000000"
    )]
    pub max_priority_fee_per_gas: String,

    #[arg(
        long,
        env = "WS_MAX_TOTAL_FEE_WEI",
        default_value = "100000000000000000"
    )]
    pub max_total_fee: String,

    /// Remote custody base URL. Redacted from diagnostics.
    #[arg(long, env = "WS_CUSTODY_URL", hide_env_values = true)]
    pub custody_url: String,

    #[arg(long, env = "WS_CUSTODY_BEARER_TOKEN", hide_env_values = true)]
    pub custody_bearer_token: String,

    #[arg(long, env = "WS_CUSTODY_CONNECT_TIMEOUT_SECONDS", default_value_t = 5)]
    pub custody_connect_timeout_seconds: u64,

    #[arg(long, env = "WS_CUSTODY_REQUEST_TIMEOUT_SECONDS", default_value_t = 30)]
    pub custody_request_timeout_seconds: u64,

    #[arg(
        long,
        env = "WS_CUSTODY_MAX_RESPONSE_BYTES",
        default_value_t = 1_048_576
    )]
    pub custody_max_response_bytes: usize,

    #[arg(long, env = "WS_CUSTODY_RETRY_ATTEMPTS", default_value_t = 3)]
    pub custody_retry_attempts: u32,

    #[arg(long, env = "WS_CUSTODY_RETRY_INITIAL_MILLIS", default_value_t = 100)]
    pub custody_retry_initial_millis: u64,

    #[arg(long, env = "WS_CUSTODY_RETRY_MAX_MILLIS", default_value_t = 2_000)]
    pub custody_retry_max_millis: u64,

    #[arg(long, env = "WS_HTTP_BIND", default_value = "127.0.0.1:8082")]
    pub http_bind: SocketAddr,

    /// Assert that a trusted upstream terminates TLS for a non-loopback bind.
    #[arg(long, env = "WS_UPSTREAM_TLS_TERMINATED", default_value_t = false)]
    pub upstream_tls_terminated: bool,

    #[arg(long, env = "WS_BEARER_TOKEN", hide_env_values = true)]
    pub bearer_token: String,

    #[arg(long, env = "WS_MAX_REQUEST_BODY_BYTES", default_value_t = 65_536)]
    pub max_request_body_bytes: usize,

    #[arg(long, env = "WS_SHUTDOWN_GRACE_SECONDS", default_value_t = 30)]
    pub shutdown_grace_seconds: u64,
}

impl ServeOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.chain_id == 0 {
            return Err(ConfigError::new("Ethereum chain ID must be non-zero"));
        }
        validate_http_endpoint(&self.rpc_url, "Ethereum RPC")?;
        self.parsed_rpc_headers()?;
        if self.rpc_timeout_seconds == 0
            || self.rpc_retry_initial_millis == 0
            || self.rpc_retry_max_millis == 0
        {
            return Err(ConfigError::new(
                "Ethereum RPC timeouts and retry delays must be greater than zero",
            ));
        }
        if self.rpc_retry_initial_millis > self.rpc_retry_max_millis {
            return Err(ConfigError::new(
                "Ethereum RPC initial retry delay must not exceed its maximum",
            ));
        }
        self.rpc_configuration()?;
        self.custody_configuration()?;
        if self.bearer_token.is_empty() {
            return Err(ConfigError::new(
                "Wallet Service bearer token must not be empty",
            ));
        }
        if !self.http_bind.ip().is_loopback() && !self.upstream_tls_terminated {
            return Err(ConfigError::new(
                "a non-loopback Wallet Service bind requires trusted upstream TLS",
            ));
        }
        if self.max_request_body_bytes == 0 || self.shutdown_grace_seconds == 0 {
            return Err(ConfigError::new(
                "request-body and shutdown-grace limits must be greater than zero",
            ));
        }
        self.server_configuration()?;
        Ok(())
    }

    pub fn rpc_configuration(&self) -> Result<EthereumHttpRpcConfig, ConfigError> {
        let limits = EthereumRpcLimits::new(
            self.max_input_bytes,
            self.gas_margin_basis_points,
            self.max_gas_limit,
            decimal_wei(&self.max_fee_per_gas, "maximum fee per gas")?,
            decimal_wei(
                &self.max_priority_fee_per_gas,
                "maximum priority fee per gas",
            )?,
            decimal_wei(&self.max_total_fee, "maximum total fee")?,
        )?;
        let retry = RetryPolicy::new(
            NonZeroU32::new(self.rpc_retry_attempts)
                .ok_or_else(|| ConfigError::new("RPC retry attempts must be greater than zero"))?,
            Duration::from_millis(self.rpc_retry_initial_millis),
            Duration::from_millis(self.rpc_retry_max_millis),
        )?;
        let mut config = EthereumHttpRpcConfig::new(
            self.rpc_url.clone(),
            self.chain_id,
            Duration::from_secs(self.rpc_timeout_seconds),
            self.rpc_max_response_bytes,
            retry,
            limits,
        )?;
        for (name, value) in self.parsed_rpc_headers()? {
            config = config.with_header(name, value);
        }
        Ok(config)
    }

    pub fn custody_configuration(&self) -> Result<RemoteSignerConfig, ConfigError> {
        let endpoint = RemoteSignerEndpoint::new(&self.custody_url)?;
        let secret = BearerSecret::new(&self.custody_bearer_token)?;
        let retry = RemoteRetryPolicy::new(
            self.custody_retry_attempts,
            Duration::from_millis(self.custody_retry_initial_millis),
            Duration::from_millis(self.custody_retry_max_millis),
        )?;
        RemoteSignerConfig::new(endpoint, secret)
            .with_timeouts(
                Duration::from_secs(self.custody_connect_timeout_seconds),
                Duration::from_secs(self.custody_request_timeout_seconds),
            )?
            .with_max_response_bytes(self.custody_max_response_bytes)
            .map(|config| config.with_retry_policy(retry))
            .map_err(Into::into)
    }

    pub fn server_configuration(&self) -> Result<HttpServerConfig, ConfigError> {
        let security = if self.http_bind.ip().is_loopback() {
            TransportSecurity::PlaintextLoopback
        } else {
            TransportSecurity::TlsTerminatedUpstream
        };
        let limits = RequestLimits::new(self.max_request_body_bytes, 100, 1_000)?;
        let config = HttpServerConfig::new(
            self.http_bind,
            security,
            Some(BearerToken::new(&self.bearer_token)?),
            limits,
        );
        config.validate()?;
        Ok(config)
    }

    #[must_use]
    pub const fn shutdown_grace(&self) -> Duration {
        Duration::from_secs(self.shutdown_grace_seconds)
    }

    fn parsed_rpc_headers(&self) -> Result<Vec<(String, String)>, ConfigError> {
        self.rpc_headers
            .iter()
            .map(|header| {
                let (name, value) = header
                    .split_once('=')
                    .ok_or_else(|| ConfigError::new("RPC headers must use the name=value form"))?;
                if name.is_empty()
                    || value.is_empty()
                    || name.bytes().any(|byte| byte.is_ascii_whitespace())
                    || value.bytes().any(|byte| byte == b'\r' || byte == b'\n')
                {
                    return Err(ConfigError::new("RPC header name or value is invalid"));
                }
                Ok((name.to_owned(), value.to_owned()))
            })
            .collect()
    }
}

impl fmt::Debug for ServeOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self
            .rpc_headers
            .iter()
            .filter_map(|header| header.split_once('=').map(|(name, _)| name))
            .collect();
        formatter
            .debug_struct("ServeOptions")
            .field("chain_id", &self.chain_id)
            .field("rpc_url", &"[REDACTED]")
            .field("rpc_header_names", &header_names)
            .field("rpc_timeout_seconds", &self.rpc_timeout_seconds)
            .field("rpc_max_response_bytes", &self.rpc_max_response_bytes)
            .field("max_input_bytes", &self.max_input_bytes)
            .field("gas_margin_basis_points", &self.gas_margin_basis_points)
            .field("max_gas_limit", &self.max_gas_limit)
            .field("custody_url", &"[REDACTED]")
            .field("custody_bearer_token", &"[REDACTED]")
            .field("http_bind", &self.http_bind)
            .field("upstream_tls_terminated", &self.upstream_tls_terminated)
            .field("bearer_token", &"[REDACTED]")
            .field("max_request_body_bytes", &self.max_request_body_bytes)
            .field("shutdown_grace_seconds", &self.shutdown_grace_seconds)
            .finish_non_exhaustive()
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

impl Error for ConfigError {}

macro_rules! config_error_from {
    ($($error:ty),+ $(,)?) => {
        $(impl From<$error> for ConfigError {
            fn from(error: $error) -> Self {
                Self::new(error.to_string())
            }
        })+
    };
}

config_error_from!(
    EthereumHttpRpcBuildError,
    HttpServerConfigError,
    HttpTransportBuildError,
    RemoteSignerConfigError,
);

fn decimal_wei(value: &str, field: &str) -> Result<Wei, ConfigError> {
    let amount = AtomicAmount::from_decimal_str(value)
        .map_err(|_| ConfigError::new(format!("{field} must be a canonical U256 decimal")))?;
    Ok(Wei(amount.0))
}

fn validate_http_endpoint(input: &str, label: &str) -> Result<(), ConfigError> {
    let url = reqwest::Url::parse(input)
        .map_err(|_| ConfigError::new(format!("{label} endpoint is invalid")))?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::new(format!(
            "{label} endpoint must be absolute and contain no credentials, query, or fragment"
        )));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_url(&url) => Ok(()),
        "http" => Err(ConfigError::new(format!(
            "non-loopback {label} endpoints require HTTPS"
        ))),
        _ => Err(ConfigError::new(format!(
            "{label} endpoint must use HTTP or HTTPS"
        ))),
    }
}

fn is_loopback_url(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ServeOptions {
        ServeOptions {
            chain_id: 31_337,
            rpc_url: "http://127.0.0.1:8545".to_owned(),
            rpc_headers: vec!["Authorization=rpc-secret".to_owned()],
            rpc_timeout_seconds: 15,
            rpc_max_response_bytes: 1024,
            rpc_retry_attempts: 3,
            rpc_retry_initial_millis: 10,
            rpc_retry_max_millis: 20,
            max_input_bytes: 4096,
            gas_margin_basis_points: 2_000,
            max_gas_limit: 500_000,
            max_fee_per_gas: "500000000000".to_owned(),
            max_priority_fee_per_gas: "100000000000".to_owned(),
            max_total_fee: "100000000000000000".to_owned(),
            custody_url: "http://127.0.0.1:8181".to_owned(),
            custody_bearer_token: "custody-secret".to_owned(),
            custody_connect_timeout_seconds: 5,
            custody_request_timeout_seconds: 30,
            custody_max_response_bytes: 1024,
            custody_retry_attempts: 3,
            custody_retry_initial_millis: 10,
            custody_retry_max_millis: 20,
            http_bind: "127.0.0.1:8082".parse().expect("test bind must parse"),
            upstream_tls_terminated: false,
            bearer_token: "wallet-secret".to_owned(),
            max_request_body_bytes: 1024,
            shutdown_grace_seconds: 30,
        }
    }

    #[test]
    fn valid_loopback_configuration_materializes_all_adapters() {
        let options = options();
        options.validate().expect("configuration must be valid");
        options
            .rpc_configuration()
            .expect("RPC configuration must materialize");
        options
            .custody_configuration()
            .expect("custody configuration must materialize");
        options
            .server_configuration()
            .expect("server configuration must materialize");
    }

    #[test]
    fn debug_redacts_urls_header_values_and_tokens() {
        let rendered = format!("{:?}", options());
        for secret in [
            "127.0.0.1:8545",
            "rpc-secret",
            "127.0.0.1:8181",
            "custody-secret",
            "wallet-secret",
        ] {
            assert!(!rendered.contains(secret));
        }
        assert!(rendered.contains("Authorization"));
    }

    #[test]
    fn non_loopback_endpoints_and_bind_require_transport_security() {
        let mut invalid_rpc = options();
        invalid_rpc.rpc_url = "http://rpc.example.test".to_owned();
        assert!(invalid_rpc.validate().is_err());

        let mut public_bind = options();
        public_bind.http_bind = "0.0.0.0:8082".parse().expect("test bind must parse");
        assert!(public_bind.validate().is_err());
        public_bind.upstream_tls_terminated = true;
        public_bind
            .validate()
            .expect("TLS assertion must permit bind");
    }
}
