#[path = "acceptance.rs"]
mod acceptance;
#[path = "bitcoin_stack.rs"]
mod bitcoin_node;
#[path = "ethereum_stack.rs"]
mod ethereum_node;
#[path = "route_contract.rs"]
mod route_contract;

use std::{net::SocketAddr, process::Stdio, time::Duration};

use bitcoin::{CompressedPublicKey, Network, PrivateKey, PublicKey};
use bitcoin_node::{BitcoinNode, FundingOutput};
use ethereum_node::{EthereumNode, Transaction};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::{Child, Command};

const TOKEN: &str = "wallet-api-system-test";
const USDC_CONTRACT: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

struct RunningApi {
    root: String,
    child: Child,
    _config: TempDir,
}

impl RunningApi {
    async fn stop(mut self) {
        let pid = self.child.id().expect("API process must have an ID");
        let status = std::process::Command::new("kill")
            .arg("-INT")
            .arg(pid.to_string())
            .status()
            .expect("API interrupt must be sent");
        assert!(status.success(), "API interrupt must succeed");
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(result) => {
                assert!(result.expect("API process must be reaped").success());
            }
            Err(_) => {
                self.child.start_kill().expect("API process must stop");
                self.child.wait().await.expect("API process must be reaped");
                panic!("API did not stop after its shutdown signal");
            }
        }
    }
}

