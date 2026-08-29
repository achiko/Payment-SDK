use indexing::{
    BlockInterpreter as _, CanonicalAddress, IndexErrorKind, MovementId, ObservationDraftStatus,
};
use serde_json::{Value, json};
use solana_address::Address as NativeAddress;
use solana_signature::Signature;
use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_ID};

use super::*;

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId(crate::CHAIN.to_owned()),
        network: "localnet".to_owned(),
    }
}

fn address(byte: u8) -> String {
    NativeAddress::from([byte; 32]).to_string()
}

fn signature(byte: u8) -> String {
    Signature::from([byte; 64]).to_string()
}

fn system_data(instruction: SystemInstruction) -> String {
    bs58::encode(bincode::serialize(&instruction).expect("system instruction encodes"))
        .into_string()
}

fn compiled(program: u8, accounts: &[u8], instruction: SystemInstruction) -> Value {
    json!({
        "programIdIndex": program,
        "accounts": accounts,
        "data": system_data(instruction),
    })
}

fn transfer(program: u8, source: u8, destination: u8, lamports: u64) -> Value {
    compiled(
        program,
        &[source, destination],
        SystemInstruction::Transfer { lamports },
    )
}

fn transaction(
    signature_byte: u8,
    keys: Vec<String>,
    instructions: Vec<Value>,
    pre: Vec<u64>,
    post: Vec<u64>,
    fee: u64,
) -> Value {
    json!({
        "transaction": {
            "signatures": [signature(signature_byte)],
            "message": {
                "header": {
                    "numRequiredSignatures": 1,
                    "numReadonlySignedAccounts": 0,
                    "numReadonlyUnsignedAccounts": 1,
                },
                "accountKeys": keys,
                "recentBlockhash": "11111111111111111111111111111111",
                "instructions": instructions,
            },
        },
        "meta": {
            "err": null,
            "fee": fee,
            "preBalances": pre,
            "postBalances": post,
            "innerInstructions": [],
        },
        "version": "legacy",
    })
}

fn baseline(signature_byte: u8) -> Value {
    transaction(
        signature_byte,
        vec![address(1), address(2), SYSTEM_ID.to_string()],
        vec![transfer(2, 0, 1, 10)],
        vec![100, 0, 0],
        vec![83, 10, 0],
        7,
    )
}

fn block(transactions: Vec<Value>) -> Block {
    let raw = serde_json::to_vec(&json!({
        "blockhash": "11111111111111111111111111111111",
        "previousBlockhash": "11111111111111111111111111111111",
        "parentSlot": 6,
        "transactions": transactions,
        "blockTime": 123,
        "blockHeight": 4,
    }))
    .expect("fixture block serializes");
    Block::parse(7, raw).expect("fixture block parses")
}

fn selected(byte: u8) -> CanonicalAddress {
    CanonicalAddress {
        scope: scope(),
        value: address(byte),
    }
}

fn inspect(
    transactions: Vec<Value>,
    selected: &[CanonicalAddress],
) -> Result<InterpretedBlock, IndexError> {
    Interpreter::new(scope())?.inspect(&block(transactions), selected)
}

#[test]
fn validates_scope_and_filter_boundaries() {
    let interpreter = Interpreter::new(scope()).expect("Solana scope");
    assert_eq!(interpreter.scope(), &scope());
    assert_eq!(
        Interpreter::new(IndexScope {
            chain: ChainId("ethereum".to_owned()),
            network: "localnet".to_owned(),
        })
        .expect_err("wrong chain")
        .kind,
        IndexErrorKind::ScopeMismatch
    );
    assert!(
        Interpreter::new(IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: " ".to_owned(),
        })
        .is_err()
    );

    let mut foreign = selected(2);
    foreign.scope.network = "other".to_owned();
    assert_eq!(
        interpreter
            .inspect(&block(vec![baseline(9)]), &[foreign])
            .expect_err("foreign filter")
            .kind,
        IndexErrorKind::ScopeMismatch
    );
}

