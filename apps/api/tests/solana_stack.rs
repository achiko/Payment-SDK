//! Explicit manual system target for the owned Agave and PostgreSQL stack.

use std::{
    fs::{self, File},
    future::Future,
    io,
    net::TcpListener,
    path::{Path, PathBuf},
    pin::Pin,
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use base::{BlockPosition, Decimal};
use chain_solana::{NativeAsset, RpcClient, RpcConfig};
use indexing::{
    BlockObservation, BlockRef, BlockSelector, Blocks as _, Checkpoint as _, Indexer as _,
    Observer, OutputRequest, Outputs, SyncConfig,
};
use sha2::{Digest, Sha256};
use tokio::{sync::watch, task::JoinSet, time::sleep};
use wallets::{HistoryRequest, SecretBytes, Wallets};

#[allow(dead_code)] // The shared fixture exposes contracts used by its owning test crate.
#[path = "../../../sdk/indexing/postgres/tests/support/mod.rs"]
mod postgres;

use postgres::TestDatabase;

const RELEASE: &str = include_str!("fixtures/agave-v3.1.14.sha256");
const VERSION: &str = "v3.1.14";
const COMMIT: &str = "3134055b562e95902233be308453fffa1c4a8902";

#[derive(Debug, PartialEq, Eq)]
struct Artifact<'a> {
    target: &'a str,
    name: &'a str,
    sha256: &'a str,
}

fn artifacts() -> Result<Vec<Artifact<'static>>, io::Error> {
    let mut lines = RELEASE.lines();
    if lines.next() != Some(&format!("version {VERSION}"))
        || lines.next() != Some(&format!("commit {COMMIT}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Agave release identity is invalid",
        ));
    }
    lines
        .map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let artifact = Artifact {
                target: fields.next().ok_or_else(invalid_manifest)?,
                name: fields.next().ok_or_else(invalid_manifest)?,
                sha256: fields.next().ok_or_else(invalid_manifest)?,
            };
            if fields.next().is_some()
                || artifact.sha256.len() != 64
                || !artifact
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(invalid_manifest());
            }
            Ok(artifact)
        })
        .collect()
}

fn invalid_manifest() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid Agave checksum manifest",
    )
}

fn artifact_for<'a>(
    artifacts: &'a [Artifact<'a>],
    target: &str,
) -> Result<&'a Artifact<'a>, io::Error> {
    artifacts
        .iter()
        .find(|artifact| artifact.target == target)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "unsupported Agave platform"))
}

fn verify(path: &Path, expected: &str) -> Result<(), io::Error> {
    let actual = format!("{:x}", Sha256::digest(fs::read(path)?));
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Agave artifact checksum mismatch",
        ))
    }
}

fn archive(artifact: &Artifact<'_>) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/solana-stack")
        .join(VERSION)
        .join(artifact.name)
}

fn extract(archive: &Path, destination: &Path) -> Result<PathBuf, io::Error> {
    let output = Command::new("tar")
        .args(["-xjf"])
        .arg(archive)
        .args(["-C"])
        .arg(destination)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "could not extract reviewed Agave artifact",
        ));
    }
    executable(destination, "solana-test-validator").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "reviewed artifact does not contain solana-test-validator",
        )
    })
}

