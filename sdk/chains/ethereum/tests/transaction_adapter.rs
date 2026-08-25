use chain_ethereum::{
    Address, BuildContext, ChainError, SignedTransaction, TransactionId, Transactions,
    TransferIntent, TransferRequest, Wei,
};
use indexing::{BoxFuture, SourceError};

struct InspectingAdapter;

impl Transactions for InspectingAdapter {
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
    ) -> BoxFuture<'a, Result<BuildContext, ChainError>> {
        Box::pin(async move {
            let nonce = match request.intent() {
                TransferIntent::Native { from, to, value } => {
                    assert_eq!(from, &Address([0x11; 20]));
                    assert_eq!(to, &Address([0x22; 20]));
                    assert_eq!(value, &Wei::from_u128(7));
                    1
                }
                TransferIntent::Erc20 {
                    from,
                    token,
                    recipient,
                    amount,
                } => {
                    assert_eq!(from, &Address([0x11; 20]));
                    assert_eq!(token, &Address([0x33; 20]));
                    assert_eq!(recipient, &Address([0x22; 20]));
                    assert_eq!(amount, &Wei::from_u128(9));
                    2
                }
            };
            Ok(BuildContext {
                chain_id: 31_337,
                nonce,
                gas_limit: 21_000,
                max_fee_per_gas: Wei::from_u128(2),
                max_priority_fee_per_gas: Wei::from_u128(1),
            })
        })
    }

    fn broadcast<'a>(
        &'a self,
        _transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, SourceError>> {
        Box::pin(async { panic!("the inspection boundary test does not broadcast") })
    }
}

#[test]
fn external_transaction_adapter_can_inspect_typed_intent() {
    let adapter = InspectingAdapter;
    let native =
        TransferRequest::native_atomic(Address([0x11; 20]), Address([0x22; 20]), Wei::from_u128(7));
    let erc20 = TransferRequest::erc20(
        Address([0x11; 20]),
        Address([0x33; 20]),
        Address([0x22; 20]),
        Wei::from_u128(9),
    );

    assert_eq!(
        futures_executor::block_on(adapter.build_context(&native))
            .expect("native intent must be inspectable")
            .nonce,
        1
    );
    assert_eq!(
        futures_executor::block_on(adapter.build_context(&erc20))
            .expect("ERC-20 intent must be inspectable")
            .nonce,
        2
    );
}
