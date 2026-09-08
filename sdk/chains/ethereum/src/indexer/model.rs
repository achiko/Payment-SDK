use std::{collections::BTreeSet, fmt};

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
use indexing::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
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

impl ParsedBlock {
    pub(super) fn parse(
        raw: &[u8],
        expected_height: Option<BlockHeight>,
        require_full_transactions: bool,
    ) -> Result<Self, ParseError> {
        // Alloy owns the official header shape, fork fields, and Ethereum quantity
        // decoding. Transaction objects remain raw Values so IX can validate only
        // the stable fields it consumes without accepting a universal tx model.
        let block: RpcBlock<Value> = serde_json::from_slice(raw).map_err(|_| {
            ParseError::new("Ethereum block result does not match the RPC block shape")
        })?;
        let height = BlockHeight(block.number());
        if expected_height.is_some_and(|expected| expected != height) {
            return Err(ParseError::new(
                "Ethereum block number does not match the requested height",
            ));
        }

        let hash = block.hash().0;
        let parent_hash = block.header.parent_hash.0;
        let reference = BlockRef {
            position: BlockPosition(height.0),
            height,
            hash: BlockHash(hash.to_vec()),
            parent: (height.0 != 0).then(|| BlockParent {
                position: BlockPosition(height.0 - 1),
                hash: BlockHash(parent_hash.to_vec()),
            }),
            timestamp: Some(block.header.timestamp),
        };

        let values = match block.transactions {
            BlockTransactions::Full(values) => values,
            BlockTransactions::Hashes(_) | BlockTransactions::Uncle
                if require_full_transactions =>
            {
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
            let transaction = ParsedTransaction::parse(value, position, height, hash)?;
            if !transaction_hashes.insert(transaction.hash) {
                return Err(ParseError::new(
                    "Ethereum block contains duplicate transaction hashes",
                ));
            }
            transactions.push(transaction);
        }

        Ok(Self {
            reference,
            transactions,
        })
    }
}

impl ParsedTransaction {
    fn parse(
        value: &Value,
        position: usize,
        block_height: BlockHeight,
        block_hash: [u8; 32],
    ) -> Result<Self, ParseError> {
        let object = value
            .as_object()
            .ok_or_else(|| ParseError::new("Ethereum transaction must be an object"))?;
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
        if required_quantity_u64(object, "blockNumber", "transaction block number")?
            != block_height.0
        {
            return Err(ParseError::new(
                "Ethereum transaction block number does not match its block",
            ));
        }

        Ok(Self {
            hash,
            from,
            to,
            value,
            index,
        })
    }
}

impl ParsedReceipt {
    pub(super) fn parse_all(
        raw_receipts: &[Vec<u8>],
        block: &ParsedBlock,
    ) -> Result<Vec<ParsedReceipt>, ParseError> {
        if raw_receipts.len() != block.transactions.len() {
            return Err(ParseError::new(
                "Ethereum receipt count does not match transaction count",
            ));
        }

        let block_hash: [u8; 32] = block
            .reference
            .hash
            .0
            .as_slice()
            .try_into()
            .map_err(|_| ParseError::new("Ethereum block hash is not 32 bytes"))?;
        let mut receipts = Vec::with_capacity(raw_receipts.len());
        let mut seen_log_indexes = BTreeSet::new();
        let mut previous_log_index = None;
        for (position, (raw, transaction)) in
            raw_receipts.iter().zip(&block.transactions).enumerate()
        {
            receipts.push(Self::parse(
                raw,
                transaction,
                position,
                block.reference.height,
                block_hash,
                &mut seen_log_indexes,
                &mut previous_log_index,
            )?);
        }
        Ok(receipts)
    }

