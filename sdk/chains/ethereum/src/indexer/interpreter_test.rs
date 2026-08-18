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
    let parsed =
        ParsedBlock::parse(&raw_block, Some(BlockHeight(10)), true).expect("test block must parse");
    Block {
        reference: parsed.reference,
        raw_block,
        raw_receipts: vec![serde_json::to_vec(&receipt).expect("test receipt JSON must serialize")],
    }
}

fn address_watch(value: &str) -> WatchTarget<WatchSelector> {
    let value = address(value);
    let selector = value.canonical(&scope());
    WatchTarget {
        id: WatchId("watch-address".to_owned()),
        scope: scope(),
        selector: selector.clone(),
        target: selector,
        idempotency_key: "address-key".to_owned(),
        start_height: BlockHeight(1),
        registered_at: None,
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
    let interpreted = inspect(&block, &[address_watch(FROM)]).expect("failure must interpret");
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
    let error = inspect(&block, &[address_watch(FROM)])
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
    let interpreted = inspect(&block, &[address_watch(FROM)]).expect("logs must interpret");
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
    let interpreted = inspect(&block, &[address_watch(FROM)])
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
    let error =
        inspect(&block, &[address_watch(FROM)]).expect_err("receipt mismatch must fail the block");
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
