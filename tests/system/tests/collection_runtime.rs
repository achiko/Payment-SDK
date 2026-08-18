mod bitcoin_stack;

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use bitcoin::{CompressedPublicKey, Network, PrivateKey, PublicKey, consensus::deserialize};
use bitcoin_stack::{BitcoinStack, FundingOutput};
use payment_api::{
    BitcoinConfig, DepositConfig, IndexerConfig, KeyConfig, Runtime, RuntimeConfig, Secrets,
    ServerConfig, WalletConfig,
};
use tempfile::TempDir;
use tokio::sync::oneshot;

const TOKEN: &str = "concrete-deposit-test-token";
const TREASURY_SECRET: &str = "0404040404040404040404040404040404040404040404040404040404040404";
const FIRST_SECRET: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const SECOND_SECRET: &str = "0202020202020202020202020202020202020202020202020202020202020202";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concrete_runtime_observes_and_sweeps_two_bitcoin_deposits()
-> Result<(), Box<dyn std::error::Error>> {
    let root = TempDir::new()?;
    let payment_database = root.path().join("payments");
    let index_database = root.path().join("index");
    let first_address = address(FIRST_SECRET);
    let second_address = address(SECOND_SECRET);
    let treasury_address = address(TREASURY_SECRET);
    let stack = BitcoinStack::start(
        &index_database,
        vec![
            FundingOutput::new(&first_address, 100_000),
            FundingOutput::new(&second_address, 120_000),
        ],
    )
    .await;
    let bind = unused_address();
    let config = runtime_config(
        bind,
        payment_database.clone(),
        stack.rpc_url.clone(),
        stack.indexer_url.clone(),
        stack.fixture.genesis_hash.clone(),
    );
    let first = start_runtime(config.clone(), secrets()).await?;
    let client = reqwest::Client::new();
    let root_url = format!("http://{bind}");
    wait_ready(&client, &root_url).await;

    for (position, expected) in ["100000", "120000"].into_iter().enumerate() {
        let response = client
            .post(format!("{root_url}/v1/deposits"))
            .bearer_auth(TOKEN)
            .header("idempotency-key", format!("open-{position}"))
            .json(&serde_json::json!({
                "id": format!("deposit-{position}"),
                "user_id": format!("user-{position}"),
                "asset": {"chain": "bitcoin", "asset": "native"},
                "expected": expected,
                "expires_at": 9_999_999_999_u64,
                "created_at": 1_u64
            }))
            .send()
            .await?;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK,
            "{}",
            response.text().await?
        );
    }
    stack.mine();
    wait_balance(&client, &root_url, "deposit-0", "100000").await;
    wait_balance(&client, &root_url, "deposit-1", "120000").await;
    let history: serde_json::Value = client
        .get(format!("{root_url}/v1/deposits/deposit-0/history"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        history["entries"]
            .as_array()
            .is_some_and(|entries| entries.len() >= 2)
    );
    let plan = serde_json::json!({
        "id": "concrete-batch",
        "job_id": "concrete-job",
        "deposit_ids": ["deposit-0", "deposit-1"],
        "created_at": 2_u64
    });
    for _ in 0..2 {
        let text = plan_collection(&client, &root_url, &plan).await?;
        let planned: serde_json::Value = serde_json::from_str(&text)?;
        assert_eq!(planned["id"], "concrete-batch");
        assert_eq!(planned["mode"], "utxo_batch");
        assert_eq!(planned["legs"][0]["state"], "required");
        assert!(text.find("evidence").is_none());
    }
    let missing_key = client
        .post(format!("{root_url}/v1/collections"))
        .bearer_auth(TOKEN)
        .json(&plan)
        .send()
        .await?;
    assert_eq!(missing_key.status(), reqwest::StatusCode::BAD_REQUEST);
    let changed = client
        .post(format!("{root_url}/v1/collections"))
        .bearer_auth(TOKEN)
        .header("idempotency-key", "concrete-batch")
        .json(&serde_json::json!({
            "id": "concrete-batch",
            "job_id": "changed-job",
            "deposit_ids": ["deposit-0", "deposit-1"],
            "created_at": 2_u64
        }))
        .send()
        .await?;
    assert_eq!(changed.status(), reqwest::StatusCode::CONFLICT);
    first.stop().await?;

    let second = start_runtime(config, secrets()).await?;
    wait_ready(&client, &root_url).await;
    let response = client
        .post(format!("{root_url}/v1/collections/concrete-batch/execute"))
        .bearer_auth(TOKEN)
        .header("idempotency-key", "concrete-batch")
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    assert!(
        status.is_success(),
        "collection failed with {status}: {text}"
    );
    let response: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(response["mode"], "utxo_batch");
    assert_eq!(response["legs"][0]["state"], "broadcast");
    let broadcasts = stack.broadcasts();
    assert_eq!(broadcasts.len(), 1);
    let transaction: bitcoin::Transaction = deserialize(&broadcasts[0])?;
    assert_eq!(transaction.input.len(), 2);
    assert_eq!(transaction.output.len(), 1);
    assert_eq!(
        transaction.output[0].script_pubkey,
        treasury_address
            .parse::<bitcoin::Address<_>>()?
            .require_network(Network::Regtest)?
            .script_pubkey()
    );

    let master_credit = transaction.output[0].value.to_sat();
    let sweep_id = transaction.compute_txid().to_string();
    let network_fee = 220_000_u64
        .checked_sub(master_credit)
        .expect("sweep output cannot exceed its inputs");
    let fee_shares = fee_shares(network_fee);

    stack.confirm();
    let confirmed = wait_collection(&client, &root_url, "completed", "confirmed").await;
    assert_eq!(confirmed["legs"][0]["transaction_id"], sweep_id);
    assert_allocations(&confirmed, &fee_shares, master_credit);
    wait_ledger(&client, &root_url, "deposit-0", "100000", "0", "100000").await;
    wait_ledger(&client, &root_url, "deposit-1", "120000", "0", "120000").await;

    stack.reorg();
    let reorged = wait_collection(&client, &root_url, "reorged", "reorged").await;
    assert_eq!(reorged["legs"][0]["transaction_id"], sweep_id);
    assert_allocations(&reorged, &fee_shares, master_credit);
    wait_ledger(&client, &root_url, "deposit-0", "100000", "100000", "0").await;
    wait_ledger(&client, &root_url, "deposit-1", "120000", "120000", "0").await;
    assert_eq!(stack.broadcasts().len(), 1);

    stack.reinclude();
    let reconfirmed = wait_collection(&client, &root_url, "completed", "confirmed").await;
    assert_eq!(reconfirmed["legs"][0]["transaction_id"], sweep_id);
    assert_allocations(&reconfirmed, &fee_shares, master_credit);
    wait_ledger(&client, &root_url, "deposit-0", "100000", "0", "100000").await;
    wait_ledger(&client, &root_url, "deposit-1", "120000", "0", "120000").await;
    assert_eq!(stack.broadcasts().len(), 1);

    second.stop().await?;
    stack.stop().await;
    Ok(())
}

