use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use base::{
    Address, Addresser, Broadcaster, Decimal, Signer, TransactionBuilder, TransactionError,
    TransactionRestore, TransactionSnapshot,
};
use deposits::{
    BatchJob, BatchParticipant, CollectionAllocation, CollectionCreator, CollectionHistory,
    CollectionId, CollectionLegKind, CollectionLegState, CollectionMode, CollectionPlan,
    CollectionReader, CommandIdentity, CommandOperation, CommandPrincipal, ConfirmLeg, CreateBatch,
    CreateLeg, DepositCreator, DepositId, DepositPlan, IdempotencyKey, JobCommands, JobId,
    JobPayload, JobPlan, KeyId, LegOutcome, OpenDeposit, PaymentStore, PolicyIdentity, RequestHash,
    ResourceId, ResourceProof, SpendResource, User, UserId, UserStore,
};
use indexing::{
    AssetId, BlockHeight, CanonicalAddress, ChainId, ConfirmationPolicy, IndexError, IndexScope,
    TransactionRef, UnwatchOutcome, UnwatchRequest, WatchId, WatchReceipt, WatchRequest, Watcher,
};
use payment_api::{Clock, DepositWallets, GasWallet, Sweeps, sweep_routes};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;
use wallets::{
    AddressEncoding, AddressFormat, AddressText, AmountFormat, BalanceReader, Collector,
    Error as WalletError, FutureResult, HistoryReader, HistoryRequest, PreparedCollection,
    SelectedOutput, TransactionFactory, Wallet,
};

struct Wallets {
    prepared: base::SignedTransaction,
    prepares: Arc<AtomicUsize>,
    broadcasts: Arc<Mutex<Vec<Vec<u8>>>>,
    fail_first: Arc<AtomicUsize>,
    transfers: Arc<Mutex<Vec<(Address, Decimal)>>>,
    decimals: u32,
    sweep_amount: Decimal,
}

impl DepositWallets for Wallets {
    fn wallet<'a>(&'a self, deposit: &'a deposits::Deposit) -> FutureResult<'a, Arc<dyn Wallet>> {
        let address = deposit.address.value.clone();
        let wallet = FixtureWallet {
            address,
            prepared: self.prepared.clone(),
            prepares: self.prepares.clone(),
            broadcasts: self.broadcasts.clone(),
            fail_first: self.fail_first.clone(),
            transfers: self.transfers.clone(),
            decimals: self.decimals,
            sweep_amount: self.sweep_amount.clone(),
        };
        Box::pin(async move { Ok(Arc::new(wallet) as Arc<dyn Wallet>) })
    }
}

struct FixtureWallet {
    address: String,
    prepared: base::SignedTransaction,
    prepares: Arc<AtomicUsize>,
    broadcasts: Arc<Mutex<Vec<Vec<u8>>>>,
    fail_first: Arc<AtomicUsize>,
    transfers: Arc<Mutex<Vec<(Address, Decimal)>>>,
    decimals: u32,
    sweep_amount: Decimal,
}

impl Addresser for FixtureWallet {
    fn address(&self) -> Address {
        Address::new(self.address.as_bytes())
    }
}

impl Signer for FixtureWallet {
    fn sign<'a>(&'a self, _: base::SignRequest) -> base::SignFuture<'a> {
        Box::pin(async { unreachable!("collection fixture signs through its collector") })
    }
}

impl AddressFormat for FixtureWallet {
    fn address_text(&self, address: &Address) -> Result<AddressText, WalletError> {
        Ok(AddressText::new(AddressEncoding::Hex, address.to_string()))
    }

    fn parse_address(&self, address: &AddressText) -> Result<Address, WalletError> {
        Ok(Address::new(address.text.as_bytes()))
    }
}

impl BalanceReader for FixtureWallet {
    fn balance<'a>(&'a self) -> FutureResult<'a, wallets::Balance> {
        Box::pin(async { unreachable!("collection fixture does not read balance") })
    }
}

impl AmountFormat for FixtureWallet {
    fn display_amount(&self, atomic: &Decimal) -> Result<Decimal, WalletError> {
        let units = atomic.to_atomic(0).map_err(|error| {
            WalletError::new(wallets::ErrorKind::InvalidAmount, error.to_string())
        })?;
        Ok(Decimal::from_atomic(units, self.decimals))
    }
}

