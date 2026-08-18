use super::*;
use crate::{Address, Wei};

fn request() -> TransferRequest {
    TransferRequest {
        from: Address([0x11; 20]),
        to: Some(Address([0x22; 20])),
        value: Wei::from_u128(7),
        data: vec![0xaa],
    }
}

fn context() -> BuildContext {
    BuildContext {
        chain_id: 1,
        nonce: 2,
        gas_limit: 21_000,
        max_fee_per_gas: Wei::from_u128(10),
        max_priority_fee_per_gas: Wei::from_u128(3),
    }
}

#[test]
fn build_preserves_chain_native_fields() {
    let request = request();
    let context = context();
    let transaction = build(request.clone(), context.clone()).expect("valid transfer must build");

    assert_eq!(transaction.chain_id, context.chain_id);
    assert_eq!(transaction.nonce, context.nonce);
    assert_eq!(transaction.from, request.from);
    assert_eq!(transaction.to, request.to);
    assert_eq!(transaction.value, request.value);
    assert_eq!(transaction.input, request.data);
    assert_eq!(transaction.gas_limit, context.gas_limit);
    assert_eq!(transaction.max_fee_per_gas, context.max_fee_per_gas);
    assert_eq!(
        transaction.max_priority_fee_per_gas,
        context.max_priority_fee_per_gas
    );
}

#[test]
fn build_rejects_invalid_fee_and_creation_constraints() {
    let mut invalid_fee = context();
    invalid_fee.max_fee_per_gas = Wei::from_u128(2);
    assert_eq!(
        build(request(), invalid_fee)
            .expect_err("priority fee above maximum must fail")
            .kind,
        ChainErrorKind::InvalidTransaction
    );

    let mut creation = request();
    creation.to = None;
    creation.data.clear();
    assert_eq!(
        build(creation, context())
            .expect_err("empty contract creation must fail")
            .kind,
        ChainErrorKind::InvalidTransaction
    );
}
