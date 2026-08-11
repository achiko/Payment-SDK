use std::{
    error::Error,
    fmt,
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    str::FromStr,
    time::Duration,
};

use chain_bitcoin::{
    BitcoinCoreConfig, BitcoinNetwork, SatoshisPerKvb, format_bitcoin_block_hash,
    parse_bitcoin_block_hash,
};
use chain_ethereum::{EthereumHttpRpcBuildError, EthereumHttpRpcConfig, EthereumRpcLimits, Wei};
use chain_identity::AtomicAmount;
use clap::{Args, Parser, Subcommand};
use http_support::{
    AuthenticationMode, BearerToken, HttpServerConfig, HttpServerConfigError,
    HttpTransportBuildError, HttpTransportConfig, RequestLimits, RetryPolicy, TransportSecurity,
};
use signer_remote::{
    BearerSecret, RemoteRetryPolicy, RemoteSignerConfig, RemoteSignerConfigError,
    RemoteSignerEndpoint,
};
use wallet_worker::BitcoinIxClientConfig;

#[derive(Parser)]
#[command(
    name = "wallet-worker",
    version,
    about = "Stateless Bitcoin and Ethereum Wallet Service"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Serve stateless Ethereum operations with the configured authentication mode.
    Serve(ServeOptions),
    /// Operate the stateless Bitcoin Wallet Service.
    Bitcoin(BitcoinOptions),
}

#[derive(Args)]
pub struct BitcoinOptions {
    #[command(subcommand)]
    pub command: BitcoinCommand,
}

#[derive(Subcommand)]
pub enum BitcoinCommand {
    /// Serve stateless Bitcoin operations with the configured authentication mode.
    Serve(BitcoinServeOptions),
}

/// Selects whether custody follows the repository-wide service mode or keeps
/// an independently enforced strict bearer boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustodyAuthenticationPolicy {
    /// The repository-owned custody adapter must report the same mode as WS.
    RepositoryModeMatched,
    /// Vendor or independently administered custody must report strict mode.
    IndependentStrict,
}

impl CustodyAuthenticationPolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryModeMatched => "repository_mode_matched",
            Self::IndependentStrict => "independent_strict",
        }
    }
}

impl fmt::Display for CustodyAuthenticationPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CustodyAuthenticationPolicy {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "repository_mode_matched" => Ok(Self::RepositoryModeMatched),
            "independent_strict" => Ok(Self::IndependentStrict),
            _ => Err(ConfigError::new(
                "custody authentication policy must be exactly `repository_mode_matched` or `independent_strict`",
            )),
        }
    }
}

#[derive(Args, Clone)]
pub struct ServeOptions {
    /// `true` requires bearers; `false` globally trusts every reachable caller.
    #[arg(
        long = "strict-authentication-mode",
        env = "STRICT_AUTHENTICATION_MODE"
    )]
    pub authentication_mode: AuthenticationMode,

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

    /// Repository custody follows the global mode; independent custody stays strict.
    #[arg(
        long,
        env = "WS_CUSTODY_AUTHENTICATION_POLICY",
        default_value = "repository_mode_matched"
    )]
    pub custody_authentication_policy: CustodyAuthenticationPolicy,

    #[arg(long, env = "WS_CUSTODY_BEARER_TOKEN", hide_env_values = true)]
    pub custody_bearer_token: Option<String>,

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

    #[arg(long, env = "WS_METRICS_BIND", default_value = "127.0.0.1:9092")]
    pub metrics_bind: SocketAddr,

    /// Assert that a trusted upstream terminates TLS for a non-loopback bind.
    #[arg(long, env = "WS_UPSTREAM_TLS_TERMINATED", default_value_t = false)]
    pub upstream_tls_terminated: bool,

    #[arg(long, env = "WS_BEARER_TOKEN", hide_env_values = true)]
    pub bearer_token: Option<String>,

    #[arg(long, env = "WS_MAX_REQUEST_BODY_BYTES", default_value_t = 65_536)]
    pub max_request_body_bytes: usize,

    #[arg(long, env = "WS_SHUTDOWN_GRACE_SECONDS", default_value_t = 30)]
    pub shutdown_grace_seconds: u64,
}

