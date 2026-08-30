use super::*;

fn bitcoin_index(node: &BitcoinNode) -> Value {
    json!({
        "network": "regtest",
        "genesis_hash": node.fixture.genesis_hash,
        "rpc": rpc_config(&node.rpc_url, true), "confirmation_depth": 1,
        "reorg_retention": 10, "poll_millis": 10, "batch_size": 10
    })
}

fn ethereum_index(node: &EthereumNode) -> Value {
    json!({
        "network": "mainnet", "chain_id": 1,
        "genesis_hash": ethereum_node::GENESIS_HASH,
        "rpc": rpc_config(&node.rpc_url, false), "confirmation_depth": 1,
        "reorg_retention": 10, "poll_millis": 10, "batch_size": 10
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configured_wallet_history_survives_restart() -> Result<(), Box<dyn std::error::Error>> {
    let bitcoin = BitcoinNode::start().await;
    let ethereum = EthereumNode::start().await;
    let database = Arc::new(TestDatabase::start().await);
    let wallets = json!([
        {"id": "btc-restart", "asset": "btc", "secret_env": "BTC_TEST_SECRET", "start_position": 1},
        {"id": "eth-restart", "asset": "eth", "secret_env": "ETH_TEST_SECRET", "start_position": 1}
    ]);
    let secrets = [
        ("BTC_TEST_SECRET", hex::encode([3_u8; 32])),
        ("ETH_TEST_SECRET", hex::encode([4_u8; 32])),
    ];
    let environment = secrets
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect::<Vec<_>>();
    let indexes = || {
        json!({
            "bitcoin": bitcoin_index(&bitcoin),
            "ethereum": ethereum_index(&ethereum)
        })
    };

    let first = start_api_with_database(
        indexes(),
        wallets.clone(),
        &environment,
        Arc::clone(&database),
    )
    .await?;
    let btc = wallet_summary(&first.root, "btc-restart").await?;
    let eth = wallet_summary(&first.root, "eth-restart").await?;
    let btc_id = bitcoin.fund(vec![FundingOutput::new(
        btc["address"].as_str().expect("Bitcoin wallet address"),
        60_000,
    )]);
    bitcoin.mine();
    let eth_id = format!("0x{}", "91".repeat(32));
    ethereum.append(vec![Transaction::native(
        0x91,
        format!("0x{}", "55".repeat(20)),
        eth["address"]
            .as_str()
            .expect("Ethereum wallet address")
            .to_owned(),
        1_000_000_000_000_000_000,
    )]);
    ethereum.append(Vec::new());
    wait_history(&first.root, "btc-restart", &btc_id).await;
    wait_history(&first.root, "eth-restart", &eth_id).await;
    first.stop().await;

    let second = start_api_with_database(indexes(), wallets, &environment, database).await?;
    assert_eq!(wallet_summary(&second.root, "btc-restart").await?, btc);
    assert_eq!(wallet_summary(&second.root, "eth-restart").await?, eth);
    wait_history(&second.root, "btc-restart", &btc_id).await;
    wait_history(&second.root, "eth-restart", &eth_id).await;

    second.stop().await;
    bitcoin.stop().await;
    ethereum.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bitcoin_and_ethereum_history_follow_canonical_reorgs()
-> Result<(), Box<dyn std::error::Error>> {
    let bitcoin = BitcoinNode::start().await;
    let ethereum = EthereumNode::start().await;
    let api = start_api(json!({
        "bitcoin": bitcoin_index(&bitcoin),
        "ethereum": ethereum_index(&ethereum)
    }))
    .await?;
    let btc = create_wallet(&api.root, "btc").await?;
    let eth = create_wallet(&api.root, "eth").await?;
    let btc_id = bitcoin.fund(vec![FundingOutput::new(
        btc["address"].as_str().expect("Bitcoin wallet address"),
        60_000,
    )]);
    bitcoin.mine();
    let eth_id = format!("0x{}", "92".repeat(32));
    ethereum.append(vec![Transaction::native(
        0x92,
        format!("0x{}", "55".repeat(20)),
        eth["address"]
            .as_str()
            .expect("Ethereum wallet address")
            .to_owned(),
        1_000_000_000_000_000_000,
    )]);
    ethereum.append(Vec::new());
    wait_history(&api.root, wallet_id(&btc), &btc_id).await;
    wait_history(&api.root, wallet_id(&eth), &eth_id).await;

    bitcoin.reorg();
    ethereum.reorg();
    wait_removed(&api.root, wallet_id(&btc), &btc_id).await;
    wait_removed(&api.root, wallet_id(&eth), &eth_id).await;

    api.stop().await;
    bitcoin.stop().await;
    ethereum.stop().await;
    Ok(())
}

async fn wait_removed(root: &str, wallet: &str, transaction: &str) {
    let url = format!("{root}/v1/wallets/{wallet}/transactions?limit=20");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if get(&url).await.is_some_and(|value| {
            value["transactions"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .all(|item| item["transaction_id"] != transaction)
            })
        }) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wallet history retained non-canonical transaction {transaction}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_chain_batch_is_rejected_before_broadcast() -> Result<(), Box<dyn std::error::Error>>
{
    let bitcoin = BitcoinNode::start().await;
    let ethereum = EthereumNode::start().await;
    let api = start_api(json!({
        "bitcoin": bitcoin_index(&bitcoin),
        "ethereum": ethereum_index(&ethereum)
    }))
    .await?;
    let btc_wallet = create_wallet(&api.root, "btc").await?;
    let eth_wallet = create_wallet(&api.root, "eth").await?;

    let response = batch_response(
        &api.root,
        &[
            (
                wallet_id(&btc_wallet),
                "bech32",
                bitcoin_destination(6),
                "0.0001",
            ),
            (
                wallet_id(&eth_wallet),
                "hex",
                format!("0x{}", "66".repeat(20)),
                "1",
            ),
        ],
    )
    .await?;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await?;
    assert_eq!(body["failed_index"], 1);
    assert!(body["transaction_ids"].as_array().is_none_or(Vec::is_empty));
    assert_eq!(bitcoin.submitted_count(), 0);
    assert!(ethereum.submitted_ids().is_empty());

    api.stop().await;
    bitcoin.stop().await;
    ethereum.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ethereum_batch_reports_the_accepted_prefix_for_an_ambiguous_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let node = EthereumNode::start().await;
    let api = start_api(json!({
        "ethereum": ethereum_index(&node)
    }))
    .await?;
    let wallet = create_wallet(&api.root, "eth").await?;
    let from = wallet["address"]
        .as_str()
        .expect("wallet address")
        .to_owned();
    let first = format!("0x{}", "77".repeat(20));
    let second = format!("0x{}", "88".repeat(20));
    node.expect_send(from.clone(), first.clone(), 1_000_000_000_000_000_000);
    node.expect_send(from, second.clone(), 2_000_000_000_000_000_000);
    node.fail_submission_after(1);

    let response = batch_response(
        &api.root,
        &[
            (wallet_id(&wallet), "hex", first, "1"),
            (wallet_id(&wallet), "hex", second, "2"),
        ],
    )
    .await?;
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await?;
    let accepted = node.submitted_ids();
    assert_eq!(accepted.len(), 1);
    assert_eq!(node.submitted_nonces(), vec![0]);
    let ambiguous = body["ambiguous_transaction_id"]
        .as_str()
        .expect("ambiguous response must preserve the local transaction ID");
    let message = body["message"]
        .as_str()
        .expect("ambiguous response must have a message");
    assert!(message.contains("outcome is ambiguous"));
    assert_eq!(
        body,
        json!({
            "message": message,
            "transaction_ids": accepted,
            "failed_index": 1,
            "ambiguous_transaction_id": ambiguous
        })
    );

    api.stop().await;
    node.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ethereum_batch_reserves_nonces_per_source_in_request_order()
-> Result<(), Box<dyn std::error::Error>> {
    let node = EthereumNode::start().await;
    let api = start_api(json!({
        "ethereum": ethereum_index(&node)
    }))
    .await?;
    let first = create_wallet(&api.root, "eth").await?;
    let second = create_wallet(&api.root, "eth").await?;
    let first_address = first["address"].as_str().expect("first address").to_owned();
    let second_address = second["address"]
        .as_str()
        .expect("second address")
        .to_owned();
    let destinations = [
        format!("0x{}", "31".repeat(20)),
        format!("0x{}", "32".repeat(20)),
        format!("0x{}", "33".repeat(20)),
    ];
    node.expect_send(first_address.clone(), destinations[0].clone(), 1);
    node.expect_send(second_address, destinations[1].clone(), 2);
    node.expect_send(first_address, destinations[2].clone(), 3);

    let ids = send_batch(
        &api.root,
        &[
            (
                wallet_id(&first),
                "hex",
                destinations[0].clone(),
                "0.000000000000000001",
            ),
            (
                wallet_id(&second),
                "hex",
                destinations[1].clone(),
                "0.000000000000000002",
            ),
            (
                wallet_id(&first),
                "hex",
                destinations[2].clone(),
                "0.000000000000000003",
            ),
        ],
    )
    .await?;

    assert_eq!(ids, node.submitted_ids());
    assert_eq!(node.submitted_nonces(), vec![0, 0, 1]);

    api.stop().await;
    node.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ethereum_batch_rejects_cumulative_overspend_before_broadcast()
-> Result<(), Box<dyn std::error::Error>> {
    let node = EthereumNode::start().await;
    let api = start_api(json!({
        "ethereum": ethereum_index(&node)
    }))
    .await?;
    let wallet = create_wallet(&api.root, "eth").await?;

    let response = batch_response(
        &api.root,
        &[
            (
                wallet_id(&wallet),
                "hex",
                format!("0x{}", "41".repeat(20)),
                "6",
            ),
            (
                wallet_id(&wallet),
                "hex",
                format!("0x{}", "42".repeat(20)),
                "6",
            ),
        ],
    )
    .await?;

    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await?;
    assert_eq!(body["failed_index"], 1);
    assert!(body["transaction_ids"].as_array().is_none_or(Vec::is_empty));
    assert!(node.submitted_ids().is_empty());

    api.stop().await;
    node.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bitcoin_batch_uses_each_source_wallet() -> Result<(), Box<dyn std::error::Error>> {
    let node = BitcoinNode::start().await;
    let api = start_api(json!({
        "bitcoin": bitcoin_index(&node)
    }))
    .await?;
    let first = create_wallet(&api.root, "btc").await?;
    let second = create_wallet(&api.root, "btc").await?;
    let first_address = first["address"].as_str().expect("first address").to_owned();
    let second_address = second["address"]
        .as_str()
        .expect("second address")
        .to_owned();
    let _funding_id = node.fund(vec![
        FundingOutput::new(&first_address, 60_000),
        FundingOutput::new(&second_address, 60_000),
    ]);
    node.mine();
    wait_balance(&api.root, wallet_id(&first), "0.0006").await;
    wait_balance(&api.root, wallet_id(&second), "0.0006").await;

    let ids = send_batch(
        &api.root,
        &[
            (
                wallet_id(&first),
                "bech32",
                bitcoin_destination(4),
                "0.0001",
            ),
            (
                wallet_id(&second),
                "bech32",
                bitcoin_destination(5),
                "0.0001",
            ),
        ],
    )
    .await?;
    assert_eq!(ids.len(), 1);
    let mut owners = node.submitted_owners();
    owners.sort();
    let mut expected = vec![first_address, second_address];
    expected.sort();
    assert_eq!(
        owners, expected,
        "every input must carry its owner's public key"
    );
    node.confirm();
    wait_history(&api.root, wallet_id(&first), &ids[0]).await;
    wait_history(&api.root, wallet_id(&second), &ids[0]).await;

    api.stop().await;
    node.stop().await;
    Ok(())
}
