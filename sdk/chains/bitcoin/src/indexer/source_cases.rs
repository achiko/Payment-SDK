use super::*;

#[test]
fn numbered_block_fetch_retains_enriched_result_and_rechecks_canonical_hash() {
    let mut replies = connect_replies();
    replies.extend([
        reply("getblockhash", Value::String(hash(2))),
        reply_for("getblock", json!([hash(2), 2]), block_result()),
        reply("getblockhash", Value::String(hash(2))),
    ]);
    let source = block_on(Blocks::connect(ScriptedClient::new(replies), config()))
        .expect("valid scripted source must connect");

    let block =
        block_on(source.block_at(BlockHeight(10))).expect("canonical verbosity-2 block must load");

    assert_eq!(block.reference.height, BlockHeight(10));
    assert_eq!(
        parse_bitcoin_block_hash(&hash(2)).expect("test hash must parse"),
        block.reference.hash
    );
    assert_eq!(
        serde_json::from_slice::<Value>(block.raw()).expect("retained block must be exact JSON"),
        block_result()
    );
}

#[test]
fn external_prevouts_are_resolved_once_from_narrow_bounded_calls() {
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

    let block = block_on(source.block_at(BlockHeight(10)))
        .expect("external previous outputs must be enriched");
    calls.assert_exhausted();

    let raw: Value = serde_json::from_slice(block.raw()).expect("enriched block JSON must decode");
    let inputs = raw["tx"][0]["vin"]
        .as_array()
        .expect("spending inputs must be retained");
    assert_eq!(
        inputs[0]["prevout"],
        json!({
            "value_satoshis": 123_456_789_u64,
            "address": address_for_script(
                &previous.output[0].script_pubkey,
                Network::Regtest,
            )
            .expect("test P2WPKH script must have an address")
            .encoded(),
        })
    );
    assert_eq!(inputs[1]["prevout"]["value_satoshis"], json!(50_000_u64));
    assert_eq!(
        inputs[2]["prevout"],
        json!({"value_satoshis": 25_000_u64, "address": null})
    );
    for input in inputs {
        let prevout = input
            .get("prevout")
            .expect("external input must retain compact data");
        assert!(prevout.get("scriptPubKey").is_none());
        assert!(prevout.get("height").is_none());
        assert!(prevout.get("generated").is_none());
        assert!(
            serde_json::to_vec(prevout)
                .expect("compact data must encode")
                .len()
                <= MAX_COMPACT_PREVOUT_JSON_BYTES
        );
    }
    assert!(
        block.raw().len() < previous.output[2].script_pubkey.len(),
        "historical script bytes must not survive retained enrichment"
    );
    assert_eq!(
        raw["tx"][0]["txid"],
        Value::String(spending.compute_txid().to_string())
    );
    assert!(
        raw["tx"][1]["vin"][0].get("prevout").is_none(),
        "same-block previous outputs must remain locally resolved"
    );
    assert_eq!(
        raw["tx"][1]["txid"],
        Value::String(child.compute_txid().to_string())
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

    let error = block_on(source.block_at(BlockHeight(10)))
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

    let error = block_on(source.block_at(BlockHeight(10)))
        .expect_err("same-height canonical replacement must retry");

    assert!(error.retryable);
    assert!(error.message.contains("changed"));
}

#[test]
fn disappearing_height_is_optional_for_canonical_hash_and_retryable_for_tip() {
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
        block_on(canonical.canonical_hash(BlockHeight(10)))
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