impl TransactionFactory for FixtureWallet {
    fn transaction(&self) -> Box<dyn TransactionBuilder> {
        Box::new(FixtureBuilder {
            prepared: self.prepared.clone(),
            prepares: self.prepares.clone(),
            transfers: self.transfers.clone(),
        })
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

impl wallets::CollectionFactory for FixtureWallet {
    fn collector(&self) -> Option<Box<dyn Collector>> {
        Some(Box::new(FixtureCollector {
            prepared: self.prepared.clone(),
            prepares: self.prepares.clone(),
            sources: Vec::new(),
            destination: None,
        }))
    }
}

impl wallets::Sweeper for FixtureWallet {
    fn sweep<'a>(&'a self, destination: Address) -> FutureResult<'a, PreparedCollection> {
        Box::pin(async move {
            self.transfers
                .lock()
                .expect("transfer mutex")
                .push((destination, self.sweep_amount.clone()));
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Ok(PreparedCollection {
                transaction: self.prepared.clone(),
                fee: wallets::PreparedFee::Limit(Decimal::from(3_u64)),
            })
        })
    }
}

struct FixtureBuilder {
    prepared: base::SignedTransaction,
    prepares: Arc<AtomicUsize>,
    transfers: Arc<Mutex<Vec<(Address, Decimal)>>>,
}

impl base::BuilderCast for FixtureBuilder {
    fn utxo(&mut self) -> Option<&mut dyn base::UtxoBuilder> {
        None
    }
}

impl TransactionBuilder for FixtureBuilder {
    fn transfer(&mut self, destination: Address, amount: Decimal) -> Result<(), TransactionError> {
        self.transfers
            .lock()
            .expect("transfer mutex")
            .push((destination, amount));
        Ok(())
    }

    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
        Ok(TransactionSnapshot::new(
            "fixture.transfer.v1",
            serde_json::json!({}),
        ))
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> base::TransactionFuture<'a, Result<base::SignedTransaction, TransactionError>> {
        Box::pin(async move {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Ok(self.prepared.clone())
        })
    }
}

impl TransactionRestore for FixtureWallet {
    fn restore(
        &self,
        _: &TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
        unreachable!("collection fixture restores from the collection envelope")
    }
}

impl HistoryReader for FixtureWallet {
    fn history<'a>(&'a self, _: HistoryRequest) -> FutureResult<'a, wallets::History> {
        Box::pin(async { unreachable!("collection fixture does not read history") })
    }
}

impl Broadcaster for FixtureWallet {
    fn broadcast<'a>(
        &'a self,
        transaction: &'a base::SignedTransaction,
    ) -> base::TransactionFuture<'a, Result<base::Submission, TransactionError>> {
        Box::pin(async move {
            self.broadcasts
                .lock()
                .expect("broadcast mutex")
                .push(transaction.envelope().as_bytes().to_vec());
            if self.fail_first.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(TransactionError::new(
                    base::TransactionErrorKind::Unavailable,
                    "simulated lost broadcast response",
                ));
            }
            Ok(base::Submission {
                id: transaction.id().clone(),
            })
        })
    }
}

struct FixtureCollector {
    prepared: base::SignedTransaction,
    prepares: Arc<AtomicUsize>,
    sources: Vec<Vec<SelectedOutput>>,
    destination: Option<Address>,
}

impl Collector for FixtureCollector {
    fn source(
        &mut self,
        _: Arc<dyn Wallet>,
        outputs: Vec<SelectedOutput>,
    ) -> Result<(), WalletError> {
        self.sources.push(outputs);
        Ok(())
    }

    fn destination(&mut self, address: Address) -> Result<(), WalletError> {
        self.destination = Some(address);
        Ok(())
    }

    fn prepare<'a>(&'a mut self) -> FutureResult<'a, PreparedCollection> {
        Box::pin(async move {
            assert_eq!(self.sources.len(), 2);
            assert_eq!(self.sources[0][0].output.index, 0);
            assert_eq!(self.sources[1][0].output.index, 1);
            assert_eq!(
                self.destination.as_ref().expect("destination").as_bytes(),
                b"master"
            );
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Ok(PreparedCollection {
                transaction: self.prepared.clone(),
                fee: wallets::PreparedFee::Exact(Decimal::from(3_u64)),
            })
        })
    }
}

