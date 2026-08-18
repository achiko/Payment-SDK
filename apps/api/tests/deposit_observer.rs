use std::sync::{Arc, Mutex};

use base::Decimal;
use deposits::{
    AddressRequest, BatchJob, BatchParticipant, BoxFuture, CaseQuery, CaseReader,
    CollectionCreator, CollectionId, CollectionLegKind, CommandIdentity, CommandOperation,
    CommandPrincipal, ConsumerCheckpointName, CreateBatch, CreateLeg, DepositAddressSource,
    DepositCreator, DepositError, DepositId, DepositPlan, IdempotencyKey, JobCommands, JobId,
    JobPayload, JobPlan, KeyId, LedgerReader, OpenDeposit, PaymentStore, PolicyIdentity,
    ProgressReader, ProvisionedAddress, ReconciliationReason, RequestHash, ResourceId,
    ResourceProof, SpendResource, User, UserId, UserStore,
};
use indexing::{
    AssetId, BlockHash, BlockHeight, BlockRef, CanonicalAddress, ChainId, Checkpoint, EventCursor,
    EventId, EventPage, EventQuery, MovementId, ObservationEvent, ObservationRevision,
    ObservedTransaction, Observer, TransactionRef, TransactionStatus, UnwatchOutcome,
    UnwatchRequest, ValueMovement, WatchReceipt, WatchRequest, Watcher,
};
use payment_api::{DepositObserver, Deposits, deposit_routes};
use storage_rocksdb::RocksDb;
use tempfile::TempDir;

struct Events(Mutex<Vec<ObservationEvent>>);

struct QueryAdapters {
    scope: indexing::IndexScope,
}

impl DepositAddressSource for QueryAdapters {
    fn address<'a>(
        &'a self,
        request: AddressRequest,
    ) -> BoxFuture<'a, Result<ProvisionedAddress, DepositError>> {
        Box::pin(async move {
            assert_eq!(request.scope, self.scope);
            panic!("query-only adapter must not provision an address")
        })
    }
}

impl Checkpoint for QueryAdapters {
    fn checkpoint<'a>(
        &'a self,
        requested: &'a indexing::IndexScope,
    ) -> indexing::BoxFuture<'a, Result<Option<BlockRef>, indexing::IndexError>> {
        Box::pin(async move {
            assert_eq!(requested, &self.scope);
            panic!("query-only adapter must not read index status")
        })
    }
}

impl Watcher for QueryAdapters {
    fn watch<'a>(
        &'a self,
        request: WatchRequest,
    ) -> indexing::BoxFuture<'a, Result<WatchReceipt, indexing::IndexError>> {
        Box::pin(async move {
            assert_eq!(request.scope, self.scope);
            panic!("query-only adapter must not register a watch")
        })
    }

    fn unwatch<'a>(
        &'a self,
        request: UnwatchRequest,
    ) -> indexing::BoxFuture<'a, Result<UnwatchOutcome, indexing::IndexError>> {
        Box::pin(async move {
            assert_eq!(request.scope, self.scope);
            panic!("query-only adapter must not remove a watch")
        })
    }
}

impl Observer for Events {
    fn events<'a>(
        &'a self,
        request: EventQuery,
    ) -> indexing::BoxFuture<'a, Result<EventPage, indexing::IndexError>> {
        Box::pin(async move {
            let mut events = self
                .0
                .lock()
                .expect("event fixture mutex must not be poisoned")
                .iter()
                .filter(|event| {
                    event.transaction.scope == request.scope
                        && request.after.is_none_or(|after| event.cursor > after)
                })
                .take(request.limit)
                .cloned()
                .collect::<Vec<_>>();
            events.sort_by_key(|event| event.cursor);
            let next = events.last().map(|event| event.cursor).or(request.after);
            Ok(EventPage { events, next })
        })
    }
}

fn chain() -> ChainId {
    ChainId("fixture".to_owned())
}

fn scope() -> indexing::IndexScope {
    indexing::IndexScope {
        chain: chain(),
        network: "test".to_owned(),
    }
}

fn asset() -> AssetId {
    AssetId {
        chain: chain(),
        asset: "native".to_owned(),
    }
}

fn address() -> CanonicalAddress {
    CanonicalAddress {
        scope: scope(),
        value: "deposit-address".to_owned(),
    }
}

fn block() -> BlockRef {
    BlockRef {
        height: BlockHeight(11),
        hash: BlockHash(vec![11; 32]),
        parent_hash: None,
        timestamp: Some(1_100),
    }
}

