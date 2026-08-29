use super::*;
use crate::{Satoshi, indexer::model::PreviousOutput};

#[test]
fn numbered_block_fetch_parses_transactions_and_rechecks_canonical_hash() {
    let mut replies = connect_replies();
    replies.extend([
        reply("getblockhash", Value::String(hash(2))),
        reply_for("getblock", json!([hash(2), 2]), block_result()),
        reply("getblockhash", Value::String(hash(2))),
    ]);
    let source = block_on(Blocks::connect(ScriptedClient::new(replies), config()))
        .expect("valid scripted source must connect");

    let block = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
        .expect("canonical verbosity-2 block must load")
        .pop()
        .expect("dense range contains its block");

    assert_eq!(block.reference.height, BlockHeight(10));
    assert_eq!(block.reference.position, BlockPosition(10));
    assert_eq!(
        parse_bitcoin_block_hash(&hash(2)).expect("test hash must parse"),
        block.reference.hash
    );
    assert_eq!(
        block.reference.parent,
        Some(indexing::BlockParent {
            position: BlockPosition(9),
            hash: parse_bitcoin_block_hash(&hash(3)).expect("test parent hash must parse"),
        })
    );
    let zero_limit = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 0))
        .expect_err("zero returned-block limit must fail before RPC");
    assert!(!zero_limit.retryable);
    let transactions = block.transactions();
    assert_eq!(transactions.len(), 1);
    assert!(transactions[0].coinbase);
    assert_eq!(transactions[0].inputs.len(), 1);
    assert!(transactions[0].inputs[0].previous_output.is_none());
    assert_eq!(transactions[0].outputs.len(), 1);
    assert_eq!(transactions[0].outputs[0].value, Satoshi(5_000_000_000));
    assert!(transactions[0].outputs[0].script_pubkey.is_empty());
}

#[test]
fn external_prevouts_are_resolved_once_into_bounded_parsed_facts() {
    let (block_result, previous, spending, child) = external_prevout_block();
    let previous_id = previous.compute_txid().to_string();
    let mut replies = connect_replies();
    replies.extend([
        reply("getblockhash", Value::String(hash(2))),
        reply_for("getblock", json!([hash(2), 2]), block_result),
        reply_for(
            "getrawtransaction",
            json!([previous_id, true]),
            json!({
                "txid": previous.compute_txid().to_string(),
                "hex": consensus::serialize(&previous).to_lower_hex_string(),
                "blockhash": hash(5),
            }),
        ),
        reply("getblockhash", Value::String(hash(2))),
    ]);
    let client = ScriptedClient::new(replies);
    let calls = client.clone();
    let source =
        block_on(Blocks::connect(client, config())).expect("valid scripted source must connect");

    let block = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
        .expect("external previous outputs must be enriched")
        .pop()
        .expect("dense range contains its block");
    calls.assert_exhausted();

    let transactions = block.transactions();
    assert_eq!(transactions.len(), 2);
    assert_eq!(
        transactions
            .iter()
            .map(|transaction| transaction.id)
            .collect::<Vec<_>>(),
        vec![
            TransactionId::from(spending.compute_txid()),
            TransactionId::from(child.compute_txid()),
        ]
    );
    let historical_id = TransactionId::from(previous.compute_txid());
    assert_eq!(
        transactions[0]
            .inputs
            .iter()
            .map(|input| input.previous_output.clone())
            .collect::<Vec<_>>(),
        previous
            .output
            .iter()
            .enumerate()
            .map(|(index, output)| {
                Some(PreviousOutput {
                    outpoint: Outpoint {
                        transaction_id: historical_id,
                        output_index: u32::try_from(index).expect("test index must fit u32"),
                    },
                    value: Satoshi(output.value.to_sat()),
                    address: address_for_script(&output.script_pubkey, Network::Regtest),
                })
            })
            .collect::<Vec<_>>()
    );
    assert!(previous.output[2].script_pubkey.len() > MAX_COMPACT_PREVOUT_JSON_BYTES);

    for output in &previous.output {
        let compact = ResolvedOutput {
            value_satoshis: output.value.to_sat(),
            address: address_for_script(&output.script_pubkey, Network::Regtest),
        }
        .compact_json()
        .expect("resolved output must fit its compact boundary");
        assert_eq!(
            compact
                .as_object()
                .expect("compact previous output must be an object")
                .len(),
            2
        );
        assert!(
            serde_json::to_vec(&compact)
                .expect("compact data must encode")
                .len()
                <= MAX_COMPACT_PREVOUT_JSON_BYTES
        );
    }

    assert_eq!(
        transactions[1].inputs[0].previous_output,
        Some(PreviousOutput {
            outpoint: Outpoint {
                transaction_id: TransactionId::from(spending.compute_txid()),
                output_index: 0,
            },
            value: Satoshi(123_521_789),
            address: None,
        })
    );
}