fn fee_shares(fee: u64) -> [u64; 2] {
    let first_numerator = u128::from(fee) * 100_000;
    let second_numerator = u128::from(fee) * 120_000;
    let mut shares = [
        u64::try_from(first_numerator / 220_000).expect("first fee share"),
        u64::try_from(second_numerator / 220_000).expect("second fee share"),
    ];
    let remainder = fee - shares[0] - shares[1];
    if remainder == 1 {
        let first_remainder = first_numerator % 220_000;
        let second_remainder = second_numerator % 220_000;
        let position = usize::from(second_remainder > first_remainder);
        shares[position] += 1;
    }
    shares
}

fn assert_allocations(collection: &serde_json::Value, shares: &[u64; 2], master: u64) {
    let allocations = collection["legs"][0]["allocations"]
        .as_array()
        .expect("collection allocations");
    assert_eq!(allocations.len(), 2);
    for (position, gross) in [100_000_u64, 120_000].into_iter().enumerate() {
        assert_eq!(
            allocations[position]["deposit_id"],
            format!("deposit-{position}")
        );
        assert_eq!(allocations[position]["gross_debit"], gross.to_string());
        assert_eq!(
            allocations[position]["allocated_fee"],
            shares[position].to_string()
        );
        assert_eq!(
            allocations[position]["master_credit"],
            (gross - shares[position]).to_string()
        );
    }
    let credited = allocations
        .iter()
        .map(|allocation| {
            allocation["master_credit"]
                .as_str()
                .expect("master credit")
                .parse::<u64>()
                .expect("numeric master credit")
        })
        .sum::<u64>();
    assert_eq!(credited, master);
}

