#[path = "ethereum_stack.rs"]
#[allow(dead_code)]
mod ethereum;

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use deposits::{CollectionReader, DepositId, LedgerReader};
use ethereum::{EthereumStack, Transaction};
use payment_api::{
    DepositConfig, EthereumAsset, EthereumConfig, IndexerConfig, KeyConfig, Runtime, RuntimeConfig,
    Secrets, ServerConfig, WalletConfig,
};
use serde_json::{Value, json};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;
use tokio::sync::oneshot;

const AUTH: &str = "token-deposit-system-secret";
const TOKEN: &str = "0x7777777777777777777777777777777777777777";
const FUNDER: &str = "0x9999999999999999999999999999999999999999";
const GAS_SENDER: &str = "0x8888888888888888888888888888888888888888";
const MASTER_KEY: &str = "0303030303030303030303030303030303030303030303030303030303030303";
const GAS_KEY: &str = "0404040404040404040404040404040404040404040404040404040404040404";
const DEPOSIT_KEY: &str = "0505050505050505050505050505050505050505050505050505050505050505";
const TOKEN_AMOUNT: u128 = 12_500_000;
const GAS_AMOUNT: u128 = 2_000_000_000_000_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_deposit_funds_gas_then_sweeps_with_separate_accounting()
-> Result<(), Box<dyn std::error::Error>> {
    let files = TempDir::new()?;
    let payment_database = files.path().join("payments");
    let stack = EthereumStack::start(&files.path().join("indexer")).await;
    stack.receipt_gas_price(2);
    let bind = unused_address();
    let running = start_runtime(
        runtime_config(
            bind,
            payment_database.clone(),
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
        .bearer_auth(AUTH)
        .header("idempotency-key", "open-token")
        .json(&json!({
            "id": "token-deposit",
            "user_id": "merchant",
            "asset": {"chain": "ethereum", "asset": TOKEN},
            "expected": TOKEN_AMOUNT.to_string(),
            "expires_at": 9_999_999_999_u64,
            "created_at": 1_u64
        }))
        .send()
        .await?;
    assert_eq!(opened.status(), reqwest::StatusCode::OK);
    let opened: Value = opened.json().await?;
    let deposit_address = opened["address"]["value"]
        .as_str()
        .expect("deposit response must contain an address")
        .to_owned();
    stack.eth_call_result(format!("0x{TOKEN_AMOUNT:064x}"));

    let mut funding = Transaction::native(0x11, FUNDER.to_owned(), TOKEN.to_owned(), 0);
    funding.logs = vec![transfer_log(FUNDER, &deposit_address, TOKEN_AMOUNT)];
    stack.append(vec![funding]);
    stack.append(Vec::new());
    wait_balance(&client, &root, "12500000", "0").await;

    let plan = json!({
        "id": "token-collection",
        "job_id": "token-job",
        "deposit_ids": ["token-deposit"],
        "created_at": 2_u64
    });
    let first_plan = plan_collection(&client, &root, &plan).await?;
    let replay = plan_collection(&client, &root, &plan).await?;
    assert_eq!(
        first_plan, replay,
        "planning retry must replay one collection"
    );
    assert_eq!(first_plan["mode"], "token_with_gas");
    assert_eq!(first_plan["legs"][0]["kind"], "gas_funding");
    assert_eq!(first_plan["legs"][1]["kind"], "sweep");
    let master = first_plan["destination"]["value"]
        .as_str()
        .expect("plan must expose its configured destination")
        .to_owned();

    stack.expect_broadcast(GAS_SENDER.to_owned(), deposit_address.clone(), GAS_AMOUNT);
    let gas = execute(&client, &root).await?;
    assert_eq!(gas["legs"][0]["state"], "broadcast");
    assert_eq!(gas["legs"][1]["state"], "required");
    let gas_retry = execute(&client, &root).await?;
    assert_eq!(gas_retry["legs"][1]["state"], "required");
    assert_eq!(stack.broadcasts(), 1, "gas retry must not rebroadcast");
    wait_leg(&client, &root, 0, "confirmed").await;
    wait_balance(&client, &root, "12500000", "0").await;

    stack.expect_transaction(Transaction {
        hash: String::new(),
        from: deposit_address.clone(),
        to: TOKEN.to_owned(),
        value: "0x0".to_owned(),
        logs: vec![transfer_log(&deposit_address, &master, TOKEN_AMOUNT)],
    });
    let token = execute(&client, &root).await?;
    assert_eq!(token["legs"][0]["state"], "confirmed");
    assert_eq!(token["legs"][1]["state"], "broadcast");
    let token_retry = execute(&client, &root).await?;
    assert_eq!(token_retry["legs"][1]["state"], "broadcast");
    assert_eq!(stack.broadcasts(), 2, "token retry must not rebroadcast");
    wait_collection(&client, &root, "completed").await;
    wait_balance(&client, &root, "0", "12500000").await;

    let history: Value = client
        .get(format!("{root}/v1/deposits/token-deposit/history"))
        .bearer_auth(AUTH)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        history["entries"]
            .as_array()
            .is_some_and(|entries| entries.len() >= 4)
    );

    stack.reorg_last_broadcast();
    wait_collection(&client, &root, "reorged").await;
    wait_balance(&client, &root, "12500000", "0").await;

    running.stop().await?;
    let store = deposits::PaymentStore::new(RocksDb::open(&payment_database)?);
    let collection = store
        .collection(&deposits::CollectionId("token-collection".to_owned()))
        .await?
        .expect("collection must remain durable");
    let allocation = collection.legs[1]
        .allocation
        .as_ref()
        .expect("confirmed token sweep must retain its allocation");
    assert_eq!(allocation.gross_debit.to_string(), "12500000");
    assert_eq!(allocation.master_credit.to_string(), "12500000");
    assert_eq!(allocation.asset.asset, TOKEN);
    assert_eq!(allocation.allocated_fee_asset.asset, "native");
    assert_eq!(allocation.allocated_fee.to_string(), "42000");
    let ledger = store
        .current(&DepositId("token-deposit".to_owned()))
        .await?
        .expect("deposit ledger must remain durable");
    assert_eq!(ledger.balances.collected.to_string(), "0");
    assert_eq!(ledger.balances.balance.to_string(), "12500000");

    stack.stop().await;
    Ok(())
}

fn transfer_log(from: &str, to: &str, amount: u128) -> Value {
    json!({
        "address": TOKEN,
        "topics": [
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
            address_topic(from),
            address_topic(to)
        ],
        "data": format!("0x{amount:064x}")
    })
}

fn address_topic(address: &str) -> String {
    format!("0x{}{}", "00".repeat(12), address.trim_start_matches("0x"))
}

struct Running {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), payment_api::CompositionError>>,
}