fn executable(directory: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(directory).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = executable(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|value| value.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

fn host_target() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else {
        "unsupported"
    }
}

#[test]
fn pins_the_reviewed_release_and_platform_artifacts() {
    let artifacts = artifacts().expect("reviewed checksum manifest");
    assert_eq!(artifacts.len(), 3);
    assert!(artifact_for(&artifacts, host_target()).is_ok());
}

#[test]
fn rejects_unsupported_platform_and_corrupt_artifact() {
    let artifacts = artifacts().expect("reviewed checksum manifest");
    assert_eq!(
        artifact_for(&artifacts, "mips64-unknown-none")
            .expect_err("unsupported platform")
            .kind(),
        io::ErrorKind::Unsupported
    );

    let directory = tempfile::tempdir().expect("temporary artifact directory");
    let path = directory.path().join("corrupt.tar.bz2");
    fs::write(&path, b"not the reviewed artifact").expect("corrupt fixture");
    assert_eq!(
        verify(&path, artifacts[0].sha256)
            .expect_err("checksum mismatch")
            .kind(),
        io::ErrorKind::InvalidData
    );
}

struct Ports {
    _reservations: Vec<TcpListener>,
    rpc: u16,
    faucet: u16,
    gossip: u16,
    dynamic: (u16, u16),
}

impl Ports {
    fn reserve() -> Result<Self, io::Error> {
        let seed = 20_000 + (std::process::id() % 1_000) as u16 * 32;
        for offset in 0..1_000_u16 {
            let base = seed.saturating_add(offset.saturating_mul(32));
            let candidates = (base..base.saturating_add(25)).collect::<Vec<_>>();
            let reservations = candidates
                .iter()
                .map(|port| TcpListener::bind(("127.0.0.1", *port)))
                .collect::<Result<Vec<_>, _>>();
            if let Ok(reservations) = reservations {
                return Ok(Self {
                    _reservations: reservations,
                    rpc: base,
                    faucet: base + 2,
                    gossip: base + 3,
                    dynamic: (base + 4, base + 24),
                });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "could not reserve isolated validator ports",
        ))
    }
}

struct Validator {
    child: Child,
    _directory: tempfile::TempDir,
    endpoint: String,
}

impl Validator {
    fn start(binary: &Path) -> Result<Self, io::Error> {
        let directory = tempfile::tempdir()?;
        let ledger = directory.path().join("ledger");
        let log = File::create(directory.path().join("validator.log"))?;
        let ports = Ports::reserve()?;
        let endpoint = format!("http://127.0.0.1:{}", ports.rpc);
        let dynamic = format!("{}-{}", ports.dynamic.0, ports.dynamic.1);
        let mut command = Command::new(binary);
        command
            .args(["--ledger"])
            .arg(&ledger)
            .args(["--bind-address", "127.0.0.1", "--rpc-port"])
            .arg(ports.rpc.to_string())
            .args(["--faucet-port"])
            .arg(ports.faucet.to_string())
            .args(["--gossip-port"])
            .arg(ports.gossip.to_string())
            .args(["--dynamic-port-range", &dynamic, "--reset", "--quiet"])
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        drop(ports);
        Ok(Self {
            child: command.spawn()?,
            _directory: directory,
            endpoint,
        })
    }
}

impl Drop for Validator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Notice(watch::Sender<Option<BlockRef>>);

impl Observer for Notice {
    fn observed<'a>(&'a self, observation: BlockObservation) -> indexing::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.0.send_replace(Some(observation.block));
        })
    }
}

#[derive(Default)]
struct Tasks(Mutex<JoinSet<()>>);

impl chain_solana::SubmissionRegistrar for Tasks {
    fn register<'a>(
        &'a self,
        task: chain_solana::SubmissionTask,
    ) -> Pin<Box<dyn Future<Output = Result<(), chain_solana::RegistrationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .spawn(task.run());
            Ok(())
        })
    }
}

impl Tasks {
    async fn drain(&self) {
        let mut tasks = {
            let mut owned = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *owned)
        };
        while let Some(result) = tasks.join_next().await {
            result.expect("owned submission task");
        }
    }
}

