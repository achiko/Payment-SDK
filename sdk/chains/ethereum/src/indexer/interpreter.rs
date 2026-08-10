use std::collections::BTreeSet;

use alloy_primitives::U256;
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use indexing::{
    BlockInterpreter, IndexError, IndexErrorKind, IndexScope, InterpretedBlock, MovementId,
    MovementKind, NetworkFee, ObservationDraft, ObservationDraftStatus, RawBlockData,
    ValueMovement, WatchId, WatchSelector, WatchTarget,
};

use crate::EthereumTransactionId;

use super::{
    EthereumBlock, EthereumUndo, EthereumWatchTarget,
    model::{
        ParsedLog, ParsedReceipt, ParsedTransaction, encode_hex, parse_and_validate_receipts,
        parse_block,
    },
};

const TRANSFER_TOPIC: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];
const ZERO_ADDRESS: [u8; 20] = [0; 20];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumBlockInterpreter {
    scope: IndexScope,
}

impl EthereumBlockInterpreter {
    pub fn new(scope: IndexScope) -> Result<Self, IndexError> {
        if scope.chain != ChainId("ethereum".to_owned()) {
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

    fn validate_watch(&self, watch: &WatchTarget<EthereumWatchTarget>) -> Result<(), IndexError> {
        if watch.scope != self.scope {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum watch belongs to a different scope",
                false,
            ));
        }
        let consistent = match (&watch.target, &watch.selector) {
            (EthereumWatchTarget::Address(address), WatchSelector::Address(selector)) => {
                selector.chain == self.scope.chain && selector.value == encode_hex(&address.0)
            }
            (
                EthereumWatchTarget::Transaction(transaction),
                WatchSelector::Transaction(selector),
            ) => selector.chain == self.scope.chain && selector.value == encode_hex(&transaction.0),
            _ => false,
        };
        if !consistent {
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
        watches: &[WatchTarget<EthereumWatchTarget>],
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
            for endpoint in [movement.from.as_ref(), movement.to.as_ref()]
                .into_iter()
                .flatten()
            {
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
            let matched = match &watch.target {
                EthereumWatchTarget::Address(address) => touched_addresses.contains(&address.0),
                EthereumWatchTarget::Transaction(transaction_id) => {
                    transaction_id.0 == transaction.hash
                }
            };
            if matched {
                ids.insert(watch.id.clone());
            }
        }
        Ok(ids.into_iter().collect())
    }
}

impl BlockInterpreter for EthereumBlockInterpreter {
    type Block = EthereumBlock;
    type Target = EthereumWatchTarget;
    type Undo = EthereumUndo;

    fn inspect(
        &self,
        block: &Self::Block,
        watches: &[WatchTarget<Self::Target>],
    ) -> Result<InterpretedBlock<Self::Undo>, IndexError> {
        let parsed = parse_block(&block.raw_block, Some(block.reference.height), true)
            .map_err(invalid_block)?;
        if parsed.reference != block.reference {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "retained Ethereum block reference does not match its raw payload",
                false,
            ));
        }
        let receipts =
            parse_and_validate_receipts(&block.raw_receipts, &parsed).map_err(invalid_block)?;
        let observed_at = block.reference.timestamp.ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "Ethereum block timestamp is unavailable",
                false,
            )
        })?;

        let mut drafts = Vec::new();
        let mut affected_transactions = Vec::new();
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
                asset: native_asset(),
                amount: atomic(fee_amount),
                payer: Some(canonical_address(transaction.from)),
            };
            let movements = if receipt.succeeded {
                successful_movements(transaction, receipt)?
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

            let transaction_id = canonical_transaction(transaction.hash);
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
            affected_transactions.push(EthereumTransactionId(transaction.hash));
        }

        Ok(InterpretedBlock {
            block: block.reference.clone(),
            drafts,
            projection: indexing::ProjectionBatch::default(),
            undo: EthereumUndo {
                affected_transactions,
            },
            raw: RawBlockData {
                block: block.raw_block.clone(),
                receipts: block.raw_receipts.clone(),
            },
        })
    }
}

fn successful_movements(
    transaction: &ParsedTransaction,
    receipt: &ParsedReceipt,
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
        movements.push(ValueMovement {
            id: MovementId(format!("{transaction_id}:value")),
            asset: native_asset(),
            amount: atomic(transaction.value),
            from: Some(canonical_address(transaction.from)),
            to: Some(canonical_address(to)),
            kind: MovementKind::Transfer,
        });
    }

    for log in &receipt.logs {
        if let Some(movement) = transfer_movement(&transaction_id, log) {
            movements.push(movement);
        }
    }
    Ok(movements)
}

