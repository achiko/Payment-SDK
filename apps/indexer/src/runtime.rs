use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::Router;
use chain_bitcoin::BlockInterpreter as BitcoinBlockInterpreter;
use chain_ethereum::{BlockInterpreter as EthereumBlockInterpreter, HeadsClient, HeadsConfig};
use http::server::{
    AuthenticationMode, BearerToken, Config, HealthState, RequestLimits, TransportSecurity,
};
use indexing::{
    BlockHash, BlockHeight, BlockRef, BlockSource, IndexChanges, IndexUndo, SourceError,
    SyncStatus, WatchSelector,
};
#[cfg(test)]
use indexing::{BlockInterpreter as _, SyncPhase};
use indexing_rocksdb::Runtime as IndexRuntime;
use tokio::{sync::watch, task::JoinSet};

use crate::{
    api::{self, State},
    config::{BitcoinServe, EthereumServe, bitcoin_bootstrap_height, bootstrap_height},
};

mod backup;
mod connect;
mod error;
mod lifecycle;
mod rebuild;
mod sync;

pub use backup::backup;
use connect::{
    bitcoin_repository, bitcoin_repository_config, connect_bitcoin_source, connect_source,
    repository, repository_config,
};
use error::{AppError, AppResult, failure};
use lifecycle::{readiness_loop, shutdown_signal, supervise_websocket};
pub use rebuild::{
    abort_bitcoin_rebuild, abort_rebuild, cleanup, cleanup_bitcoin, rebuild, rebuild_bitcoin,
};
use sync::sync_loop;

struct SyncRuntime<S, I> {
    indexer: IndexRuntime<SharedSource<S>, I>,
    snapshot: SharedOperationalSnapshot,
    poll_interval: Duration,
}

struct RuntimeConfig {
    http_bind: std::net::SocketAddr,
    authentication_mode: AuthenticationMode,
    bearer_token: Option<String>,
    ready_max_lag: u64,
    ready_max_age: Duration,
}

enum WakeSource {
    PollOnly,
    EthereumNewHeads(String),
}

struct SharedSource<S> {
    source: Arc<S>,
}

impl<S> Clone for SharedSource<S> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
        }
    }
}

impl<S> SharedSource<S> {
    fn new(source: Arc<S>) -> Self {
        Self { source }
    }
}

impl<S> BlockSource for SharedSource<S>
where
    S: BlockSource,
{
    type Block = S::Block;

    fn tip<'a>(&'a self) -> indexing::BoxFuture<'a, Result<BlockRef, SourceError>> {
        self.source.tip()
    }

    fn block_at<'a>(
        &'a self,
        height: BlockHeight,
    ) -> indexing::BoxFuture<'a, Result<Self::Block, SourceError>> {
        self.source.block_at(height)
    }

    fn canonical_hash<'a>(
        &'a self,
        height: BlockHeight,
    ) -> indexing::BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
        self.source.canonical_hash(height)
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct OperationalSnapshot {
    pub(super) status: Option<SyncStatus>,
    pub(super) last_reconciled: Option<Instant>,
}

pub(super) type SharedOperationalSnapshot = Arc<Mutex<OperationalSnapshot>>;

pub async fn serve(options: EthereumServe) -> AppResult<()> {
    serve_until(options, async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| Box::new(error) as AppError)
    })
    .await
}

