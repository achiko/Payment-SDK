use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{process::Command, time};
use uuid::Uuid;

use crate::{
    cli::{AuthenticationProfile, ScenarioSelection},
    error::{HarnessError, OptionContext, Result, ResultContext},
    process::{ProcessSpec, ProcessSupervisor, Redactor},
};

const REGTEST_GENESIS: &str = "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
const READY_TIMEOUT: Duration = Duration::from_secs(180);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CONFIRMATION_DEPTH: u64 = 2;

#[derive(Clone, Debug)]
pub struct FixtureConfig {
    pub bitcoind: PathBuf,
    pub bitcoin_cli: PathBuf,
    pub profile: AuthenticationProfile,
    pub scenario: ScenarioSelection,
    pub case_artifacts: PathBuf,
    pub keep_workdir: bool,
}

#[derive(Clone, Debug)]
struct ServiceBinaries {
    indexer: PathBuf,
    custody: PathBuf,
    wallet: PathBuf,
    payment: PathBuf,
}

impl ServiceBinaries {
    fn discover() -> Result<Self> {
        let executable = std::env::current_exe()
            .context(|| "resolving acceptance-runner executable".to_owned())?;
        let directory = executable
            .parent()
            .context(|| "acceptance-runner executable has no parent directory".to_owned())?;
        let binaries = Self {
            indexer: directory.join("indexer-worker"),
            custody: directory.join("custody-worker"),
            wallet: directory.join("wallet-worker"),
            payment: directory.join("payment-api"),
        };
        for (name, path) in [
            ("indexer-worker", &binaries.indexer),
            ("custody-worker", &binaries.custody),
            ("wallet-worker", &binaries.wallet),
            ("payment-api", &binaries.payment),
        ] {
            if !path.is_file() {
                return Err(HarnessError::new(format!(
                    "required service binary {name} is missing at {}; run the checked-in wrapper so every binary is built together",
                    path.display()
                )));
            }
        }
        Ok(binaries)
    }
}

#[derive(Clone, Copy, Debug)]
struct Ports {
    core_rpc: u16,
    ix_http: u16,
    ix_metrics: u16,
    custody_http: u16,
    custody_metrics: u16,
    ws_http: u16,
    ws_metrics: u16,
    ps_http: u16,
    ps_metrics: u16,
}

impl Ports {
    fn allocate() -> Result<Self> {
        let mut allocated = BTreeSet::new();
        let mut next = || -> Result<u16> {
            loop {
                let listener =
                    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                        .context(|| "allocating a loopback acceptance port".to_owned())?;
                let port = listener
                    .local_addr()
                    .context(|| "reading allocated loopback port".to_owned())?
                    .port();
                drop(listener);
                if allocated.insert(port) {
                    return Ok(port);
                }
            }
        };
        Ok(Self {
            core_rpc: next()?,
            ix_http: next()?,
            ix_metrics: next()?,
            custody_http: next()?,
            custody_metrics: next()?,
            ws_http: next()?,
            ws_metrics: next()?,
            ps_http: next()?,
            ps_metrics: next()?,
        })
    }
}

#[derive(Clone, Debug)]
struct Credentials {
    ix: String,
    custody: String,
    ws: String,
    ps_ordinary: String,
    ps_admin: String,
}

impl Credentials {
    fn fresh() -> Self {
        let token = |label: &str| format!("acceptance-{label}-{}", Uuid::now_v7());
        Self {
            ix: token("ix"),
            custody: token("custody"),
            ws: token("ws"),
            ps_ordinary: token("ps-ordinary"),
            ps_admin: token("ps-admin"),
        }
    }

    fn register(&self, redactor: &mut Redactor) {
        for token in [
            &self.ix,
            &self.custody,
            &self.ws,
            &self.ps_ordinary,
            &self.ps_admin,
        ] {
            redactor.register(token.clone());
        }
    }
}

#[derive(Clone)]
pub struct ApiClient {
    base: String,
    bearer: Option<String>,
    client: reqwest::Client,
}

pub struct HttpJson {
    pub status: StatusCode,
    pub body: Value,
}

impl ApiClient {
    fn new(port: u16, bearer: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context(|| "building acceptance HTTP client".to_owned())?;
        Ok(Self {
            base: format!("http://127.0.0.1:{port}"),
            bearer,
            client,
        })
    }

    pub async fn get(&self, path: &str) -> Result<HttpJson> {
        self.request(Method::GET, path, None, None).await
    }

    pub async fn post(
        &self,
        path: &str,
        body: Value,
        idempotency_key: Option<&str>,
    ) -> Result<HttpJson> {
        self.request(Method::POST, path, Some(body), idempotency_key)
            .await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency_key: Option<&str>,
    ) -> Result<HttpJson> {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.base, path));
        if let Some(bearer) = &self.bearer {
            request = request.bearer_auth(bearer);
        }
        if let Some(idempotency_key) = idempotency_key {
            request = request.header("Idempotency-Key", idempotency_key);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .context(|| format!("calling HTTP path {path}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context(|| format!("reading HTTP response from {path}"))?;
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .context(|| format!("decoding JSON response from {path}"))?
        };
        Ok(HttpJson { status, body })
    }

    #[must_use]
    pub fn without_bearer(&self) -> Self {
        Self {
            base: self.base.clone(),
            bearer: None,
            client: self.client.clone(),
        }
    }
}

#[derive(Clone)]
struct CoreCli {
    binary: PathBuf,
    datadir: PathBuf,
    rpc_port: u16,
}

