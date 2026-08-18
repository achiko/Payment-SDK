#[path = "ethereum_stack.rs"]
#[allow(dead_code)]
mod ethereum;

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use ethereum::{EthereumStack, Transaction};
use payment_api::{
    DepositConfig, EthereumAsset, EthereumConfig, IndexerConfig, KeyConfig, Runtime, RuntimeConfig,
    Secrets, ServerConfig, WalletConfig,
};
use tempfile::TempDir;
use tokio::sync::oneshot;

const TOKEN: &str = "ethereum-deposit-test-token";
const TREASURY_KEY: &str = "0404040404040404040404040404040404040404040404040404040404040404";
const DEPOSIT_KEY: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const AMOUNT: u128 = 1_000_000_000_000_000_000;
const GAS_LIMIT: u64 = 25_200;
const MAX_GAS_PRICE: u128 = 3;
const GAS_USED: u64 = 21_000;
const GAS_PRICE: u128 = 2;
const FEE_LIMIT: u128 = GAS_LIMIT as u128 * MAX_GAS_PRICE;
const FEE: u128 = GAS_USED as u128 * GAS_PRICE;
const SENT: u128 = AMOUNT - FEE_LIMIT;
const GROSS: u128 = SENT + FEE;
const RESIDUAL: u128 = FEE_LIMIT - FEE;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concrete_runtime_observes_and_collects_native_ethereum()
-> Result<(), Box<dyn std::error::Error>> {
    let files = TempDir::new()?;
    let stack = EthereumStack::start(&files.path().join("indexer")).await;
    stack.receipt_gas_used(GAS_USED);
    stack.receipt_gas_price(GAS_PRICE);
    let bind = unused_address();
    let runtime = start_runtime(
        config(
            bind,
            files.path().join("payments"),
            stack.rpc_url.clone(),
            stack.indexer_url.clone(),
        ),
        secrets(),
    )
    .await?;
    let client = reqwest::Client::new();
    let root = format!("http://{bind}");
    wait_ready(&client, &root).await;

    let opened = client
        .post(format!("{root}/v1/deposits"))
        .bearer_auth(TOKEN)
        .header("idempotency-key", "open-ethereum-deposit")
        .json(&serde_json::json!({
            "id": "ethereum-deposit",
            "user_id": "merchant",
            "asset": {"chain": "ethereum", "asset": "native"},
            "expected": AMOUNT.to_string(),
            "expires_at": 9_999_999_999_u64,
            "created_at": 1_u64
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    let deposit_address = opened["address"]["value"]
        .as_str()
        .expect("deposit response must contain an address")
        .to_owned();

    stack.append(vec![Transaction::native(
        0x71,
        format!("0x{}", "55".repeat(20)),
        deposit_address.clone(),
        AMOUNT,
    )]);
    stack.append(Vec::new());
    wait_balance(&client, &root, AMOUNT.to_string()).await;

    let history = client
        .get(format!("{root}/v1/deposits/ethereum-deposit/history"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    assert!(
        history["entries"]
            .as_array()
            .is_some_and(|entries| entries.len() >= 2)
    );

    let planned = plan(&client, &root).await?;
    assert_eq!(planned["mode"], "account_transfer");
    assert_eq!(planned["legs"][0]["state"], "required");
    let treasury = planned["destination"]["value"]
        .as_str()
        .expect("collection must contain its server-owned destination")
        .to_owned();
    stack.expect_broadcast(deposit_address, treasury, SENT);

    let executed = client
        .post(format!("{root}/v1/collections/native-sweep/execute"))
        .bearer_auth(TOKEN)
        .header("idempotency-key", "native-sweep")
        .send()
        .await?;
    let status = executed.status();
    let body = executed.text().await?;
    assert!(
        status.is_success(),
        "collection execution failed with {status}: {body}"
    );
    let executed: serde_json::Value = serde_json::from_str(&body)?;
    assert_eq!(executed["legs"][0]["state"], "broadcast");
    assert_eq!(stack.broadcasts(), 1);

    wait_collection(&client, &root).await;
    let completed = collection(&client, &root).await?;
    assert_eq!(
        completed["legs"][0]["allocations"][0]["gross_debit"],
        GROSS.to_string()
    );
    assert_eq!(
        completed["legs"][0]["allocations"][0]["master_credit"],
        SENT.to_string()
    );
    assert_eq!(
        completed["legs"][0]["allocations"][0]["allocated_fee"],
        FEE.to_string()
    );
    let balance = client
        .get(format!("{root}/v1/deposits/ethereum-deposit/balance"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    assert_eq!(balance["entry"]["balances"]["collected"], GROSS.to_string());
    assert_eq!(
        balance["entry"]["balances"]["balance"],
        RESIDUAL.to_string()
    );
    stack.reorg_last_broadcast();
    wait_state(&client, &root, "reorged").await;
    let corrected = client
        .get(format!("{root}/v1/deposits/ethereum-deposit/balance"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    assert_eq!(corrected["entry"]["balances"]["collected"], "0");
    assert_eq!(
        corrected["entry"]["balances"]["balance"],
        AMOUNT.to_string()
    );

    runtime.stop().await?;
    stack.stop().await;
    Ok(())
}

async fn collection(
    client: &reqwest::Client,
    root: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    Ok(client
        .get(format!("{root}/v1/collections/native-sweep"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn plan(
    client: &reqwest::Client,
    root: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let body = serde_json::json!({
        "id": "native-sweep", "job_id": "native-job",
        "deposit_ids": ["ethereum-deposit"], "created_at": 2_u64
    });
    for _ in 0..300 {
        let response = client
            .post(format!("{root}/v1/collections"))
            .bearer_auth(TOKEN)
            .header("idempotency-key", "native-sweep")
            .json(&body)
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(response.json().await?);
        }
        if response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE
            && response.status() != reqwest::StatusCode::CONFLICT
        {
            return Err(format!("collection planning failed: {}", response.text().await?).into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err("native collection did not become eligible".into())
}

async fn wait_collection(client: &reqwest::Client, root: &str) {
    wait_state(client, root, "completed").await;
}

async fn wait_state(client: &reqwest::Client, root: &str, expected: &str) {
    let mut last = String::new();
    for _ in 0..400 {
        if let Ok(response) = client
            .get(format!("{root}/v1/collections/native-sweep"))
            .bearer_auth(TOKEN)
            .send()
            .await
        {
            if let Ok(text) = response.text().await {
                last = text.clone();
                if serde_json::from_str::<serde_json::Value>(&text)
                    .is_ok_and(|value| value["state"] == expected)
                {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("native collection did not become {expected}: {last}");
}

async fn wait_balance(client: &reqwest::Client, root: &str, expected: String) {
    let mut last = String::new();
    for _ in 0..400 {
        if let Ok(response) = client
            .get(format!("{root}/v1/deposits/ethereum-deposit/balance"))
            .bearer_auth(TOKEN)
            .send()
            .await
        {
            if let Ok(text) = response.text().await {
                last = text.clone();
                if serde_json::from_str::<serde_json::Value>(&text)
                    .is_ok_and(|value| value["entry"]["balances"]["confirmed"] == expected)
                {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("native deposit balance did not become confirmed: {last}");
}

struct Running {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), payment_api::CompositionError>>,
}
impl Running {
    async fn stop(mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.stop.take().expect("stop sender").send(()).ok();
        self.task.await??;
        Ok(())
    }
}
async fn start_runtime(
    config: RuntimeConfig,
    secrets: Secrets,
) -> Result<Running, Box<dyn std::error::Error>> {
    let runtime = Runtime::build(config, secrets).await?;
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(runtime.run_until(async move {
        let _ignored = stopped.await;
    }));
    Ok(Running {
        stop: Some(stop),
        task,
    })
}

fn config(
    bind: SocketAddr,
    database: PathBuf,
    rpc_url: String,
    indexer_url: String,
) -> RuntimeConfig {
    RuntimeConfig {
        bind,
        server: ServerConfig {
            bearer_token_env: "TOKEN".to_owned(),
            max_request_body_bytes: 65_536,
            tls_terminated_upstream: false,
        },
        database,
        indexer: IndexerConfig {
            endpoints: vec![indexer_url],
            bearer_token_env: None,
            timeout_seconds: 2,
            max_response_bytes: 1_048_576,
        },
        wallets: vec![WalletConfig::Ethereum(EthereumConfig {
            id: "treasury".to_owned(),
            network: "mainnet".to_owned(),
            rpc_urls: vec![rpc_url],
            rpc_headers: Vec::new(),
            chain_id: 1,
            asset: EthereumAsset::Native,
            secret_env: "TREASURY_KEY".to_owned(),
            timeout_seconds: 2,
            max_response_bytes: 1_048_576,
            max_input_bytes: 1024,
            gas_margin_basis_points: 2_000,
            max_gas_limit: 1_000_000,
            max_fee_per_gas: 1_000_000_000_000,
            max_priority_fee_per_gas: 100_000_000_000,
            max_total_fee: 1_000_000_000_000_000_000,
        })],
        deposits: Some(DepositConfig {
            wallet: "treasury".to_owned(),
            asset: "native".to_owned(),
            gas_wallet: None,
            policy_version: "v1".to_owned(),
            policy_digest: "09".repeat(32),
            minimum_collection: "1".to_owned(),
            minimum_confirmations: 1,
            coinbase_maturity: 0,
            max_participants: 1,
            max_inputs: 1,
            gas_amount: None,
            keys: vec![KeyConfig {
                purpose: "merchant".to_owned(),
                secret_env: "DEPOSIT_KEY".to_owned(),
            }],
        }),
        reconcile_seconds: 1,
        reconcile_limit: 100,
    }
}

fn secrets() -> Secrets {
    let mut value = Secrets::new();
    value.insert("TOKEN", TOKEN);
    value.insert("TREASURY_KEY", TREASURY_KEY);
    value.insert("DEPOSIT_KEY", DEPOSIT_KEY);
    value
}
fn unused_address() -> SocketAddr {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("temporary listener")
        .local_addr()
        .expect("temporary address")
}
async fn wait_ready(client: &reqwest::Client, root: &str) {
    for _ in 0..200 {
        if client
            .get(format!("{root}/health/ready"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("Payment Service did not become ready");
}