pub(crate) async fn serve_until<F>(options: EthereumServe, shutdown: F) -> AppResult<()>
where
    F: Future<Output = AppResult<()>>,
{
    tokio::pin!(shutdown);
    options.validate()?;
    let scope = options.repository.scope()?;
    let source =
        match startup_or_shutdown(connect_source(&scope, &options.source), shutdown.as_mut())
            .await?
        {
            Some(source) => Arc::new(source),
            None => return Ok(()),
        };
    let interpreter = EthereumBlockInterpreter::new(scope.clone())?;
    let shared_source = SharedSource::new(Arc::clone(&source));
    let indexer = IndexRuntime::open(
        &options.repository.database.database_path,
        repository_config(&options.repository)?,
        shared_source.clone(),
        interpreter.clone(),
    )?;
    let handle = indexer.handle();

    let limits = RequestLimits::default();
    let api_state = Arc::new(State::new(
        scope.clone(),
        Arc::new(handle),
        bootstrap_height(&options.repository),
    ));
    let health = HealthState::new(false);
    let snapshot = Arc::new(Mutex::new(OperationalSnapshot::default()));
    let runtime = SyncRuntime {
        indexer,
        snapshot: Arc::clone(&snapshot),
        poll_interval: options.poll_interval(),
    };
    let wake_source = if let Some(url) = options.source.rpc_ws_url.clone() {
        WakeSource::EthereumNewHeads(url)
    } else {
        WakeSource::PollOnly
    };
    tracing::info!(
        network = %options.repository.network,
        authentication_mode = %options.authentication_mode,
        "Indexer Service started"
    );
    let ready_max_age = options.ready_max_age();
    run_service_until(
        runtime,
        api::router(api_state),
        health,
        limits,
        RuntimeConfig {
            http_bind: options.http_bind,
            authentication_mode: options.authentication_mode,
            bearer_token: options.bearer_token,
            ready_max_lag: options.ready_max_lag,
            ready_max_age,
        },
        wake_source,
        snapshot,
        shutdown.as_mut(),
    )
    .await
}

pub async fn serve_bitcoin(options: BitcoinServe) -> AppResult<()> {
    serve_bitcoin_until(options, async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| Box::new(error) as AppError)
    })
    .await
}