#[test]
fn interprets_legacy_transfer_identity_fee_and_empty_outputs() {
    let interpreted = inspect(vec![baseline(9)], &[selected(2)]).expect("legacy transfer");
    assert!(interpreted.outputs.created.is_empty());
    assert!(interpreted.outputs.spent.is_empty());
    assert!(interpreted.outputs.tracked_spends.is_empty());
    let draft = &interpreted.transactions[0];
    assert_eq!(draft.transaction_id.value, signature(9));
    assert_eq!(draft.status, ObservationDraftStatus::Included);
    assert_eq!(draft.movements.len(), 1);
    assert_eq!(
        draft.movements[0].id(),
        &MovementId(format!("{}:ix:0", signature(9)))
    );
    assert_eq!(draft.movements[0].from(), Some(&selected(1)));
    assert_eq!(draft.movements[0].to(), Some(&selected(2)));
    assert_eq!(draft.movements[0].amount().to_string(), "10");
    assert_eq!(draft.movements[0].amount().scale(), 0);
    let fee = draft.fee.as_ref().expect("exact fee");
    assert_eq!(fee.amount.to_string(), "7");
    assert_eq!(fee.payer.as_ref(), Some(&selected(1)));
}

#[test]
fn resolves_version_zero_writable_then_readonly_loaded_keys() {
    let mut value = transaction(
        10,
        vec![address(1)],
        vec![transfer(2, 0, 1, 10)],
        vec![100, 0, 0],
        vec![88, 10, 0],
        2,
    );
    value["version"] = json!(0);
    value["transaction"]["message"]["header"]["numReadonlyUnsignedAccounts"] = json!(0);
    value["meta"]["loadedAddresses"] = json!({
        "writable": [address(2)],
        "readonly": [SYSTEM_ID.to_string()],
    });

    let interpreted = inspect(vec![value], &[selected(2)]).expect("version-zero transfer");
    assert_eq!(
        interpreted.transactions[0].movements[0].to(),
        Some(&selected(2))
    );
}

#[test]
fn keeps_distinct_fee_payer_source_and_destination() {
    let mut value = transaction(
        17,
        vec![address(1), address(3), address(2), SYSTEM_ID.to_string()],
        vec![transfer(3, 1, 2, 10)],
        vec![100, 20, 0, 0],
        vec![97, 10, 10, 0],
        3,
    );
    value["transaction"]["signatures"] = json!([signature(17), signature(18)]);
    value["transaction"]["message"]["header"]["numRequiredSignatures"] = json!(2);

    let interpreted = inspect(vec![value], &[selected(2)]).expect("distinct payer and source");
    let draft = &interpreted.transactions[0];
    assert_eq!(draft.transaction_id.value, signature(17));
    assert_eq!(
        draft.fee.as_ref().and_then(|fee| fee.payer.as_ref()),
        Some(&selected(1))
    );
    assert_eq!(draft.movements[0].from(), Some(&selected(3)));
    assert_eq!(draft.movements[0].to(), Some(&selected(2)));
}

#[test]
fn preserves_outer_inner_seed_repeated_self_and_zero_occurrences() {
    let owner = NativeAddress::from([44; 32]);
    let mut value = transaction(
        11,
        vec![
            address(1),
            address(2),
            address(3),
            address(4),
            address(5),
            address(6),
            SYSTEM_ID.to_string(),
        ],
        vec![transfer(6, 0, 1, 5)],
        vec![100, 0, 10, 0, 0, 0, 0],
        vec![93, 5, 3, 3, 0, 4, 0],
        2,
    );
    value["meta"]["innerInstructions"] = json!([{
        "index": 0,
        "instructions": [
            transfer(6, 2, 3, 3),
            compiled(6, &[2, 4, 5], SystemInstruction::TransferWithSeed {
                lamports: 4,
                from_seed: "seed".to_owned(),
                from_owner: owner,
            }),
            transfer(6, 1, 1, 2),
            transfer(6, 0, 1, 0),
        ],
    }]);

    let interpreted = inspect(vec![value], &[selected(2)]).expect("complete inner transfers");
    let movements = &interpreted.transactions[0].movements;
    assert_eq!(movements.len(), 4);
    assert_eq!(movements[1].from(), Some(&selected(3)));
    assert_eq!(movements[1].to(), Some(&selected(4)));
    assert_eq!(movements[2].to(), Some(&selected(6)));
    assert_ne!(movements[2].to(), Some(&selected(5)));
    assert_eq!(movements[3].from(), movements[3].to());
    assert_eq!(
        movements
            .iter()
            .map(|movement| movement.id().0.clone())
            .collect::<Vec<_>>(),
        vec![
            format!("{}:ix:0", signature(11)),
            format!("{}:ix:0:inner:0", signature(11)),
            format!("{}:ix:0:inner:1", signature(11)),
            format!("{}:ix:0:inner:2", signature(11)),
        ]
    );
}