struct Indexer {
    requests: Mutex<Vec<WatchRequest>>,
}

struct FixedClock(u64);

impl Clock for FixedClock {
    fn now(&self) -> u64 {
        self.0
    }
}

impl Watcher for Indexer {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> indexing::BoxFuture<'a, Result<WatchReceipt, IndexError>> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("watch mutex")
                .push(request.clone());
            Ok(WatchReceipt {
                id: WatchId("collection-watch".to_owned()),
                scope: request.scope,
                selector: request.selector,
                start_height: request.start_height,
                registered_at: None,
                inactive_from: None,
                confirmation_policy: ConfirmationPolicy {
                    minimum_confirmations: 2,
                    require_chain_finality: false,
                },
            })
        })
    }

    fn unwatch<'a>(
        &'a self,
        _: UnwatchRequest,
    ) -> indexing::BoxFuture<'a, Result<UnwatchOutcome, IndexError>> {
        Box::pin(async { Ok(UnwatchOutcome::Deactivated) })
    }
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("fixture".to_owned()),
        network: "test".to_owned(),
    }
}

fn asset() -> AssetId {
    AssetId {
        chain: scope().chain,
        asset: "native".to_owned(),
    }
}

fn resource(transaction: &str, index: u32, amount: u64) -> SpendResource {
    SpendResource {
        id: ResourceId {
            transaction_id: TransactionRef {
                scope: scope(),
                value: transaction.to_owned(),
            },
            output_index: index,
        },
        amount: Decimal::from(amount),
        evidence: ResourceProof::new(vec![index as u8 + 1]).expect("valid evidence"),
    }
}