impl Running {
    async fn stop(mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.stop.take().expect("runtime stop sender").send(()).ok();
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
        let _ = stopped.await;
    }));
    Ok(Running {
        stop: Some(stop),
        task,
    })
}

fn runtime_config(
    bind: SocketAddr,
    database: PathBuf,
    rpc_url: String,
    indexer_url: String,
) -> RuntimeConfig {
    let common = |id: &str, asset: EthereumAsset, secret_env: &str| {
        WalletConfig::Ethereum(EthereumConfig {
            id: id.to_owned(),
            network: "mainnet".to_owned(),
            rpc_urls: vec![rpc_url.clone()],
            rpc_headers: Vec::new(),
            chain_id: 1,
            asset,
            secret_env: secret_env.to_owned(),
            timeout_seconds: 2,
            max_response_bytes: 1024 * 1024,
            max_input_bytes: 1024,
            gas_margin_basis_points: 0,
            max_gas_limit: 1_000_000,
            max_fee_per_gas: 1_000_000_000_000,
            max_priority_fee_per_gas: 100_000_000_000,
            max_total_fee: 1_000_000_000_000_000_000,
        })
    };
    RuntimeConfig {
        bind,
        server: ServerConfig {
            bearer_token_env: "AUTH".to_owned(),
            max_request_body_bytes: 64 * 1024,
            tls_terminated_upstream: false,
        },
        database,
        indexer: IndexerConfig {
            endpoints: vec![indexer_url],
            bearer_token_env: None,
            timeout_seconds: 2,
            max_response_bytes: 1024 * 1024,
        },
        wallets: vec![
            common(
                "token-master",
                EthereumAsset::Erc20 {
                    contract: TOKEN.to_owned(),
                    decimals: 6,
                },
                "MASTER_KEY",
            ),
            common("gas-master", EthereumAsset::Native, "GAS_KEY"),
        ],
        deposits: Some(DepositConfig {
            wallet: "token-master".to_owned(),
            asset: TOKEN.to_owned(),
            gas_wallet: Some("gas-master".to_owned()),
            policy_version: "v1".to_owned(),
            policy_digest: "09".repeat(32),
            minimum_collection: "1".to_owned(),
            minimum_confirmations: 1,
            coinbase_maturity: 0,
            max_participants: 1,
            max_inputs: 1,
            gas_amount: Some(GAS_AMOUNT.to_string()),
            keys: vec![KeyConfig {
                purpose: "merchant-token".to_owned(),
                secret_env: "DEPOSIT_KEY".to_owned(),
            }],
        }),
        reconcile_seconds: 1,
        reconcile_limit: 100,
    }
}