#[derive(Args, Clone)]
pub struct BitcoinServeOptions {
    /// `true` requires bearers; `false` globally trusts every reachable caller.
    #[arg(
        long = "strict-authentication-mode",
        env = "STRICT_AUTHENTICATION_MODE"
    )]
    pub authentication_mode: AuthenticationMode,

    /// Bitcoin Core network: mainnet, testnet3, testnet4, signet, or regtest.
    #[arg(long, env = "WS_BITCOIN_NETWORK")]
    pub network: String,

    /// Conventional 64-hex block-zero hash in Bitcoin display byte order.
    #[arg(long, env = "WS_BITCOIN_EXPECTED_GENESIS_HASH")]
    pub expected_genesis_hash: String,

    /// Bitcoin Core 31.x HTTP JSON-RPC endpoint. Redacted from diagnostics.
    #[arg(long, env = "WS_BITCOIN_CORE_RPC_URL", hide_env_values = true)]
    pub core_rpc_url: String,

    /// Repeatable Core header as `name=value`; comma-delimited in the env var.
    #[arg(
        long = "core-rpc-header",
        env = "WS_BITCOIN_CORE_RPC_HEADERS",
        hide_env_values = true,
        value_delimiter = ','
    )]
    pub core_rpc_headers: Vec<String>,

    /// Complete Core Authorization header value (for example Basic credentials).
    #[arg(
        long,
        env = "WS_BITCOIN_CORE_RPC_AUTHORIZATION",
        hide_env_values = true
    )]
    pub core_rpc_authorization: Option<String>,

    #[arg(
        long,
        env = "WS_BITCOIN_CORE_RPC_TIMEOUT_SECONDS",
        default_value_t = 15
    )]
    pub core_rpc_timeout_seconds: u64,

    #[arg(
        long,
        env = "WS_BITCOIN_CORE_RPC_MAX_RESPONSE_BYTES",
        default_value_t = 67_108_864
    )]
    pub core_rpc_max_response_bytes: usize,

    #[arg(long, env = "WS_BITCOIN_CORE_RPC_RETRY_ATTEMPTS", default_value_t = 3)]
    pub core_rpc_retry_attempts: u32,

    #[arg(
        long,
        env = "WS_BITCOIN_CORE_RPC_RETRY_INITIAL_MILLIS",
        default_value_t = 100
    )]
    pub core_rpc_retry_initial_millis: u64,

    #[arg(
        long,
        env = "WS_BITCOIN_CORE_RPC_RETRY_MAX_MILLIS",
        default_value_t = 2_000
    )]
    pub core_rpc_retry_max_millis: u64,

    /// Bitcoin IX base URL. Redacted from diagnostics.
    #[arg(long, env = "WS_BITCOIN_IX_URL", hide_env_values = true)]
    pub ix_url: String,

    /// Repeatable IX header as `name=value`; comma-delimited in the env var.
    #[arg(
        long = "ix-header",
        env = "WS_BITCOIN_IX_HEADERS",
        hide_env_values = true,
        value_delimiter = ','
    )]
    pub ix_headers: Vec<String>,

    #[arg(long, env = "WS_BITCOIN_IX_BEARER_TOKEN", hide_env_values = true)]
    pub ix_bearer_token: Option<String>,

    #[arg(long, env = "WS_BITCOIN_IX_TIMEOUT_SECONDS", default_value_t = 15)]
    pub ix_timeout_seconds: u64,

    #[arg(
        long,
        env = "WS_BITCOIN_IX_MAX_RESPONSE_BYTES",
        default_value_t = 4_194_304
    )]
    pub ix_max_response_bytes: usize,

    #[arg(long, env = "WS_BITCOIN_IX_PAGE_SIZE", default_value_t = 100)]
    pub ix_page_size: usize,

    #[arg(long, env = "WS_BITCOIN_IX_MAX_PAGES", default_value_t = 20)]
    pub ix_max_pages: usize,

    #[arg(long, env = "WS_BITCOIN_IX_RETRY_ATTEMPTS", default_value_t = 3)]
    pub ix_retry_attempts: u32,

    /// Explicit deployment confirmation policy; no implicit Bitcoin default.
    #[arg(long, env = "WS_BITCOIN_MINIMUM_CONFIRMATIONS")]
    pub minimum_confirmations: u64,

    #[arg(long, env = "WS_BITCOIN_FEE_TARGET_BLOCKS", default_value_t = 6)]
    pub fee_target_blocks: u16,

    #[arg(long, env = "WS_BITCOIN_MAX_SATOSHIS_PER_KVB")]
    pub maximum_satoshis_per_kvb: u64,

    #[arg(long, env = "WS_BITCOIN_MAX_INPUTS", default_value_t = 200)]
    pub maximum_inputs: usize,

    #[arg(long, env = "WS_BITCOIN_MAX_OUTPUTS", default_value_t = 200)]
    pub maximum_outputs: usize,

    /// Remote custody base URL. Redacted from diagnostics.
    #[arg(long, env = "WS_CUSTODY_URL", hide_env_values = true)]
    pub custody_url: String,

    /// Repository custody follows the global mode; independent custody stays strict.
    #[arg(
        long,
        env = "WS_CUSTODY_AUTHENTICATION_POLICY",
        default_value = "repository_mode_matched"
    )]
    pub custody_authentication_policy: CustodyAuthenticationPolicy,

    #[arg(long, env = "WS_CUSTODY_BEARER_TOKEN", hide_env_values = true)]
    pub custody_bearer_token: Option<String>,

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

    #[arg(long, env = "WS_METRICS_BIND", default_value = "127.0.0.1:9092")]
    pub metrics_bind: SocketAddr,

    /// Assert that a trusted upstream terminates TLS for a non-loopback bind.
    #[arg(long, env = "WS_UPSTREAM_TLS_TERMINATED", default_value_t = false)]
    pub upstream_tls_terminated: bool,

    #[arg(long, env = "WS_BEARER_TOKEN", hide_env_values = true)]
    pub bearer_token: Option<String>,

    #[arg(long, env = "WS_MAX_REQUEST_BODY_BYTES", default_value_t = 1_048_576)]
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
        if !self.http_bind.ip().is_loopback() && !self.upstream_tls_terminated {
            return Err(ConfigError::new(
                "a non-loopback Wallet Service bind requires trusted upstream TLS",
            ));
        }
        if !self.metrics_bind.ip().is_loopback() {
            return Err(ConfigError::new(
                "Wallet Service metrics may bind only to a loopback address",
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
        let retry = RemoteRetryPolicy::new(
            self.custody_retry_attempts,
            Duration::from_millis(self.custody_retry_initial_millis),
            Duration::from_millis(self.custody_retry_max_millis),
        )?;
        let config = custody_signer_configuration(
            endpoint,
            self.authentication_mode,
            self.custody_authentication_policy,
            self.custody_bearer_token.as_deref(),
        )?;
        config
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
        let bearer_token = strict_bearer_token(
            self.authentication_mode,
            self.bearer_token.as_deref(),
            "WS_BEARER_TOKEN",
        )?;
        let config = HttpServerConfig::new(self.http_bind, security, bearer_token, limits)
            .with_authentication_mode(self.authentication_mode);
        config.validate()?;
        Ok(config)
    }

    pub fn metrics_server_configuration(&self) -> Result<HttpServerConfig, ConfigError> {
        metrics_server_configuration(self.metrics_bind, self.max_request_body_bytes)
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

impl BitcoinServeOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.bitcoin_network()?;
        self.core_configuration()?;
        self.core_transport_configuration()?;
        self.ix_configuration()?;
        self.custody_configuration()?;
        self.operation_policy()?;
        self.server_configuration()?;
        if !self.http_bind.ip().is_loopback() && !self.upstream_tls_terminated {
            return Err(ConfigError::new(
                "a non-loopback Wallet Service bind requires trusted upstream TLS",
            ));
        }
        if !self.metrics_bind.ip().is_loopback() {
            return Err(ConfigError::new(
                "Wallet Service metrics may bind only to a loopback address",
            ));
        }
        if self.shutdown_grace_seconds == 0 {
            return Err(ConfigError::new(
                "Wallet Service shutdown grace must be greater than zero",
            ));
        }
        Ok(())
    }

    pub fn bitcoin_network(&self) -> Result<BitcoinNetwork, ConfigError> {
        match self.network.as_str() {
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

    pub fn core_configuration(&self) -> Result<BitcoinCoreConfig, ConfigError> {
        let expected_genesis_hash = parse_bitcoin_block_hash(&self.expected_genesis_hash)
            .map_err(|error| ConfigError::new(error.message))?;
        let canonical = format_bitcoin_block_hash(&expected_genesis_hash)
            .map_err(|error| ConfigError::new(error.message))?;
        if canonical != self.expected_genesis_hash {
            return Err(ConfigError::new(
                "Bitcoin genesis hash must use canonical lowercase display encoding",
            ));
        }
        let config = BitcoinCoreConfig {
            expected_network: self.bitcoin_network()?,
            expected_genesis_hash,
        };
        config
            .validate()
            .map_err(|error| ConfigError::new(error.message))?;
        Ok(config)
    }

    pub fn core_transport_configuration(&self) -> Result<HttpTransportConfig, ConfigError> {
        validate_http_endpoint(&self.core_rpc_url, "Bitcoin Core RPC")?;
        if self.core_rpc_timeout_seconds == 0
            || self.core_rpc_max_response_bytes == 0
            || self.core_rpc_retry_initial_millis == 0
            || self.core_rpc_retry_max_millis == 0
        {
            return Err(ConfigError::new(
                "Bitcoin Core timeout and response limits must be greater than zero",
            ));
        }
        if self.core_rpc_retry_initial_millis > self.core_rpc_retry_max_millis {
            return Err(ConfigError::new(
                "Bitcoin Core initial retry delay must not exceed its maximum",
            ));
        }
        let attempts = NonZeroU32::new(self.core_rpc_retry_attempts).ok_or_else(|| {
            ConfigError::new("Bitcoin Core retry attempts must be greater than zero")
        })?;
        let mut headers = parse_named_headers(&self.core_rpc_headers, "Bitcoin Core RPC")?;
        if let Some(authorization) = self.core_rpc_authorization.as_deref() {
            validate_header_secret(authorization, "Bitcoin Core authorization")?;
            insert_unique_header(
                &mut headers,
                "authorization".to_owned(),
                authorization.to_owned(),
                "Bitcoin Core RPC",
            )?;
        }
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        {
            return Err(ConfigError::new(
                "Bitcoin Core RPC requires exactly one authorization header",
            ));
        }
        let mut config = HttpTransportConfig::new(
            self.core_rpc_url.clone(),
            Duration::from_secs(self.core_rpc_timeout_seconds),
        );
        config.max_response_bytes = self.core_rpc_max_response_bytes;
        config.default_headers = headers;
        config.retry_policy = RetryPolicy::new(
            attempts,
            Duration::from_millis(self.core_rpc_retry_initial_millis),
            Duration::from_millis(self.core_rpc_retry_max_millis),
        )?;
        Ok(config)
    }

    pub fn ix_configuration(&self) -> Result<BitcoinIxClientConfig, ConfigError> {
        validate_http_endpoint(&self.ix_url, "Bitcoin IX")?;
        let mut headers = match self.authentication_mode {
            AuthenticationMode::Strict => parse_named_headers(&self.ix_headers, "Bitcoin IX")?,
            AuthenticationMode::GlobalTrusted => parse_named_headers(
                &without_authorization_headers(&self.ix_headers),
                "Bitcoin IX",
            )?,
        };
        if self.authentication_mode == AuthenticationMode::Strict {
            let token = self.ix_bearer_token.as_deref().ok_or_else(|| {
                ConfigError::new(
                    "WS_BITCOIN_IX_BEARER_TOKEN is required in strict authentication mode",
                )
            })?;
            validate_bearer_token(token, "Bitcoin IX bearer token")?;
            insert_unique_header(
                &mut headers,
                "authorization".to_owned(),
                format!("Bearer {token}"),
                "Bitcoin IX",
            )?;
        }
        let config = BitcoinIxClientConfig {
            endpoint: self.ix_url.clone(),
            authentication_mode: self.authentication_mode,
            headers,
            request_timeout: Duration::from_secs(self.ix_timeout_seconds),
            maximum_response_bytes: self.ix_max_response_bytes,
            page_size: self.ix_page_size,
            maximum_pages_per_address: self.ix_max_pages,
            retry_attempts: self.ix_retry_attempts,
        };
        if config.request_timeout.is_zero()
            || config.maximum_response_bytes == 0
            || config.page_size == 0
            || config.page_size > 1_000
            || config.maximum_pages_per_address == 0
            || config.retry_attempts == 0
        {
            return Err(ConfigError::new(
                "Bitcoin IX timeout and request limits must be positive and page size at most 1000",
            ));
        }
        Ok(config)
    }

    pub fn custody_configuration(&self) -> Result<RemoteSignerConfig, ConfigError> {
        let endpoint = RemoteSignerEndpoint::new(&self.custody_url)?;
        let retry = RemoteRetryPolicy::new(
            self.custody_retry_attempts,
            Duration::from_millis(self.custody_retry_initial_millis),
            Duration::from_millis(self.custody_retry_max_millis),
        )?;
        let config = custody_signer_configuration(
            endpoint,
            self.authentication_mode,
            self.custody_authentication_policy,
            self.custody_bearer_token.as_deref(),
        )?;
        config
            .with_timeouts(
                Duration::from_secs(self.custody_connect_timeout_seconds),
                Duration::from_secs(self.custody_request_timeout_seconds),
            )?
            .with_max_response_bytes(self.custody_max_response_bytes)
            .map(|config| config.with_retry_policy(retry))
            .map_err(Into::into)
    }

    pub fn operation_policy(&self) -> Result<wallet_worker::BitcoinOperationPolicy, ConfigError> {
        let policy = wallet_worker::BitcoinOperationPolicy {
            minimum_confirmations: self.minimum_confirmations,
            fee_target_blocks: self.fee_target_blocks,
            maximum_fee_rate: SatoshisPerKvb::new(self.maximum_satoshis_per_kvb),
            maximum_inputs: self.maximum_inputs,
            maximum_outputs: self.maximum_outputs,
        };
        policy
            .validate()
            .map_err(|error| ConfigError::new(error.message))
    }

    pub fn server_configuration(&self) -> Result<HttpServerConfig, ConfigError> {
        let security = if self.http_bind.ip().is_loopback() {
            TransportSecurity::PlaintextLoopback
        } else {
            TransportSecurity::TlsTerminatedUpstream
        };
        let limits = RequestLimits::new(self.max_request_body_bytes, 100, 1_000)?;
        let bearer_token = strict_bearer_token(
            self.authentication_mode,
            self.bearer_token.as_deref(),
            "WS_BEARER_TOKEN",
        )?;
        let config = HttpServerConfig::new(self.http_bind, security, bearer_token, limits)
            .with_authentication_mode(self.authentication_mode);
        config.validate()?;
        Ok(config)
    }

    pub fn metrics_server_configuration(&self) -> Result<HttpServerConfig, ConfigError> {
        metrics_server_configuration(self.metrics_bind, self.max_request_body_bytes)
    }

    #[must_use]
    pub const fn shutdown_grace(&self) -> Duration {
        Duration::from_secs(self.shutdown_grace_seconds)
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
            .field("authentication_mode", &self.authentication_mode)
            .field("chain_id", &self.chain_id)
            .field("rpc_url", &"[REDACTED]")
            .field("rpc_header_names", &header_names)
            .field("rpc_timeout_seconds", &self.rpc_timeout_seconds)
            .field("rpc_max_response_bytes", &self.rpc_max_response_bytes)
            .field("max_input_bytes", &self.max_input_bytes)
            .field("gas_margin_basis_points", &self.gas_margin_basis_points)
            .field("max_gas_limit", &self.max_gas_limit)
            .field("custody_url", &"[REDACTED]")
            .field(
                "custody_authentication_policy",
                &self.custody_authentication_policy,
            )
            .field("custody_bearer_token", &"[REDACTED]")
            .field("http_bind", &self.http_bind)
            .field("metrics_bind", &self.metrics_bind)
            .field("upstream_tls_terminated", &self.upstream_tls_terminated)
            .field("bearer_token", &"[REDACTED]")
            .field("max_request_body_bytes", &self.max_request_body_bytes)
            .field("shutdown_grace_seconds", &self.shutdown_grace_seconds)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for BitcoinServeOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let core_header_names = header_names(&self.core_rpc_headers);
        let ix_header_names = header_names(&self.ix_headers);
        formatter
            .debug_struct("BitcoinServeOptions")
            .field("authentication_mode", &self.authentication_mode)
            .field("network", &self.network)
            .field("expected_genesis_hash", &self.expected_genesis_hash)
            .field("core_rpc_url", &"[REDACTED]")
            .field("core_rpc_header_names", &core_header_names)
            .field("core_rpc_authorization", &"[REDACTED]")
            .field("ix_url", &"[REDACTED]")
            .field("ix_header_names", &ix_header_names)
            .field("ix_bearer_token", &"[REDACTED]")
            .field("minimum_confirmations", &self.minimum_confirmations)
            .field("fee_target_blocks", &self.fee_target_blocks)
            .field("maximum_satoshis_per_kvb", &self.maximum_satoshis_per_kvb)
            .field("maximum_inputs", &self.maximum_inputs)
            .field("maximum_outputs", &self.maximum_outputs)
            .field("custody_url", &"[REDACTED]")
            .field(
                "custody_authentication_policy",
                &self.custody_authentication_policy,
            )
            .field("custody_bearer_token", &"[REDACTED]")
            .field("http_bind", &self.http_bind)
            .field("metrics_bind", &self.metrics_bind)
            .field("upstream_tls_terminated", &self.upstream_tls_terminated)
            .field("bearer_token", &"[REDACTED]")
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

fn parse_named_headers(
    encoded: &[String],
    label: &str,
) -> Result<Vec<(String, String)>, ConfigError> {
    let mut parsed = Vec::with_capacity(encoded.len());
    for header in encoded {
        let (name, value) = header
            .split_once('=')
            .ok_or_else(|| ConfigError::new(format!("{label} headers must use name=value")))?;
        if name.is_empty()
            || !name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#'
                            | b'$'
                            | b'%'
                            | b'&'
                            | b'\''
                            | b'*'
                            | b'+'
                            | b'-'
                            | b'.'
                            | b'^'
                            | b'_'
                            | b'`'
                            | b'|'
                            | b'~'
                    )
            })
        {
            return Err(ConfigError::new(format!("{label} header name is invalid")));
        }
        validate_header_secret(value, &format!("{label} header value"))?;
        insert_unique_header(
            &mut parsed,
            name.to_ascii_lowercase(),
            value.to_owned(),
            label,
        )?;
    }
    Ok(parsed)
}

fn insert_unique_header(
    headers: &mut Vec<(String, String)>,
    name: String,
    value: String,
    label: &str,
) -> Result<(), ConfigError> {
    if headers
        .iter()
        .any(|(existing, _)| existing.eq_ignore_ascii_case(&name))
    {
        return Err(ConfigError::new(format!(
            "{label} header names must be unique"
        )));
    }
    headers.push((name, value));
    Ok(())
}

fn validate_header_secret(value: &str, label: &str) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.trim() != value
        || value
            .bytes()
            .any(|byte| byte == b'\r' || byte == b'\n' || byte.is_ascii_control())
    {
        return Err(ConfigError::new(format!("{label} is invalid")));
    }
    Ok(())
}

fn validate_bearer_token(value: &str, label: &str) -> Result<(), ConfigError> {
    validate_header_secret(value, label)?;
    if value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(ConfigError::new(format!("{label} is invalid")));
    }
    Ok(())
}

