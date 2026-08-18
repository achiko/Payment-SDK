use std::{collections::BTreeSet, sync::LazyLock};

use alloy_primitives::{B256, U256};
use base::Decimal;
use indexing::{
    AssetId, BlockInterpreter as IndexBlockInterpreter, CanonicalAddress, ChainId, IndexChanges,
    IndexError, IndexErrorKind, IndexScope, IndexUndo, InterpretedBlock, MovementId, NetworkFee,
    ObservationDraft, ObservationDraftStatus, RawBlock, TransactionRef, ValueMovement, WatchId,
    WatchSelector, WatchTarget,
};
use num_bigint::BigUint;

use super::{
    Block,
    model::{ParsedBlock, ParsedLog, ParsedReceipt, ParsedTransaction, encode_hex},
};

const TRANSFER_TOPIC: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];
const ZERO_ADDRESS: [u8; 20] = [0; 20];
static CHAIN_ID: LazyLock<ChainId> = LazyLock::new(|| ChainId(crate::CHAIN.to_owned()));
static NATIVE_ASSET: LazyLock<AssetId> = LazyLock::new(|| AssetId {
    chain: (*CHAIN_ID).clone(),
    asset: "native".to_owned(),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInterpreter {
    scope: IndexScope,
}

impl BlockInterpreter {
    pub fn new(scope: IndexScope) -> Result<Self, IndexError> {
        if scope.chain != *CHAIN_ID {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum interpreter scope must use the ethereum chain ID",
                false,
            ));
        }
        if scope.network.trim().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum interpreter network slug must not be empty",
                false,
            ));
        }
        Ok(Self { scope })
    }

    #[must_use]
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }

    fn validate_watch(&self, watch: &WatchTarget<WatchSelector>) -> Result<(), IndexError> {
        if watch.scope != self.scope {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum watch belongs to a different scope",
                false,
            ));
        }
        if watch.target != watch.selector {
            return Err(IndexError::new(
                IndexErrorKind::InvalidWatch,
                "Ethereum watch target does not match its canonical selector",
                false,
            ));
        }
        Ok(())
    }

    fn watch_ids(
        &self,
        transaction: &ParsedTransaction,
        receipt: &ParsedReceipt,
        movements: &[ValueMovement],
        watches: &[WatchTarget<WatchSelector>],
        height: indexing::BlockHeight,
    ) -> Result<Vec<WatchId>, IndexError> {
        let mut touched_addresses = BTreeSet::from([transaction.from]);
        if let Some(to) = transaction.to {
            touched_addresses.insert(to);
        }
        if let Some(contract) = receipt.contract_address {
            touched_addresses.insert(contract);
        }
        for movement in movements {
            for endpoint in [movement.from(), movement.to()].into_iter().flatten() {
                let bytes = parse_canonical_address(endpoint)?;
                touched_addresses.insert(bytes);
            }
        }

        let mut ids = BTreeSet::new();
        for watch in watches {
            self.validate_watch(watch)?;
            if !watch.is_active_at(height) {
                continue;
            }
            let matched = match &watch.selector {
                WatchSelector::Address(address) => {
                    if !address.belongs_to(&self.scope) {
                        return Err(IndexError::new(
                            IndexErrorKind::ScopeMismatch,
                            "Ethereum address watch belongs to a different scope",
                            false,
                        ));
                    }
                    touched_addresses.contains(&parse_canonical_address(address)?)
                }
                WatchSelector::Transaction(transaction_id) => {
                    if !transaction_id.belongs_to(&self.scope) {
                        return Err(IndexError::new(
                            IndexErrorKind::ScopeMismatch,
                            "Ethereum transaction watch belongs to a different scope",
                            false,
                        ));
                    }
                    parse_canonical_transaction(transaction_id)? == transaction.hash
                }
            };
            if matched {
                ids.insert(watch.id.clone());
            }
        }
        Ok(ids.into_iter().collect())
    }
}

impl IndexBlockInterpreter for BlockInterpreter {
    type Block = Block;
    type Target = WatchSelector;
    type Effect = IndexChanges;
    type Undo = IndexUndo;

