use std::{collections::BTreeMap, error::Error, fmt, net::SocketAddr, time::Duration};

use chain_bitcoin::{AddressType, FeeRate, Network};
use wallets::SecretBytes;

pub const BIND_ENV: &str = "WS_HTTP_BIND";
pub const BEARER_ENV: &str = "WS_HTTP_BEARER_TOKEN";
pub const TLS_ENV: &str = "WS_HTTP_TLS_TERMINATED_UPSTREAM";
pub const DEFAULT_BIND: &str = "127.0.0.1:8082";

pub const ENV_KEYS: &[&str] = &[
    BIND_ENV,
    BEARER_ENV,
    TLS_ENV,
    "WS_BITCOIN_WALLET_ID",
    "WS_BITCOIN_PRIVATE_KEY_HEX",
    "WS_BITCOIN_NETWORK",
    "WS_BITCOIN_ADDRESS_FORMAT",
    "WS_BITCOIN_RPC_URL",
    "WS_BITCOIN_RPC_URLS",
    "WS_BITCOIN_RPC_AUTHORIZATION",
    "WS_BITCOIN_GENESIS_HASH",
    "WS_BITCOIN_INDEXER_URL",
    "WS_BITCOIN_INDEXER_URLS",
    "WS_BITCOIN_INDEXER_TOKEN",
    "WS_BITCOIN_TIMEOUT_SECONDS",
    "WS_BITCOIN_FEE_TARGET_BLOCKS",
    "WS_BITCOIN_MAX_FEE_RATE_SAT_PER_KVB",
    "WS_ETHEREUM_WALLET_ID",
    "WS_ETHEREUM_PRIVATE_KEY_HEX",
    "WS_ETHEREUM_NETWORK",
    "WS_ETHEREUM_CHAIN_ID",
    "WS_ETHEREUM_RPC_URL",
    "WS_ETHEREUM_RPC_URLS",
    "WS_ETHEREUM_RPC_AUTHORIZATION",
    "WS_ETHEREUM_INDEXER_URL",
    "WS_ETHEREUM_INDEXER_URLS",
    "WS_ETHEREUM_INDEXER_TOKEN",
    "WS_ETHEREUM_TIMEOUT_SECONDS",
];

pub struct Config {
    pub bind: SocketAddr,
    pub(crate) bearer_token: http_support::server::BearerToken,
    pub(crate) tls_terminated_upstream: bool,
    pub(crate) bitcoin: Option<Bitcoin>,
    pub(crate) ethereum: Option<Ethereum>,
}

pub(crate) struct Bitcoin {
    pub id: String,
    pub secret: SecretBytes,
    pub network: Network,
    pub address_type: AddressType,
    pub rpc_urls: Vec<String>,
    pub rpc_authorization: Option<String>,
    pub genesis_hash: String,
    pub indexer_urls: Vec<String>,
    pub indexer_token: Option<String>,
    pub timeout: Duration,
    pub fee_target_blocks: u16,
    pub max_fee_rate: FeeRate,
}

pub(crate) struct Ethereum {
    pub id: String,
    pub secret: SecretBytes,
    pub network: String,
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    pub rpc_authorization: Option<String>,
    pub indexer_urls: Vec<String>,
    pub indexer_token: Option<String>,
    pub timeout: Duration,
}