#[test]
fn failed_transaction_is_fee_only_and_visible_only_to_payer() {
    let mut value = baseline(12);
    value["meta"]["err"] = json!({"InstructionError": [0, "Custom"]});
    value["meta"]["fee"] = json!(5);
    value["meta"]["postBalances"] = json!([95, 0, 0]);
    value["meta"]["innerInstructions"] = Value::Null;

    let payer = inspect(vec![value.clone()], &[selected(1)]).expect("failed payer history");
    let draft = &payer.transactions[0];
    assert!(draft.movements.is_empty());
    assert!(matches!(
        draft.status,
        ObservationDraftStatus::Failed { .. }
    ));
    assert_eq!(
        draft.fee.as_ref().expect("failed fee").amount.to_string(),
        "5"
    );

    let endpoint = inspect(vec![value], &[selected(2)]).expect("attempted endpoint ignored");
    assert!(endpoint.transactions.is_empty());
}

#[test]
fn retains_successful_fee_only_transaction_for_selected_payer() {
    let value = transaction(
        13,
        vec![address(1), SYSTEM_ID.to_string()],
        Vec::new(),
        vec![10, 0],
        vec![7, 0],
        3,
    );
    let interpreted = inspect(vec![value], &[selected(1)]).expect("fee-only success");
    assert_eq!(interpreted.transactions.len(), 1);
    assert!(interpreted.transactions[0].movements.is_empty());
}

#[test]
fn ignores_readonly_mentions_and_transfer_lookalikes_from_other_programs() {
    let fake_program = address(90);
    let mut value = transaction(
        14,
        vec![address(1), address(2), address(8), fake_program],
        vec![transfer(3, 0, 1, 10)],
        vec![100, 0, 7, 0],
        vec![99, 10, 7, 0],
        1,
    );
    value["transaction"]["message"]["header"]["numReadonlyUnsignedAccounts"] = json!(2);
    value["meta"]["innerInstructions"] = Value::Null;

    let interpreted = inspect(vec![value], &[selected(8)]).expect("readonly mention ignored");
    assert!(interpreted.transactions.is_empty());
}

#[test]
fn rejects_incomplete_or_unexplained_selected_value_effects() {
    let mut unexplained = baseline(15);
    unexplained["meta"]["postBalances"] = json!([83, 11, 0]);
    let error = inspect(vec![unexplained], &[selected(2)]).expect_err("unexplained delta");
    assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    assert!(
        error
            .message
            .contains("unsupported native SOL value effect")
    );

    let mut incomplete = baseline(16);
    incomplete["meta"]["innerInstructions"] = Value::Null;
    let error = inspect(vec![incomplete], &[selected(2)]).expect_err("missing inner metadata");
    assert!(error.message.contains("incomplete inner instructions"));
}

#[test]
fn malformed_transaction_poisoning_is_all_or_nothing() {
    let mut unsupported = baseline(21);
    unsupported["version"] = json!(1);
    let mut no_meta = baseline(22);
    no_meta["meta"] = Value::Null;
    let mut bad_signature_count = baseline(23);
    bad_signature_count["transaction"]["signatures"] = json!([]);
    let mut malformed_signature = baseline(28);
    malformed_signature["transaction"]["signatures"] = json!(["not-a-signature"]);
    let mut malformed_address = baseline(29);
    malformed_address["transaction"]["message"]["accountKeys"][1] = json!("not-an-address");
    let mut bad_balances = baseline(24);
    bad_balances["meta"]["postBalances"] = json!([83, 10]);
    let mut duplicate_inner = baseline(25);
    duplicate_inner["meta"]["innerInstructions"] = json!([
        {"index": 0, "instructions": []},
        {"index": 0, "instructions": []},
    ]);
    let mut invalid_index = baseline(26);
    invalid_index["transaction"]["message"]["instructions"][0]["accounts"] = json!([0, 99]);
    let mut missing_loaded = baseline(27);
    missing_loaded["version"] = json!(0);

    for malformed in [
        unsupported,
        no_meta,
        bad_signature_count,
        malformed_signature,
        malformed_address,
        bad_balances,
        duplicate_inner,
        invalid_index,
        missing_loaded,
    ] {
        let error = inspect(vec![baseline(20), malformed], &[selected(2)])
            .expect_err("one malformed transaction must poison the block");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    }
}

#[test]
fn rejects_duplicate_first_signature_identity() {
    let error = inspect(vec![baseline(30), baseline(30)], &[selected(2)])
        .expect_err("duplicate first signature");
    assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    assert!(error.message.contains("duplicate first signatures"));
}