impl Drop for RunningApi {
    fn drop(&mut self) {
        // A failed assertion must not leave the composed API running in the
        // background and holding its temporary redb files open.
        let _ = self.child.start_kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bitcoin_wallet_is_generated_and_indexed() -> Result<(), Box<dyn std::error::Error>> {
    let files = TempDir::new()?;
    let node = BitcoinNode::start().await;
    let api = start_api(json!({
        "bitcoin": {
            "database": files.path().join("bitcoin.redb"),
            "network": "regtest",
            "genesis_hash": node.fixture.genesis_hash,
            "rpc": rpc_config(&node.rpc_url, true),
            "confirmation_depth": 1,
            "reorg_retention": 10,
            "poll_millis": 10,
            "batch_size": 10
        }
    }))
    .await?;

    let wallet = create_wallet(&api.root, "btc").await?;
    assert_eq!(wallet["asset"], "btc");
    assert_eq!(wallet["chain"], "bitcoin");
    assert_eq!(wallet["network"], "regtest");
    let address = wallet["address"]
        .as_str()
        .expect("wallet response must contain an address");
    assert_eq!(wallet_summary(&api.root, wallet_id(&wallet)).await?, wallet);
    let transaction_id = node.fund(vec![FundingOutput::new(address, 100_000)]);
    node.mine();

    wait_balance(&api.root, wallet_id(&wallet), "0.001").await;
    let history = wait_history(&api.root, wallet_id(&wallet), &transaction_id).await;
    assert_eq!(history["transactions"][0]["scope"]["chain"], "bitcoin");
    let history_text = serde_json::to_string(&history)?;
    assert!(history_text.contains(address));
    assert!(history_text.contains("\"amount\":\"0.001\""));

    let submitted = send(
        &api.root,
        wallet_id(&wallet),
        "bech32",
        &bitcoin_destination(9),
        "0.0004",
    )
    .await?;
    node.confirm();
    wait_history(&api.root, wallet_id(&wallet), &submitted).await;
    let remaining = wait_atomic_below(&api.root, wallet_id(&wallet), 8, 60_000).await;
    assert!(remaining > 0, "Bitcoin change must remain in the wallet");

    let batch = send_batch(
        &api.root,
        &[
            (
                wallet_id(&wallet),
                "bech32",
                bitcoin_destination(8),
                "0.0001",
            ),
            (
                wallet_id(&wallet),
                "bech32",
                bitcoin_destination(7),
                "0.0001",
            ),
        ],
    )
    .await?;
    assert_eq!(batch.len(), 1, "Bitcoin must group compatible transfers");
    node.confirm();
    wait_history(&api.root, wallet_id(&wallet), &batch[0]).await;
    let remaining = wait_atomic_below(&api.root, wallet_id(&wallet), 8, 40_000).await;
    assert!(remaining > 0, "Bitcoin batch change must remain");

    api.stop().await;
    node.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ethereum_wallet_is_generated_and_indexed() -> Result<(), Box<dyn std::error::Error>> {
    let files = TempDir::new()?;
    let node = EthereumNode::start().await;
    let api = start_api(json!({
        "ethereum": {
            "database": files.path().join("ethereum.redb"),
            "network": "mainnet",
            "chain_id": 1,
            "genesis_hash": ethereum_node::GENESIS_HASH,
            "rpc": rpc_config(&node.rpc_url, false),
            "confirmation_depth": 1,
            "reorg_retention": 10,
            "poll_millis": 10,
            "batch_size": 10
        }
    }))
    .await?;

    let wallet = create_wallet(&api.root, "eth").await?;
    assert_eq!(wallet["asset"], "eth");
    assert_eq!(wallet["chain"], "ethereum");
    assert_eq!(wallet["network"], "mainnet");
    let address = wallet["address"]
        .as_str()
        .expect("wallet response must contain an address")
        .to_owned();
    assert_eq!(wallet_summary(&api.root, wallet_id(&wallet)).await?, wallet);
    let transaction_id = format!("0x{}", "71".repeat(32));
    node.append(vec![Transaction::native(
        0x71,
        format!("0x{}", "55".repeat(20)),
        address.clone(),
        1_000_000_000_000_000_000,
    )]);
    node.append(Vec::new());

    wait_balance(&api.root, wallet_id(&wallet), "10").await;
    let history = wait_history(&api.root, wallet_id(&wallet), &transaction_id).await;
    assert_eq!(history["transactions"][0]["scope"]["chain"], "ethereum");
    let history_text = serde_json::to_string(&history)?;
    assert!(history_text.contains(&address));
    assert!(history_text.contains("\"amount\":\"1\""));

    let destination = format!("0x{}", "22".repeat(20));
    node.expect_send(address, destination.clone(), 1_000_000_000_000_000_000);
    let submitted = send(&api.root, wallet_id(&wallet), "hex", &destination, "1").await?;
    node.confirm();
    wait_history(&api.root, wallet_id(&wallet), &submitted).await;
    wait_balance(&api.root, wallet_id(&wallet), "9").await;

    let first = format!("0x{}", "33".repeat(20));
    let second = format!("0x{}", "44".repeat(20));
    node.expect_send(
        wallet["address"]
            .as_str()
            .expect("wallet address")
            .to_owned(),
        first.clone(),
        1_000_000_000_000_000_000,
    );
    node.expect_send(
        wallet["address"]
            .as_str()
            .expect("wallet address")
            .to_owned(),
        second.clone(),
        2_000_000_000_000_000_000,
    );
    let batch = send_batch(
        &api.root,
        &[
            (wallet_id(&wallet), "hex", first, "1"),
            (wallet_id(&wallet), "hex", second, "2"),
        ],
    )
    .await?;
    assert_eq!(
        batch,
        node.submitted_ids(),
        "Ethereum IDs must keep request order"
    );
    node.confirm();
    for transaction in &batch {
        wait_history(&api.root, wallet_id(&wallet), transaction).await;
    }
    wait_balance(&api.root, wallet_id(&wallet), "6").await;

    api.stop().await;
    node.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usdc_wallet_is_generated_sent_and_indexed() -> Result<(), Box<dyn std::error::Error>> {
    let files = TempDir::new()?;
    let node = EthereumNode::start().await;
    let api = start_api(json!({
        "ethereum": {
            "database": files.path().join("ethereum-usdc.redb"),
            "network": "mainnet",
            "chain_id": 1,
            "genesis_hash": ethereum_node::GENESIS_HASH,
            "rpc": rpc_config(&node.rpc_url, false),
            "confirmation_depth": 1,
            "reorg_retention": 10,
            "poll_millis": 10,
            "batch_size": 10,
            "usdc": {"contract": USDC_CONTRACT}
        }
    }))
    .await?;

    let eth = create_wallet(&api.root, "eth").await?;
    let usdc = create_wallet(&api.root, "usdc").await?;
    assert_eq!(eth["asset"], "eth");
    assert_eq!(usdc["asset"], "usdc");
    assert_eq!(usdc["chain"], "ethereum");
    assert_eq!(usdc["network"], "mainnet");
    assert_ne!(
        eth["address"], usdc["address"],
        "each generated asset wallet must have its own address"
    );

    let mixed = batch_response(
        &api.root,
        &[
            (
                wallet_id(&eth),
                "hex",
                format!("0x{}", "31".repeat(20)),
                "0.1",
            ),
            (
                wallet_id(&usdc),
                "hex",
                format!("0x{}", "32".repeat(20)),
                "1",
            ),
        ],
    )
    .await?;
    assert_eq!(mixed.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = mixed.json().await?;
    assert_eq!(body["failed_index"], 1);
    assert!(body["transaction_ids"].as_array().is_none_or(Vec::is_empty));
    assert!(
        node.submitted_ids().is_empty(),
        "mixed ETH and USDC families must be rejected before broadcast"
    );

    let usdc_address = usdc["address"]
        .as_str()
        .expect("USDC wallet response must contain an address")
        .to_owned();
    let incoming = format!("0x{}", "72".repeat(32));
    node.append(vec![Transaction::erc20(
        0x72,
        USDC_CONTRACT.to_owned(),
        format!("0x{}", "55".repeat(20)),
        usdc_address.clone(),
        10_000_000,
    )]);
    node.append(Vec::new());

    wait_balance(&api.root, wallet_id(&usdc), "10").await;
    let incoming_history = wait_history(&api.root, wallet_id(&usdc), &incoming).await;
    let incoming_entry = transaction(&incoming_history, &incoming);
    assert_eq!(incoming_entry["scope"]["chain"], "ethereum");
    assert_eq!(incoming_entry["movements"][0]["asset"]["id"], USDC_CONTRACT);
    assert_eq!(incoming_entry["movements"][0]["asset"]["decimals"], 6);
    assert_eq!(incoming_entry["movements"][0]["amount"], "10");
    assert_eq!(incoming_entry["movements"][0]["to"]["value"], usdc_address);

    let destination = format!("0x{}", "23".repeat(20));
    node.expect_token_send(
        USDC_CONTRACT.to_owned(),
        usdc_address,
        destination.clone(),
        1_000_000,
    );
    let submitted = send(&api.root, wallet_id(&usdc), "hex", &destination, "1").await?;
    node.confirm();
    let outgoing_history = wait_history(&api.root, wallet_id(&usdc), &submitted).await;
    let outgoing_entry = transaction(&outgoing_history, &submitted);
    assert_eq!(outgoing_entry["movements"][0]["asset"]["id"], USDC_CONTRACT);
    assert_eq!(outgoing_entry["movements"][0]["amount"], "1");
    assert_eq!(outgoing_entry["movements"][0]["to"]["value"], destination);
    assert_eq!(outgoing_entry["fee"]["asset"]["id"], "native");
    assert_eq!(outgoing_entry["fee"]["asset"]["decimals"], 18);
    wait_balance(&api.root, wallet_id(&usdc), "9").await;

    api.stop().await;
    node.stop().await;
    Ok(())
}

async fn start_api(indexes: Value) -> Result<RunningApi, Box<dyn std::error::Error>> {
    start_api_with(indexes, json!([]), &[]).await
}

async fn start_api_with(
    indexes: Value,
    wallets: Value,
    secrets: &[(&str, &str)],
) -> Result<RunningApi, Box<dyn std::error::Error>> {
    let bind = unused_address();
    let config_dir = TempDir::new()?;
    let config_path = config_dir.path().join("payment-api.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "bind": bind,
            "bearer_token_env": "SYSTEM_TEST_TOKEN",
            "indexes": indexes,
            "wallets": wallets
        }))?,
    )?;
    let child = Command::new(env!("CARGO_BIN_EXE_payment-api"))
        .arg(config_path)
        .env("SYSTEM_TEST_TOKEN", TOKEN)
        .envs(secrets.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let running = RunningApi {
        root: format!("http://{bind}"),
        child,
        _config: config_dir,
    };
    wait_ready(&running.root).await;
    let unauthorized = reqwest::Client::new()
        .post(format!("{}/v1/wallets", running.root))
        .json(&json!({"asset": "btc"}))
        .send()
        .await?;
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    Ok(running)
}

fn rpc_config(endpoint: &str, bitcoin: bool) -> Value {
    json!({
        "endpoints": [endpoint],
        "headers": if bitcoin { vec![("authorization", "Basic test")] } else { Vec::new() },
        "timeout_seconds": 2,
        "max_response_bytes": 1024 * 1024
    })
}

async fn create_wallet(root: &str, asset: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let response = reqwest::Client::new()
        .post(format!("{root}/v1/wallets"))
        .bearer_auth(TOKEN)
        .json(&json!({"asset": asset}))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(format!("wallet creation failed with {status}: {body}").into());
    }
    Ok(serde_json::from_str(&body)?)
}

async fn wallet_summary(root: &str, wallet: &str) -> Result<Value, reqwest::Error> {
    reqwest::Client::new()
        .get(format!("{root}/v1/wallets/{wallet}"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

async fn send(
    root: &str,
    wallet: &str,
    encoding: &str,
    destination: &str,
    amount: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let response: Value = reqwest::Client::new()
        .post(format!("{root}/v1/wallets/{wallet}/transactions"))
        .bearer_auth(TOKEN)
        .json(&json!({
            "destination": {"encoding": encoding, "text": destination},
            "amount": amount
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(response["transaction_id"]
        .as_str()
        .expect("submission must contain a transaction ID")
        .to_owned())
}

async fn send_batch(
    root: &str,
    transfers: &[(&str, &str, String, &str)],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let response = batch_response(root, transfers).await?;
    let response: Value = response.error_for_status()?.json().await?;
    Ok(response["transaction_ids"]
        .as_array()
        .expect("batch response must contain transaction IDs")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("transaction ID must be text")
                .to_owned()
        })
        .collect())
}

async fn batch_response(
    root: &str,
    transfers: &[(&str, &str, String, &str)],
) -> Result<reqwest::Response, reqwest::Error> {
    let transfers = transfers
        .iter()
        .map(|(wallet, encoding, destination, amount)| {
            json!({
                "wallet_id": wallet,
                "destination": {"encoding": encoding, "text": destination},
                "amount": amount
            })
        })
        .collect::<Vec<_>>();
    reqwest::Client::new()
        .post(format!("{root}/v1/transactions"))
        .bearer_auth(TOKEN)
        .json(&json!({"transfers": transfers}))
        .send()
        .await
}

async fn wait_balance(root: &str, wallet: &str, expected: &str) {
    let url = format!("{root}/v1/wallets/{wallet}/balance");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if get(&url)
            .await
            .is_some_and(|value| value["amount"] == expected)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wallet balance did not become {expected}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_atomic_below(root: &str, wallet: &str, decimals: u32, ceiling: u64) -> u64 {
    let url = format!("{root}/v1/wallets/{wallet}/balance");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = None;
    loop {
        if let Some(amount) = get(&url).await.and_then(|value| {
            value["amount"]
                .as_str()?
                .parse::<base::Decimal>()
                .ok()?
                .to_atomic_u64(decimals)
                .ok()
        }) {
            if amount < ceiling {
                return amount;
            }
            last = Some(amount);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wallet balance did not fall below {ceiling} atomic units; last balance: {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_history(root: &str, wallet: &str, transaction: &str) -> Value {
    let url = format!("{root}/v1/wallets/{wallet}/transactions?limit=20");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut last = None;
    loop {
        if let Some(value) = get(&url).await {
            if value["transactions"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["transaction_id"] == transaction)
            }) {
                return value;
            }
            last = Some(value);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wallet history did not contain transaction {transaction}; last response: {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn transaction<'a>(history: &'a Value, transaction_id: &str) -> &'a Value {
    history["transactions"]
        .as_array()
        .expect("history must contain transactions")
        .iter()
        .find(|transaction| transaction["transaction_id"] == transaction_id)
        .expect("requested transaction must be present in history")
}

async fn get(url: &str) -> Option<Value> {
    reqwest::Client::new()
        .get(url)
        .bearer_auth(TOKEN)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()
}

async fn wait_ready(root: &str) {
    let url = format!("{root}/health/ready");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if reqwest::get(&url)
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wallet API did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn wallet_id(wallet: &Value) -> &str {
    wallet["id"]
        .as_str()
        .expect("wallet response must contain an ID")
}

fn unused_address() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("temporary listener must bind")
        .local_addr()
        .expect("temporary address must exist")
}

fn bitcoin_destination(byte: u8) -> String {
    let private = PrivateKey::from_slice(&[byte; 32], Network::Regtest)
        .expect("fixed destination key must be valid");
    let public = PublicKey::from_private_key(&bitcoin::secp256k1::Secp256k1::new(), &private);
    bitcoin::Address::p2wpkh(
        &CompressedPublicKey::try_from(public).expect("public key must be compressed"),
        Network::Regtest,
    )
    .to_string()
}
