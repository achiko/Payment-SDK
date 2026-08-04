use std::{collections::BTreeSet, fmt};

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
use indexing::{BlockHash, BlockHeight, BlockRef};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParsedBlock {
    pub reference: BlockRef,
    pub transactions: Vec<ParsedTransaction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParsedTransaction {
    pub hash: [u8; 32],
    pub from: [u8; 20],
    pub to: Option<[u8; 20]>,
    pub value: U256,
    pub index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParsedReceipt {
    pub transaction_hash: [u8; 32],
    pub transaction_index: u64,
    pub succeeded: bool,
    pub gas_used: u64,
    pub effective_gas_price: U256,
    pub contract_address: Option<[u8; 20]>,
    pub logs: Vec<ParsedLog>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParsedLog {
    pub address: [u8; 20],
    pub topics: Vec<[u8; 32]>,
    pub data: Vec<u8>,
    pub log_index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParseError {
    message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub(super) fn parse_block(
    raw: &[u8],
    expected_height: Option<BlockHeight>,
    require_full_transactions: bool,
) -> Result<ParsedBlock, ParseError> {
    // Alloy owns the official header shape, fork fields, and Ethereum quantity
    // decoding. Transaction objects remain raw Values so IX can validate only
    // the stable fields it consumes without accepting a universal tx model.
    let block: RpcBlock<Value> = serde_json::from_slice(raw)
        .map_err(|_| ParseError::new("Ethereum block result does not match the RPC block shape"))?;
    let height = BlockHeight(block.number());
    if expected_height.is_some_and(|expected| expected != height) {
        return Err(ParseError::new(
            "Ethereum block number does not match the requested height",
        ));
    }

    let hash = block.hash().0;
    let parent_hash = block.header.parent_hash.0;
    let reference = BlockRef {
        height,
        hash: BlockHash(hash.to_vec()),
        parent_hash: (height.0 != 0).then(|| BlockHash(parent_hash.to_vec())),
        timestamp: Some(block.header.timestamp),
    };

    let values = match block.transactions {
        BlockTransactions::Full(values) => values,
        BlockTransactions::Hashes(_) | BlockTransactions::Uncle if require_full_transactions => {
            return Err(ParseError::new(
                "Ethereum full-block request returned transaction hashes only",
            ));
        }
        BlockTransactions::Hashes(_) | BlockTransactions::Uncle => Vec::new(),
    };

    let mut transactions = Vec::with_capacity(values.len());
    let mut transaction_hashes = BTreeSet::new();
    for (position, value) in values.iter().enumerate() {
        if !require_full_transactions && !value.is_object() {
            continue;
        }
        let transaction = parse_transaction(value, position, height, hash)?;
        if !transaction_hashes.insert(transaction.hash) {
            return Err(ParseError::new(
                "Ethereum block contains duplicate transaction hashes",
            ));
        }
        transactions.push(transaction);
    }

    Ok(ParsedBlock {
        reference,
        transactions,
    })
}

fn parse_transaction(
    value: &Value,
    position: usize,
    block_height: BlockHeight,
    block_hash: [u8; 32],
) -> Result<ParsedTransaction, ParseError> {
    let object = object(value, "Ethereum transaction must be an object")?;
    let hash = required_hash(object, "hash", "transaction hash")?;
    let from = required_address(object, "from", "transaction sender")?;
    let to = optional_address(object, "to", "transaction recipient")?;
    let value = required_quantity(object, "value", "transaction value")?;
    let index = required_quantity_u64(object, "transactionIndex", "transaction index")?;
    let expected_index = u64::try_from(position)
        .map_err(|_| ParseError::new("Ethereum transaction position exceeds u64"))?;
    if index != expected_index {
        return Err(ParseError::new(
            "Ethereum transaction index does not match block order",
        ));
    }
    if required_hash(object, "blockHash", "transaction block hash")? != block_hash {
        return Err(ParseError::new(
            "Ethereum transaction block hash does not match its block",
        ));
    }
    if required_quantity_u64(object, "blockNumber", "transaction block number")? != block_height.0 {
        return Err(ParseError::new(
            "Ethereum transaction block number does not match its block",
        ));
    }

    Ok(ParsedTransaction {
        hash,
        from,
        to,
        value,
        index,
    })
}

pub(super) fn parse_and_validate_receipts(
    raw_receipts: &[Vec<u8>],
    block: &ParsedBlock,
) -> Result<Vec<ParsedReceipt>, ParseError> {
    if raw_receipts.len() != block.transactions.len() {
        return Err(ParseError::new(
            "Ethereum receipt count does not match transaction count",
        ));
    }

    let block_hash = fixed_hash(&block.reference.hash)?;
    let mut receipts = Vec::with_capacity(raw_receipts.len());
    let mut seen_log_indexes = BTreeSet::new();
    let mut previous_log_index = None;
    for (position, (raw, transaction)) in raw_receipts.iter().zip(&block.transactions).enumerate() {
        let value: Value = serde_json::from_slice(raw)
            .map_err(|_| ParseError::new("Ethereum receipt result is not valid JSON"))?;
        let object = object(&value, "Ethereum receipt must be an object")?;
        let transaction_hash = required_hash(object, "transactionHash", "receipt transaction")?;
        if transaction_hash != transaction.hash {
            return Err(ParseError::new(
                "Ethereum receipt transaction hash does not match block order",
            ));
        }
        let transaction_index =
            required_quantity_u64(object, "transactionIndex", "receipt transaction index")?;
        if transaction_index != transaction.index
            || transaction_index
                != u64::try_from(position)
                    .map_err(|_| ParseError::new("Ethereum receipt position exceeds u64"))?
        {
            return Err(ParseError::new(
                "Ethereum receipt transaction index does not match block order",
            ));
        }
        if required_hash(object, "blockHash", "receipt block hash")? != block_hash {
            return Err(ParseError::new(
                "Ethereum receipt block hash does not match its block",
            ));
        }
        if required_quantity_u64(object, "blockNumber", "receipt block number")?
            != block.reference.height.0
        {
            return Err(ParseError::new(
                "Ethereum receipt block number does not match its block",
            ));
        }
        if required_address(object, "from", "receipt sender")? != transaction.from {
            return Err(ParseError::new(
                "Ethereum receipt sender does not match its transaction",
            ));
        }
        if optional_address(object, "to", "receipt recipient")? != transaction.to {
            return Err(ParseError::new(
                "Ethereum receipt recipient does not match its transaction",
            ));
        }

        let status = required_quantity(object, "status", "receipt status")?;
        let succeeded = if status == U256::ZERO {
            false
        } else if status == U256::from(1_u8) {
            true
        } else {
            return Err(ParseError::new(
                "Ethereum receipt status must be zero or one",
            ));
        };
        let gas_used = required_quantity_u64(object, "gasUsed", "receipt gas used")?;
        let effective_gas_price =
            required_quantity(object, "effectiveGasPrice", "receipt effective gas price")?;
        let contract_address =
            optional_address(object, "contractAddress", "receipt contract address")?;
        if transaction.to.is_some() && contract_address.is_some() {
            return Err(ParseError::new(
                "non-creation receipt unexpectedly contains a contract address",
            ));
        }
        if succeeded
            && transaction.to.is_none()
            && !transaction.value.is_zero()
            && contract_address.is_none()
        {
            return Err(ParseError::new(
                "successful value-bearing contract creation has no contract address",
            ));
        }

        let log_values = object
            .get("logs")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::new("Ethereum receipt logs must be an array"))?;
        let mut logs = Vec::with_capacity(log_values.len());
        for log_value in log_values {
            let log = parse_log(
                log_value,
                block.reference.height,
                block_hash,
                transaction_hash,
                transaction_index,
            )?;
            if !seen_log_indexes.insert(log.log_index) {
                return Err(ParseError::new(
                    "Ethereum block contains duplicate log indexes",
                ));
            }
            if previous_log_index.is_some_and(|previous| log.log_index <= previous) {
                return Err(ParseError::new(
                    "Ethereum logs are not ordered by log index",
                ));
            }
            previous_log_index = Some(log.log_index);
            logs.push(log);
        }

        receipts.push(ParsedReceipt {
            transaction_hash,
            transaction_index,
            succeeded,
            gas_used,
            effective_gas_price,
            contract_address,
            logs,
        });
    }
    Ok(receipts)
}

fn parse_log(
    value: &Value,
    block_height: BlockHeight,
    block_hash: [u8; 32],
    transaction_hash: [u8; 32],
    transaction_index: u64,
) -> Result<ParsedLog, ParseError> {
    let object = object(value, "Ethereum receipt log must be an object")?;
    if required_hash(object, "blockHash", "log block hash")? != block_hash
        || required_quantity_u64(object, "blockNumber", "log block number")? != block_height.0
        || required_hash(object, "transactionHash", "log transaction hash")? != transaction_hash
        || required_quantity_u64(object, "transactionIndex", "log transaction index")?
            != transaction_index
    {
        return Err(ParseError::new(
            "Ethereum log identity does not match its receipt and block",
        ));
    }
    if object.get("removed").and_then(Value::as_bool) != Some(false) {
        return Err(ParseError::new(
            "canonical Ethereum receipt contains a removed log",
        ));
    }
    let address = required_address(object, "address", "log address")?;
    let topics = object
        .get("topics")
        .and_then(Value::as_array)
        .ok_or_else(|| ParseError::new("Ethereum log topics must be an array"))?
        .iter()
        .map(|topic| {
            topic
                .as_str()
                .ok_or_else(|| ParseError::new("Ethereum log topic must be a hash"))
                .and_then(|topic| parse_hash(topic, "log topic"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let data = object
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::new("Ethereum log data must be a hex byte string"))
        .and_then(|data| parse_hex_bytes(data, "log data"))?;
    let log_index = required_quantity_u64(object, "logIndex", "log index")?;

    Ok(ParsedLog {
        address,
        topics,
        data,
        log_index,
    })
}

fn object<'a>(value: &'a Value, message: &str) -> Result<&'a Map<String, Value>, ParseError> {
    value.as_object().ok_or_else(|| ParseError::new(message))
}

fn required_hash(
    object: &Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<[u8; 32], ParseError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::new(format!("Ethereum {label} is missing")))
        .and_then(|value| parse_hash(value, label))
}

fn parse_hash(value: &str, label: &str) -> Result<[u8; 32], ParseError> {
    value
        .parse::<B256>()
        .map(Into::into)
        .map_err(|_| ParseError::new(format!("Ethereum {label} is not a 32-byte hash")))
}

fn required_address(
    object: &Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<[u8; 20], ParseError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::new(format!("Ethereum {label} is missing")))
        .and_then(|value| parse_address(value, label))
}

fn optional_address(
    object: &Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<Option<[u8; 20]>, ParseError> {
    match object.get(key) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::String(value)) => parse_address(value, label).map(Some),
        Some(_) => Err(ParseError::new(format!(
            "Ethereum {label} must be an address or null"
        ))),
    }
}

fn parse_address(value: &str, label: &str) -> Result<[u8; 20], ParseError> {
    value
        .parse::<Address>()
        .map(Address::into_array)
        .map_err(|_| ParseError::new(format!("Ethereum {label} is not a 20-byte address")))
}

fn required_quantity(
    object: &Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<U256, ParseError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::new(format!("Ethereum {label} is missing")))
        .and_then(|value| parse_quantity(value, label))
}

fn required_quantity_u64(
    object: &Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<u64, ParseError> {
    let value = required_quantity(object, key, label)?;
    u64::try_from(value).map_err(|_| ParseError::new(format!("Ethereum {label} exceeds u64")))
}

pub(super) fn parse_quantity(value: &str, label: &str) -> Result<U256, ParseError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or_else(|| ParseError::new(format!("Ethereum {label} is not a hex quantity")))?;
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return Err(ParseError::new(format!(
            "Ethereum {label} is not a canonical hex quantity"
        )));
    }
    U256::from_str_radix(digits, 16)
        .map_err(|_| ParseError::new(format!("Ethereum {label} exceeds 256 bits")))
}

fn parse_hex_bytes(value: &str, label: &str) -> Result<Vec<u8>, ParseError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or_else(|| ParseError::new(format!("Ethereum {label} is not hex encoded")))?;
    if digits.len() % 2 != 0 {
        return Err(ParseError::new(format!(
            "Ethereum {label} has an odd number of hex digits"
        )));
    }
    digits
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair)
                .map_err(|_| ParseError::new(format!("Ethereum {label} is not valid hex")))?;
            u8::from_str_radix(pair, 16)
                .map_err(|_| ParseError::new(format!("Ethereum {label} is not valid hex")))
        })
        .collect()
}

fn fixed_hash(hash: &BlockHash) -> Result<[u8; 32], ParseError> {
    hash.0
        .as_slice()
        .try_into()
        .map_err(|_| ParseError::new("Ethereum block hash is not 32 bytes"))
}

pub(super) fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(2 + bytes.len() * 2);
    encoded.push_str("0x");
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