    fn parse(
        raw: &[u8],
        transaction: &ParsedTransaction,
        position: usize,
        block_height: BlockHeight,
        block_hash: [u8; 32],
        seen_log_indexes: &mut BTreeSet<u64>,
        previous_log_index: &mut Option<u64>,
    ) -> Result<Self, ParseError> {
        let value: Value = serde_json::from_slice(raw)
            .map_err(|_| ParseError::new("Ethereum receipt result is not valid JSON"))?;
        let object = value
            .as_object()
            .ok_or_else(|| ParseError::new("Ethereum receipt must be an object"))?;
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
        if required_quantity_u64(object, "blockNumber", "receipt block number")? != block_height.0 {
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
            let log = ParsedLog::parse(
                log_value,
                block_height,
                block_hash,
                transaction_hash,
                transaction_index,
            )?;
            if !seen_log_indexes.insert(log.log_index) {
                return Err(ParseError::new(
                    "Ethereum block contains duplicate log indexes",
                ));
            }
            let out_of_order = previous_log_index.is_some_and(|previous| log.log_index <= previous);
            if out_of_order {
                return Err(ParseError::new(
                    "Ethereum logs are not ordered by log index",
                ));
            }
            *previous_log_index = Some(log.log_index);
            logs.push(log);
        }

        Ok(Self {
            transaction_hash,
            transaction_index,
            succeeded,
            gas_used,
            effective_gas_price,
            contract_address,
            logs,
        })
    }
}

impl ParsedLog {
    fn parse(
        value: &Value,
        block_height: BlockHeight,
        block_hash: [u8; 32],
        transaction_hash: [u8; 32],
        transaction_index: u64,
    ) -> Result<Self, ParseError> {
        let object = value
            .as_object()
            .ok_or_else(|| ParseError::new("Ethereum receipt log must be an object"))?;
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
                let topic = topic
                    .as_str()
                    .ok_or_else(|| ParseError::new("Ethereum log topic must be a hash"))?;
                topic
                    .parse::<B256>()
                    .map(Into::into)
                    .map_err(|_| ParseError::new("Ethereum log topic is not a 32-byte hash"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let data = object
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::new("Ethereum log data must be a hex byte string"))?;
        let digits = data
            .strip_prefix("0x")
            .ok_or_else(|| ParseError::new("Ethereum log data is not hex encoded"))?;
        if digits.len() % 2 != 0 {
            return Err(ParseError::new(
                "Ethereum log data has an odd number of hex digits",
            ));
        }
        let data = digits
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair)
                    .map_err(|_| ParseError::new("Ethereum log data is not valid hex"))?;
                u8::from_str_radix(pair, 16)
                    .map_err(|_| ParseError::new("Ethereum log data is not valid hex"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let log_index = required_quantity_u64(object, "logIndex", "log index")?;

        Ok(Self {
            address,
            topics,
            data,
            log_index,
        })
    }
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
        .and_then(|value| {
            value
                .parse::<B256>()
                .map(Into::into)
                .map_err(|_| ParseError::new(format!("Ethereum {label} is not a 32-byte hash")))
        })
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
        .and_then(|value| {
            value
                .parse::<Address>()
                .map(Address::into_array)
                .map_err(|_| ParseError::new(format!("Ethereum {label} is not a 20-byte address")))
        })
}

fn optional_address(
    object: &Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<Option<[u8; 20]>, ParseError> {
    match object.get(key) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::String(value)) => value
            .parse::<Address>()
            .map(|address| Some(address.into_array()))
            .map_err(|_| ParseError::new(format!("Ethereum {label} is not a 20-byte address"))),
        Some(_) => Err(ParseError::new(format!(
            "Ethereum {label} must be an address or null"
        ))),
    }
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

// design-lint: allow unclassified-free-function -- shared indexing quantity decoder preserves field context and established native U256 parsing semantics across transaction fields and source chain-ID checks
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block(hash: Vec<u8>) -> ParsedBlock {
        ParsedBlock {
            reference: BlockRef {
                position: BlockPosition(10),
                height: BlockHeight(10),
                hash: BlockHash(hash),
                parent: Some(BlockParent {
                    position: BlockPosition(9),
                    hash: BlockHash(vec![0xbb; 32]),
                }),
                timestamp: Some(100),
            },
            transactions: vec![ParsedTransaction {
                hash: [0xcc; 32],
                from: [0x11; 20],
                to: Some([0x22; 20]),
                value: U256::from(42_u8),
                index: 0,
            }],
        }
    }

    fn receipt(transaction: &ParsedTransaction, log_indexes: &[u64]) -> Value {
        let logs = log_indexes
            .iter()
            .map(|index| {
                json!({
                    "blockHash": B256::from([0xaa; 32]).to_string(),
                    "blockNumber": "0xa",
                    "transactionHash": B256::from(transaction.hash).to_string(),
                    "transactionIndex": format!("0x{:x}", transaction.index),
                    "removed": false,
                    "address": Address::from([0x33; 20]).to_string(),
                    "topics": [],
                    "data": "0x00abff",
                    "logIndex": format!("0x{index:x}")
                })
            })
            .collect::<Vec<_>>();
        json!({
            "transactionHash": B256::from(transaction.hash).to_string(),
            "transactionIndex": format!("0x{:x}", transaction.index),
            "blockHash": B256::from([0xaa; 32]).to_string(),
            "blockNumber": "0xa",
            "from": Address::from(transaction.from).to_string(),
            "to": transaction.to.map(|address| Address::from(address).to_string()),
            "contractAddress": null,
            "status": "0x1",
            "gasUsed": "0x5208",
            "effectiveGasPrice": "0x3",
            "logs": logs
        })
    }

    #[test]
    fn receipt_log_order_and_uniqueness_are_block_wide() {
        let mut block = block(vec![0xaa; 32]);
        block.transactions.push(ParsedTransaction {
            hash: [0xdd; 32],
            index: 1,
            ..block.transactions[0].clone()
        });
        let first = serde_json::to_vec(&receipt(&block.transactions[0], &[0, 4])).unwrap();
        let second = serde_json::to_vec(&receipt(&block.transactions[1], &[7, 9])).unwrap();
        let receipts = ParsedReceipt::parse_all(&[first.clone(), second], &block).unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].transaction_hash, [0xcc; 32]);
        assert_eq!(receipts[1].transaction_hash, [0xdd; 32]);
        assert_eq!(receipts[1].transaction_index, 1);
        assert_eq!(
            receipts
                .iter()
                .flat_map(|receipt| &receipt.logs)
                .map(|log| (log.log_index, log.data.as_slice()))
                .collect::<Vec<_>>(),
            vec![
                (0, &[0, 0xab, 0xff][..]),
                (4, &[0, 0xab, 0xff][..]),
                (7, &[0, 0xab, 0xff][..]),
                (9, &[0, 0xab, 0xff][..]),
            ]
        );