    fn inspect(
        &self,
        block: &Self::Block,
        watches: &[WatchTarget<Self::Target>],
    ) -> Result<InterpretedBlock<Self::Effect, Self::Undo>, IndexError> {
        let parsed = ParsedBlock::parse(&block.raw_block, Some(block.reference.height), true)
            .map_err(invalid_block)?;
        if parsed.reference != block.reference {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "retained Ethereum block reference does not match its raw payload",
                false,
            ));
        }
        let receipts =
            ParsedReceipt::parse_all(&block.raw_receipts, &parsed).map_err(invalid_block)?;
        let observed_at = block.reference.timestamp.ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "Ethereum block timestamp is unavailable",
                false,
            )
        })?;

        let mut drafts = Vec::new();
        for (transaction, receipt) in parsed.transactions.iter().zip(&receipts) {
            let fee_amount = receipt
                .effective_gas_price
                .checked_mul(U256::from(receipt.gas_used))
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "Ethereum receipt fee exceeds 256-bit atomic units",
                        false,
                    )
                })?;
            let fee = NetworkFee {
                asset: (*NATIVE_ASSET).clone(),
                amount: atomic_decimal(fee_amount),
                payer: Some(transaction.from.canonical(&self.scope)),
            };
            let movements = if receipt.succeeded {
                successful_movements(transaction, receipt, &self.scope)?
            } else {
                Vec::new()
            };
            let watch_ids = self.watch_ids(
                transaction,
                receipt,
                &movements,
                watches,
                block.reference.height,
            )?;
            if watch_ids.is_empty() {
                continue;
            }

            let transaction_id = transaction.hash.canonical(&self.scope);
            drafts.push(ObservationDraft {
                scope: self.scope.clone(),
                transaction_id,
                status: if receipt.succeeded {
                    ObservationDraftStatus::Included
                } else {
                    ObservationDraftStatus::Failed {
                        reason: Some("ethereum receipt status is zero".to_owned()),
                    }
                },
                movements,
                fee: Some(fee),
                watch_ids,
                first_seen_at: observed_at,
                observed_at,
            });
        }

        Ok(InterpretedBlock {
            block: block.reference.clone(),
            drafts,
            effect: IndexChanges::default(),
            undo: IndexUndo::default(),
            raw: RawBlock {
                block: block.raw_block.clone(),
                receipts: block.raw_receipts.clone(),
            },
        })
    }
}

fn successful_movements(
    transaction: &ParsedTransaction,
    receipt: &ParsedReceipt,
    scope: &IndexScope,
) -> Result<Vec<ValueMovement>, IndexError> {
    let transaction_id = encode_hex(&transaction.hash);
    let mut movements = Vec::new();
    if !transaction.value.is_zero() {
        let to = transaction.to.or(receipt.contract_address).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "successful value-bearing Ethereum transaction has no recipient",
                false,
            )
        })?;
        movements.push(ValueMovement::Transfer {
            id: MovementId(format!("{transaction_id}:value")),
            asset: (*NATIVE_ASSET).clone(),
            amount: atomic_decimal(transaction.value),
            from: transaction.from.canonical(scope),
            to: to.canonical(scope),
        });
    }

    for log in &receipt.logs {
        if let Some(movement) = transfer_movement(&transaction_id, log, scope) {
            movements.push(movement);
        }
    }
    Ok(movements)
}

fn transfer_movement(
    transaction_id: &str,
    log: &ParsedLog,
    scope: &IndexScope,
) -> Option<ValueMovement> {
    if log.topics.len() != 3 || log.topics[0] != TRANSFER_TOPIC || log.data.len() != 32 {
        return None;
    }
    let from = topic_address(log.topics[1])?;
    let to = topic_address(log.topics[2])?;
    let amount = U256::from_be_slice(&log.data);
    let id = MovementId(format!("{transaction_id}:{}", log.log_index));
    let asset = AssetId {
        chain: (*CHAIN_ID).clone(),
        asset: encode_hex(&log.address),
    };
    if from == ZERO_ADDRESS {
        (to != ZERO_ADDRESS).then(|| ValueMovement::Mint {
            id,
            asset,
            amount: atomic_decimal(amount),
            to: to.canonical(scope),
        })
    } else if to == ZERO_ADDRESS {
        Some(ValueMovement::Burn {
            id,
            asset,
            amount: atomic_decimal(amount),
            from: from.canonical(scope),
        })
    } else {
        Some(ValueMovement::Transfer {
            id,
            asset,
            amount: atomic_decimal(amount),
            from: from.canonical(scope),
            to: to.canonical(scope),
        })
    }
}