fn event(
    cursor: u64,
    status: TransactionStatus,
    previous: Option<TransactionStatus>,
) -> ObservationEvent {
    ObservationEvent {
        id: EventId(format!("event-{cursor}")),
        cursor: EventCursor(cursor),
        watch_ids: Vec::new(),
        previous_status: previous,
        transaction: ObservedTransaction {
            scope: scope(),
            transaction_id: TransactionRef {
                scope: scope(),
                value: "transaction-1".to_owned(),
            },
            revision: ObservationRevision(cursor),
            status,
            movements: vec![ValueMovement::Transfer {
                id: MovementId("credit-1".to_owned()),
                asset: asset(),
                amount: Decimal::from(25_u64),
                from: CanonicalAddress {
                    scope: scope(),
                    value: "sender".to_owned(),
                },
                to: address(),
            }],
            fee: None,
            first_seen_at: 1_000,
            observed_at: 1_100 + cursor,
        },
    }
}

fn spend_event(
    cursor: u64,
    status: TransactionStatus,
    previous: Option<TransactionStatus>,
) -> ObservationEvent {
    let mut value = event(cursor, status, previous);
    value.id = EventId(format!("spend-{cursor}"));
    value.transaction.transaction_id.value = "unknown-spend".to_owned();
    value.transaction.movements = vec![ValueMovement::Input {
        id: MovementId("reserved-input".to_owned()),
        asset: asset(),
        amount: Decimal::from(25_u64),
        owner: Some(address()),
    }];
    value
}