fn secrets() -> Secrets {
    let mut secrets = Secrets::new();
    secrets.insert("AUTH", AUTH);
    secrets.insert("MASTER_KEY", MASTER_KEY);
    secrets.insert("GAS_KEY", GAS_KEY);
    secrets.insert("DEPOSIT_KEY", DEPOSIT_KEY);
    secrets
}

async fn plan_collection(
    client: &reqwest::Client,
    root: &str,
    body: &Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    for _ in 0..300 {
        let response = client
            .post(format!("{root}/v1/collections"))
            .bearer_auth(AUTH)
            .header("idempotency-key", "token-collection")
            .json(body)
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
    Err("token collection did not become plannable".into())
}

async fn execute(
    client: &reqwest::Client,
    root: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(client
        .post(format!("{root}/v1/collections/token-collection/execute"))
        .bearer_auth(AUTH)
        .header("idempotency-key", "token-collection")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn wait_ready(client: &reqwest::Client, root: &str) {
    for _ in 0..300 {
        if client
            .get(format!("{root}/health/ready"))
            .bearer_auth(AUTH)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("token Payment Service did not become ready");
}

async fn wait_balance(client: &reqwest::Client, root: &str, balance: &str, collected: &str) {
    let mut last = String::new();
    for _ in 0..500 {
        if let Ok(response) = client
            .get(format!("{root}/v1/deposits/token-deposit/balance"))
            .bearer_auth(AUTH)
            .send()
            .await
        {
            if let Ok(body) = response.json::<Value>().await {
                last = body.to_string();
                if body["entry"]["balances"]["balance"] == balance
                    && body["entry"]["balances"]["collected"] == collected
                {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("token balance did not converge: {last}");
}

async fn wait_leg(client: &reqwest::Client, root: &str, index: usize, state: &str) {
    for _ in 0..500 {
        if let Ok(response) = client
            .get(format!("{root}/v1/collections/token-collection"))
            .bearer_auth(AUTH)
            .send()
            .await
            && let Ok(body) = response.json::<Value>().await
            && body["legs"][index]["state"] == state
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("collection leg did not reach {state}");
}

async fn wait_collection(client: &reqwest::Client, root: &str, state: &str) {
    for _ in 0..500 {
        if let Ok(response) = client
            .get(format!("{root}/v1/collections/token-collection"))
            .bearer_auth(AUTH)
            .send()
            .await
            && let Ok(body) = response.json::<Value>().await
            && body["state"] == state
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("collection did not reach {state}");
}

fn unused_address() -> SocketAddr {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("temporary listener")
        .local_addr()
        .expect("temporary address")
}
