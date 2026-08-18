use std::{net::SocketAddr, path::PathBuf};

use super::*;

#[test]
fn parses_supported_networks_only() {
    assert_eq!(
        bitcoin_network("regtest").expect("known network"),
        Network::Regtest
    );
    assert!(bitcoin_network("fixture").is_err());
}

#[test]
fn errors_do_not_echo_secret_values() {
    let error = CompositionError::invalid("private key must be hexadecimal");
    assert_eq!(error.to_string(), "private key must be hexadecimal");
}

#[test]
fn builds_bitcoin_failover_without_contacting_nodes() {
    let config = BitcoinConfig {
        id: "treasury".to_owned(),
        network: "regtest".to_owned(),
        rpc_urls: vec![
            "http://127.0.0.1:18443".to_owned(),
            "http://127.0.0.1:28443".to_owned(),
        ],
        rpc_headers: vec![("authorization".to_owned(), "secret".to_owned())],
        genesis_hash: String::new(),
        secret_env: "PAYMENT_TEST_KEY".to_owned(),
        taproot: false,
        fee_target_blocks: 6,
        max_fee_rate: 1,
        timeout_seconds: 1,
        max_response_bytes: 1024,
    };
    let failover = bitcoin_transport(&config).expect("valid transports must compose");
    assert_eq!(failover.len(), 2);
    let diagnostics = format!("{failover:?}");
    assert!(!diagnostics.contains("18443"));
    assert!(!diagnostics.contains("secret"));
}

#[tokio::test]
async fn composition_resolves_keys_only_at_the_application_boundary() {
    const MISSING: &str = "PAYMENT_API_TEST_KEY_THAT_MUST_NOT_EXIST_91D247";
    let directory = tempfile::tempdir().expect("temporary database directory");
    let config = RuntimeConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        server: crate::ServerConfig {
            bearer_token_env: "PAYMENT_API_TEST_TOKEN".to_owned(),
            max_request_body_bytes: 1024,
            tls_terminated_upstream: false,
        },
        database: PathBuf::from(directory.path()).join("payments"),
        indexer: crate::IndexerConfig {
            endpoints: vec!["http://127.0.0.1:1".to_owned()],
            bearer_token_env: None,
            timeout_seconds: 1,
            max_response_bytes: 1024,
        },
        wallets: vec![WalletConfig::Ethereum(EthereumConfig {
            id: "treasury".to_owned(),
            network: "sepolia".to_owned(),
            rpc_urls: vec!["http://127.0.0.1:1".to_owned()],
            rpc_headers: Vec::new(),
            chain_id: 11_155_111,
            asset: crate::EthereumAsset::Native,
            secret_env: MISSING.to_owned(),
            timeout_seconds: 1,
            max_response_bytes: 1024,
            max_input_bytes: 1024,
            gas_margin_basis_points: 100,
            max_gas_limit: 1_000_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            max_total_fee: 1_000_000,
        })],
        deposits: None,
        reconcile_seconds: 1,
        reconcile_limit: 1,
    };
    let mut secrets = Secrets::new();
    secrets.insert("PAYMENT_API_TEST_TOKEN", "gateway-secret");
    let error = Runtime::build(config, secrets)
        .await
        .err()
        .expect("missing key reference must stop composition");
    assert!(error.to_string().contains(MISSING));
}
