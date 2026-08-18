//! Opt-in acceptance against a disposable local Bitcoin Core regtest node.

use std::{
    env,
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use base::Decimal;
use base64::{Engine, engine::general_purpose::STANDARD};
use chain_bitcoin::{AddressType, FeeRate, Fees, IndexUtxos, WalletConfig, WalletProvider};
use chain_bitcoin::{CoreConfig, Network, RpcClient, parse_bitcoin_block_hash};
use http::client::{Config as HttpConfig, Reqwest};
use indexer_worker::{AuthenticationMode, BitcoinConfig, BitcoinService};
use indexing::{
    BlockHeight, BoxFuture, CanonicalAddress, ChainId, IndexScope, OutputQuery, WatchRequest,
    WatchSelector, Watcher,
};
use indexing_http::{Config as IndexerConfig, Remote};
use json_rpc::TransportClient;
use tempfile::TempDir;
use tokio::{process::Command, sync::oneshot, task::JoinHandle, time::sleep};
use wallets::{AddressText, HistoryRequest, SecretBytes, Wallets};

const REGTEST_GENESIS: &str = "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206";
const RPC_USER: &str = "payment-sdk-acceptance";
const RPC_PASSWORD: &str = "disposable-regtest-only";
const WALLET_SECRET: [u8; 32] = [7; 32];

struct Core {
    child: tokio::process::Child,
    data: TempDir,
    cli: PathBuf,
    rpc_port: u16,
}

impl Core {
    async fn start(bitcoind: &Path, cli: PathBuf) -> Self {
        require_core_31(bitcoind).await;
        let data = tempfile::tempdir().expect("a disposable Core datadir must be created");
        let rpc_port = available_port();
        let mut child = Command::new(bitcoind)
            .args([
                "-regtest",
                "-server=1",
                "-txindex=1",
                "-listen=0",
                "-discover=0",
                "-dnsseed=0",
                "-fixedseeds=0",
                "-rpcbind=127.0.0.1",
                "-rpcallowip=127.0.0.1",
                &format!("-rpcport={rpc_port}"),
                &format!("-rpcuser={RPC_USER}"),
                &format!("-rpcpassword={RPC_PASSWORD}"),
                &format!("-datadir={}", data.path().display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("BITCOIND must point to an executable Bitcoin Core binary");

        let arguments = cli_arguments(rpc_port);
        for _ in 0..100 {
            if child
                .try_wait()
                .expect("Core process status must remain readable")
                .is_some()
            {
                panic!("disposable Bitcoin Core exited before RPC became ready");
            }
            if Command::new(&cli)
                .args(&arguments)
                .arg("getblockchaininfo")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|status| status.success())
            {
                return Self {
                    child,
                    data,
                    cli,
                    rpc_port,
                };
            }
            sleep(Duration::from_millis(100)).await;
        }
        panic!("disposable Bitcoin Core RPC did not become ready within ten seconds");
    }

    async fn call(&self, method: &str, arguments: &[&str]) -> String {
        let output = Command::new(&self.cli)
            .args(cli_arguments(self.rpc_port))
            .arg(method)
            .args(arguments)
            .output()
            .await
            .expect("bitcoin-cli must remain executable");
        assert!(
            output.status.success(),
            "local bitcoin-cli {method} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("bitcoin-cli output must be UTF-8")
            .trim()
            .to_owned()
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        // Keep the temporary directory owned until after the process handle is
        // asked to stop. TempDir then removes all disposable chain material.
        let _ = self.data.path();
    }
}

struct RunningIndexer {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), indexer_worker::ServiceError>>,
}

impl RunningIndexer {
    async fn stop(mut self) {
        self.shutdown
            .take()
            .expect("indexer shutdown sender must exist")
            .send(())
            .ok();
        self.task
            .await
            .expect("indexer task must not panic")
            .expect("indexer must stop cleanly");
    }
}

enum FixedFees {
    Regtest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WalletKey {
    Hot,
}

impl Fees for FixedFees {
    fn estimate<'a>(
        &'a self,
        _target_blocks: u16,
    ) -> BoxFuture<'a, Result<FeeRate, indexing::SourceError>> {
        Box::pin(async { Ok(FeeRate::new(1_000)) })
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires explicit local Bitcoin Core 31 binaries; run via tests/system/run-bitcoin-core-acceptance.sh"]
async fn real_wallet_broadcast_is_indexed_on_disposable_regtest() {
    let bitcoind = required_binary("BITCOIND");
    let cli = required_binary("BITCOIN_CLI");
    let core = Core::start(&bitcoind, cli).await;

    core.call("createwallet", &["acceptance"]).await;
    let mining_address = core.call("getnewaddress", &["", "bech32"]).await;
    core.call("generatetoaddress", &["101", &mining_address])
        .await;
    core.call("syncwithvalidationinterfacequeue", &[]).await;

    let endpoint = format!("http://127.0.0.1:{}", core.rpc_port);
    let authorization = format!(
        "Basic {}",
        STANDARD.encode(format!("{RPC_USER}:{RPC_PASSWORD}"))
    );
    let rpc = rpc_transport(&endpoint, &authorization);
    let client = RpcClient::connect(
        rpc,
        CoreConfig {
            expected_network: Network::Regtest,
            expected_genesis_hash: parse_bitcoin_block_hash(REGTEST_GENESIS)
                .expect("the fixed regtest genesis hash must parse"),
        },
    )
    .await
    .expect("the real chain RPC wrapper must accept the isolated regtest node");

    let status = client.node().status().await.expect("node status must load");
    assert_eq!(status.network, Network::Regtest);
    assert_eq!(status.height, BlockHeight(101));
    assert_eq!(
        client
            .node()
            .canonical_hash(BlockHeight(0))
            .await
            .expect("canonical hash lookup must succeed"),
        Some(parse_bitcoin_block_hash(REGTEST_GENESIS).expect("genesis hash must parse"))
    );

    let files = tempfile::tempdir().expect("indexer storage must be temporary");
    let indexer_address = available_socket();
    let indexer = start_indexer(
        files.path().join("indexer"),
        indexer_address,
        &endpoint,
        &authorization,
    )
    .await;
    let indexer_endpoint = format!("http://{indexer_address}");
    wait_ready(&format!("{indexer_endpoint}/health/ready")).await;
    let remote = Arc::new(
        Remote::connect(IndexerConfig::new(&indexer_endpoint))
            .expect("the loopback indexer client must build"),
    );
    let scope = IndexScope {
        chain: ChainId("bitcoin".to_owned()),
        network: "regtest".to_owned(),
    };
    let outputs: Arc<dyn OutputQuery> = remote.clone();
    let utxos = Arc::new(
        IndexUtxos::new(scope.clone(), Network::Regtest, outputs)
            .expect("indexed UTXO adapter must build"),
    );
    let provider = WalletProvider::new(
        WalletConfig {
            scope: scope.clone(),
            network: Network::Regtest,
            address_type: AddressType::SegwitV0,
            fee_target_blocks: 2,
            max_fee_rate: FeeRate::new(10_000),
        },
        utxos,
        Arc::new(FixedFees::Regtest),
        Arc::new(client.transactions()),
        remote.clone(),
    );
    let mut wallets = Wallets::new();
    wallets
        .register(WalletKey::Hot, provider)
        .expect("wallet registration must be unique");
    let wallet = wallets
        .new_wallet(&WalletKey::Hot, SecretBytes::new(WALLET_SECRET))
        .await
        .expect("the concrete Bitcoin wallet must be created");
    let wallet_address = wallet
        .address_text(&wallet.address())
        .expect("wallet address must encode")
        .text;
    remote
        .watch(WatchRequest {
            scope: scope.clone(),
            selector: WatchSelector::Address(CanonicalAddress {
                scope: scope.clone(),
                value: wallet_address.clone(),
            }),
            start_height: BlockHeight(102),
            idempotency_key: "live-regtest-wallet".to_owned(),
        })
        .await
        .expect("wallet watch must be durable before funding");

    core.call("sendtoaddress", &[&wallet_address, "0.01"]).await;
    core.call("generatetoaddress", &["1", &mining_address])
        .await;
    core.call("syncwithvalidationinterfacequeue", &[]).await;
    wait_balance(wallet.as_ref(), "0.01").await;

    let destination = core.call("getnewaddress", &["", "bech32"]).await;
    let destination = wallet
        .parse_address(&AddressText::new(
            wallets::AddressEncoding::Bech32,
            destination,
        ))
        .expect("Core destination must be a valid regtest address");
    let mut builder = wallet.transaction();
    builder
        .transfer(
            destination,
            "0.005".parse::<Decimal>().expect("amount must parse"),
        )
        .expect("transfer must configure");
    let signed = builder
        .prepare()
        .await
        .expect("wallet must build and sign the indexed UTXO transaction");
    let submitted = wallet
        .broadcaster()
        .broadcast(&signed)
        .await
        .expect("the concrete RPC client must broadcast the signed transaction");
    let mempool = core.call("getrawmempool", &[]).await;
    assert!(mempool.contains(submitted.id.as_str()));

    core.call("generatetoaddress", &["1", &mining_address])
        .await;
    core.call("syncwithvalidationinterfacequeue", &[]).await;
    wait_history(wallet.as_ref(), submitted.id.as_str()).await;
    let balance = wallet.balance().await.expect("indexed balance must load");
    assert!(balance.amount < "0.005".parse::<Decimal>().expect("amount must parse"));

    indexer.stop().await;
}

fn rpc_transport(endpoint: &str, authorization: &str) -> TransportClient<Reqwest> {
    let transport = Reqwest::new(HttpConfig::new(endpoint, Duration::from_secs(3)))
        .expect("loopback RPC transport must build");
    TransportClient::new(transport, endpoint).with_header("authorization", authorization)
}

async fn start_indexer(
    database: PathBuf,
    api: SocketAddr,
    rpc: &str,
    authorization: &str,
) -> RunningIndexer {
    let mut config = BitcoinConfig::new(
        database,
        Network::Regtest,
        101,
        1,
        10,
        REGTEST_GENESIS,
        rpc,
        AuthenticationMode::GlobalTrusted,
    );
    config.rpc_headers = vec![format!("authorization={authorization}")];
    config.http_bind = api;
    config.poll_seconds = 1;
    let service = BitcoinService::new(config).expect("indexer configuration must validate");
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async move {
        let _ignored = receiver.await;
    }));
    RunningIndexer {
        shutdown: Some(shutdown),
        task,
    }
}

async fn wait_ready(url: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if reqwest::get(url)
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "indexer did not become ready"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_balance(wallet: &dyn wallets::Wallet, expected: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if wallet
            .balance()
            .await
            .is_ok_and(|balance| balance.amount.to_string() == expected)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "funding balance was not indexed"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_history(wallet: &dyn wallets::Wallet, transaction_id: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if wallet
            .history(HistoryRequest::first(20))
            .await
            .is_ok_and(|history| {
                history.transactions.iter().any(|transaction| {
                    transaction.transaction_id == transaction_id
                        && matches!(transaction.status, wallets::HistoryStatus::Confirmed { .. })
                })
            })
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "broadcast was not indexed as confirmed"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

async fn require_core_31(bitcoind: &Path) {
    let output = Command::new(bitcoind)
        .arg("--version")
        .output()
        .await
        .expect("BITCOIND must point to an executable Bitcoin Core binary");
    let version = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && version.contains("v31."),
        "live acceptance requires an explicit Bitcoin Core 31.x binary"
    );
}

fn required_binary(name: &str) -> PathBuf {
    let path = PathBuf::from(env::var_os(name).unwrap_or_else(|| {
        panic!("{name} must name an explicit local executable; no binary is downloaded")
    }));
    assert!(path.is_absolute(), "{name} must be an absolute path");
    assert!(path.is_file(), "{name} must identify a regular file");
    path
}

fn available_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("a loopback RPC port must be available")
        .local_addr()
        .expect("the loopback listener must have an address")
        .port()
}

fn available_socket() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, available_port()))
}

fn cli_arguments(rpc_port: u16) -> Vec<String> {
    vec![
        "-regtest".to_owned(),
        "-rpcconnect=127.0.0.1".to_owned(),
        format!("-rpcport={rpc_port}"),
        format!("-rpcuser={RPC_USER}"),
        format!("-rpcpassword={RPC_PASSWORD}"),
    ]
}