#[tokio::test]
async fn resumes_the_exact_signed_batch_after_a_lost_broadcast_response()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let store = Arc::new(PaymentStore::new(RocksDb::open(directory.path())?));
    let owner = CommandPrincipal("exchange".to_owned());
    let mut created = Vec::new();
    for (position, amount) in [100_u64, 300].into_iter().enumerate() {
        let user = UserId(format!("user-{position}"));
        let deposit = DepositId(format!("deposit-{position}"));
        store
            .ensure_user(User {
                id: user.clone(),
                owner: owner.clone(),
                first_seen_at: 1,
            })
            .await?;
        created.push(
            store
                .create_with_ledger(OpenDeposit {
                    deposit: DepositPlan {
                        id: deposit,
                        idempotency_key: IdempotencyKey(format!("open-{position}")),
                        user_id: user,
                        asset: asset(),
                        address: CanonicalAddress {
                            scope: scope(),
                            value: format!("source-{position}"),
                        },
                        key: KeyId::Identifier(format!("key-{position}")),
                        key_purpose: "collection-test".to_owned(),
                        expected: Decimal::from(amount),
                        birthday: BlockHeight(5 + position as u64),
                        expires_at: 1_000,
                        created_at: 1,
                    },
                    ledger_recorded_at: 1,
                })
                .await?,
        );
    }
    let collection_id = CollectionId("batch".to_owned());
    let job_id = JobId("job".to_owned());
    store
        .create_or_replay(JobPlan {
            id: job_id.clone(),
            command: CommandIdentity {
                principal: owner.clone(),
                operation: CommandOperation::CollectionPlan,
                client_key: IdempotencyKey("collect".to_owned()),
                request_hash: RequestHash([7; 32]),
            },
            payload: JobPayload::CreateBatch(BatchJob {
                collection_id: collection_id.clone(),
                deposit_ids: created
                    .iter()
                    .map(|value| value.deposit.id.clone())
                    .collect(),
            }),
            user_owner: owner,
            policy: PolicyIdentity {
                version: "v1".to_owned(),
                digest: [9; 32],
            },
            created_at: 2,
        })
        .await?;
    let participants = created
        .iter()
        .enumerate()
        .map(|(position, value)| BatchParticipant {
            user_id: value.deposit.user_id.clone(),
            deposit_id: value.deposit.id.clone(),
            expected_ledger_head: value.ledger.id.clone(),
            reservation_amount: Decimal::from(if position == 0 { 100_u64 } else { 300 }),
            spend_resources: vec![resource(
                &format!("funding-{position}"),
                position as u32,
                if position == 0 { 100 } else { 300 },
            )],
        })
        .collect();
    store
        .create_or_replay_utxo_batch(CreateBatch {
            id: collection_id.clone(),
            job_id,
            asset: asset(),
            destination: CanonicalAddress {
                scope: scope(),
                value: "master".to_owned(),
            },
            policy: PolicyIdentity {
                version: "v1".to_owned(),
                digest: [9; 32],
            },
            participants,
            leg: CreateLeg {
                id: deposits::LegId("sweep".to_owned()),
                kind: CollectionLegKind::Sweep,
                planned_amount: None,
            },
            created_at: 2,
        })
        .await?;

    let prepared = base::SignedTransaction::new(
        "fixture.signed.v1",
        base::TransactionId::new("batch-tx"),
        base::TransactionEnvelope::new([1, 2, 3, 4]),
    );
    let prepares = Arc::new(AtomicUsize::new(0));
    let broadcasts = Arc::new(Mutex::new(Vec::new()));
    let source = Arc::new(Wallets {
        prepared: prepared.clone(),
        prepares: prepares.clone(),
        broadcasts: broadcasts.clone(),
        fail_first: Arc::new(AtomicUsize::new(0)),
        transfers: Arc::new(Mutex::new(Vec::new())),
        decimals: 0,
        sweep_amount: Decimal::zero(),
    });
    let indexer = Arc::new(Indexer {
        requests: Mutex::new(Vec::new()),
    });
    let sweeps = Arc::new(Sweeps::new(store.clone(), source, indexer.clone(), scope()));

    sweeps
        .execute(&collection_id, 10)
        .await
        .expect_err("first broadcast response is lost");
    let signed = store
        .collection(&collection_id)
        .await?
        .expect("signed collection must remain durable");
    assert!(matches!(
        signed.legs[0].state,
        CollectionLegState::Signed { .. }
    ));

    let completed = sweeps.execute(&collection_id, 11).await?;
    assert!(matches!(
        completed.legs[0].state,
        CollectionLegState::Broadcast { .. }
    ));
    assert_eq!(
        completed.legs[0].watch_id,
        Some(WatchId("collection-watch".to_owned()))
    );
    assert_eq!(prepares.load(Ordering::SeqCst), 1);
    {
        let submitted = broadcasts.lock().expect("broadcast mutex");
        assert_eq!(submitted.as_slice(), &[vec![1, 2, 3, 4], vec![1, 2, 3, 4]]);
        let watches = indexer.requests.lock().expect("watch mutex");
        assert_eq!(watches.len(), 2);
        assert_eq!(watches[0], watches[1]);
        assert_eq!(watches[0].start_height, BlockHeight(5));
    }

    let replay = sweeps.execute(&collection_id, 12).await?;
    assert_eq!(replay, completed);
    assert_eq!(prepares.load(Ordering::SeqCst), 1);
    assert_eq!(broadcasts.lock().expect("broadcast mutex").len(), 2);
    assert_eq!(indexer.requests.lock().expect("watch mutex").len(), 2);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        axum::serve(listener, sweep_routes(sweeps, Arc::new(FixedClock(13)))).await
    });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/v1/collections/batch");
    let status: serde_json::Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(status["state"], "in_progress");
    assert_eq!(status["legs"][0]["state"], "broadcast");
    assert_eq!(status["legs"][0]["transaction_id"], "batch-tx");

    let missing_key = client.post(format!("{url}/execute")).send().await?;
    assert_eq!(missing_key.status(), reqwest::StatusCode::BAD_REQUEST);
    let replayed: serde_json::Value = client
        .post(format!("{url}/execute"))
        .header("idempotency-key", "batch")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(replayed["legs"][0]["state"], "broadcast");
    assert_eq!(prepares.load(Ordering::SeqCst), 1);
    assert_eq!(broadcasts.lock().expect("broadcast mutex").len(), 2);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn account_sweep_converts_atomic_amount_and_retries_exact_signed_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let store = Arc::new(PaymentStore::new(RocksDb::open(directory.path())?));
    let user_id = UserId("account-user".to_owned());
    let deposit_id = DepositId("account-deposit".to_owned());
    store
        .ensure_user(User {
            id: user_id.clone(),
            owner: CommandPrincipal("exchange".to_owned()),
            first_seen_at: 1,
        })
        .await?;
    store
        .create_with_ledger(OpenDeposit {
            deposit: DepositPlan {
                id: deposit_id.clone(),
                idempotency_key: IdempotencyKey("open-account".to_owned()),
                user_id: user_id.clone(),
                asset: asset(),
                address: CanonicalAddress {
                    scope: scope(),
                    value: "source".to_owned(),
                },
                key: KeyId::Identifier("account-key".to_owned()),
                key_purpose: "collection-test".to_owned(),
                expected: Decimal::from(1_250_000_u64),
                birthday: BlockHeight(7),
                expires_at: 1_000,
                created_at: 1,
            },
            ledger_recorded_at: 1,
        })
        .await?;
    let collection_id = CollectionId("account-sweep".to_owned());
    store
        .create_or_replay_collection(CollectionPlan {
            id: collection_id.clone(),
            job_id: JobId("account-job".to_owned()),
            user_id,
            deposit_id,
            mode: CollectionMode::AccountTransfer,
            asset: asset(),
            destination: CanonicalAddress {
                scope: scope(),
                value: "master".to_owned(),
            },
            policy: PolicyIdentity {
                version: "v1".to_owned(),
                digest: [5; 32],
            },
            reservation_amount: Decimal::from(1_250_000_u64),
            legs: vec![CreateLeg {
                id: deposits::LegId("sweep".to_owned()),
                kind: CollectionLegKind::Sweep,
                planned_amount: None,
            }],
            created_at: 2,
        })
        .await?;

    let prepared = base::SignedTransaction::new(
        "fixture.signed.v1",
        base::TransactionId::new("account-tx"),
        base::TransactionEnvelope::new([9, 8, 7]),
    );
    let prepares = Arc::new(AtomicUsize::new(0));
    let broadcasts = Arc::new(Mutex::new(Vec::new()));
    let transfers = Arc::new(Mutex::new(Vec::new()));
    let source = Arc::new(Wallets {
        prepared,
        prepares: prepares.clone(),
        broadcasts: broadcasts.clone(),
        fail_first: Arc::new(AtomicUsize::new(0)),
        transfers: transfers.clone(),
        decimals: 6,
        sweep_amount: "1.25".parse()?,
    });
    let indexer = Arc::new(Indexer {
        requests: Mutex::new(Vec::new()),
    });
    let sweeps = Sweeps::new(store.clone(), source, indexer.clone(), scope());

    sweeps
        .execute(&collection_id, 10)
        .await
        .expect_err("first broadcast response is lost");
    let completed = sweeps.execute(&collection_id, 11).await?;

    assert!(matches!(
        completed.legs[0].state,
        CollectionLegState::Broadcast { .. }
    ));
    assert_eq!(prepares.load(Ordering::SeqCst), 1);
    assert_eq!(
        transfers.lock().expect("transfer mutex").as_slice(),
        &[(Address::new(b"master"), "1.25".parse::<Decimal>()?)]
    );
    assert_eq!(
        broadcasts.lock().expect("broadcast mutex").as_slice(),
        &[vec![9, 8, 7], vec![9, 8, 7]]
    );
    assert_eq!(indexer.requests.lock().expect("watch mutex").len(), 2);
    let envelope = store
        .signed_envelope(&collection_id, &completed.legs[0].id)
        .await?
        .expect("account signed bytes remain durable after broadcast");
    let durable: base::SignedTransaction = serde_json::from_slice(envelope.bytes.as_bytes())?;
    assert_eq!(durable.id().as_str(), "account-tx");
    assert_eq!(durable.envelope().as_bytes(), &[9, 8, 7]);
    Ok(())
}

