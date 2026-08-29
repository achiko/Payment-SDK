use std::str::FromStr;

use bitcoin::{BlockHash as NativeBlockHash, hashes::Hash};
use indexing::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, SourceError};
use serde_json::{Map, Number, Value};

use crate::{FeeRate, Network};

use super::{
    BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB, SATOSHIS_PER_BITCOIN,
    error::{map_json_rpc_error, source_error},
    transport::RawJson,
};

impl Network {
    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet3 => "testnet3",
            Self::Testnet4 => "testnet4",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        }
    }

    pub const fn core_chain_name(self) -> &'static str {
        match self {
            Self::Mainnet => "main",
            Self::Testnet3 => "test",
            Self::Testnet4 => "testnet4",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        }
    }

    pub(crate) const fn from_core_chain_name(name: &str) -> Option<Self> {
        match name.as_bytes() {
            b"main" => Some(Self::Mainnet),
            b"test" => Some(Self::Testnet3),
            b"testnet4" => Some(Self::Testnet4),
            b"signet" => Some(Self::Signet),
            b"regtest" => Some(Self::Regtest),
            _ => None,
        }
    }

    pub(crate) const fn native(self) -> bitcoin::Network {
        match self {
            Self::Mainnet => bitcoin::Network::Bitcoin,
            Self::Testnet3 => bitcoin::Network::Testnet,
            Self::Testnet4 => bitcoin::Network::Testnet4,
            Self::Signet => bitcoin::Network::Signet,
            Self::Regtest => bitcoin::Network::Regtest,
        }
    }
}

pub(crate) fn parse_header(
    raw: &RawJson,
    expected_height: Option<BlockHeight>,
) -> Result<BlockRef, SourceError> {
    let result = parse_object(raw, "Bitcoin getblockheader result")?;
    let height = BlockHeight(required_u64(
        &result,
        "height",
        "Bitcoin block-header height",
    )?);
    if expected_height.is_some_and(|expected| expected != height) {
        return Err(source_error(
            "Bitcoin block header does not match the requested height",
            true,
        ));
    }
    let hash = parse_bitcoin_block_hash(&required_string(
        &result,
        "hash",
        "Bitcoin block-header hash",
    )?)?;
    let parent = if height.0 == 0 {
        None
    } else {
        Some(BlockParent {
            position: BlockPosition(height.0 - 1),
            hash: parse_bitcoin_block_hash(&required_string(
                &result,
                "previousblockhash",
                "Bitcoin previous block hash",
            )?)?,
        })
    };
    let timestamp = required_u64(&result, "time", "Bitcoin block-header timestamp")?;
    Ok(BlockRef {
        position: BlockPosition(height.0),
        height,
        hash,
        parent,
        timestamp: Some(timestamp),
    })
}

pub fn parse_bitcoin_block_hash(value: &str) -> Result<BlockHash, SourceError> {
    value
        .parse::<NativeBlockHash>()
        .map(|hash| BlockHash(hash.to_byte_array().to_vec()))
        .map_err(|_| source_error("Bitcoin RPC returned an invalid block hash", true))
}

pub fn format_bitcoin_block_hash(hash: &BlockHash) -> Result<String, SourceError> {
    let bytes: [u8; 32] = hash
        .0
        .as_slice()
        .try_into()
        .map_err(|_| source_error("Bitcoin block hash must be 32 bytes", false))?;
    Ok(NativeBlockHash::from_byte_array(bytes).to_string())
}

pub(super) fn parse_object(
    raw: &RawJson,
    context: &'static str,
) -> Result<Map<String, Value>, SourceError> {
    raw.deserialize::<Value>()
        .map_err(map_json_rpc_error)?
        .as_object()
        .cloned()
        .ok_or_else(|| source_error(format!("{context} must be an object"), true))
}

pub(super) fn required_string(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<String, SourceError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| source_error(format!("{context} is missing or invalid"), true))
}

pub(super) fn required_u64(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<u64, SourceError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| source_error(format!("{context} is missing or invalid"), true))
}

pub(super) fn required_bool(
    object: &Map<String, Value>,
    field: &'static str,
    context: &'static str,
) -> Result<bool, SourceError> {
    object
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| source_error(format!("{context} is missing or invalid"), true))
}

pub(super) fn parse_btc_amount(value: &Value, context: &'static str) -> Result<u64, SourceError> {
    let lexical = value
        .as_number()
        .map(Number::to_string)
        .ok_or_else(|| source_error(format!("{context} must be a JSON number"), true))?;
    if lexical.starts_with('-') || lexical.contains(['e', 'E', '+']) {
        return Err(source_error(
            format!("{context} must be a non-negative fixed-point decimal"),
            true,
        ));
    }
    let mut parts = lexical.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 8
    {
        return Err(source_error(
            format!("{context} is not an exact Bitcoin amount"),
            true,
        ));
    }
    let whole = whole
        .parse::<u64>()
        .map_err(|_| source_error(format!("{context} exceeds u64 satoshis"), true))?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        let parsed = fraction
            .parse::<u64>()
            .map_err(|_| source_error(format!("{context} is invalid"), true))?;
        let power = u32::try_from(8_usize.saturating_sub(fraction.len()))
            .map_err(|_| source_error(format!("{context} precision is invalid"), true))?;
        parsed
            .checked_mul(10_u64.pow(power))
            .ok_or_else(|| source_error(format!("{context} exceeds u64 satoshis"), true))?
    };
    whole
        .checked_mul(SATOSHIS_PER_BITCOIN)
        .and_then(|satoshis| satoshis.checked_add(fraction))
        .ok_or_else(|| source_error(format!("{context} exceeds u64 satoshis"), true))
}

pub(super) fn fee_rate_json(fee_rate: FeeRate) -> Result<Number, SourceError> {
    let satoshis = fee_rate.satoshis_per_kvb();
    if satoshis == 0 {
        return Err(source_error(
            "Bitcoin maximum fee rate must be greater than zero",
            false,
        ));
    }
    if satoshis > BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB {
        return Err(source_error(
            "Bitcoin maximum fee rate exceeds Bitcoin Core's 1 BTC/kvB limit",
            false,
        ));
    }
    let whole = satoshis / SATOSHIS_PER_BITCOIN;
    let remainder = satoshis % SATOSHIS_PER_BITCOIN;
    let lexical = if remainder == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{remainder:08}")
            .trim_end_matches('0')
            .to_owned()
    };
    Number::from_str(&lexical)
        .map_err(|_| source_error("Bitcoin maximum fee rate could not be encoded", false))
}