impl CoreCli {
    async fn json(&self, wallet: Option<&str>, arguments: &[String]) -> Result<Value> {
        let output = self.output(wallet, arguments).await?;
        if !output.status.success() {
            return Err(HarnessError::new(format!(
                "bitcoin-cli command {} failed with {}: {}",
                arguments.first().map_or("<missing>", String::as_str),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        match serde_json::from_slice(&output.stdout) {
            Ok(value) => Ok(value),
            Err(json_error) => {
                let scalar = String::from_utf8(output.stdout).context(|| {
                    format!(
                        "decoding bitcoin-cli scalar for {}",
                        arguments.first().map_or("<missing>", String::as_str)
                    )
                })?;
                let scalar = scalar.trim();
                if scalar.is_empty() {
                    Ok(Value::Null)
                } else if scalar.contains('\r') || scalar.contains('\n') {
                    Err(HarnessError::new(format!(
                        "decoding bitcoin-cli result for {} failed: {json_error}",
                        arguments.first().map_or("<missing>", String::as_str)
                    )))
                } else {
                    Ok(Value::String(scalar.to_owned()))
                }
            }
        }
    }

    async fn succeeds(&self, wallet: Option<&str>, arguments: &[String]) -> Result<bool> {
        Ok(self.output(wallet, arguments).await?.status.success())
    }

    async fn output(
        &self,
        wallet: Option<&str>,
        arguments: &[String],
    ) -> Result<std::process::Output> {
        let mut command = Command::new(&self.binary);
        command
            .env_clear()
            .arg("-regtest")
            .arg(format!("-datadir={}", self.datadir.display()))
            .arg(format!("-rpcport={}", self.rpc_port));
        if let Some(wallet) = wallet {
            command.arg(format!("-rpcwallet={wallet}"));
        }
        command.args(arguments).stdin(Stdio::null());
        time::timeout(RPC_TIMEOUT, command.output())
            .await
            .map_err(|_| {
                HarnessError::new(format!(
                    "bitcoin-cli command {} timed out",
                    arguments.first().map_or("<missing>", String::as_str)
                ))
            })?
            .context(|| {
                format!(
                    "executing bitcoin-cli command {}",
                    arguments.first().map_or("<missing>", String::as_str)
                )
            })
    }
}

#[derive(Debug, Deserialize)]
pub struct IndexedUtxo {
    pub transaction_id: String,
    pub output_index: String,
    pub value_sats: String,
    pub script_pubkey: String,
    pub address: String,
    pub confirmations: String,
}

#[derive(Debug, Deserialize)]
struct UtxoPage {
    outputs: Vec<IndexedUtxo>,
}

#[derive(Debug)]
pub struct WalletAddress {
    pub address: String,
    pub key_locator: Value,
}

#[derive(Debug)]
pub struct DepositHandle {
    pub deposit_id: String,
    pub job_id: String,
    pub address: String,
}

#[derive(Debug)]
pub struct CollectionHandle {
    pub collection_id: String,
    pub job_id: String,
}

#[derive(Debug)]
pub struct SignedTransaction {
    pub transaction_id: String,
    pub raw_transaction: String,
    pub fee_satoshis: String,
    pub virtual_size: String,
}

pub struct Fixture {
    config: FixtureConfig,
    binaries: ServiceBinaries,
    ports: Ports,
    credentials: Credentials,
    root: Option<TempDir>,
    core_datadir: PathBuf,
    ix_database: PathBuf,
    ps_database: PathBuf,
    policy_path: PathBuf,
    private_logs: PathBuf,
    supervisor: ProcessSupervisor,
    redactor: Redactor,
    core: CoreCli,
    core_authorization: Option<String>,
    miner_address: Option<String>,
    pub ix: ApiClient,
    pub custody: ApiClient,
    pub ws: ApiClient,
    pub ps: ApiClient,
    pub ps_admin: ApiClient,
    assertions: Vec<String>,
    evidence: BTreeMap<String, String>,
    process_generation: u32,
}

impl Fixture {
    pub fn new(config: FixtureConfig) -> Result<Self> {
        if config.profile == AuthenticationProfile::All {
            return Err(HarnessError::new(
                "fixture requires one concrete authentication profile",
            ));
        }
        if config.scenario == ScenarioSelection::All {
            return Err(HarnessError::new("fixture requires one concrete scenario"));
        }
        let binaries = ServiceBinaries::discover()?;
        let ports = Ports::allocate()?;
        let credentials = Credentials::fresh();
        let root = tempfile::Builder::new()
            .prefix("payment-sdk-btc31-")
            .tempdir()
            .context(|| "creating private regtest fixture directory".to_owned())?;
        let root_path = root.path().to_path_buf();
        let core_datadir = root_path.join("core");
        let ix_database = root_path.join("indexer/database");
        let ps_database = root_path.join("payment/database");
        let policy_path = root_path.join("payment/bitcoin-policy.json");
        let private_logs = root_path.join("logs");
        for directory in [&core_datadir, &ix_database, &ps_database, &private_logs] {
            fs::create_dir_all(directory)
                .context(|| format!("creating private fixture path {}", directory.display()))?;
        }
        let strict = config.profile == AuthenticationProfile::Strict;
        let bearer = |token: &String| strict.then(|| token.clone());
        let ix = ApiClient::new(ports.ix_http, bearer(&credentials.ix))?;
        let custody = ApiClient::new(ports.custody_http, bearer(&credentials.custody))?;
        let ws = ApiClient::new(ports.ws_http, bearer(&credentials.ws))?;
        let ps = ApiClient::new(ports.ps_http, bearer(&credentials.ps_ordinary))?;
        let ps_admin = ApiClient::new(ports.ps_http, bearer(&credentials.ps_admin))?;
        let core = CoreCli {
            binary: config.bitcoin_cli.clone(),
            datadir: core_datadir.clone(),
            rpc_port: ports.core_rpc,
        };
        let mut redactor = Redactor::default();
        credentials.register(&mut redactor);
        Ok(Self {
            config,
            binaries,
            ports,
            credentials,
            root: Some(root),
            core_datadir,
            ix_database,
            ps_database,
            policy_path,
            private_logs,
            supervisor: ProcessSupervisor::default(),
            redactor,
            core,
            core_authorization: None,
            miner_address: None,
            ix,
            custody,
            ws,
            ps,
            ps_admin,
            assertions: Vec::new(),
            evidence: BTreeMap::new(),
            process_generation: 0,
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        self.start_core().await?;
        self.wait_for_core().await?;
        self.refresh_core_authorization()?;
        self.verify_live_core().await?;
        self.prepare_miner_and_fee_estimator().await?;
        self.write_payment_policy().await?;
        self.start_ix().await?;
        self.start_custody().await?;
        let ix = self.ix.clone();
        self.wait_for_service_ready("IX", &ix, "/health/ready")
            .await?;
        self.wait_for_ix_phase("ready").await?;
        let custody = self.custody.clone();
        self.wait_for_service_ready("custody", &custody, "/health/ready")
            .await?;
        self.start_ws().await?;
        let ws = self.ws.clone();
        self.wait_for_service_ready("WS", &ws, "/health/ready")
            .await?;
        self.start_ps().await?;
        let ps = self.ps.clone();
        self.wait_for_service_ready("PS", &ps, "/health/ready")
            .await?;
        self.wait_for_ps_ready().await?;
        self.assert_authentication_posture().await?;
        Ok(())
    }

    pub fn assert(&mut self, name: impl Into<String>, condition: bool) -> Result<()> {
        let name = name.into();
        if !condition {
            return Err(HarnessError::new(format!("assertion failed: {name}")));
        }
        self.assertions.push(name);
        Ok(())
    }

    pub fn evidence(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.evidence.insert(key.into(), value.into());
    }

    #[must_use]
    pub fn take_assertions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.assertions)
    }

    #[must_use]
    pub fn take_evidence(&mut self) -> BTreeMap<String, String> {
        std::mem::take(&mut self.evidence)
    }

    pub fn miner_address(&self) -> Result<&str> {
        self.miner_address
            .as_deref()
            .context(|| "miner address is unavailable".to_owned())
    }

    fn next_log(&mut self, service: &str) -> PathBuf {
        self.process_generation = self.process_generation.saturating_add(1);
        self.private_logs
            .join(format!("{:03}-{service}.log", self.process_generation))
    }

    async fn start_core(&mut self) -> Result<()> {
        let log_path = self.next_log("bitcoind");
        self.supervisor
            .start(ProcessSpec {
                name: "core".to_owned(),
                program: self.config.bitcoind.clone(),
                args: [
                    "-regtest".to_owned(),
                    "-nosettings".to_owned(),
                    format!("-datadir={}", self.core_datadir.display()),
                    "-server=1".to_owned(),
                    "-txindex=1".to_owned(),
                    "-prune=0".to_owned(),
                    "-listen=0".to_owned(),
                    "-discover=0".to_owned(),
                    "-persistmempool=0".to_owned(),
                    "-rpcbind=127.0.0.1".to_owned(),
                    "-rpcallowip=127.0.0.1".to_owned(),
                    format!("-rpcport={}", self.ports.core_rpc),
                    "-fallbackfee=0.00010000".to_owned(),
                    "-printtoconsole=1".to_owned(),
                ]
                .into_iter()
                .map(OsString::from)
                .collect(),
                environment: Vec::new(),
                log_path,
            })
            .await
    }

    async fn wait_for_core(&mut self) -> Result<()> {
        let started = Instant::now();
        loop {
            self.supervisor.ensure_running("core")?;
            let arguments = vec!["getblockchaininfo".to_owned()];
            if self.core.succeeds(None, &arguments).await? {
                return Ok(());
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(
                    "Bitcoin Core did not become RPC-ready before the fixture timeout",
                ));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    fn refresh_core_authorization(&mut self) -> Result<()> {
        let cookie_path = self.core_datadir.join("regtest/.cookie");
        let cookie = fs::read_to_string(&cookie_path)
            .context(|| format!("reading private Core cookie {}", cookie_path.display()))?;
        let cookie = cookie.trim().to_owned();
        if cookie.is_empty() {
            return Err(HarnessError::new("Bitcoin Core RPC cookie is empty"));
        }
        self.redactor.register(cookie.clone());
        let authorization = format!("Basic {}", BASE64.encode(cookie.as_bytes()));
        self.redactor.register(authorization.clone());
        self.core_authorization = Some(authorization);
        Ok(())
    }

    async fn verify_live_core(&mut self) -> Result<()> {
        let network = self.core.json(None, &["getnetworkinfo".to_owned()]).await?;
        self.assert(
            "Core live numeric version is exactly 31.1.0",
            network.get("version").and_then(Value::as_u64) == Some(310_100),
        )?;
        let chain = self
            .core
            .json(None, &["getblockchaininfo".to_owned()])
            .await?;
        self.assert(
            "Core reports the regtest chain",
            chain.get("chain").and_then(Value::as_str) == Some("regtest"),
        )?;
        self.assert(
            "Core is unpruned and its block and header heights agree",
            chain.get("pruned").and_then(Value::as_bool) == Some(false)
                && chain.get("blocks").and_then(Value::as_u64)
                    == chain.get("headers").and_then(Value::as_u64),
        )?;
        let genesis = self
            .core
            .json(None, &["getblockhash".to_owned(), "0".to_owned()])
            .await?;
        self.assert(
            "Core regtest genesis matches the configured service identity",
            genesis.as_str() == Some(REGTEST_GENESIS),
        )?;
        let index = self
            .core
            .json(None, &["getindexinfo".to_owned(), "txindex".to_owned()])
            .await?;
        self.assert(
            "Core txindex is synchronized",
            index
                .get("txindex")
                .and_then(|value| value.get("synced"))
                .and_then(Value::as_bool)
                == Some(true),
        )?;
        self.evidence("core_network", "regtest");
        self.evidence("core_numeric_version", "310100");
        self.evidence("core_genesis", REGTEST_GENESIS);
        Ok(())
    }

    async fn prepare_miner_and_fee_estimator(&mut self) -> Result<()> {
        self.core
            .json(None, &["createwallet".to_owned(), "miner".to_owned()])
            .await?;
        let miner_address = self
            .core
            .json(
                Some("miner"),
                &[
                    "getnewaddress".to_owned(),
                    "acceptance-miner".to_owned(),
                    "bech32".to_owned(),
                ],
            )
            .await?
            .as_str()
            .context(|| "Core miner address response is not a string".to_owned())?
            .to_owned();
        self.assert(
            "miner address belongs to regtest",
            miner_address.starts_with("bcrt1"),
        )?;
        let maturity = self
            .core
            .json(
                None,
                &[
                    "generatetoaddress".to_owned(),
                    "101".to_owned(),
                    miner_address.clone(),
                ],
            )
            .await?;
        self.assert(
            "Core mined 101 coinbase-maturity blocks",
            maturity
                .as_array()
                .is_some_and(|blocks| blocks.len() == 101),
        )?;
        self.miner_address = Some(miner_address.clone());

        let mut estimate_ready = false;
        for round in 1..=50_u32 {
            if self.fee_estimate_ready().await? {
                estimate_ready = true;
                break;
            }
            let warm_address = self
                .core
                .json(
                    Some("miner"),
                    &[
                        "getnewaddress".to_owned(),
                        format!("fee-warmup-{round}"),
                        "bech32".to_owned(),
                    ],
                )
                .await?
                .as_str()
                .context(|| "Core fee-warmup address is not a string".to_owned())?
                .to_owned();
            for _ in 0..12 {
                self.core
                    .json(
                        Some("miner"),
                        &[
                            "-named".to_owned(),
                            "sendtoaddress".to_owned(),
                            format!("address={warm_address}"),
                            "amount=0.00100000".to_owned(),
                            "fee_rate=2".to_owned(),
                        ],
                    )
                    .await?;
            }
            self.core
                .json(
                    None,
                    &[
                        "generatetoaddress".to_owned(),
                        "1".to_owned(),
                        miner_address.clone(),
                    ],
                )
                .await?;
        }
        if !estimate_ready {
            estimate_ready = self.fee_estimate_ready().await?;
        }
        self.assert(
            "Core fee estimator returns a positive conservative rate",
            estimate_ready,
        )?;
        let ready_chain = self
            .core
            .json(None, &["getblockchaininfo".to_owned()])
            .await?;
        self.assert(
            "Core exits initial block download after local regtest mining",
            ready_chain
                .get("initialblockdownload")
                .and_then(Value::as_bool)
                == Some(false)
                && ready_chain.get("blocks").and_then(Value::as_u64)
                    == ready_chain.get("headers").and_then(Value::as_u64),
        )?;
        Ok(())
    }

    async fn fee_estimate_ready(&self) -> Result<bool> {
        let estimate = self
            .core
            .json(
                None,
                &[
                    "estimatesmartfee".to_owned(),
                    "6".to_owned(),
                    "conservative".to_owned(),
                ],
            )
            .await?;
        Ok(estimate
            .get("feerate")
            .and_then(Value::as_f64)
            .is_some_and(|rate| rate > 0.0))
    }

    async fn write_payment_policy(&mut self) -> Result<()> {
        let destination = self
            .core
            .json(
                Some("miner"),
                &[
                    "getnewaddress".to_owned(),
                    "payment-sdk-master".to_owned(),
                    "bech32".to_owned(),
                ],
            )
            .await?
            .as_str()
            .context(|| "Core master destination is not a string".to_owned())?
            .to_owned();
        self.assert(
            "Payment Service master destination belongs to regtest",
            destination.starts_with("bcrt1"),
        )?;
        let policy = json!({
            "version": 1,
            "scope": {"chain": "bitcoin", "network": "regtest"},
            "deposit_address_kind": "p2wpkh",
            "deposit_ttl_seconds": 3600,
            "master_destination": destination,
            "minimum_collection_satoshis": "10000",
            "minimum_spend_confirmations": 1,
            "requested_satoshis_per_kvb": "1000",
            "maximum_satoshis_per_kvb": "5000",
            "maximum_absolute_fee_satoshis": "50000",
            "maximum_deposits": 20,
            "maximum_inputs": 200
        });
        let bytes = serde_json::to_vec_pretty(&policy)
            .context(|| "serializing temporary Bitcoin PS policy".to_owned())?;
        fs::write(&self.policy_path, bytes).context(|| {
            format!(
                "writing temporary Bitcoin PS policy {}",
                self.policy_path.display()
            )
        })?;
        Ok(())
    }

    fn common_environment(&self) -> Vec<(OsString, OsString)> {
        vec![
            (OsString::from("RUST_BACKTRACE"), OsString::from("0")),
            (OsString::from("RUST_LOG"), OsString::from("info")),
            (
                OsString::from("STRICT_AUTHENTICATION_MODE"),
                OsString::from(self.config.profile.strict_value()),
            ),
        ]
    }

    async fn start_ix(&mut self) -> Result<()> {
        let core_authorization = self
            .core_authorization
            .as_deref()
            .context(|| "Core authorization is unavailable for IX".to_owned())?;
        let mut environment = self.common_environment();
        environment.extend(environment_entries([
            ("IX_DATABASE_PATH", self.ix_database.display().to_string()),
            ("IX_NETWORK", "regtest".to_owned()),
            ("IX_BOOTSTRAP_HEIGHT", "0".to_owned()),
            ("IX_CONFIRMATION_DEPTH", CONFIRMATION_DEPTH.to_string()),
            ("IX_REORG_RETENTION", "20".to_owned()),
            ("IX_EXPECTED_GENESIS_HASH", REGTEST_GENESIS.to_owned()),
            (
                "IX_RPC_HTTP_URL",
                format!("http://127.0.0.1:{}", self.ports.core_rpc),
            ),
            (
                "IX_RPC_HEADERS",
                format!("authorization={core_authorization}"),
            ),
            ("IX_RPC_TIMEOUT_SECONDS", "15".to_owned()),
            ("IX_RPC_MAX_RESPONSE_BYTES", "268435456".to_owned()),
            ("IX_HTTP_BIND", format!("127.0.0.1:{}", self.ports.ix_http)),
            (
                "IX_METRICS_BIND",
                format!("127.0.0.1:{}", self.ports.ix_metrics),
            ),
            ("IX_POLL_SECONDS", "1".to_owned()),
            ("IX_READY_MAX_LAG", "0".to_owned()),
            ("IX_READY_MAX_AGE_SECONDS", "30".to_owned()),
        ]));
        if self.config.profile == AuthenticationProfile::Strict {
            environment.push((
                OsString::from("IX_BEARER_TOKEN"),
                OsString::from(&self.credentials.ix),
            ));
        }
        let log_path = self.next_log("indexer");
        self.supervisor
            .start(ProcessSpec {
                name: "ix".to_owned(),
                program: self.binaries.indexer.clone(),
                args: vec![OsString::from("bitcoin"), OsString::from("serve")],
                environment,
                log_path,
            })
            .await
    }

    async fn start_custody(&mut self) -> Result<()> {
        let mut environment = self.common_environment();
        environment.extend(environment_entries([
            (
                "CUSTODY_BIND",
                format!("127.0.0.1:{}", self.ports.custody_http),
            ),
            (
                "CUSTODY_METRICS_BIND",
                format!("127.0.0.1:{}", self.ports.custody_metrics),
            ),
            ("CUSTODY_SHUTDOWN_GRACE_SECONDS", "3".to_owned()),
        ]));
        if self.config.profile == AuthenticationProfile::Strict {
            environment.push((
                OsString::from("CUSTODY_BEARER_TOKEN"),
                OsString::from(&self.credentials.custody),
            ));
        }
        let log_path = self.next_log("custody");
        self.supervisor
            .start(ProcessSpec {
                name: "custody".to_owned(),
                program: self.binaries.custody.clone(),
                args: vec![OsString::from("serve")],
                environment,
                log_path,
            })
            .await
    }

    async fn start_ws(&mut self) -> Result<()> {
        let core_authorization = self
            .core_authorization
            .as_deref()
            .context(|| "Core authorization is unavailable for WS".to_owned())?;
        let mut environment = self.common_environment();
        environment.extend(environment_entries([
            ("WS_BITCOIN_NETWORK", "regtest".to_owned()),
            (
                "WS_BITCOIN_EXPECTED_GENESIS_HASH",
                REGTEST_GENESIS.to_owned(),
            ),
            (
                "WS_BITCOIN_CORE_RPC_URL",
                format!("http://127.0.0.1:{}", self.ports.core_rpc),
            ),
            (
                "WS_BITCOIN_CORE_RPC_AUTHORIZATION",
                core_authorization.to_owned(),
            ),
            (
                "WS_BITCOIN_IX_URL",
                format!("http://127.0.0.1:{}", self.ports.ix_http),
            ),
            ("WS_BITCOIN_MINIMUM_CONFIRMATIONS", "1".to_owned()),
            ("WS_BITCOIN_FEE_TARGET_BLOCKS", "6".to_owned()),
            ("WS_BITCOIN_MAX_SATOSHIS_PER_KVB", "100000".to_owned()),
            (
                "WS_CUSTODY_URL",
                format!("http://127.0.0.1:{}", self.ports.custody_http),
            ),
            (
                "WS_CUSTODY_AUTHENTICATION_POLICY",
                "repository_mode_matched".to_owned(),
            ),
            ("WS_HTTP_BIND", format!("127.0.0.1:{}", self.ports.ws_http)),
            (
                "WS_METRICS_BIND",
                format!("127.0.0.1:{}", self.ports.ws_metrics),
            ),
            ("WS_SHUTDOWN_GRACE_SECONDS", "3".to_owned()),
        ]));
        if self.config.profile == AuthenticationProfile::Strict {
            environment.extend(environment_entries([
                ("WS_BEARER_TOKEN", self.credentials.ws.clone()),
                ("WS_BITCOIN_IX_BEARER_TOKEN", self.credentials.ix.clone()),
                ("WS_CUSTODY_BEARER_TOKEN", self.credentials.custody.clone()),
            ]));
        }
        let log_path = self.next_log("wallet");
        self.supervisor
            .start(ProcessSpec {
                name: "ws".to_owned(),
                program: self.binaries.wallet.clone(),
                args: vec![OsString::from("bitcoin"), OsString::from("serve")],
                environment,
                log_path,
            })
            .await
    }

    async fn start_ps(&mut self) -> Result<()> {
        let mut environment = self.common_environment();
        environment.extend(environment_entries([
            ("PS_DATABASE_PATH", self.ps_database.display().to_string()),
            ("PS_POLICY_PATH", self.policy_path.display().to_string()),
            (
                "PS_INDEXER_URL",
                format!("http://127.0.0.1:{}", self.ports.ix_http),
            ),
            ("PS_INDEXER_NETWORK", "regtest".to_owned()),
            (
                "PS_WALLET_URL",
                format!("http://127.0.0.1:{}", self.ports.ws_http),
            ),
            ("PS_HTTP_BIND", format!("127.0.0.1:{}", self.ports.ps_http)),
            (
                "PS_METRICS_BIND",
                format!("127.0.0.1:{}", self.ports.ps_metrics),
            ),
            ("PS_WORKER_INTERVAL_MILLIS", "100".to_owned()),
            ("PS_WORKER_PAGE_SIZE", "100".to_owned()),
            ("PS_SHUTDOWN_GRACE_SECONDS", "3".to_owned()),
        ]));
        if self.config.profile == AuthenticationProfile::Strict {
            environment.extend(environment_entries([
                ("PS_API_BEARER_TOKEN", self.credentials.ps_ordinary.clone()),
                ("PS_ADMIN_BEARER_TOKEN", self.credentials.ps_admin.clone()),
                ("PS_INDEXER_BEARER_TOKEN", self.credentials.ix.clone()),
                ("PS_WALLET_BEARER_TOKEN", self.credentials.ws.clone()),
            ]));
        }
        let log_path = self.next_log("payment");
        self.supervisor
            .start(ProcessSpec {
                name: "ps".to_owned(),
                program: self.binaries.payment.clone(),
                args: vec![OsString::from("bitcoin"), OsString::from("serve")],
                environment,
                log_path,
            })
            .await
    }

    async fn wait_for_service_ready(
        &mut self,
        label: &str,
        client: &ApiClient,
        path: &str,
    ) -> Result<Value> {
        let started = Instant::now();
        loop {
            if let Ok(response) = client.get(path).await
                && response.status == StatusCode::OK
                && response.body.get("status").and_then(Value::as_str) == Some("ready")
                && response
                    .body
                    .get("authentication_mode")
                    .and_then(Value::as_str)
                    == Some(self.config.profile.canonical_name())
            {
                self.assert(
                    format!("{label} reports the configured authentication mode"),
                    true,
                )?;
                return Ok(response.body);
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(format!(
                    "{label} did not become ready before the fixture timeout"
                )));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn wait_for_ix_phase(&mut self, expected: &str) -> Result<Value> {
        let started = Instant::now();
        loop {
            if let Ok(response) = self.ix.get("/v1/scopes/bitcoin/regtest/status").await
                && response.status == StatusCode::OK
                && response.body.get("phase").and_then(Value::as_str) == Some(expected)
            {
                return Ok(response.body);
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(format!(
                    "IX did not reach phase {expected} before the fixture timeout"
                )));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn wait_for_ps_ready(&mut self) -> Result<Value> {
        let started = Instant::now();
        loop {
            if let Ok(response) = self.ps_admin.get("/v1/admin/status").await
                && response.status == StatusCode::OK
                && response.body.get("ready").and_then(Value::as_bool) == Some(true)
                && response.body.get("indexer_ready").and_then(Value::as_bool) == Some(true)
                && response.body.get("wallet_ready").and_then(Value::as_bool) == Some(true)
            {
                return Ok(response.body);
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(
                    "Payment Service dependencies did not become ready before the fixture timeout",
                ));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn assert_authentication_posture(&mut self) -> Result<()> {
        if self.config.profile == AuthenticationProfile::Strict {
            let ix = self
                .ix
                .without_bearer()
                .get("/v1/scopes/bitcoin/regtest/status")
                .await?;
            let custody = self
                .custody
                .without_bearer()
                .get("/v1/capabilities")
                .await?;
            let ws = self
                .ws
                .without_bearer()
                .post("/v1/bitcoin/balances", json!({}), None)
                .await?;
            let ps = self
                .ps_admin
                .without_bearer()
                .get("/v1/admin/status")
                .await?;
            self.assert(
                "strict mode rejects unauthenticated IX, custody, WS, and PS requests",
                [ix.status, custody.status, ws.status, ps.status]
                    .into_iter()
                    .all(|status| status == StatusCode::UNAUTHORIZED),
            )?;
        } else {
            self.assert(
                "global-trusted mode serves the composed APIs without repository bearers",
                self.ix
                    .get("/v1/scopes/bitcoin/regtest/status")
                    .await?
                    .status
                    == StatusCode::OK
                    && self.custody.get("/v1/capabilities").await?.status == StatusCode::OK
                    && self.ps_admin.get("/v1/admin/status").await?.status == StatusCode::OK,
            )?;
        }
        Ok(())
    }

    pub fn success(&self, response: HttpJson, operation: &str) -> Result<Value> {
        if response.status.is_success() {
            Ok(response.body)
        } else {
            Err(HarnessError::new(format!(
                "{operation} returned HTTP {}: {}",
                response.status,
                self.redactor.sanitize(&response.body.to_string())
            )))
        }
    }

    pub async fn generate_wallet_address(
        &mut self,
        address_kind: &str,
        purpose: &str,
    ) -> Result<WalletAddress> {
        let response = self
            .ws
            .post(
                "/v1/bitcoin/addresses",
                json!({
                    "operation_id": format!("acceptance-{purpose}-{}", Uuid::now_v7()),
                    "address_kind": address_kind,
                    "key_purpose": purpose
                }),
                None,
            )
            .await?;
        let body = self.success(response, "generating WS Bitcoin address")?;
        let address = required_string(&body, "address")?;
        let key_locator = body
            .get("key_locator")
            .cloned()
            .context(|| "WS address response has no key_locator".to_owned())?;
        self.redactor.register(key_locator.to_string());
        let validation = self
            .core
            .json(None, &["validateaddress".to_owned(), address.clone()])
            .await?;
        let expected_witness_version = match address_kind {
            "p2wpkh" => Some(0),
            "p2tr" => Some(1),
            _ => None,
        };
        self.assert(
            format!("Core validates the WS {address_kind} regtest address"),
            address.starts_with("bcrt1")
                && validation.get("isvalid").and_then(Value::as_bool) == Some(true)
                && validation.get("iswitness").and_then(Value::as_bool) == Some(true)
                && validation.get("witness_version").and_then(Value::as_u64)
                    == expected_witness_version,
        )?;
        Ok(WalletAddress {
            address,
            key_locator,
        })
    }

    pub async fn register_watch(
        &self,
        selector_type: &str,
        value: &str,
        idempotency_suffix: &str,
    ) -> Result<Value> {
        let height = self.block_count().await?;
        let response = self
            .ix
            .post(
                "/v1/scopes/bitcoin/regtest/watches",
                json!({
                    "selector": {"type": selector_type, "value": value},
                    "start_height": height.to_string(),
                    "idempotency_key": format!("acceptance-watch-{idempotency_suffix}")
                }),
                None,
            )
            .await?;
        let body = self.success(response, "registering IX watch")?;
        required_string(&body, "id")?;
        Ok(body)
    }

    pub async fn core_wallet_address(&self, label: &str) -> Result<String> {
        self.core
            .json(
                Some("miner"),
                &[
                    "getnewaddress".to_owned(),
                    label.to_owned(),
                    "bech32".to_owned(),
                ],
            )
            .await?
            .as_str()
            .map(str::to_owned)
            .context(|| "Core wallet address response is not a string".to_owned())
    }

    pub async fn fund_address(&self, address: &str, satoshis: u64) -> Result<String> {
        let amount = format_satoshis_as_btc(satoshis);
        self.core
            .json(
                Some("miner"),
                &["sendtoaddress".to_owned(), address.to_owned(), amount],
            )
            .await?
            .as_str()
            .map(str::to_owned)
            .context(|| "Core sendtoaddress response is not a transaction ID".to_owned())
    }

    pub async fn mine_blocks(&self, count: u64) -> Result<Vec<String>> {
        let miner_address = self.miner_address()?.to_owned();
        let value = self
            .core
            .json(
                None,
                &[
                    "generatetoaddress".to_owned(),
                    count.to_string(),
                    miner_address,
                ],
            )
            .await?;
        value
            .as_array()
            .context(|| "generatetoaddress response is not an array".to_owned())?
            .iter()
            .map(|hash| {
                hash.as_str()
                    .map(str::to_owned)
                    .context(|| "generated block hash is not a string".to_owned())
            })
            .collect()
    }

    pub async fn mine_empty_block(&self) -> Result<String> {
        let miner_address = self.miner_address()?.to_owned();
        let value = self
            .core
            .json(
                None,
                &["generateblock".to_owned(), miner_address, "[]".to_owned()],
            )
            .await?;
        required_string(&value, "hash")
    }

    pub async fn block_count(&self) -> Result<u64> {
        self.core
            .json(None, &["getblockcount".to_owned()])
            .await?
            .as_u64()
            .context(|| "Core block count is not an unsigned integer".to_owned())
    }

    pub async fn wait_ix_transaction(&self, transaction_id: &str, kind: &str) -> Result<Value> {
        let path = format!("/v1/scopes/bitcoin/regtest/transactions/{transaction_id}");
        let started = Instant::now();
        loop {
            if let Ok(response) = self.ix.get(&path).await
                && response.status == StatusCode::OK
                && response.body.get("transaction_id").and_then(Value::as_str)
                    == Some(transaction_id)
                && response
                    .body
                    .get("status")
                    .and_then(|status| status.get("kind"))
                    .and_then(Value::as_str)
                    == Some(kind)
            {
                return Ok(response.body);
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(format!(
                    "IX transaction {transaction_id} did not reach {kind} before timeout"
                )));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    pub async fn ix_utxos(&self, address: &str) -> Result<Vec<IndexedUtxo>> {
        let response = self
            .ix
            .get(&format!(
                "/v1/scopes/bitcoin/regtest/addresses/{address}/utxos?limit=100"
            ))
            .await?;
        let body = self.success(response, "reading IX Bitcoin UTXOs")?;
        let page: UtxoPage =
            serde_json::from_value(body).context(|| "decoding IX Bitcoin UTXO page".to_owned())?;
        Ok(page.outputs)
    }

    pub async fn wait_ix_utxo_total(
        &self,
        address: &str,
        expected: u64,
    ) -> Result<Vec<IndexedUtxo>> {
        let started = Instant::now();
        loop {
            if let Ok(outputs) = self.ix_utxos(address).await {
                let total = outputs.iter().try_fold(0_u64, |total, output| {
                    output
                        .value_sats
                        .parse::<u64>()
                        .ok()
                        .and_then(|value| total.checked_add(value))
                });
                if total == Some(expected) {
                    return Ok(outputs);
                }
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(format!(
                    "IX UTXO total for {address} did not become {expected} satoshis"
                )));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    pub async fn sign_transfer(
        &mut self,
        source: &WalletAddress,
        input: &IndexedUtxo,
        recipient: &str,
        recipient_satoshis: u64,
    ) -> Result<SignedTransaction> {
        let response = self
            .ws
            .post(
                "/v1/bitcoin/transfers/sign",
                json!({
                    "operation_id": format!("acceptance-sign-{}", Uuid::now_v7()),
                    "inputs": [{
                        "transaction_id": input.transaction_id,
                        "output_index": input.output_index,
                        "value_satoshis": input.value_sats,
                        "script_pubkey": input.script_pubkey,
                        "address": input.address,
                        "key_locator": source.key_locator
                    }],
                    "recipients": [{
                        "address": recipient,
                        "value_satoshis": recipient_satoshis.to_string()
                    }],
                    "change_address": source.address,
                    "fee_rate_satoshis_per_kvb": "1000"
                }),
                None,
            )
            .await?;
        let body = self.success(response, "signing Bitcoin transfer")?;
        let signed = SignedTransaction {
            transaction_id: required_string(&body, "transaction_id")?,
            raw_transaction: required_string(&body, "raw_transaction")?,
            fee_satoshis: required_string(&body, "fee_satoshis")?,
            virtual_size: required_string(&body, "virtual_size")?,
        };
        self.redactor.register(signed.raw_transaction.clone());
        Ok(signed)
    }

    pub async fn core_knows_mempool_transaction(&self, transaction_id: &str) -> Result<bool> {
        self.core
            .succeeds(
                None,
                &["getmempoolentry".to_owned(), transaction_id.to_owned()],
            )
            .await
    }

    pub async fn test_mempool_accept(&self, raw_transaction: &str) -> Result<Value> {
        let raw = raw_transaction
            .strip_prefix("0x")
            .unwrap_or(raw_transaction);
        let transactions = serde_json::to_string(&[raw])
            .context(|| "serializing testmempoolaccept input".to_owned())?;
        self.core
            .json(None, &["testmempoolaccept".to_owned(), transactions])
            .await
    }

    pub async fn broadcast(&self, signed: &SignedTransaction) -> Result<String> {
        let response = self
            .ws
            .post(
                "/v1/bitcoin/transactions/broadcast",
                json!({
                    "expected_transaction_id": signed.transaction_id,
                    "raw_transaction": signed.raw_transaction
                }),
                None,
            )
            .await?;
        let body = self.success(response, "broadcasting exact Bitcoin transaction")?;
        required_string(&body, "transaction_id")
    }

    pub async fn receipt(&self, transaction_id: &str) -> Result<Value> {
        let response = self
            .ws
            .post(
                "/v1/bitcoin/receipts",
                json!({"transaction_id": transaction_id}),
                None,
            )
            .await?;
        self.success(response, "reading WS Bitcoin receipt")
    }

    pub async fn create_deposit(
        &self,
        user_id: &str,
        expected_satoshis: u64,
        key_suffix: &str,
    ) -> Result<DepositHandle> {
        let response = self
            .ps
            .post(
                "/v1/deposits",
                json!({
                    "user_id": user_id,
                    "scope": {"chain": "bitcoin", "network": "regtest"},
                    "asset": "native",
                    "expected_amount": expected_satoshis.to_string()
                }),
                Some(&format!("acceptance-deposit-{key_suffix}")),
            )
            .await?;
        let body = self.success(response, "creating PS Bitcoin deposit")?;
        let deposit_id = required_string(&body, "deposit_id")?;
        let job_id = required_string(&body, "job_id")?;
        let deposit = self.wait_deposit_active(&deposit_id).await?;
        Ok(DepositHandle {
            deposit_id,
            job_id,
            address: required_string(&deposit, "address")?,
        })
    }

    pub async fn wait_deposit_active(&self, deposit_id: &str) -> Result<Value> {
        let path = format!("/v1/deposits/{deposit_id}");
        let started = Instant::now();
        loop {
            if let Ok(response) = self.ps.get(&path).await
                && response.status == StatusCode::OK
                && response.body.get("state").and_then(Value::as_str) == Some("active")
                && response
                    .body
                    .get("address")
                    .and_then(Value::as_str)
                    .is_some()
            {
                return Ok(response.body);
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(format!(
                    "PS deposit {deposit_id} did not become active before timeout"
                )));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    pub async fn ps_balances(&self, deposit_id: &str) -> Result<Value> {
        let response = self
            .ps
            .get(&format!("/v1/deposits/{deposit_id}/balances"))
            .await?;
        self.success(response, "reading PS deposit balances")
    }

    pub async fn wait_ps_balance(
        &self,
        deposit_id: &str,
        field: &str,
        expected: &str,
    ) -> Result<Value> {
        let started = Instant::now();
        loop {
            if let Ok(balance) = self.ps_balances(deposit_id).await
                && balance.get(field).and_then(Value::as_str) == Some(expected)
            {
                return Ok(balance);
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(format!(
                    "PS balance field {field} for {deposit_id} did not become {expected}"
                )));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    pub async fn ps_ledger(&self, deposit_id: &str) -> Result<Value> {
        let response = self
            .ps
            .get(&format!("/v1/deposits/{deposit_id}/ledger?limit=100"))
            .await?;
        self.success(response, "reading PS deposit ledger")
    }

    pub async fn ps_job(&self, job_id: &str) -> Result<Value> {
        let response = self.ps.get(&format!("/v1/jobs/{job_id}")).await?;
        self.success(response, "reading PS job")
    }

    pub async fn wait_job_terminal(&self, job_id: &str) -> Result<Value> {
        let started = Instant::now();
        loop {
            if let Ok(job) = self.ps_job(job_id).await
                && matches!(
                    job.get("state").and_then(Value::as_str),
                    Some("succeeded" | "failed")
                )
            {
                return Ok(job);
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(format!(
                    "PS job {job_id} did not reach a terminal state before timeout"
                )));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    pub async fn create_collection(
        &self,
        deposit_ids: &[String],
        key_suffix: &str,
    ) -> Result<CollectionHandle> {
        let response = self
            .ps
            .post(
                "/v1/collections",
                json!({"deposit_ids": deposit_ids}),
                Some(&format!("acceptance-collection-{key_suffix}")),
            )
            .await?;
        let body = self.success(response, "creating PS Bitcoin collection")?;
        Ok(CollectionHandle {
            collection_id: required_string(&body, "collection_id")?,
            job_id: required_string(&body, "job_id")?,
        })
    }

    pub async fn collection(&self, collection_id: &str) -> Result<HttpJson> {
        self.ps
            .get(&format!("/v1/collections/{collection_id}"))
            .await
    }

    pub async fn wait_collection_state(
        &self,
        collection_id: &str,
        expected: &[&str],
    ) -> Result<Value> {
        let started = Instant::now();
        loop {
            if let Ok(response) = self.collection(collection_id).await
                && response.status == StatusCode::OK
                && response
                    .body
                    .get("state")
                    .and_then(Value::as_str)
                    .is_some_and(|state| expected.contains(&state))
            {
                return Ok(response.body);
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(format!(
                    "PS collection {collection_id} did not reach one of {expected:?} before timeout"
                )));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    pub async fn wait_collection_transaction(&self, collection_id: &str) -> Result<Value> {
        let started = Instant::now();
        loop {
            if let Ok(response) = self.collection(collection_id).await
                && response.status == StatusCode::OK
                && response
                    .body
                    .get("legs")
                    .and_then(Value::as_array)
                    .and_then(|legs| legs.first())
                    .and_then(|leg| leg.get("transaction_id"))
                    .and_then(Value::as_str)
                    .is_some()
            {
                return Ok(response.body);
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(HarnessError::new(format!(
                    "PS collection {collection_id} did not persist a transaction before timeout"
                )));
            }
            time::sleep(POLL_INTERVAL).await;
        }
    }

    pub async fn retry_collection(&self, collection_id: &str, key_suffix: &str) -> Result<Value> {
        let response = self
            .ps
            .post(
                &format!("/v1/collections/{collection_id}/retry"),
                json!({}),
                Some(&format!("acceptance-retry-{key_suffix}")),
            )
            .await?;
        self.success(response, "retrying retained Bitcoin collection")
    }

    pub async fn ix_status(&self) -> Result<Value> {
        let response = self.ix.get("/v1/scopes/bitcoin/regtest/status").await?;
        self.success(response, "reading IX status")
    }

    pub async fn ix_events(&self) -> Result<Value> {
        let response = self.ix.get("/v1/events?limit=1000").await?;
        self.success(response, "reading IX event feed")
    }

    pub async fn ps_admin_status(&self) -> Result<Value> {
        let response = self.ps_admin.get("/v1/admin/status").await?;
        self.success(response, "reading PS administrator status")
    }

    pub async fn core_verbose_transaction(&self, transaction_id: &str) -> Result<Value> {
        self.core
            .json(
                None,
                &[
                    "getrawtransaction".to_owned(),
                    transaction_id.to_owned(),
                    "true".to_owned(),
                ],
            )
            .await
    }

    pub async fn invalidate_block(&self, block_hash: &str) -> Result<()> {
        self.core
            .json(None, &["invalidateblock".to_owned(), block_hash.to_owned()])
            .await?;
        Ok(())
    }

    pub async fn stop_application_services_for_restart(&mut self) -> Result<()> {
        self.supervisor.stop("ps").await?;
        self.supervisor.stop("ws").await?;
        self.supervisor.stop("ix").await?;
        Ok(())
    }

    pub async fn stop_payment_service(&mut self) -> Result<()> {
        self.supervisor.stop("ps").await
    }

    pub async fn stop_indexer_and_wallet(&mut self) -> Result<()> {
        self.supervisor.stop("ws").await?;
        self.supervisor.stop("ix").await
    }

    pub async fn start_indexer_and_wallet(&mut self) -> Result<()> {
        self.start_ix().await?;
        let ix = self.ix.clone();
        self.wait_for_service_ready("IX after restart", &ix, "/health/ready")
            .await?;
        self.wait_for_ix_phase("ready").await?;
        self.start_ws().await?;
        let ws = self.ws.clone();
        self.wait_for_service_ready("WS after restart", &ws, "/health/ready")
            .await?;
        Ok(())
    }

    pub async fn start_payment_service(&mut self) -> Result<()> {
        self.start_ps().await?;
        let ps = self.ps.clone();
        self.wait_for_service_ready("PS after restart", &ps, "/health/ready")
            .await?;
        self.wait_for_ps_ready().await?;
        Ok(())
    }

    pub async fn restart_application_services(&mut self) -> Result<()> {
        self.start_indexer_and_wallet().await?;
        self.start_payment_service().await
    }

    pub async fn restart_core(&mut self) -> Result<()> {
        if !self.core.succeeds(None, &["stop".to_owned()]).await? {
            return Err(HarnessError::new("Bitcoin Core stop RPC failed"));
        }
        self.supervisor.wait_after_external_stop("core").await?;
        self.core_authorization = None;
        self.start_core().await?;
        self.wait_for_core().await?;
        self.refresh_core_authorization()?;
        self.verify_live_core().await?;
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<Option<PathBuf>> {
        let stop_result = self.supervisor.stop_all().await;
        let log_result = self
            .supervisor
            .write_sanitized_logs(&self.config.case_artifacts.join("logs"), &self.redactor);
        let retained = if self.config.keep_workdir {
            self.root.take().map(TempDir::keep)
        } else {
            None
        };
        stop_result?;
        log_result?;
        Ok(retained)
    }
}

fn environment_entries<const N: usize>(
    entries: [(&'static str, String); N],
) -> Vec<(OsString, OsString)> {
    entries
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context(|| format!("JSON response field {field} is missing or not a string"))
}

fn format_satoshis_as_btc(satoshis: u64) -> String {
    format!("{}.{:08}", satoshis / 100_000_000, satoshis % 100_000_000)
}

pub fn unix_timestamp() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context(|| "system clock precedes the Unix epoch".to_owned())
        .map(|duration| duration.as_secs())
}

pub async fn verify_core_binary(binary: &Path, expected_label: &str) -> Result<String> {
    if !binary.is_file() {
        return Err(HarnessError::new(format!(
            "{expected_label} binary does not exist at {}",
            binary.display()
        )));
    }
    let mut command = Command::new(binary);
    command.env_clear();
    let mut version_datadir = None;
    if expected_label == "bitcoind" {
        let datadir = tempfile::Builder::new()
            .prefix("payment-sdk-btc-version-")
            .tempdir()
            .context(|| "creating private Core version-check datadir".to_owned())?;
        command
            .arg("-nosettings")
            .arg(format!("-datadir={}", datadir.path().display()));
        version_datadir = Some(datadir);
    }
    let output = command
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .await
        .context(|| format!("reading {expected_label} version"))?;
    drop(version_datadir);
    if !output.status.success() {
        return Err(HarnessError::new(format!(
            "{expected_label} --version exited with {}",
            output.status
        )));
    }
    let version = String::from_utf8(output.stdout)
        .context(|| format!("decoding {expected_label} version output"))?;
    let first_line = version.lines().next().unwrap_or_default().trim().to_owned();
    if !first_line.contains("31.1.0") {
        return Err(HarnessError::new(format!(
            "{expected_label} must be exactly Bitcoin Core 31.1.0; found {first_line}"
        )));
    }
    Ok(first_line)
}

#[cfg(test)]
mod tests {
    use super::{Ports, format_satoshis_as_btc};

    #[test]
    fn satoshi_format_is_exact_and_never_uses_floating_point() {
        assert_eq!(format_satoshis_as_btc(1), "0.00000001");
        assert_eq!(format_satoshis_as_btc(250_000), "0.00250000");
        assert_eq!(format_satoshis_as_btc(100_000_000), "1.00000000");
    }

    #[test]
    fn allocated_ports_are_loopback_ephemeral_and_unique() {
        let ports = Ports::allocate().expect("loopback port allocation must succeed");
        let values = [
            ports.core_rpc,
            ports.ix_http,
            ports.ix_metrics,
            ports.custody_http,
            ports.custody_metrics,
            ports.ws_http,
            ports.ws_metrics,
            ports.ps_http,
            ports.ps_metrics,
        ];
        let unique = values
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), values.len());
        assert!(values.into_iter().all(|port| port > 0));
    }
}