struct FundingWallet(Arc<dyn Wallet>);

impl GasWallet for FundingWallet {
    fn wallet<'a>(&'a self, _: &'a deposits::Collection) -> FutureResult<'a, Arc<dyn Wallet>> {
        let wallet = self.0.clone();
        Box::pin(async move { Ok(wallet) })
    }
}

fn fixture_wallet(
    address: &str,
    transaction: base::SignedTransaction,
    prepares: Arc<AtomicUsize>,
    broadcasts: Arc<Mutex<Vec<Vec<u8>>>>,
    transfers: Arc<Mutex<Vec<(Address, Decimal)>>>,
    decimals: u32,
) -> Arc<dyn Wallet> {
    Arc::new(FixtureWallet {
        address: address.to_owned(),
        prepared: transaction,
        prepares,
        broadcasts,
        fail_first: Arc::new(AtomicUsize::new(0)),
        transfers,
        decimals,
        sweep_amount: Decimal::zero(),
    })
}

#[tokio::test]
async fn token_sweep_confirms_gas_before_preparing_token_and_retries_exact_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let store = Arc::new(PaymentStore::new(RocksDb::open(directory.path())?));
    let user_id = UserId("token-user".to_owned());
    let deposit_id = DepositId("token-deposit".to_owned());
    let token = AssetId {
        chain: scope().chain,
        asset: "token".to_owned(),
    };
    let native = asset();
    store
        .ensure_user(User {
            id: user_id.clone(),
            owner: CommandPrincipal("exchange".to_owned()),
            first_seen_at: 1,
        })
        .await?;
    store
        .create_with_ledger(OpenDeposit {
            deposit: DepositPlan {
                id: deposit_id.clone(),
                idempotency_key: IdempotencyKey("open-token".to_owned()),
                user_id: user_id.clone(),
                asset: token.clone(),
                address: CanonicalAddress {
                    scope: scope(),
                    value: "deposit-address".to_owned(),
                },
                key: KeyId::Identifier("token-key".to_owned()),
                key_purpose: "collection-test".to_owned(),
                expected: Decimal::from(12_500_000_u64),
                birthday: BlockHeight(9),
                expires_at: 1_000,
                created_at: 1,
            },
            ledger_recorded_at: 1,
        })
        .await?;
    let collection_id = CollectionId("token-sweep".to_owned());
    store
        .create_or_replay_collection(CollectionPlan {
            id: collection_id.clone(),
            job_id: JobId("token-job".to_owned()),
            user_id,
            deposit_id: deposit_id.clone(),
            mode: CollectionMode::TokenWithGas,
            asset: token.clone(),
            destination: CanonicalAddress {
                scope: scope(),
                value: "master".to_owned(),
            },
            policy: PolicyIdentity {
                version: "v1".to_owned(),
                digest: [6; 32],
            },
            reservation_amount: Decimal::from(12_500_000_u64),
            legs: vec![
                CreateLeg {
                    id: deposits::LegId("gas".to_owned()),
                    kind: CollectionLegKind::GasFunding,
                    planned_amount: Some(Decimal::from(2_000_000_000_000_000_u64)),
                },
                CreateLeg {
                    id: deposits::LegId("sweep".to_owned()),
                    kind: CollectionLegKind::Sweep,
                    planned_amount: None,
                },
            ],
            created_at: 2,
        })
        .await?;

    let gas_prepares = Arc::new(AtomicUsize::new(0));
    let gas_broadcasts = Arc::new(Mutex::new(Vec::new()));
    let gas_transfers = Arc::new(Mutex::new(Vec::new()));
    let gas_transaction = base::SignedTransaction::new(
        "fixture.gas.v1",
        base::TransactionId::new("gas-tx"),
        base::TransactionEnvelope::new([1, 3, 5]),
    );
    let gas_wallet = fixture_wallet(
        "treasury",
        gas_transaction,
        gas_prepares.clone(),
        gas_broadcasts.clone(),
        gas_transfers.clone(),
        18,
    );

    let token_prepares = Arc::new(AtomicUsize::new(0));
    let token_broadcasts = Arc::new(Mutex::new(Vec::new()));
    let token_transfers = Arc::new(Mutex::new(Vec::new()));
    let token_transaction = base::SignedTransaction::new(
        "fixture.token.v1",
        base::TransactionId::new("token-tx"),
        base::TransactionEnvelope::new([2, 4, 6]),
    );
    let source = Arc::new(Wallets {
        prepared: token_transaction,
        prepares: token_prepares.clone(),
        broadcasts: token_broadcasts.clone(),
        fail_first: Arc::new(AtomicUsize::new(0)),
        transfers: token_transfers.clone(),
        decimals: 6,
        sweep_amount: "12.5".parse()?,
    });
    let indexer = Arc::new(Indexer {
        requests: Mutex::new(Vec::new()),
    });
    let sweeps = Sweeps::new(store.clone(), source, indexer.clone(), scope())
        .with_gas_wallet(Arc::new(FundingWallet(gas_wallet)));

    sweeps
        .execute(&collection_id, 10)
        .await
        .expect_err("lost gas broadcast response leaves exact signed bytes");
    let gas_broadcast = sweeps.execute(&collection_id, 11).await?;
    assert!(matches!(
        gas_broadcast.legs[0].state,
        CollectionLegState::Broadcast { .. }
    ));
    assert_eq!(token_prepares.load(Ordering::SeqCst), 0);
    let unchanged = sweeps.execute(&collection_id, 12).await?;
    assert_eq!(unchanged, gas_broadcast);
    assert_eq!(token_prepares.load(Ordering::SeqCst), 0);
    assert_eq!(gas_prepares.load(Ordering::SeqCst), 1);
    assert_eq!(
        gas_transfers.lock().expect("gas transfers").as_slice(),
        &[(
            Address::new(b"deposit-address"),
            "0.002".parse::<Decimal>()?
        )]
    );
    assert_eq!(
        gas_broadcasts.lock().expect("gas broadcasts").as_slice(),
        &[vec![1, 3, 5], vec![1, 3, 5]]
    );

    let gas_confirmed = store
        .confirm_leg(ConfirmLeg {
            collection_id: collection_id.clone(),
            leg_id: gas_broadcast.legs[0].id.clone(),
            expected: deposits::TransitionGuard {
                collection_state: gas_broadcast.state,
                leg_state: gas_broadcast.legs[0].state.clone(),
            },
            transaction_id: TransactionRef {
                scope: scope(),
                value: "gas-tx".to_owned(),
            },
            allocation: None,
            confirmed_at: 13,
        })
        .await?;
    assert!(matches!(
        gas_confirmed.legs[0].state,
        CollectionLegState::Confirmed { .. }
    ));

    sweeps
        .execute(&collection_id, 14)
        .await
        .expect_err("lost token broadcast response leaves exact signed bytes");
    let token_broadcast = sweeps.execute(&collection_id, 15).await?;
    assert!(matches!(
        token_broadcast.legs[1].state,
        CollectionLegState::Broadcast { .. }
    ));
    assert_eq!(token_prepares.load(Ordering::SeqCst), 1);
    assert_eq!(
        token_transfers.lock().expect("token transfers").as_slice(),
        &[(Address::new(b"master"), "12.5".parse::<Decimal>()?)]
    );
    assert_eq!(
        token_broadcasts
            .lock()
            .expect("token broadcasts")
            .as_slice(),
        &[vec![2, 4, 6], vec![2, 4, 6]]
    );
    let completed = store
        .confirm_leg(ConfirmLeg {
            collection_id: collection_id.clone(),
            leg_id: token_broadcast.legs[1].id.clone(),
            expected: deposits::TransitionGuard {
                collection_state: token_broadcast.state,
                leg_state: token_broadcast.legs[1].state.clone(),
            },
            transaction_id: TransactionRef {
                scope: scope(),
                value: "token-tx".to_owned(),
            },
            allocation: Some(CollectionAllocation {
                deposit_id,
                asset: token,
                gross_debit: Decimal::from(12_500_000_u64),
                master_credit: Decimal::from(12_500_000_u64),
                allocated_fee_asset: native,
                allocated_fee: Decimal::from(21_000_u64),
            }),
            confirmed_at: 16,
        })
        .await?;
    assert_eq!(completed.state, deposits::CollectionState::Completed);
    assert_eq!(
        completed.legs[1]
            .allocation
            .as_ref()
            .expect("sweep allocation")
            .allocated_fee,
        Decimal::from(21_000_u64)
    );
    assert_ne!(
        completed.legs[1]
            .allocation
            .as_ref()
            .expect("sweep allocation")
            .asset,
        completed.legs[1]
            .allocation
            .as_ref()
            .expect("sweep allocation")
            .allocated_fee_asset
    );
    assert_eq!(indexer.requests.lock().expect("watch mutex").len(), 4);
    Ok(())
}