impl Config {
    pub fn from_variables(
        values: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, ConfigError> {
        let values = values.into_iter().collect::<BTreeMap<_, _>>();
        let get = |name: &str| values.get(name).cloned();
        let bind = get(BIND_ENV)
            .unwrap_or_else(|| DEFAULT_BIND.to_owned())
            .parse()
            .map_err(|_| ConfigError::new("WS_HTTP_BIND must be a socket address"))?;
        let token = required(&get, BEARER_ENV)?;
        let bearer_token = http_support::server::BearerToken::new(token)
            .map_err(|error| ConfigError::new(error.to_string()))?;
        let tls_terminated_upstream = boolean(get(TLS_ENV).as_deref(), TLS_ENV)?;
        let bitcoin = group_enabled(&values, "WS_BITCOIN_")
            .then(|| Bitcoin::parse(&get))
            .transpose()?;
        let ethereum = group_enabled(&values, "WS_ETHEREUM_")
            .then(|| Ethereum::parse(&get))
            .transpose()?;
        if bitcoin
            .as_ref()
            .zip(ethereum.as_ref())
            .is_some_and(|(a, b)| a.id == b.id)
        {
            return Err(ConfigError::new(
                "Bitcoin and Ethereum wallet IDs must be distinct",
            ));
        }
        Ok(Self {
            bind,
            bearer_token,
            tls_terminated_upstream,
            bitcoin,
            ethereum,
        })
    }
}

impl Bitcoin {
    fn parse(get: &impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let network = match required(get, "WS_BITCOIN_NETWORK")?.as_str() {
            "mainnet" => Network::Mainnet,
            "testnet3" => Network::Testnet3,
            "testnet4" => Network::Testnet4,
            "signet" => Network::Signet,
            "regtest" => Network::Regtest,
            _ => return Err(ConfigError::new("WS_BITCOIN_NETWORK is unsupported")),
        };
        let address_type = match required(get, "WS_BITCOIN_ADDRESS_FORMAT")?.as_str() {
            "segwit_v0" => AddressType::SegwitV0,
            "taproot" => AddressType::Taproot,
            _ => {
                return Err(ConfigError::new(
                    "WS_BITCOIN_ADDRESS_FORMAT must be segwit_v0 or taproot",
                ));
            }
        };
        Ok(Self {
            id: required(get, "WS_BITCOIN_WALLET_ID")?,
            secret: secret(get, "WS_BITCOIN_PRIVATE_KEY_HEX")?,
            network,
            address_type,
            rpc_urls: urls(get, "WS_BITCOIN_RPC_URLS", "WS_BITCOIN_RPC_URL")?,
            rpc_authorization: get("WS_BITCOIN_RPC_AUTHORIZATION"),
            genesis_hash: required(get, "WS_BITCOIN_GENESIS_HASH")?,
            indexer_urls: urls(get, "WS_BITCOIN_INDEXER_URLS", "WS_BITCOIN_INDEXER_URL")?,
            indexer_token: get("WS_BITCOIN_INDEXER_TOKEN"),
            timeout: duration(get, "WS_BITCOIN_TIMEOUT_SECONDS", 15)?,
            fee_target_blocks: number(get, "WS_BITCOIN_FEE_TARGET_BLOCKS", 6)?,
            max_fee_rate: FeeRate::new(number(
                get,
                "WS_BITCOIN_MAX_FEE_RATE_SAT_PER_KVB",
                10_000_000,
            )?),
        })
    }
}

impl Ethereum {
    fn parse(get: &impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let network = required(get, "WS_ETHEREUM_NETWORK")?;
        let chain_id = number(get, "WS_ETHEREUM_CHAIN_ID", 0_u64)?;
        let expected = match network.as_str() {
            "mainnet" => 1,
            "sepolia" => 11_155_111,
            _ => {
                return Err(ConfigError::new(
                    "WS_ETHEREUM_NETWORK must be mainnet or sepolia",
                ));
            }
        };
        if chain_id != expected {
            return Err(ConfigError::new(
                "WS_ETHEREUM_CHAIN_ID does not match network",
            ));
        }
        Ok(Self {
            id: required(get, "WS_ETHEREUM_WALLET_ID")?,
            secret: secret(get, "WS_ETHEREUM_PRIVATE_KEY_HEX")?,
            network,
            chain_id,
            rpc_urls: urls(get, "WS_ETHEREUM_RPC_URLS", "WS_ETHEREUM_RPC_URL")?,
            rpc_authorization: get("WS_ETHEREUM_RPC_AUTHORIZATION"),
            indexer_urls: urls(get, "WS_ETHEREUM_INDEXER_URLS", "WS_ETHEREUM_INDEXER_URL")?,
            indexer_token: get("WS_ETHEREUM_INDEXER_TOKEN"),
            timeout: duration(get, "WS_ETHEREUM_TIMEOUT_SECONDS", 15)?,
        })
    }
}

fn group_enabled(values: &BTreeMap<String, String>, prefix: &str) -> bool {
    values.keys().any(|key| key.starts_with(prefix))
}
fn required(get: &impl Fn(&str) -> Option<String>, name: &str) -> Result<String, ConfigError> {
    get(name)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ConfigError::new(format!("{name} is required")))
}
fn boolean(value: Option<&str>, name: &str) -> Result<bool, ConfigError> {
    match value {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        _ => Err(ConfigError::new(format!("{name} must be true or false"))),
    }
}
fn number<T: std::str::FromStr + Copy>(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: T,
) -> Result<T, ConfigError> {
    get(name).map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|_| ConfigError::new(format!("{name} is invalid")))
    })
}
fn duration(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: u64,
) -> Result<Duration, ConfigError> {
    let seconds = number(get, name, default)?;
    if seconds == 0 {
        return Err(ConfigError::new(format!("{name} must be positive")));
    }
    Ok(Duration::from_secs(seconds))
}
fn secret(get: &impl Fn(&str) -> Option<String>, name: &str) -> Result<SecretBytes, ConfigError> {
    let bytes = hex::decode(required(get, name)?)
        .map_err(|_| ConfigError::new(format!("{name} must be hexadecimal")))?;
    if bytes.len() != 32 {
        return Err(ConfigError::new(format!(
            "{name} must contain exactly 32 bytes"
        )));
    }
    Ok(SecretBytes::new(bytes))
}
fn urls(
    get: &impl Fn(&str) -> Option<String>,
    plural: &str,
    singular: &str,
) -> Result<Vec<String>, ConfigError> {
    match (get(plural), get(singular)) {
        (Some(_), Some(_)) => Err(ConfigError::new(format!(
            "{plural} cannot be combined with {singular}"
        ))),
        (Some(value), None) => {
            let values = value
                .split(',')
                .map(str::trim)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if values.is_empty() || values.iter().any(String::is_empty) {
                Err(ConfigError::new(format!(
                    "{plural} requires non-empty URLs"
                )))
            } else {
                Ok(values)
            }
        }
        (None, Some(value)) if !value.trim().is_empty() => Ok(vec![value]),
        _ => Err(ConfigError::new(format!(
            "{plural} or {singular} is required"
        ))),
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for ConfigError {}
