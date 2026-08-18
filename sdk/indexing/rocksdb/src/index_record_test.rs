use super::*;

fn chain() -> ChainId {
    ChainId("test-chain".to_owned())
}

fn scope() -> indexing::IndexScope {
    indexing::IndexScope {
        chain: chain(),
        network: "testnet".to_owned(),
    }
}

fn address() -> CanonicalAddress {
    CanonicalAddress {
        scope: scope(),
        value: "address-1".to_owned(),
    }
}

fn output() -> IndexedOutput {
    IndexedOutput {
        id: OutputId {
            transaction: TransactionRef {
                scope: scope(),
                value: "transaction-1".to_owned(),
            },
            index: 7,
        },
        address: address(),
        asset: AssetId {
            chain: chain(),
            asset: "native".to_owned(),
        },
        amount: "123456789.000000000000000001"
            .parse()
            .expect("test amount must parse"),
        evidence: vec![1, 2, 3, 4],
        created_at: BlockHeight(42),
        coinbase: false,
    }
}

#[test]
fn selectors_round_trip_without_chain_native_types() {
    let selector = address();
    let encoded = encode_target(&selector).expect("selector must encode");
    assert_eq!(
        decode_target(&encoded).expect("selector must decode"),
        selector
    );
}

#[test]
fn output_projection_round_trips_typed_values_and_markers() {
    let output = output();
    let effect = IndexChanges {
        outputs: indexing::OutputChanges {
            created: vec![output.clone()],
            spent: vec![output.key()],
            tracked_spends: Vec::new(),
        },
    };
    let projection = project(&effect).expect("effect must project");
    let ProjectionMutation::Put { key, value } = &projection.mutations[0] else {
        panic!("creation must be an unconditional put");
    };
    assert_eq!(
        decode_output(key, value).expect("output must decode"),
        output
    );
    let ProjectionMutation::Put { key, value } = &projection.mutations[1] else {
        panic!("spend must be an unconditional put");
    };
    assert_eq!(
        decode_spent(key, value).expect("marker must decode"),
        output.key()
    );
}

#[test]
fn undo_round_trips_and_rejects_trailing_bytes() {
    let key = output().key();
    let undo = IndexUndo {
        created: vec![key.clone()],
        spent: vec![key],
    };
    let encoded = encode_undo(&undo).expect("undo must encode");
    assert_eq!(decode_undo(&encoded).expect("undo must decode"), undo);
    let mut malformed = encoded;
    malformed.push(0);
    assert!(decode_undo(&malformed).is_err());
}

#[test]
fn output_rejects_negative_and_corrupt_amounts() {
    let output = output();
    let key = output_key(&output.key(), CREATED_OUTPUT).expect("key must encode");
    for amount in ["-1", "01", "invalid"] {
        let mut value = Vec::new();
        value.extend_from_slice(VALUE_MAGIC);
        value.push(VALUE_ENCODING);
        write_text(&mut value, amount).expect("test amount must encode");
        value.extend_from_slice(&output.created_at.0.to_be_bytes());
        value.push(0);
        write_asset(&mut value, &output.asset).expect("asset must encode");
        write_bytes(&mut value, &output.evidence).expect("evidence must encode");

        assert!(decode_output(&key, &value).is_err());
    }
}