fn strict_bearer_token(
    authentication_mode: AuthenticationMode,
    value: Option<&str>,
    variable: &str,
) -> Result<Option<BearerToken>, ConfigError> {
    match authentication_mode {
        AuthenticationMode::Strict => value
            .ok_or_else(|| {
                ConfigError::new(format!(
                    "{variable} is required in strict authentication mode"
                ))
            })
            .and_then(|value| BearerToken::new(value).map(Some).map_err(Into::into)),
        AuthenticationMode::GlobalTrusted => Ok(None),
    }
}

fn custody_signer_configuration(
    endpoint: RemoteSignerEndpoint,
    service_authentication_mode: AuthenticationMode,
    custody_authentication_policy: CustodyAuthenticationPolicy,
    bearer_token: Option<&str>,
) -> Result<RemoteSignerConfig, ConfigError> {
    match (custody_authentication_policy, service_authentication_mode) {
        (CustodyAuthenticationPolicy::RepositoryModeMatched, AuthenticationMode::GlobalTrusted) => {
            Ok(RemoteSignerConfig::global_trusted(endpoint))
        }
        (CustodyAuthenticationPolicy::RepositoryModeMatched, AuthenticationMode::Strict)
        | (CustodyAuthenticationPolicy::IndependentStrict, _) => {
            let bearer_token = bearer_token.ok_or_else(|| {
                ConfigError::new(
                    "WS_CUSTODY_BEARER_TOKEN is required by the selected custody authentication policy",
                )
            })?;
            Ok(RemoteSignerConfig::new(
                endpoint,
                BearerSecret::new(bearer_token)?,
            ))
        }
    }
}