        for (index, expected) in [
            (4, "Ethereum block contains duplicate log indexes"),
            (0, "Ethereum block contains duplicate log indexes"),
            (3, "Ethereum logs are not ordered by log index"),
        ] {
            let second = serde_json::to_vec(&receipt(&block.transactions[1], &[index])).unwrap();
            let error = ParsedReceipt::parse_all(&[first.clone(), second], &block).unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn receipt_log_validation_stops_at_the_first_invalid_log() {
        let block = block(vec![0xaa; 32]);
        let mut value = receipt(&block.transactions[0], &[4, 4, 5]);
        value["logs"][2]["data"] = json!("0xgg");
        let error =
            ParsedReceipt::parse_all(&[serde_json::to_vec(&value).unwrap()], &block).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Ethereum block contains duplicate log indexes"
        );

        value["logs"][1]["data"] = json!("0xgg");
        let error =
            ParsedReceipt::parse_all(&[serde_json::to_vec(&value).unwrap()], &block).unwrap_err();
        assert_eq!(error.to_string(), "Ethereum log data is not valid hex");

        value["transactionHash"] = json!(B256::from([0xdd; 32]).to_string());
        let error =
            ParsedReceipt::parse_all(&[serde_json::to_vec(&value).unwrap()], &block).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Ethereum receipt transaction hash does not match block order"
        );
    }

    #[test]
    fn indexing_quantity_preserves_library_grammar_and_field_errors() {
        for (text, expected) in [("0x0", 0), ("0xAbC", 0xabc), ("0x_", 0), ("0x1_0", 16)] {
            assert_eq!(
                parse_quantity(text, "chain ID").unwrap(),
                U256::from(expected)
            );
        }
        assert_eq!(
            parse_quantity(&format!("0x{}", "f".repeat(64)), "value").unwrap(),
            U256::MAX
        );
        for (text, reason) in [
            ("0X1", "is not a hex quantity"),
            ("0x", "is not a canonical hex quantity"),
            ("0x0g", "is not a canonical hex quantity"),
            ("0xg", "exceeds 256 bits"),
            ("0xé", "exceeds 256 bits"),
        ] {
            assert_eq!(
                parse_quantity(text, "chain ID").unwrap_err().to_string(),
                format!("Ethereum chain ID {reason}")
            );
        }
        assert_eq!(
            parse_quantity(&format!("0x1{}", "0".repeat(64)), "value")
                .unwrap_err()
                .to_string(),
            "Ethereum value exceeds 256 bits"
        );
    }