fn transfer_movement(transaction_id: &str, log: &ParsedLog) -> Option<ValueMovement> {
    if log.topics.len() != 3 || log.topics[0] != TRANSFER_TOPIC || log.data.len() != 32 {
        return None;
    }
    let from = topic_address(log.topics[1])?;
    let to = topic_address(log.topics[2])?;
    let amount = U256::from_be_slice(&log.data);
    let (from_endpoint, to_endpoint, kind) = if from == ZERO_ADDRESS {
        (
            None,
            (to != ZERO_ADDRESS).then(|| canonical_address(to)),
            MovementKind::Mint,
        )
    } else if to == ZERO_ADDRESS {
        (Some(canonical_address(from)), None, MovementKind::Burn)
    } else {
        (
            Some(canonical_address(from)),
            Some(canonical_address(to)),
            MovementKind::Transfer,
        )
    };

    Some(ValueMovement {
        id: MovementId(format!("{transaction_id}:{}", log.log_index)),
        asset: AssetId {
            chain: ethereum_chain(),
            asset: encode_hex(&log.address),
        },
        amount: atomic(amount),
        from: from_endpoint,
        to: to_endpoint,
        kind,
    })
}

fn topic_address(topic: [u8; 32]) -> Option<[u8; 20]> {
    if topic[..12] != [0; 12] {
        return None;
    }
    topic[12..].try_into().ok()
}

fn parse_canonical_address(address: &CanonicalAddress) -> Result<[u8; 20], IndexError> {
    if address.chain != ethereum_chain() {
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

fn atomic(value: U256) -> AtomicAmount {
    AtomicAmount(value.to_be_bytes::<32>())
}

fn native_asset() -> AssetId {
    AssetId {
        chain: ethereum_chain(),
        asset: "native".to_owned(),
    }
}

fn canonical_address(value: [u8; 20]) -> CanonicalAddress {
    CanonicalAddress {
        chain: ethereum_chain(),
        value: encode_hex(&value),
    }
}

fn canonical_transaction(value: [u8; 32]) -> CanonicalTransactionId {
    CanonicalTransactionId {
        chain: ethereum_chain(),
        value: encode_hex(&value),
    }
}

fn ethereum_chain() -> ChainId {
    ChainId("ethereum".to_owned())
}

fn invalid_block(error: impl ToString) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidBlock, error.to_string(), false)
}

#[cfg(test)]
mod tests {
    use chain_identity::ChainId;
    use indexing::{
        BlockHash, BlockHeight, BlockRef, IndexedBlock, MovementKind, WatchId, WatchSelector,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::EthereumAddress;

    const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PARENT_HASH: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TX_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const FROM: &str = "0x1111111111111111111111111111111111111111";
    const TO: &str = "0x2222222222222222222222222222222222222222";
    const CONTRACT: &str = "0x3333333333333333333333333333333333333333";
    const TOKEN: &str = "0x4444444444444444444444444444444444444444";

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("ethereum".to_owned()),
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

    fn ethereum_block(transaction: Value, receipt: Value) -> EthereumBlock {
        let raw_block =
            serde_json::to_vec(&block_value(transaction)).expect("test block JSON must serialize");
        let parsed =
            parse_block(&raw_block, Some(BlockHeight(10)), true).expect("test block must parse");
        EthereumBlock {
            reference: parsed.reference,
            raw_block,
            raw_receipts: vec![
                serde_json::to_vec(&receipt).expect("test receipt JSON must serialize"),
            ],
        }
    }

    fn transaction_watch() -> WatchTarget<EthereumWatchTarget> {
        WatchTarget {
            id: WatchId("watch-tx".to_owned()),
            scope: scope(),
            selector: WatchSelector::Transaction(canonical_transaction(hash(TX_HASH))),
            target: EthereumWatchTarget::Transaction(EthereumTransactionId(hash(TX_HASH))),
            idempotency_key: "tx-key".to_owned(),
            start_height: BlockHeight(1),
            registered_at: None,
            inactive_from: None,
        }
    }

    fn address_watch(value: &str) -> WatchTarget<EthereumWatchTarget> {
        let value = address(value);
        WatchTarget {
            id: WatchId("watch-address".to_owned()),
            scope: scope(),
            selector: WatchSelector::Address(canonical_address(value)),
            target: EthereumWatchTarget::Address(EthereumAddress(value)),
            idempotency_key: "address-key".to_owned(),
            start_height: BlockHeight(1),
            registered_at: None,
            inactive_from: None,
        }
    }

    fn inspect(
        block: &EthereumBlock,
        watches: &[WatchTarget<EthereumWatchTarget>],
    ) -> Result<InterpretedBlock<EthereumUndo>, IndexError> {
        EthereumBlockInterpreter::new(scope())?.inspect(block, watches)
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
            draft.movements[0].id,
            MovementId(format!("{TX_HASH}:value"))
        );
        assert_eq!(draft.movements[0].amount, atomic(U256::from(42_u8)));
        assert_eq!(
            draft.fee.as_ref().expect("fee must exist").amount,
            atomic(U256::from(21_000_u64 * 3))
        );
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
            interpreted.drafts[0].movements[0].to,
            Some(canonical_address(address(CONTRACT)))
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
        assert_eq!(movements[0].kind, MovementKind::Transfer);
        assert_eq!(movements[1].kind, MovementKind::Mint);
        assert_eq!(movements[1].from, None);
        assert_eq!(movements[2].kind, MovementKind::Burn);
        assert_eq!(movements[2].to, None);
        assert_eq!(movements[2].id, MovementId(format!("{TX_HASH}:2")));
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