async fn ready(endpoint: &str) -> RpcClient<json_rpc::Http> {
    let config = RpcConfig::new(
        endpoint,
        Duration::from_secs(2),
        1024 * 1024,
        64 * 1024 * 1024,
    )
    .expect("owned RPC configuration");
    for _ in 0..300 {
        let client = RpcClient::connect(config.clone()).expect("owned RPC client");
        if client.health().await.is_ok() {
            return client;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("owned validator did not become ready")
}

async fn airdrop(endpoint: &str, address: &str, lamports: u64) {
    let response = reqwest::Client::new()
        .post(endpoint)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "requestAirdrop",
            "params": [address, lamports, {"commitment": "finalized"}]
        }))
        .send()
        .await
        .expect("owned validator airdrop response");
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.expect("airdrop JSON");
    assert!(
        body.get("result")
            .and_then(serde_json::Value::as_str)
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn native_sol_submission_indexing_and_central_storage() {
    let artifacts = artifacts().expect("reviewed checksum manifest");
    let artifact = artifact_for(&artifacts, host_target()).expect("supported platform");
    let archive = archive(artifact);
    verify(&archive, artifact.sha256).expect("exact reviewed validator artifact");
    let extracted = tempfile::tempdir().expect("isolated extraction");
    let binary = extract(&archive, extracted.path()).expect("reviewed validator binary");
    let version = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("validator version");
    assert!(String::from_utf8_lossy(&version.stdout).contains("3.1.14"));

    let validator = Validator::start(&binary).expect("owned validator");
    let client = ready(&validator.endpoint).await;
    let genesis = client.genesis_hash().await.expect("owned genesis");
    client
        .verify_genesis(&genesis)
        .await
        .expect("exact genesis");
    client
        .verify_memo()
        .await
        .expect("bundled executable Memo-v3");

    let database = TestDatabase::start().await;
    assert!(database.registry_sentinel_unchanged().await);
    let pool = database.pool_for_schema(database.schema());
    indexing_postgres::validate_schema(&pool, database.schema())
        .await
        .expect("read-only startup schema validation");
    let asset = NativeAsset::new("solana-stack").expect("owned scope");
    let scope = asset.scope().clone();
    let repository =
        indexing_postgres::Repository::new(pool.clone(), scope.clone()).expect("Solana repository");
    let (progress, progress_rx) = watch::channel(None);
    let mut service = indexing::Service::new(
        chain_solana::Source::new(client.clone()),
        chain_solana::BlockInterpreter::new(scope.clone()).expect("Solana interpreter"),
        repository.clone(),
        SyncConfig::new(scope.clone(), 1, 32, 100).expect("synchronization policy"),
    );
    service.observe(Arc::new(Notice(progress)));
    let service = Arc::new(service);
    let tasks = Arc::new(Tasks::default());
    let coordinator = Arc::new(chain_solana::Coordinator::new(
        client.clone(),
        tasks.clone(),
        service.clone(),
        service.clone(),
        scope.clone(),
        progress_rx,
    ));
    let provider =
        chain_solana::WalletProvider::new(asset, client.clone(), service.clone(), coordinator);
    let sender = provider.transactions();
    let mut wallets = Wallets::<String, payment_api::WalletAsset>::new(service.clone());
    wallets
        .register(
            payment_api::WalletAsset::Sol,
            scope.clone(),
            provider,
            sender,
            None,
        )
        .expect("native SOL family");
    let source = wallets
        .import(
            "source".to_owned(),
            &payment_api::WalletAsset::Sol,
            SecretBytes::new(vec![7; 32]),
            BlockPosition(0),
        )
        .await
        .expect("source import");
    let destination = wallets
        .import(
            "destination".to_owned(),
            &payment_api::WalletAsset::Sol,
            SecretBytes::new(vec![9; 32]),
            BlockPosition(0),
        )
        .await
        .expect("destination import");

    airdrop(&validator.endpoint, &source.address.text, 2_000_000).await;
    airdrop(&validator.endpoint, &destination.address.text, 1_000).await;
    let amount = "0.00001".parse::<Decimal>().expect("exact SOL amount");
    let transaction = wallets
        .send(
            &"source".to_owned(),
            destination.address.clone(),
            amount.clone(),
        )
        .await
        .expect("native SOL submission");
    tasks.drain().await;

    let mut observed = None;
    for _ in 0..100 {
        service
            .sync(&wallets)
            .await
            .expect("sparse synchronization");
        let history = wallets
            .history(&"source".to_owned(), HistoryRequest::first(100))
            .await
            .expect("canonical SOL history");
        observed = history
            .transactions
            .into_iter()
            .find(|entry| entry.transaction_id.value == transaction.as_str());
        if observed.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let observed = observed.expect("submitted transaction became canonical");
    assert_eq!(observed.movements.len(), 1);
    assert_eq!(observed.movements[0].amount, amount);
    assert_eq!(
        observed.fee.expect("exact network fee").amount,
        "0.000005".parse::<Decimal>().expect("exact SOL fee")
    );

    let address = chain_solana::Address::from_str(&source.address.text).expect("source address");
    let outputs = repository
        .list(OutputRequest {
            scope: scope.clone(),
            address: indexing::CanonicalAddress {
                scope: scope.clone(),
                value: address.to_string(),
            },
            after: None,
            limit: 100,
        })
        .await
        .expect("Solana output projection");
    assert!(outputs.outputs.is_empty());

    let checkpoint_before = service
        .checkpoint(&scope)
        .await
        .expect("checkpoint before retained rollback")
        .expect("persisted Solana checkpoint");
    let mut removed = 0_u64;
    loop {
        let history = wallets
            .history(&"source".to_owned(), HistoryRequest::first(100))
            .await
            .expect("history during retained rollback");
        if history
            .transactions
            .iter()
            .all(|entry| entry.transaction_id.value != transaction.as_str())
        {
            break;
        }
        let tip = repository
            .get(BlockSelector::Tip(scope.clone()))
            .await
            .expect("retained tip read")
            .expect("retained tip");
        repository
            .remove(scope.clone(), tip)
            .await
            .expect("storage-derived retained rollback");
        removed += 1;
        assert!(removed <= 32, "transaction must remain inside retention");
    }

    drop(wallets);
    drop(service);
    let restarted_repository = indexing_postgres::Repository::new(pool, scope.clone())
        .expect("restarted Solana repository");
    let restarted_checkpoint = restarted_repository
        .get(BlockSelector::Tip(scope.clone()))
        .await
        .expect("restarted checkpoint read");
    assert_ne!(restarted_checkpoint.as_ref(), Some(&checkpoint_before));
    let (restarted_progress, restarted_progress_rx) = watch::channel(restarted_checkpoint);
    let mut restarted_service = indexing::Service::new(
        chain_solana::Source::new(client.clone()),
        chain_solana::BlockInterpreter::new(scope.clone()).expect("restarted interpreter"),
        restarted_repository.clone(),
        SyncConfig::new(scope.clone(), 1, 32, 100).expect("restarted synchronization policy"),
    );
    restarted_service.observe(Arc::new(Notice(restarted_progress)));
    let restarted_service = Arc::new(restarted_service);
    let restarted_tasks = Arc::new(Tasks::default());
    let restarted_coordinator = Arc::new(chain_solana::Coordinator::new(
        client.clone(),
        restarted_tasks,
        restarted_service.clone(),
        restarted_service.clone(),
        scope.clone(),
        restarted_progress_rx,
    ));
    let restarted_provider = chain_solana::WalletProvider::new(
        NativeAsset::new("solana-stack").expect("restarted native asset"),
        client,
        restarted_service.clone(),
        restarted_coordinator,
    );
    let restarted_sender = restarted_provider.transactions();
    let mut restarted_wallets =
        Wallets::<String, payment_api::WalletAsset>::new(restarted_service.clone());
    restarted_wallets
        .register(
            payment_api::WalletAsset::Sol,
            scope.clone(),
            restarted_provider,
            restarted_sender,
            None,
        )
        .expect("restarted native SOL family");
    restarted_wallets
        .import(
            "source".to_owned(),
            &payment_api::WalletAsset::Sol,
            SecretBytes::new(vec![7; 32]),
            BlockPosition(0),
        )
        .await
        .expect("restarted source import");
    restarted_wallets
        .import(
            "destination".to_owned(),
            &payment_api::WalletAsset::Sol,
            SecretBytes::new(vec![9; 32]),
            BlockPosition(0),
        )
        .await
        .expect("restarted destination import");
    let mut restored = false;
    for _ in 0..100 {
        restarted_service
            .sync(&restarted_wallets)
            .await
            .expect("restart synchronization");
        let history = restarted_wallets
            .history(&"source".to_owned(), HistoryRequest::first(100))
            .await
            .expect("history after restart");
        restored = history
            .transactions
            .iter()
            .any(|entry| entry.transaction_id.value == transaction.as_str());
        if restored {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        restored,
        "restart must refill the retained canonical suffix"
    );
    assert!(
        restarted_repository
            .list(OutputRequest {
                scope: scope.clone(),
                address: indexing::CanonicalAddress {
                    scope,
                    value: address.to_string(),
                },
                after: None,
                limit: 100,
            })
            .await
            .expect("restarted Solana output projection")
            .outputs
            .is_empty()
    );
    assert!(database.registry_sentinel_unchanged().await);
}