    #[test]
    fn address_and_hash_fields_keep_alloy_syntax_and_contextual_errors() {
        for prefix in ["", "0x", "0X"] {
            let value = json!({"address": format!("{prefix}{}", "aB".repeat(20)),
                               "hash": format!("{prefix}{}", "aB".repeat(32))});
            let object = value.as_object().unwrap();
            assert_eq!(
                required_address(object, "address", "sender").unwrap(),
                [0xab; 20]
            );
            assert_eq!(
                optional_address(object, "address", "recipient").unwrap(),
                Some([0xab; 20])
            );
            assert_eq!(
                required_hash(object, "hash", "transaction hash").unwrap(),
                [0xab; 32]
            );
        }
        for invalid in ["0x", "0Xabcd", "0x0xabcd", "é", "zz"] {
            let value = json!({"address": invalid, "hash": invalid});
            let object = value.as_object().unwrap();
            assert_eq!(
                required_address(object, "address", "sender")
                    .unwrap_err()
                    .to_string(),
                "Ethereum sender is not a 20-byte address"
            );
            assert_eq!(
                optional_address(object, "address", "recipient")
                    .unwrap_err()
                    .to_string(),
                "Ethereum recipient is not a 20-byte address"
            );
            assert_eq!(
                required_hash(object, "hash", "transaction hash")
                    .unwrap_err()
                    .to_string(),
                "Ethereum transaction hash is not a 32-byte hash"
            );
        }
        let object = Map::new();
        assert_eq!(
            optional_address(&object, "address", "recipient").unwrap(),
            None
        );
        assert_eq!(
            required_address(&object, "address", "sender")
                .unwrap_err()
                .to_string(),
            "Ethereum sender is missing"
        );
        assert_eq!(
            required_hash(&object, "hash", "transaction hash")
                .unwrap_err()
                .to_string(),
            "Ethereum transaction hash is missing"
        );
        let value = json!({"address": null});
        assert_eq!(
            optional_address(value.as_object().unwrap(), "address", "recipient").unwrap(),
            None
        );
        let value = json!({"address": 1});
        assert_eq!(
            optional_address(value.as_object().unwrap(), "address", "recipient")
                .unwrap_err()
                .to_string(),
            "Ethereum recipient must be an address or null"
        );
    }

    #[test]
    fn object_shape_errors_precede_identity_field_validation() {
        let block = block(vec![0xaa; 32]);
        for value in [
            json!(null),
            json!([]),
            json!("object"),
            json!(42),
            json!(false),
        ] {
            let transaction = ParsedTransaction::parse(&value, 0, BlockHeight(10), [0xaa; 32])
                .expect_err("transaction requires an object");
            assert_eq!(
                transaction.to_string(),
                "Ethereum transaction must be an object"
            );
            let receipt = ParsedReceipt::parse_all(&[serde_json::to_vec(&value).unwrap()], &block)
                .expect_err("receipt requires an object");
            assert_eq!(receipt.to_string(), "Ethereum receipt must be an object");
            let log = ParsedLog::parse(&value, BlockHeight(10), [0xaa; 32], [0xcc; 32], 0)
                .expect_err("log requires an object");
            assert_eq!(log.to_string(), "Ethereum receipt log must be an object");
        }
    }

    #[test]
    fn receipt_parsing_preserves_all_32_block_hash_bytes() {
        let mut block = block((0_u8..32).collect());
        let raw = serde_json::to_vec(&json!({
            "transactionHash": "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "transactionIndex": "0x0",
            "blockHash": "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "blockNumber": "0xa",
            "from": "0x1111111111111111111111111111111111111111",
            "to": "0x2222222222222222222222222222222222222222",
            "contractAddress": null,
            "status": "0x1",
            "gasUsed": "0x5208",
            "effectiveGasPrice": "0x3",
            "logs": []
        }))
        .expect("receipt fixture must serialize");

        let receipts = ParsedReceipt::parse_all(std::slice::from_ref(&raw), &block)
            .expect("receipt identity must match the exact block hash");
        assert_eq!(
            receipts,
            vec![ParsedReceipt {
                transaction_hash: [0xcc; 32],
                transaction_index: 0,
                succeeded: true,
                gas_used: 21_000,
                effective_gas_price: U256::from(3_u8),
                contract_address: None,
                logs: Vec::new(),
            }]
        );

        block.reference.hash.0.reverse();
        let error = ParsedReceipt::parse_all(&[raw], &block)
            .expect_err("changing block hash byte order must invalidate receipt identity");
        assert_eq!(
            error.to_string(),
            "Ethereum receipt block hash does not match its block"
        );
    }