#[test]
fn external_prevout_lookup_must_return_confirmed_transaction_data() {
    let (block_result, previous, _, _) = external_prevout_block();
    let mut replies = connect_replies();
    replies.extend([
        reply("getblockhash", Value::String(hash(2))),
        reply_for("getblock", json!([hash(2), 2]), block_result),
        reply_for(
            "getrawtransaction",
            json!([previous.compute_txid().to_string(), true]),
            json!({
                "txid": previous.compute_txid().to_string(),
                "hex": consensus::serialize(&previous).to_lower_hex_string(),
            }),
        ),
    ]);
    let client = ScriptedClient::new(replies);
    let calls = client.clone();
    let source =
        block_on(Blocks::connect(client, config())).expect("valid scripted source must connect");

    let error = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
        .expect_err("mempool-only previous-output data must retry");
    calls.assert_exhausted();
    assert!(error.retryable);
    assert!(error.message.contains("block hash"));
}

#[test]
fn external_prevout_bound_is_above_consensus_maximum_and_fails_before_growth() {
    const CONSENSUS_MAX_BLOCK_WEIGHT: usize = 4_000_000;
    const MINIMUM_INPUT_WEIGHT: usize = 41 * 4;

    let consensus_upper_bound = CONSENSUS_MAX_BLOCK_WEIGHT / MINIMUM_INPUT_WEIGHT;
    assert_eq!(consensus_upper_bound, 24_390);
    assert!(MAX_EXTERNAL_PREVOUTS_PER_BLOCK > consensus_upper_bound);
    assert_eq!(MAX_COMPACT_PREVOUT_TOTAL_BYTES, 4_800_000);

    let transaction_id = TransactionId([0x33; 32]);
    let mut outputs = BTreeMap::new();
    let mut count = 0;
    for output_index in 0..MAX_EXTERNAL_PREVOUTS_PER_BLOCK {
        record_external_prevout(
            &mut outputs,
            &mut count,
            transaction_id,
            u32::try_from(output_index).expect("test output index must fit u32"),
        )
        .expect("consensus-complete safety window must remain accepted");
    }
    let error = record_external_prevout(
        &mut outputs,
        &mut count,
        transaction_id,
        u32::try_from(MAX_EXTERNAL_PREVOUTS_PER_BLOCK).expect("test output index must fit u32"),
    )
    .expect_err("the first out-of-bound prevout must fail before insertion");
    assert!(!error.retryable);
    assert_eq!(count, MAX_EXTERNAL_PREVOUTS_PER_BLOCK);
    assert_eq!(
        outputs
            .get(&transaction_id)
            .expect("bounded transaction entry must exist")
            .len(),
        MAX_EXTERNAL_PREVOUTS_PER_BLOCK
    );
}

#[test]
fn numbered_block_fetch_rejects_same_height_reorg_race() {
    let mut replies = connect_replies();
    replies.extend([
        reply("getblockhash", Value::String(hash(2))),
        reply_for("getblock", json!([hash(2), 2]), block_result()),
        reply("getblockhash", Value::String(hash(4))),
    ]);
    let source = block_on(Blocks::connect(ScriptedClient::new(replies), config()))
        .expect("valid scripted source must connect");

    let error = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
        .expect_err("same-height canonical replacement must retry");

    assert!(error.retryable);
    assert!(error.message.contains("changed"));
}

#[test]
fn disappearing_position_is_optional_for_canonical_reference_and_retryable_for_tip() {
    let mut canonical_replies = connect_replies();
    canonical_replies.extend([
        reply("getblockcount", json!(10)),
        failure("getblockhash", -8),
    ]);
    let canonical = block_on(Blocks::connect(
        ScriptedClient::new(canonical_replies),
        config(),
    ))
    .expect("valid scripted source must connect");
    assert_eq!(
        block_on(canonical.canonical_at(BlockPosition(10)))
            .expect("a vanished reorg height is not a fatal source error"),
        None
    );

    let mut tip_replies = connect_replies();
    tip_replies.extend([
        reply("getblockcount", json!(10)),
        failure("getblockhash", -8),
    ]);
    let tip = block_on(Blocks::connect(ScriptedClient::new(tip_replies), config()))
        .expect("valid scripted source must connect");
    let error = block_on(tip.tip()).expect_err("tip height race must retry");
    assert!(error.retryable);
}