fn topic_address(topic: [u8; 32]) -> Option<[u8; 20]> {
    if topic[..12] != [0; 12] {
        return None;
    }
    topic[12..].try_into().ok()
}

fn parse_canonical_address(address: &CanonicalAddress) -> Result<[u8; 20], IndexError> {
    if address.scope.chain != *CHAIN_ID {
        return Err(IndexError::new(
            IndexErrorKind::InvalidBlock,
            "Ethereum movement contains a foreign-chain address",
            false,
        ));
    }
    address
        .value
        .parse::<alloy_primitives::Address>()
        .map(alloy_primitives::Address::into_array)
        .map_err(|_| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "Ethereum movement contains a malformed canonical address",
                false,
            )
        })
}

fn parse_canonical_transaction(transaction: &TransactionRef) -> Result<[u8; 32], IndexError> {
    if transaction.scope.chain != *CHAIN_ID {
        return Err(IndexError::new(
            IndexErrorKind::InvalidWatch,
            "Ethereum transaction watch belongs to another chain",
            false,
        ));
    }
    transaction
        .value
        .parse::<B256>()
        .map(|value| value.0)
        .map_err(|_| {
            IndexError::new(
                IndexErrorKind::InvalidWatch,
                "Ethereum transaction watch is malformed",
                false,
            )
        })
}

fn atomic_decimal(value: U256) -> Decimal {
    Decimal::from_atomic(BigUint::from_bytes_be(&value.to_be_bytes::<32>()), 0)
}

trait Canonicalize {
    type Output;

    fn canonical(self, scope: &IndexScope) -> Self::Output;
}

impl Canonicalize for [u8; 20] {
    type Output = CanonicalAddress;

    fn canonical(self, scope: &IndexScope) -> Self::Output {
        CanonicalAddress {
            scope: scope.clone(),
            value: encode_hex(&self),
        }
    }
}

impl Canonicalize for [u8; 32] {
    type Output = TransactionRef;

    fn canonical(self, scope: &IndexScope) -> Self::Output {
        TransactionRef {
            scope: scope.clone(),
            value: encode_hex(&self),
        }
    }
}

fn invalid_block(error: impl ToString) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidBlock, error.to_string(), false)
}

#[cfg(test)]
mod tests {
    use indexing::ChainId;
    use indexing::{
        BlockHash, BlockHeight, BlockRef, IndexedBlock, MovementKind, WatchId, WatchSelector,
    };
    use serde_json::{Value, json};

    use super::*;