pub(crate) async fn serve_bitcoin_until<F>(options: BitcoinServe, shutdown: F) -> AppResult<()>
where
    F: Future<Output = AppResult<()>>,
{
    tokio::pin!(shutdown);
    options.validate()?;
    let scope = options.repository.scope()?;
    let network = options.repository.network()?;
    let source = match startup_or_shutdown(
        connect_bitcoin_source(&scope, network, &options.source),
        shutdown.as_mut(),
    )
    .await?
    {
        Some(source) => Arc::new(source),
        None => return Ok(()),
    };
    let interpreter = BitcoinBlockInterpreter::new(scope.clone(), network)?;
    let shared_source = SharedSource::new(Arc::clone(&source));
    let indexer = IndexRuntime::open(
        &options.repository.database.database_path,
        bitcoin_repository_config(&options.repository)?,
        shared_source.clone(),
        interpreter.clone(),
    )?;
    let handle = indexer.handle();
    let limits = RequestLimits::default();
    let utxo_repository: Arc<dyn indexing::OutputQuery> = Arc::new(handle.outputs());
    let health = HealthState::new(false);
    let api_state = Arc::new(State::new_bitcoin(
        scope.clone(),
        network,
        Arc::new(handle),
        utxo_repository,
        health.clone(),
        bitcoin_bootstrap_height(&options.repository),
    ));
    let snapshot = Arc::new(Mutex::new(OperationalSnapshot::default()));
    let runtime = SyncRuntime {
        indexer,
        snapshot: Arc::clone(&snapshot),
        poll_interval: options.poll_interval(),
    };

    tracing::info!(
        network = %options.repository.network,
        authentication_mode = %options.authentication_mode,
        "Bitcoin Indexer Service started"
    );
    let ready_max_age = options.ready_max_age();
    run_service_until(
        runtime,
        api::router(api_state),
        health,
        limits,
        RuntimeConfig {
            http_bind: options.http_bind,
            authentication_mode: options.authentication_mode,
            bearer_token: options.bearer_token,
            ready_max_lag: options.ready_max_lag,
            ready_max_age,
        },
        WakeSource::PollOnly,
        snapshot,
        shutdown.as_mut(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_service_until<S, I, F>(
    runtime: SyncRuntime<S, I>,
    api_router: Router,
    health: HealthState,
    limits: RequestLimits,
    options: RuntimeConfig,
    wake_source: WakeSource,
    snapshot: SharedOperationalSnapshot,
    shutdown: Pin<&mut F>,
) -> AppResult<()>
where
    S: BlockSource + 'static,
    I: indexing::BlockInterpreter<
            Block = S::Block,
            Target = WatchSelector,
            Effect = IndexChanges,
            Undo = IndexUndo,
        > + Clone
        + Send
        + Sync
        + 'static,
    F: Future<Output = AppResult<()>>,
{
    let bearer_token = if options.authentication_mode.is_strict() {
        options
            .bearer_token
            .as_deref()
            .map(BearerToken::new)
            .transpose()?
    } else {
        tracing::warn!(
            authentication_mode = %options.authentication_mode,
            ignored_bearer_variable = "IX_BEARER_TOKEN",
            "STRICT AUTHENTICATION IS DISABLED: every reachable Indexer caller is globally trusted"
        );
        None
    };
    let security = if options.http_bind.ip().is_loopback() {
        TransportSecurity::PlaintextLoopback
    } else {
        TransportSecurity::TlsTerminatedUpstream
    };
    let server_config = Config::new(options.http_bind, security, bearer_token, limits)
        .with_authentication_mode(options.authentication_mode);
    let application_router =
        http::server::service_router(api_router, &server_config, health.clone())?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = JoinSet::<AppResult<()>>::new();

    let api_shutdown = shutdown_rx.clone();
    tasks.spawn(async move {
        http::server::serve(
            application_router,
            &server_config,
            shutdown_signal(api_shutdown),
        )
        .await
        .map_err(|error| Box::new(error) as AppError)
    });
    let (wake_tx, wake_rx) = tokio::sync::mpsc::channel(1);
    let sync_shutdown = shutdown_rx.clone();
    tasks.spawn(sync_loop(runtime, wake_rx, sync_shutdown));

    let readiness_shutdown = shutdown_rx.clone();
    tasks.spawn(readiness_loop(
        health,
        snapshot,
        options.ready_max_lag,
        options.ready_max_age,
        readiness_shutdown,
    ));

    match wake_source {
        WakeSource::PollOnly => {}
        WakeSource::EthereumNewHeads(url) => {
            let websocket_shutdown = shutdown_rx.clone();
            let websocket_wake_tx = wake_tx.clone();
            tasks.spawn(async move {
                let client = HeadsClient::new(HeadsConfig::new(Some(url), Duration::from_secs(1))?);
                let websocket = client.run_with_events(
                    move |_| {
                        let _ignored = websocket_wake_tx.try_send(());
                        true
                    },
                    |_| {},
                );
                supervise_websocket(websocket, websocket_shutdown).await
            });
        }
    }

    supervise_tasks_until(&mut tasks, shutdown_tx, wake_tx, shutdown).await
}

async fn startup_or_shutdown<T, F, S>(startup: F, shutdown: Pin<&mut S>) -> AppResult<Option<T>>
where
    F: Future<Output = AppResult<T>>,
    S: Future<Output = AppResult<()>>,
{
    tokio::select! {
        biased;
        result = shutdown => {
            result?;
            Ok(None)
        }
        result = startup => result.map(Some),
    }
}

async fn supervise_tasks_until<F>(
    tasks: &mut JoinSet<AppResult<()>>,
    shutdown_tx: watch::Sender<bool>,
    wake_sender_guard: tokio::sync::mpsc::Sender<()>,
    shutdown: Pin<&mut F>,
) -> AppResult<()>
where
    F: Future<Output = AppResult<()>>,
{
    let termination = tokio::select! {
        result = shutdown => {
            result?;
            None
        }
        result = tasks.join_next() => Some(result),
    };
    let _ignored = shutdown_tx.send(true);
    drop(wake_sender_guard);

    let shutdown_deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(shutdown_deadline);
    while !tasks.is_empty() {
        tokio::select! {
            _ = &mut shutdown_deadline => {
                tasks.abort_all();
                break;
            }
            result = tasks.join_next() => {
                match result {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(error))) => tracing::error!(error = %error, "service task failed during shutdown"),
                    Some(Err(error)) => tracing::error!(error = %error, "service task panicked during shutdown"),
                    None => break,
                }
            }
        }
    }

    match termination {
        None => Ok(()),
        Some(Some(Ok(Ok(())))) => Err(failure("a supervised service task stopped unexpectedly")),
        Some(Some(Ok(Err(error)))) => Err(error),
        Some(Some(Err(error))) => Err(Box::new(error)),
        Some(None) => Err(failure("service supervisor has no running tasks")),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll},
    };

    use bitcoin::{
        Address, Amount, CompressedPublicKey, OutPoint, PublicKey, ScriptBuf, Sequence,
        Transaction, TxIn, TxOut, Txid, Witness, absolute, consensus, hashes::Hash,
        hex::DisplayHex, transaction::Version,
    };
    use chain_bitcoin::{
        BitcoinAddress, Block, BlockInterpreter as BitcoinBlockInterpreter, Network,
    };
    use indexing::{CanonicalAddress, ChainId};
    use indexing::{ConfirmationPolicy, IndexScope, WatchId, WatchSelector, WatchTarget};
    use serde_json::{Number, Value, json};

    use super::lifecycle::readiness;
    use super::*;

    struct PendingWebsocket {
        dropped: Arc<AtomicBool>,
    }

    impl Future for PendingWebsocket {
        type Output = Result<(), SourceError>;

        fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingWebsocket {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn status(phase: SyncPhase, checkpoint: u64, tip: u64) -> SyncStatus {
        SyncStatus {
            scope: IndexScope {
                chain: ChainId(chain_ethereum::CHAIN.to_owned()),
                network: "test".to_owned(),
            },
            checkpoint: Some(BlockRef {
                height: BlockHeight(checkpoint),
                hash: BlockHash(vec![1; 32]),
                parent_hash: None,
                timestamp: None,
            }),
            observed_tip: Some(BlockRef {
                height: BlockHeight(tip),
                hash: BlockHash(vec![2; 32]),
                parent_hash: None,
                timestamp: None,
            }),
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: 12,
                require_chain_finality: false,
            },
            phase,
            rebuild_reason: None,
            halted_reason: None,
        }
    }

    fn regtest_p2wpkh_address(prefix: u8) -> Address {
        let public_key = PublicKey::from_slice(&[
            prefix, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        Address::p2wpkh(
            &CompressedPublicKey::try_from(public_key).expect("test public key must be compressed"),
            bitcoin::Network::Regtest,
        )
    }

    fn bitcoin_amount(satoshis: u64) -> Value {
        let whole = satoshis / 100_000_000;
        let remainder = satoshis % 100_000_000;
        let lexical = if remainder == 0 {
            whole.to_string()
        } else {
            format!("{whole}.{remainder:08}")
                .trim_end_matches('0')
                .to_owned()
        };
        Value::Number(Number::from_str(&lexical).expect("test BTC amount must encode"))
    }

    #[test]
    fn readiness_requires_phase_lag_and_recent_reconciliation() {
        let mut snapshot = OperationalSnapshot {
            status: Some(status(SyncPhase::Ready, 10, 12)),
            last_reconciled: Some(Instant::now()),
        };
        assert!(readiness(&snapshot, 2, Duration::from_secs(30)).0);
        snapshot.status = Some(status(SyncPhase::CatchingUp, 10, 12));
        assert!(!readiness(&snapshot, 2, Duration::from_secs(30)).0);
        snapshot.status = Some(status(SyncPhase::Ready, 9, 12));
        assert!(!readiness(&snapshot, 2, Duration::from_secs(30)).0);
    }

    #[test]
    fn bitcoin_backfill_keeps_watched_fact_and_drops_unrelated_conditional_spend() {
        let scope = IndexScope {
            chain: ChainId(chain_bitcoin::CHAIN.to_owned()),
            network: "regtest".to_owned(),
        };
        let watched_native = regtest_p2wpkh_address(0x02);
        let unrelated_native = regtest_p2wpkh_address(0x03);
        let watched = BitcoinAddress::from_encoded(watched_native.to_string());
        let selector = WatchSelector::Address(CanonicalAddress {
            scope: scope.clone(),
            value: watched.to_string(),
        });
        let watch = WatchTarget {
            id: WatchId("historical-address".to_owned()),
            scope: scope.clone(),
            selector: selector.clone(),
            target: selector,
            idempotency_key: "historical-address-key".to_owned(),
            start_height: BlockHeight(0),
            registered_at: None,
            inactive_from: None,
        };
        let coinbase = Transaction {
            version: Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 1]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: watched_native.script_pubkey(),
            }],
        };
        let unrelated_spend = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([7; 32]), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(19_000),
                script_pubkey: ScriptBuf::new_op_return([]),
            }],
        };
        let block_hash = bitcoin::BlockHash::from_byte_array([0xaa; 32]);
        let parent_hash = bitcoin::BlockHash::from_byte_array([0xbb; 32]);
        let raw_block = serde_json::to_vec(&json!({
            "hash": block_hash.to_string(),
            "height": 10,
            "previousblockhash": parent_hash.to_string(),
            "time": 100,
            "nTx": 2,
            "tx": [
                {
                    "txid": coinbase.compute_txid().to_string(),
                    "hex": consensus::serialize(&coinbase).to_lower_hex_string(),
                    "vin": [{"coinbase": "01"}],
                    "vout": [{
                        "value": bitcoin_amount(50_000),
                        "n": 0,
                        "scriptPubKey": {
                            "hex": watched_native.script_pubkey().as_bytes().to_lower_hex_string()
                        }
                    }]
                },
                {
                    "txid": unrelated_spend.compute_txid().to_string(),
                    "hex": consensus::serialize(&unrelated_spend).to_lower_hex_string(),
                    "vin": [{
                        "txid": unrelated_spend.input[0].previous_output.txid.to_string(),
                        "vout": 0,
                        "prevout": {
                            "generated": false,
                            "height": 9,
                            "value": bitcoin_amount(20_000),
                            "scriptPubKey": {
                                "hex": unrelated_native
                                    .script_pubkey()
                                    .as_bytes()
                                    .to_lower_hex_string()
                            }
                        }
                    }],
                    "vout": [{
                        "value": bitcoin_amount(19_000),
                        "n": 0,
                        "scriptPubKey": {
                            "hex": unrelated_spend.output[0]
                                .script_pubkey
                                .as_bytes()
                                .to_lower_hex_string()
                        }
                    }]
                }
            ]
        }))
        .expect("test Bitcoin block must encode");
        let block = Block::parse(
            raw_block,
            Some(BlockHeight(10)),
            Some(&BlockHash(block_hash.to_byte_array().to_vec())),
            Network::Regtest,
        )
        .expect("test Bitcoin block must parse once at its boundary");

        let interpreted = BitcoinBlockInterpreter::new(scope, Network::Regtest)
            .expect("test Bitcoin interpreter must construct")
            .inspect(&block, &[watch])
            .expect("watched fact and unrelated input must interpret");
        assert_eq!(interpreted.drafts.len(), 1);
        assert_eq!(interpreted.effect.outputs.created.len(), 1);
        assert_eq!(interpreted.effect.outputs.tracked_spends.len(), 1);

        let backfill = BitcoinBlockInterpreter::new(
            IndexScope {
                chain: ChainId(chain_bitcoin::CHAIN.to_owned()),
                network: "regtest".to_owned(),
            },
            Network::Regtest,
        )
        .expect("test Bitcoin interpreter must construct")
        .backfill_effect(interpreted.effect)
        .expect("Bitcoin backfill must discard only unrelated spend candidates");
        assert_eq!(backfill.outputs.created.len(), 1);
        assert!(backfill.outputs.tracked_spends.is_empty());
    }

    #[tokio::test]
    async fn websocket_supervisor_cancels_a_silent_stream_on_shutdown() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(supervise_websocket(
            PendingWebsocket {
                dropped: Arc::clone(&dropped),
            },
            shutdown_rx,
        ));
        tokio::task::yield_now().await;
        shutdown_tx
            .send(true)
            .expect("test shutdown receiver must remain open");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("silent WebSocket supervision must stop promptly")
            .expect("WebSocket supervision task must not panic")
            .expect("WebSocket supervision shutdown must succeed");

        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn caller_shutdown_cancels_async_startup() {
        let mut shutdown = std::future::ready(Ok(()));
        let result = startup_or_shutdown(
            std::future::pending::<AppResult<()>>(),
            Pin::new(&mut shutdown),
        )
        .await
        .expect("caller shutdown must complete cleanly");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn caller_shutdown_drains_supervised_resource_owners() {
        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut tasks = JoinSet::<AppResult<()>>::new();
        tasks.spawn(async move {
            let _resource = DropFlag(task_dropped);
            shutdown_signal(shutdown_rx).await;
            Ok(())
        });
        let (wake_tx, _wake_rx) = tokio::sync::mpsc::channel(1);
        let mut shutdown = std::future::ready(Ok(()));

        supervise_tasks_until(&mut tasks, shutdown_tx, wake_tx, Pin::new(&mut shutdown))
            .await
            .expect("caller shutdown must drain supervised tasks");

        assert!(tasks.is_empty());
        assert!(dropped.load(Ordering::Acquire));
    }
}