async fn wait_collection(
    client: &reqwest::Client,
    root: &str,
    state: &str,
    leg: &str,
) -> serde_json::Value {
    let mut last = String::new();
    for _ in 0..500 {
        if let Ok(response) = client
            .get(format!("{root}/v1/collections/concrete-batch"))
            .bearer_auth(TOKEN)
            .send()
            .await
        {
            let status = response.status();
            if let Ok(text) = response.text().await {
                last = format!("{status}: {text}");
                if let Ok(body) = serde_json::from_str::<serde_json::Value>(&text)
                    && body["state"] == state
                    && body["legs"][0]["state"] == leg
                {
                    return body;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("collection did not reach {state}/{leg}; last response: {last}");
}

async fn wait_ledger(
    client: &reqwest::Client,
    root: &str,
    id: &str,
    received: &str,
    balance: &str,
    collected: &str,
) {
    let mut last = String::new();
    for _ in 0..500 {
        if let Ok(response) = client
            .get(format!("{root}/v1/deposits/{id}/balance"))
            .bearer_auth(TOKEN)
            .send()
            .await
        {
            let status = response.status();
            if let Ok(text) = response.text().await {
                last = format!("{status}: {text}");
                if serde_json::from_str::<serde_json::Value>(&text).is_ok_and(|body| {
                    body["entry"]["balances"]["received"] == received
                        && body["entry"]["balances"]["confirmed"] == received
                        && body["entry"]["balances"]["balance"] == balance
                        && body["entry"]["balances"]["collected"] == collected
                        && body["entry"]["balances"]["accounted"] == "0"
                }) {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("deposit {id} ledger did not reach balance={balance}, collected={collected}; {last}");
}

async fn plan_collection(
    client: &reqwest::Client,
    root: &str,
    body: &serde_json::Value,
) -> Result<String, Box<dyn std::error::Error>> {
    for _ in 0..200 {
        let response = client
            .post(format!("{root}/v1/collections"))
            .bearer_auth(TOKEN)
            .header("idempotency-key", "concrete-batch")
            .json(body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if status.is_success() {
            return Ok(text);
        }
        if status != reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Err(format!("planning failed with {status}: {text}").into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err("Indexer did not become ready for collection planning".into())
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
    genesis_hash: String,
) -> RuntimeConfig {
    RuntimeConfig {
        bind,
        server: ServerConfig {
            bearer_token_env: "TEST_TOKEN".to_owned(),
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
        wallets: vec![WalletConfig::Bitcoin(BitcoinConfig {
            id: "treasury".to_owned(),
            network: "regtest".to_owned(),
            rpc_urls: vec![rpc_url],
            rpc_headers: Vec::new(),
            genesis_hash,
            secret_env: "TREASURY_KEY".to_owned(),
            taproot: false,
            fee_target_blocks: 2,
            max_fee_rate: 10_000,
            timeout_seconds: 2,
            max_response_bytes: 1024 * 1024,
        })],
        deposits: Some(DepositConfig {
            wallet: "treasury".to_owned(),
            gas_wallet: None,
            asset: "native".to_owned(),
            policy_version: "v1".to_owned(),
            policy_digest: "09".repeat(32),
            minimum_collection: "1".to_owned(),
            minimum_confirmations: 1,
            coinbase_maturity: 100,
            max_participants: 100,
            max_inputs: 1_000,
            gas_amount: None,
            keys: vec![
                KeyConfig {
                    purpose: "merchant-a".to_owned(),
                    secret_env: "FIRST_KEY".to_owned(),
                },
                KeyConfig {
                    purpose: "merchant-b".to_owned(),
                    secret_env: "SECOND_KEY".to_owned(),
                },
            ],
        }),
        reconcile_seconds: 1,
        reconcile_limit: 100,
    }
}

fn secrets() -> Secrets {
    let mut secrets = Secrets::new();
    secrets.insert("TEST_TOKEN", TOKEN);
    secrets.insert("TREASURY_KEY", TREASURY_SECRET);
    secrets.insert("FIRST_KEY", FIRST_SECRET);
    secrets.insert("SECOND_KEY", SECOND_SECRET);
    secrets
}

fn address(secret: &str) -> String {
    let private = PrivateKey::from_slice(&hex::decode(secret).expect("hex key"), Network::Regtest)
        .expect("private key");
    let public = PublicKey::from_private_key(&bitcoin::secp256k1::Secp256k1::new(), &private);
    bitcoin::Address::p2wpkh(
        &CompressedPublicKey::try_from(public).expect("compressed key"),
        Network::Regtest,
    )
    .to_string()
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
    panic!("concrete Payment Service did not become ready");
}

async fn wait_balance(client: &reqwest::Client, root: &str, id: &str, expected: &str) {
    let mut last = String::new();
    for _ in 0..300 {
        if let Ok(response) = client
            .get(format!("{root}/v1/deposits/{id}/balance"))
            .bearer_auth(TOKEN)
            .send()
            .await
        {
            let status = response.status();
            if let Ok(text) = response.text().await {
                last = format!("{status}: {text}");
                if serde_json::from_str::<serde_json::Value>(&text)
                    .is_ok_and(|body| body["entry"]["balances"]["confirmed"] == expected)
                {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("concrete indexed deposit balance did not appear; last response: {last}");
}

fn unused_address() -> SocketAddr {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("bind temporary listener")
        .local_addr()
        .expect("temporary address")
}
