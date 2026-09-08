use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;

use super::*;

#[test]
fn openapi_contains_every_route_method_and_schema() {
    let document = openapi_document();

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
        "Scope",
        "SendFunds",
        "Status",
        "Submission",
        "Transaction",
        "TransactionPage",
        "TransferRequest",
        "TransferResponse",
        "Wallet",
        "WalletAsset",
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
    assert_eq!(
        document.pointer("/components/schemas/Transaction/properties/transaction_id/type"),
        Some(&Value::String("string".to_owned())),
    );
    assert!(document.pointer("/components/schemas/ScopedId").is_none());
    assert!(document.pointer("/components/schemas/Proof").is_none());
    assert!(
        document["components"]["schemas"]["MovementKind"]["enum"]
            .as_array()
            .is_some_and(|variants| {
                variants
                    .iter()
                    .all(|value| value.as_str() != Some("internal_transfer"))
            })
    );
    assert_pointer(
        &document,
        "/paths/~1v1~1wallets~1{id}~1transactions/get/responses/409",
    );

    for schema in [
        "AddressInput",
        "SendFunds",
        "WalletTransfer",
        "TransferRequest",
    ] {
        assert_eq!(
            document.pointer(&format!(
                "/components/schemas/{schema}/additionalProperties"
            )),
            Some(&Value::Bool(false)),
            "{schema} must reject and document unknown properties"
        );
    }
}

#[test]
fn openapi_publishes_native_solana_and_plain_base58_without_spl() {
    let document = openapi_document();

    assert_eq!(
        document["components"]["schemas"]["Chain"]["enum"],
        serde_json::json!(["bitcoin", "ethereum", "solana"])
    );
    assert_eq!(
        document["components"]["schemas"]["WalletAsset"]["enum"],
        serde_json::json!(["btc", "eth", "usdc", "sol"])
    );
    assert_eq!(
        document["components"]["schemas"]["AddressEncoding"]["enum"],
        serde_json::json!(["base58", "base58_check", "bech32", "bech32m", "hex"])
    );
    assert!(
        !document.to_string().contains("spl"),
        "native Solana support must not publish an SPL asset or route"
    );
}

#[test]
fn openapi_publishes_exact_transaction_contracts() {
    let document = openapi_document();
    let transfers = "/components/schemas/TransferRequest/properties/transfers";

    for (schema, properties) in [
        ("AddressInput", &["encoding", "text"][..]),
        ("SendFunds", &["destination", "amount"]),
        ("WalletTransfer", &["wallet_id", "destination", "amount"]),
        ("TransferRequest", &["transfers"]),
    ] {
        assert_exact_required_properties(&document, schema, properties);
    }
    for pointer in [
        "/components/schemas/AddressInput/properties/text/type",
        "/components/schemas/SendFunds/properties/amount/type",
        "/components/schemas/WalletTransfer/properties/wallet_id/type",
        "/components/schemas/WalletTransfer/properties/amount/type",
    ] {
        assert_eq!(
            document.pointer(pointer),
            Some(&serde_json::json!("string"))
        );
    }
    for pointer in [
        "/components/schemas/SendFunds/properties/destination/$ref",
        "/components/schemas/WalletTransfer/properties/destination/$ref",
    ] {
        assert_eq!(
            document.pointer(pointer),
            Some(&serde_json::json!("#/components/schemas/AddressInput")),
        );
    }
    assert_eq!(
        document.pointer(&format!("{transfers}/minItems")),
        Some(&serde_json::json!(1)),
    );
    assert_eq!(
        document.pointer(&format!("{transfers}/maxItems")),
        Some(&serde_json::json!(wallets::MAX_TRANSFERS)),
    );
    assert_eq!(
        document.pointer(&format!("{transfers}/items/$ref")),
        Some(&Value::String(
            "#/components/schemas/WalletTransfer".to_owned()
        )),
    );
    for unsupported_keyword in ["uniqueItems", "default"] {
        assert!(
            document
                .pointer(&format!("{transfers}/{unsupported_keyword}"))
                .is_none(),
            "ordered batch occurrences must not publish {unsupported_keyword}"
        );
    }

    assert_eq!(
        document.pointer("/components/schemas/ErrorBody/required"),
        Some(&serde_json::json!(["message"])),
    );
    assert_description_contains(
        &document,
        "/components/schemas/ErrorBody/properties/transaction_ids/description",
        &["Definitely acknowledged", "Present only"],
    );
    assert_description_contains(
        &document,
        "/components/schemas/ErrorBody/properties/failed_index/description",
        &["original request index", "Present only"],
    );
    assert_description_contains(
        &document,
        "/components/schemas/ErrorBody/properties/ambiguous_transaction_id/description",
        &["exact locally signed envelope", "Present only", "503"],
    );

    for operation in [
        "/paths/~1v1~1wallets~1{id}~1transactions/post/description",
        "/paths/~1v1~1transactions/post/description",
    ] {
        assert_description_contains(&document, operation, &["native SOL", "shared route"]);
    }
    assert_description_contains(
        &document,
        "/paths/~1v1~1transactions/post/description",
        &["Definitely acknowledged", "only when"],
    );
    assert!(
        document["paths"]
            .as_object()
            .is_some_and(|paths| paths.keys().all(|path| !path.contains("solana"))),
        "native SOL must use the shared transaction routes"
    );
    assert_eq!(
        document.pointer(
            "/paths/~1v1~1wallets~1{id}~1transactions/post/requestBody/content/application~1json/schema/$ref"
        ),
        Some(&serde_json::json!("#/components/schemas/SendFunds")),
    );
    assert_eq!(
        document.pointer(
            "/paths/~1v1~1transactions/post/requestBody/content/application~1json/schema/$ref"
        ),
        Some(&serde_json::json!("#/components/schemas/TransferRequest")),
    );
}

fn openapi_document() -> Value {
    let (_, mut contract) = OpenApiRouter::new()
        .merge(wallet::routes())
        .merge(transaction::routes())
        .split_for_parts();
    let (_, public) = OpenApiRouter::new()
        .merge(health::routes())
        .merge(openapi::routes())
        .split_for_parts();
    contract.merge(public);
    serde_json::to_value(contract).expect("OpenAPI contract must serialize")
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

fn assert_description_contains(document: &Value, pointer: &str, expected: &[&str]) {
    let description = document
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("OpenAPI contract is missing description {pointer}"));
    for fragment in expected {
        assert!(
            description.contains(fragment),
            "OpenAPI description {pointer} is missing `{fragment}`: {description}"
        );
    }
}

fn assert_exact_required_properties(document: &Value, schema: &str, expected: &[&str]) {
    let schema_pointer = format!("/components/schemas/{schema}");
    let properties = document
        .pointer(&format!("{schema_pointer}/properties"))
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("OpenAPI schema {schema} is missing properties"));
    assert_eq!(
        properties.len(),
        expected.len(),
        "OpenAPI schema {schema} has unexpected properties: {properties:?}"
    );
    for property in expected {
        assert!(
            properties.contains_key(*property),
            "OpenAPI schema {schema} is missing property {property}"
        );
    }

    let required = document
        .pointer(&format!("{schema_pointer}/required"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("OpenAPI schema {schema} is missing required properties"));
    assert_eq!(required.len(), expected.len());
    for property in expected {
        assert!(
            required
                .iter()
                .any(|value| value.as_str() == Some(property)),
            "OpenAPI schema {schema} does not require {property}"
        );
    }
}
