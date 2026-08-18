use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;

use super::*;

#[test]
fn openapi_contains_every_route_method_and_schema() {
    let (_, mut contract) = OpenApiRouter::new()
        .merge(wallet::routes())
        .merge(transaction::routes())
        .split_for_parts();
    let (_, public) = OpenApiRouter::new()
        .merge(health::routes())
        .merge(contract::routes())
        .split_for_parts();
    contract.merge(public);
    let document = serde_json::to_value(contract).expect("OpenAPI contract must serialize");

    for (path, method) in [
        ("/health/live", "get"),
        ("/health/ready", "get"),
        ("/openapi.json", "get"),
        ("/v1/wallets", "post"),
        ("/v1/wallets/{id}", "get"),
        ("/v1/wallets/{id}/balance", "get"),
        ("/v1/wallets/{id}/transactions", "get"),
        ("/v1/wallets/{id}/transactions", "post"),
        ("/v1/transactions", "post"),
    ] {
        assert_pointer(&document, &format!("/paths/{}/{method}", escape(path)));
    }

    for schema in [
        "AddressEncoding",
        "AddressInput",
        "Address",
        "Asset",
        "Balance",
        "Block",
        "Chain",
        "CreateWallet",
        "ErrorBody",
        "Fee",
        "Movement",
        "MovementKind",
        "Proof",
        "Scope",
        "ScopedId",
        "SendFunds",
        "Status",
        "Submission",
        "Transaction",
        "TransactionPage",
        "TransferRequest",
        "TransferResponse",
        "Wallet",
        "WalletTransfer",
    ] {
        assert_pointer(&document, &format!("/components/schemas/{schema}"));
    }

    assert_pointer(
        &document,
        "/components/schemas/TransactionPage/properties/transactions/items/$ref",
    );
    assert!(
        document
            .pointer("/components/schemas/TransactionPage/properties/items")
            .is_none(),
        "transaction history must not expose anonymous items"
    );
    assert_pointer(
        &document,
        "/components/schemas/Transaction/properties/movements/items/$ref",
    );
}

fn escape(path: &str) -> String {
    path.replace('~', "~0").replace('/', "~1")
}

fn assert_pointer(document: &Value, pointer: &str) {
    assert!(
        document.pointer(pointer).is_some(),
        "OpenAPI contract is missing {pointer}"
    );
}