    const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PARENT_HASH: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TX_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const FROM: &str = "0x1111111111111111111111111111111111111111";
    const TO: &str = "0x2222222222222222222222222222222222222222";
    const CONTRACT: &str = "0x3333333333333333333333333333333333333333";
    const TOKEN: &str = "0x4444444444444444444444444444444444444444";

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: "test".to_owned(),
        }
    }

    fn hash(value: &str) -> [u8; 32] {
        value
            .parse::<alloy_primitives::B256>()
            .expect("test hash must be valid")
            .into()
    }

    fn address(value: &str) -> [u8; 20] {
        value
            .parse::<alloy_primitives::Address>()
            .expect("test address must be valid")
            .into_array()
    }

    fn block_value(transaction: Value) -> Value {
        json!({
            "hash": BLOCK_HASH,
            "parentHash": PARENT_HASH,
            "sha3Uncles": "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "transactionsRoot": "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "receiptsRoot": "0xabababababababababababababababababababababababababababababababab",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": "0xa",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x5208",
            "timestamp": "0x64",
            "extraData": "0x",
            "mixHash": "0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
            "nonce": "0x0000000000000000",
            "uncles": [],
            "transactions": [transaction]
        })
    }

    fn transaction(to: Option<&str>, value: &str) -> Value {
        json!({
            "hash": TX_HASH,
            "from": FROM,
            "to": to,
            "value": value,
            "transactionIndex": "0x0",
            "blockHash": BLOCK_HASH,
            "blockNumber": "0xa"
        })
    }

    fn receipt(
        succeeded: bool,
        to: Option<&str>,
        contract: Option<&str>,
        gas_price: &str,
        logs: Vec<Value>,
    ) -> Value {
        json!({
            "transactionHash": TX_HASH,
            "transactionIndex": "0x0",
            "blockHash": BLOCK_HASH,
            "blockNumber": "0xa",
            "from": FROM,
            "to": to,
            "contractAddress": contract,
            "status": if succeeded { "0x1" } else { "0x0" },
            "gasUsed": "0x5208",
            "effectiveGasPrice": gas_price,
            "logs": logs
        })
    }

    fn log(index: u64, from: &str, to: &str, data: &str) -> Value {
        let topic_address = |address: &str| format!("0x{}{}", "00".repeat(12), &address[2..]);
        json!({
            "address": TOKEN,
            "topics": [
                encode_hex(&TRANSFER_TOPIC),
                topic_address(from),
                topic_address(to)
            ],
            "data": data,
            "blockHash": BLOCK_HASH,
            "blockNumber": "0xa",
            "transactionHash": TX_HASH,
            "transactionIndex": "0x0",
            "logIndex": format!("0x{index:x}"),
            "removed": false
        })
    }

    fn ethereum_block(transaction: Value, receipt: Value) -> Block {
        let raw_block =
            serde_json::to_vec(&block_value(transaction)).expect("test block JSON must serialize");
        let parsed = ParsedBlock::parse(&raw_block, Some(BlockHeight(10)), true)
            .expect("test block must parse");
        Block {
            reference: parsed.reference,
            raw_block,
            raw_receipts: vec![
                serde_json::to_vec(&receipt).expect("test receipt JSON must serialize"),
            ],
        }
    }

    fn transaction_watch() -> WatchTarget<WatchSelector> {
        let selector = WatchSelector::Transaction(hash(TX_HASH).canonical(&scope()));
        WatchTarget {
            id: WatchId("watch-tx".to_owned()),
            scope: scope(),
            selector: selector.clone(),
            target: selector,
            idempotency_key: "tx-key".to_owned(),
            start_height: BlockHeight(1),
            registered_at: None,
            inactive_from: None,
        }
    }

    fn address_watch(value: &str) -> WatchTarget<WatchSelector> {
        let value = address(value);
        let selector = WatchSelector::Address(value.canonical(&scope()));
        WatchTarget {
            id: WatchId("watch-address".to_owned()),
            scope: scope(),
            selector: selector.clone(),
            target: selector,
            idempotency_key: "address-key".to_owned(),
            start_height: BlockHeight(1),
            registered_at: None,
            inactive_from: None,
        }
    }

    fn inspect(
        block: &Block,
        watches: &[WatchTarget<WatchSelector>],
    ) -> Result<InterpretedBlock<IndexChanges, IndexUndo>, IndexError> {
        BlockInterpreter::new(scope())?.inspect(block, watches)
    }

    #[test]
    fn interprets_successful_native_transfer_and_actual_fee() {
        let block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x3", Vec::new()),
        );
        let interpreted = inspect(&block, &[address_watch(TO)]).expect("block must interpret");
        let draft = interpreted.drafts.first().expect("watched tx must emit");
        assert_eq!(draft.movements.len(), 1);
        assert_eq!(
            draft.movements[0].id(),
            &MovementId(format!("{TX_HASH}:value"))
        );
        assert_eq!(
            draft.movements[0].amount(),
            &atomic_decimal(U256::from(42_u8))
        );
        assert_eq!(
            draft.fee.as_ref().expect("fee must exist").amount,
            atomic_decimal(U256::from(21_000_u64 * 3))
        );
        assert_eq!(draft.movements[0].amount().scale(), 0);
        assert_eq!(draft.status, ObservationDraftStatus::Included);
        assert_eq!(interpreted.raw.block, block.raw_block);
    }

    #[test]
    fn sends_contract_creation_value_to_receipt_contract() {
        let block = ethereum_block(
            transaction(None, "0x7"),
            receipt(true, None, Some(CONTRACT), "0x1", Vec::new()),
        );
        let interpreted =
            inspect(&block, &[address_watch(CONTRACT)]).expect("contract creation must interpret");
        assert_eq!(
            interpreted.drafts[0].movements[0].to(),
            Some(&address(CONTRACT).canonical(&scope()))
        );
    }

    #[test]
    fn failed_receipt_is_fee_only() {
        let block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(false, Some(TO), None, "0x2", Vec::new()),
        );
        let interpreted = inspect(&block, &[transaction_watch()]).expect("failure must interpret");
        let draft = &interpreted.drafts[0];
        assert!(draft.movements.is_empty());
        assert!(matches!(
            draft.status,
            ObservationDraftStatus::Failed { .. }
        ));
        assert!(draft.fee.is_some());
    }

    #[test]
    fn rejects_fee_multiplication_overflow() {
        let block = ethereum_block(
            transaction(Some(TO), "0x0"),
            receipt(
                true,
                Some(TO),
                None,
                "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                Vec::new(),
            ),
        );
        let error = inspect(&block, &[transaction_watch()])
            .expect_err("overflowing actual fee must fail the block");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(error.message.contains("fee exceeds"));
    }

    #[test]
    fn interprets_transfer_mint_and_burn_logs() {
        let zero = "0x0000000000000000000000000000000000000000";
        let logs = vec![
            log(0, FROM, TO, &format!("0x{:064x}", 1)),
            log(1, zero, TO, &format!("0x{:064x}", 2)),
            log(2, FROM, zero, &format!("0x{:064x}", 3)),
        ];
        let block = ethereum_block(
            transaction(Some(TO), "0x0"),
            receipt(true, Some(TO), None, "0x1", logs),
        );
        let interpreted = inspect(&block, &[transaction_watch()]).expect("logs must interpret");
        let movements = &interpreted.drafts[0].movements;
        assert_eq!(movements.len(), 3);
        assert_eq!(movements[0].kind(), MovementKind::Transfer);
        assert_eq!(movements[1].kind(), MovementKind::Mint);
        assert_eq!(movements[1].from(), None);
        assert_eq!(movements[2].kind(), MovementKind::Burn);
        assert_eq!(movements[2].to(), None);
        assert_eq!(movements[2].id(), &MovementId(format!("{TX_HASH}:2")));
    }

    #[test]
    fn ignores_structurally_malformed_transfer_log() {
        let mut malformed = log(0, FROM, TO, &format!("0x{:064x}", 1));
        malformed["topics"] = json!([encode_hex(&TRANSFER_TOPIC)]);
        let block = ethereum_block(
            transaction(Some(TO), "0x0"),
            receipt(true, Some(TO), None, "0x1", vec![malformed]),
        );
        let interpreted = inspect(&block, &[transaction_watch()])
            .expect("malformed token log must not poison the block");
        assert!(interpreted.drafts[0].movements.is_empty());
    }

    #[test]
    fn rejects_receipt_transaction_mismatch() {
        let mut wrong_receipt = receipt(true, Some(TO), None, "0x1", Vec::new());
        wrong_receipt["transactionHash"] = Value::String(
            "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_owned(),
        );
        let block = ethereum_block(transaction(Some(TO), "0x0"), wrong_receipt);
        let error = inspect(&block, &[transaction_watch()])
            .expect_err("receipt mismatch must fail the block");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(error.message.contains("transaction hash"));
    }

    #[test]
    fn raw_block_reference_is_stable() {
        let block = ethereum_block(
            transaction(Some(TO), "0x0"),
            receipt(true, Some(TO), None, "0x1", Vec::new()),
        );
        assert_eq!(
            block.block_ref(),
            BlockRef {
                height: BlockHeight(10),
                hash: BlockHash(hash(BLOCK_HASH).to_vec()),
                parent_hash: Some(BlockHash(hash(PARENT_HASH).to_vec())),
                timestamp: Some(100),
            }
        );
    }
}