    #[test]
    fn receipt_parsing_checks_hash_length_before_receipt_json() {
        for length in [31, 33] {
            let block = block(vec![0xaa; length]);
            let error = ParsedReceipt::parse_all(&[b"not-json".to_vec()], &block)
                .expect_err("invalid block hash length must fail before receipt decoding");

            assert_eq!(error.to_string(), "Ethereum block hash is not 32 bytes");
        }
    }

    #[test]
    fn receipt_parsing_checks_count_before_hash_length() {
        let block = block(vec![0xaa; 31]);
        let error = ParsedReceipt::parse_all(&[], &block)
            .expect_err("missing receipt must fail before block hash validation");

        assert_eq!(
            error.to_string(),
            "Ethereum receipt count does not match transaction count"
        );
    }

    #[test]
    fn log_parsing_preserves_accepted_data_bytes() {
        let mut value = json!({
            "blockHash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "blockNumber": "0xa",
            "transactionHash": "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "transactionIndex": "0x0",
            "removed": false,
            "address": "0x1111111111111111111111111111111111111111",
            "topics": [],
            "logIndex": "0x2"
        });
        for (data, expected) in [
            ("0x", vec![]),
            ("0x00aBfF", vec![0, 0xab, 0xff]),
            // Preserve the existing decoder's acceptance of a plus-prefixed pair.
            ("0x+1+a", vec![1, 10]),
        ] {
            value["data"] = json!(data);
            let parsed = ParsedLog::parse(&value, BlockHeight(10), [0xaa; 32], [0xcc; 32], 0)
                .expect("previously accepted log data must still decode");
            assert_eq!(parsed.data, expected, "data: {data}");
            assert_eq!(parsed.log_index, 2);
        }
    }

    #[test]
    fn log_parsing_preserves_data_errors_before_log_index_validation() {
        let mut value = json!({
            "blockHash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "blockNumber": "0xa",
            "transactionHash": "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "transactionIndex": "0x0",
            "removed": false,
            "address": "0x1111111111111111111111111111111111111111",
            "topics": []
        });
        for (data, message) in [
            (json!(null), "Ethereum log data must be a hex byte string"),
            (json!(17), "Ethereum log data must be a hex byte string"),
            (json!("00"), "Ethereum log data is not hex encoded"),
            (json!("0X00"), "Ethereum log data is not hex encoded"),
            (
                json!("0x0"),
                "Ethereum log data has an odd number of hex digits",
            ),
            (
                json!("0x€"),
                "Ethereum log data has an odd number of hex digits",
            ),
            (json!("0xgg"), "Ethereum log data is not valid hex"),
            (json!("0xé"), "Ethereum log data is not valid hex"),
            (json!("0x€a"), "Ethereum log data is not valid hex"),
        ] {
            value["data"] = data;
            let error = ParsedLog::parse(&value, BlockHeight(10), [0xaa; 32], [0xcc; 32], 0)
                .expect_err("invalid log data must fail before the missing log index");
            assert_eq!(error.to_string(), message, "data: {}", value["data"]);
        }

        value.as_object_mut().unwrap().remove("data");
        let error = ParsedLog::parse(&value, BlockHeight(10), [0xaa; 32], [0xcc; 32], 0)
            .expect_err("missing log data must fail before the missing log index");
        assert_eq!(
            error.to_string(),
            "Ethereum log data must be a hex byte string"
        );

        value["data"] = json!("0x");
        let error = ParsedLog::parse(&value, BlockHeight(10), [0xaa; 32], [0xcc; 32], 0)
            .expect_err("valid data must allow log index validation to run");
        assert_eq!(error.to_string(), "Ethereum log index is missing");
    }
}