fn without_authorization_headers(headers: &[String]) -> Vec<String> {
    headers
        .iter()
        .filter(|header| {
            !header
                .split_once('=')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        })
        .cloned()
        .collect()
}

fn metrics_server_configuration(
    bind: SocketAddr,
    maximum_request_body_bytes: usize,
) -> Result<HttpServerConfig, ConfigError> {
    if !bind.ip().is_loopback() {
        return Err(ConfigError::new(
            "Wallet Service metrics may bind only to a loopback address",
        ));
    }
    let config = HttpServerConfig::new(
        bind,
        TransportSecurity::PlaintextLoopback,
        None,
        RequestLimits::new(maximum_request_body_bytes, 100, 1_000)?,
    )
    .with_authentication_mode(AuthenticationMode::GlobalTrusted);
    config.validate()?;
    Ok(config)
}

fn header_names(headers: &[String]) -> Vec<&str> {
    headers
        .iter()
        .filter_map(|header| header.split_once('=').map(|(name, _)| name))
        .collect()
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
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    fn options() -> ServeOptions {
        ServeOptions {
            authentication_mode: AuthenticationMode::Strict,
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
            custody_authentication_policy: CustodyAuthenticationPolicy::RepositoryModeMatched,
            custody_bearer_token: Some("custody-secret".to_owned()),
            custody_connect_timeout_seconds: 5,
            custody_request_timeout_seconds: 30,
            custody_max_response_bytes: 1024,
            custody_retry_attempts: 3,
            custody_retry_initial_millis: 10,
            custody_retry_max_millis: 20,
            http_bind: "127.0.0.1:8082".parse().expect("test bind must parse"),
            metrics_bind: "127.0.0.1:9092"
                .parse()
                .expect("test metrics bind must parse"),
            upstream_tls_terminated: false,
            bearer_token: Some("wallet-secret".to_owned()),
            max_request_body_bytes: 1024,
            shutdown_grace_seconds: 30,
        }
    }

    fn bitcoin_options() -> BitcoinServeOptions {
        BitcoinServeOptions {
            authentication_mode: AuthenticationMode::Strict,
            network: "regtest".to_owned(),
            expected_genesis_hash:
                "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206".to_owned(),
            core_rpc_url: "http://127.0.0.1:18443".to_owned(),
            core_rpc_headers: vec!["x-core-token=core-secret".to_owned()],
            core_rpc_authorization: Some("Basic hidden".to_owned()),
            core_rpc_timeout_seconds: 15,
            core_rpc_max_response_bytes: 1024,
            core_rpc_retry_attempts: 3,
            core_rpc_retry_initial_millis: 10,
            core_rpc_retry_max_millis: 20,
            ix_url: "http://127.0.0.1:8081".to_owned(),
            ix_headers: vec!["x-ix-token=ix-header-secret".to_owned()],
            ix_bearer_token: Some("ix-secret".to_owned()),
            ix_timeout_seconds: 15,
            ix_max_response_bytes: 1024,
            ix_page_size: 100,
            ix_max_pages: 10,
            ix_retry_attempts: 3,
            minimum_confirmations: 6,
            fee_target_blocks: 6,
            maximum_satoshis_per_kvb: 100_000,
            maximum_inputs: 100,
            maximum_outputs: 100,
            custody_url: "http://127.0.0.1:8181".to_owned(),
            custody_authentication_policy: CustodyAuthenticationPolicy::RepositoryModeMatched,
            custody_bearer_token: Some("custody-secret".to_owned()),
            custody_connect_timeout_seconds: 5,
            custody_request_timeout_seconds: 30,
            custody_max_response_bytes: 1024,
            custody_retry_attempts: 3,
            custody_retry_initial_millis: 10,
            custody_retry_max_millis: 20,
            http_bind: "127.0.0.1:8082".parse().expect("test bind must parse"),
            metrics_bind: "127.0.0.1:9092"
                .parse()
                .expect("test metrics bind must parse"),
            upstream_tls_terminated: false,
            bearer_token: Some("wallet-secret".to_owned()),
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
    fn strict_mode_requires_wallet_and_custody_tokens() {
        let mut missing_wallet = options();
        missing_wallet.bearer_token = None;
        assert!(missing_wallet.validate().is_err());

        let mut missing_custody = options();
        missing_custody.custody_bearer_token = None;
        assert!(missing_custody.validate().is_err());

        let mut missing_ix = bitcoin_options();
        missing_ix.ix_bearer_token = None;
        assert!(missing_ix.validate().is_err());
    }

    #[test]
    fn global_trusted_mode_ignores_repo_owned_tokens_and_ix_authorization() {
        let mut ethereum = options();
        ethereum.authentication_mode = AuthenticationMode::GlobalTrusted;
        ethereum.bearer_token = Some("ignored invalid wallet token".to_owned());
        ethereum.custody_bearer_token = Some("ignored invalid custody token".to_owned());
        ethereum
            .validate()
            .expect("global-trusted Ethereum configuration must ignore tokens");

        let mut bitcoin = bitcoin_options();
        bitcoin.authentication_mode = AuthenticationMode::GlobalTrusted;
        bitcoin.bearer_token = None;
        bitcoin.custody_bearer_token = Some("ignored invalid custody token".to_owned());
        bitcoin.ix_bearer_token = Some("ignored invalid IX token".to_owned());
        bitcoin
            .ix_headers
            .push("Authorization=ignored\r\ninvalid".to_owned());
        bitcoin
            .validate()
            .expect("global-trusted Bitcoin configuration must ignore repo-owned credentials");
        let ix = bitcoin
            .ix_configuration()
            .expect("global-trusted IX configuration must materialize");
        assert!(
            ix.headers
                .iter()
                .all(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
        );
    }

    #[test]
    fn independent_custody_stays_strict_when_wallet_is_global_trusted() {
        let mut ethereum = options();
        ethereum.authentication_mode = AuthenticationMode::GlobalTrusted;
        ethereum.custody_authentication_policy = CustodyAuthenticationPolicy::IndependentStrict;
        ethereum.bearer_token = None;
        ethereum.custody_bearer_token = None;
        let error = ethereum
            .custody_configuration()
            .expect_err("independent custody must keep requiring its bearer");
        assert!(error.to_string().contains("WS_CUSTODY_BEARER_TOKEN"));

        ethereum.custody_bearer_token = Some("independent-custody-secret".to_owned());
        let custody = ethereum
            .custody_configuration()
            .expect("independent custody must materialize with a bearer");
        let rendered = format!("{custody:?}");
        assert!(rendered.contains("Strict"));
        assert!(!rendered.contains("independent-custody-secret"));

        let mut repository = options();
        repository.authentication_mode = AuthenticationMode::GlobalTrusted;
        repository.custody_bearer_token = None;
        assert!(
            format!(
                "{:?}",
                repository
                    .custody_configuration()
                    .expect("repository custody must match global-trusted mode")
            )
            .contains("GlobalTrusted")
        );

        let mut bitcoin = bitcoin_options();
        bitcoin.authentication_mode = AuthenticationMode::GlobalTrusted;
        bitcoin.custody_authentication_policy = CustodyAuthenticationPolicy::IndependentStrict;
        bitcoin.bearer_token = None;
        bitcoin.ix_bearer_token = None;
        bitcoin.custody_bearer_token = None;
        assert!(bitcoin.validate().is_err());
        bitcoin.custody_bearer_token = Some("independent-bitcoin-custody".to_owned());
        bitcoin
            .validate()
            .expect("global-trusted Bitcoin WS must support independent strict custody");
    }

    #[test]
    fn custody_authentication_policy_parsing_is_exact() {
        assert_eq!(
            "repository_mode_matched".parse(),
            Ok(CustodyAuthenticationPolicy::RepositoryModeMatched)
        );
        assert_eq!(
            "independent_strict".parse(),
            Ok(CustodyAuthenticationPolicy::IndependentStrict)
        );
        assert!(
            "IndependentStrict"
                .parse::<CustodyAuthenticationPolicy>()
                .is_err()
        );
        assert!(
            " independent_strict"
                .parse::<CustodyAuthenticationPolicy>()
                .is_err()
        );
    }

    #[test]
    fn metrics_listener_must_remain_on_loopback() {
        let mut ethereum = options();
        ethereum.metrics_bind = "0.0.0.0:9092"
            .parse()
            .expect("test metrics bind must parse");
        assert!(ethereum.validate().is_err());
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

    #[test]
    fn valid_bitcoin_configuration_materializes_every_boundary() {
        let options = bitcoin_options();
        options
            .validate()
            .expect("Bitcoin configuration must be valid");
        options
            .core_transport_configuration()
            .expect("Core transport must materialize");
        options
            .core_configuration()
            .expect("Core identity must materialize");
        options
            .ix_configuration()
            .expect("IX client must materialize");
        options
            .custody_configuration()
            .expect("custody must materialize");
        options
            .operation_policy()
            .expect("Bitcoin policy must materialize");

        let mut header_authorization = bitcoin_options();
        header_authorization.core_rpc_authorization = None;
        header_authorization
            .core_rpc_headers
            .push("Authorization=Basic alternate".to_owned());
        header_authorization
            .validate()
            .expect("one authorization header must satisfy Core authentication");
    }

    #[test]
    fn bitcoin_configuration_rejects_unsafe_endpoints_and_headers() {
        let mut public_plaintext = bitcoin_options();
        public_plaintext.ix_url = "http://ix.example.test".to_owned();
        assert!(public_plaintext.validate().is_err());

        let mut credentials = bitcoin_options();
        credentials.core_rpc_url = "https://user:secret@core.example.test".to_owned();
        assert!(credentials.validate().is_err());

        let mut query = bitcoin_options();
        query.ix_url = "https://ix.example.test?token=secret".to_owned();
        assert!(query.validate().is_err());

        let mut noncanonical_genesis = bitcoin_options();
        noncanonical_genesis.expected_genesis_hash = noncanonical_genesis
            .expected_genesis_hash
            .to_ascii_uppercase();
        assert!(noncanonical_genesis.validate().is_err());

        let mut duplicate = bitcoin_options();
        duplicate
            .core_rpc_headers
            .push("X-Core-Token=second".to_owned());
        assert!(duplicate.validate().is_err());

        let mut missing_core_auth = bitcoin_options();
        missing_core_auth.core_rpc_authorization = None;
        assert!(missing_core_auth.validate().is_err());

        let mut injected = bitcoin_options();
        injected.ix_headers[0] = "x-ix-token=hidden\r\nx-evil=yes".to_owned();
        assert!(injected.validate().is_err());

        let mut oversized_page = bitcoin_options();
        oversized_page.ix_page_size = 1_001;
        assert!(oversized_page.validate().is_err());

        let mut unsupported_max_fee_rate = bitcoin_options();
        unsupported_max_fee_rate.maximum_satoshis_per_kvb = 100_000_001;
        assert!(unsupported_max_fee_rate.validate().is_err());
    }

    #[test]
    fn bitcoin_debug_redacts_every_endpoint_and_credential() {
        let rendered = format!("{:?}", bitcoin_options());
        for secret in [
            "127.0.0.1:18443",
            "core-secret",
            "Basic hidden",
            "127.0.0.1:8081",
            "ix-header-secret",
            "ix-secret",
            "127.0.0.1:8181",
            "custody-secret",
            "wallet-secret",
        ] {
            assert!(!rendered.contains(secret));
        }
        assert!(rendered.contains("x-core-token"));
        assert!(rendered.contains("x-ix-token"));
    }
}