#[tokio::test]
async fn restart_resumes_and_reorg_reverses_the_same_ledger()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let store = Arc::new(PaymentStore::new(RocksDb::open(directory.path())?));
    store
        .create_with_ledger(OpenDeposit {
            deposit: DepositPlan {
                id: DepositId("deposit-1".to_owned()),
                idempotency_key: IdempotencyKey("open-1".to_owned()),
                user_id: UserId("user-1".to_owned()),
                asset: asset(),
                address: address(),
                key: KeyId::Identifier("key-1".to_owned()),
                key_purpose: "test".to_owned(),
                expected: Decimal::from(25_u64),
                birthday: BlockHeight(10),
                expires_at: 2_000,
                created_at: 1_000,
            },
            ledger_recorded_at: 1_000,
        })
        .await?;
    let included = TransactionStatus::Included {
        block: block(),
        confirmations: 1,
    };
    let confirmed = TransactionStatus::Confirmed {
        block: block(),
        proof: indexing::ConfirmationProof::Depth {
            required: 2,
            observed: 2,
        },
    };
    let source = Arc::new(Events(Mutex::new(vec![event(1, included.clone(), None)])));
    let observer = DepositObserver::new(scope(), source.clone(), store.clone());
    assert_eq!(observer.pass(10, 1_101).await?.projected, 1);
    let included_head = store
        .current(&DepositId("deposit-1".to_owned()))
        .await?
        .expect("included event appends a ledger row");
    assert_eq!(included_head.balances.received, Decimal::from(25_u64));
    assert_eq!(included_head.balances.confirmed, Decimal::zero());

    source
        .0
        .lock()
        .expect("event fixture mutex must not be poisoned")
        .extend([
            event(2, confirmed.clone(), Some(included)),
            event(
                3,
                TransactionStatus::Reorged {
                    previous_block: block(),
                },
                Some(confirmed),
            ),
        ]);
    let restarted = DepositObserver::new(scope(), source, store.clone());
    assert_eq!(restarted.pass(10, 1_103).await?.projected, 2);
    let corrected = store
        .current(&DepositId("deposit-1".to_owned()))
        .await?
        .expect("reorg appends a correcting ledger row");
    assert_eq!(corrected.balances.received, Decimal::zero());
    assert_eq!(corrected.balances.confirmed, Decimal::zero());
    assert_eq!(corrected.balances.balance, Decimal::zero());
    assert_eq!(
        store
            .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
            .await?
            .cursor,
        Some(EventCursor(3))
    );
    assert_eq!(
        store
            .consumer_checkpoint(ConsumerCheckpointName::IxProjection)
            .await?
            .cursor,
        Some(EventCursor(3))
    );
    assert_eq!(restarted.pass(10, 1_104).await?.projected, 0);

    let adapters = Arc::new(QueryAdapters { scope: scope() });
    let deposits = Arc::new(Deposits::new(store, adapters.clone(), adapters, scope()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let server_address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, deposit_routes(deposits)).await });
    let client = reqwest::Client::new();
    let endpoint = format!("http://{server_address}/v1/deposits/deposit-1");
    let balance: serde_json::Value = client
        .get(format!("{endpoint}/balance"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(balance["asset"]["chain"], "fixture");
    assert_eq!(balance["asset"]["asset"], "native");
    assert_eq!(balance["entry"]["balances"]["received"], "0");
    assert_eq!(balance["entry"]["balances"]["confirmed"], "0");
    assert_eq!(balance["entry"]["balances"]["balance"], "0");

    let history: serde_json::Value = client
        .get(format!("{endpoint}/history?limit=2"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let first_page = history["entries"].as_array().expect("ledger entries");
    assert_eq!(first_page.len(), 2);
    let cursor = history["next"].as_str().expect("history cursor");
    let second: serde_json::Value = client
        .get(format!("{endpoint}/history?limit=2&after={cursor}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let second_page = second["entries"].as_array().expect("ledger entries");
    assert_eq!(second_page.len(), 2);
    assert_eq!(second["next"], serde_json::Value::Null);
    assert_eq!(second_page[0]["cause"]["status"]["kind"], "confirmed");
    assert_eq!(second_page[0]["balances"]["confirmed"], "25");
    assert_eq!(second_page[1]["cause"]["status"]["kind"], "reorged");
    assert_eq!(second_page[1]["balances"]["received"], "0");
    assert_eq!(second_page[1]["balances"]["confirmed"], "0");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn reserved_unknown_spend_opens_one_durable_case_and_reorgs_safely()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let store = Arc::new(PaymentStore::new(RocksDb::open(directory.path())?));
    let owner = CommandPrincipal("exchange".to_owned());
    let user = UserId("user-1".to_owned());
    store
        .ensure_user(User {
            id: user.clone(),
            owner: owner.clone(),
            first_seen_at: 1,
        })
        .await?;
    let created = store
        .create_with_ledger(OpenDeposit {
            deposit: DepositPlan {
                id: DepositId("deposit-1".to_owned()),
                idempotency_key: IdempotencyKey("open-1".to_owned()),
                user_id: user.clone(),
                asset: asset(),
                address: address(),
                key: KeyId::Identifier("key-1".to_owned()),
                key_purpose: "test".to_owned(),
                expected: Decimal::from(25_u64),
                birthday: BlockHeight(10),
                expires_at: 2_000,
                created_at: 1_000,
            },
            ledger_recorded_at: 1_000,
        })
        .await?;
    let included = TransactionStatus::Included {
        block: block(),
        confirmations: 1,
    };
    let source = Arc::new(Events(Mutex::new(vec![event(1, included.clone(), None)])));
    DepositObserver::new(scope(), source.clone(), store.clone())
        .pass(10, 1_101)
        .await?;
    let funded = store
        .current(&created.deposit.id)
        .await?
        .expect("funding projection has a ledger head");
    let job_id = JobId("job-1".to_owned());
    let collection_id = CollectionId("collection-1".to_owned());
    store
        .create_or_replay(JobPlan {
            id: job_id.clone(),
            command: CommandIdentity {
                principal: owner.clone(),
                operation: CommandOperation::CollectionPlan,
                client_key: IdempotencyKey("collect-1".to_owned()),
                request_hash: RequestHash([7; 32]),
            },
            payload: JobPayload::CreateBatch(BatchJob {
                collection_id: collection_id.clone(),
                deposit_ids: vec![created.deposit.id.clone()],
            }),
            user_owner: owner,
            policy: PolicyIdentity {
                version: "v1".to_owned(),
                digest: [9; 32],
            },
            created_at: 1_102,
        })
        .await?;
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
            participants: vec![BatchParticipant {
                user_id: user,
                deposit_id: created.deposit.id.clone(),
                expected_ledger_head: funded.id,
                reservation_amount: Decimal::from(25_u64),
                spend_resources: vec![SpendResource {
                    id: ResourceId {
                        transaction_id: TransactionRef {
                            scope: scope(),
                            value: "funding".to_owned(),
                        },
                        output_index: 0,
                    },
                    amount: Decimal::from(25_u64),
                    evidence: ResourceProof::new(vec![1])?,
                }],
            }],
            leg: CreateLeg {
                id: deposits::LegId("sweep".to_owned()),
                kind: CollectionLegKind::Sweep,
                planned_amount: None,
            },
            created_at: 1_102,
        })
        .await?;
    source
        .0
        .lock()
        .expect("event fixture mutex must not be poisoned")
        .push(spend_event(2, included.clone(), None));
    let observer = DepositObserver::new(scope(), source.clone(), store.clone());
    observer.pass(10, 1_103).await?;
    let cases = store
        .cases(CaseQuery {
            deposit_id: Some(created.deposit.id.clone()),
            after: None,
            limit: 10,
            open_only: true,
        })
        .await?;
    assert_eq!(cases.cases.len(), 1);
    assert_eq!(
        cases.cases[0].reason,
        ReconciliationReason::ReservedSpendConflict {
            collection_id,
            transaction_id: TransactionRef {
                scope: scope(),
                value: "unknown-spend".to_owned(),
            },
        }
    );

    source
        .0
        .lock()
        .expect("event fixture mutex must not be poisoned")
        .push(spend_event(
            3,
            TransactionStatus::Reorged {
                previous_block: block(),
            },
            Some(included),
        ));
    let restarted = DepositObserver::new(scope(), source, store.clone());
    assert_eq!(restarted.pass(10, 1_104).await?.projected, 1);
    assert_eq!(restarted.pass(10, 1_105).await?.projected, 0);
    assert_eq!(
        store
            .cases(CaseQuery {
                deposit_id: Some(created.deposit.id),
                after: None,
                limit: 10,
                open_only: true,
            })
            .await?
            .cases
            .len(),
        1
    );
    Ok(())
}
