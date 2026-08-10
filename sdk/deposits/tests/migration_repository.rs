use bincode::Encode;
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, ChainId};
use deposits::{
    CommandIdentity, CommandOperation, CommandPrincipal, CreateDeposit, CreateDepositJob,
    CreateDepositWithLedger, CreateJob, DepositErrorKind, DepositId, DepositPageRequest,
    DepositStateKind, DepositStore, EnsureUser, IdempotencyKey, InitializePaymentDatabase, JobId,
    JobPayload, JobStore, MigratePaymentDatabase, PAYMENT_DOMAIN_SCHEMA_VERSION,
    PAYMENT_SERVICE_OWNER, PaymentDatabaseMetadataStore, PersistentPaymentRepository,
    PolicyIdentity, RequestHash, UserId, UserStore,
};
use indexing::{BlockHeight, IndexScope};
use signer::KeyLocator;
use storage::{Key, Namespace, Operation, ScanRequest, Storage, Value, WriteBatch};
use storage_rocksdb::RocksDbStorage;
use tempfile::TempDir;

fn amount(value: u64) -> AtomicAmount {
    let mut bytes = [0_u8; 32];
    bytes[24..].copy_from_slice(&value.to_be_bytes());
    AtomicAmount(bytes)
}

fn scope(network: &str) -> IndexScope {
    IndexScope {
        chain: ChainId("ethereum".to_owned()),
        network: network.to_owned(),
    }
}

fn policy(version: &str, byte: u8) -> PolicyIdentity {
    PolicyIdentity {
        version: version.to_owned(),
        digest: [byte; 32],
    }
}

fn migrate_command(network: &str, policy: PolicyIdentity) -> MigratePaymentDatabase {
    MigratePaymentDatabase {
        scope: scope(network),
        active_policy: policy,
        migrated_at: 5_000,
        page_size: 10,
    }
}

fn create_deposit_job() -> CreateJob {
    let scope = scope("sepolia");
    CreateJob {
        id: JobId("legacy-create-deposit-job".to_owned()),
        command: CommandIdentity {
            principal: CommandPrincipal("exchange-backend".to_owned()),
            operation: CommandOperation::CreateDeposit,
            client_key: IdempotencyKey("exchange-order-1".to_owned()),
            request_hash: RequestHash([7; 32]),
        },
        payload: JobPayload::CreateDeposit(CreateDepositJob {
            deposit_id: DepositId("deposit-1".to_owned()),
            user_id: UserId("user-1".to_owned()),
            scope: scope.clone(),
            asset: AssetId {
                chain: scope.chain,
                asset: "native".to_owned(),
            },
            expected: amount(100),
            expires_at: 10_000,
            created_at: 1_000,
            key_purpose: "payment-service-deposit-address-v1".to_owned(),
        }),
        user_owner: CommandPrincipal("exchange-backend".to_owned()),
        policy: policy("policy-v1", 1),
        created_at: 1_000,
    }
}

fn create_deposit() -> CreateDepositWithLedger {
    CreateDepositWithLedger {
        deposit: CreateDeposit {
            id: DepositId("deposit-1".to_owned()),
            idempotency_key: IdempotencyKey("create-deposit-1".to_owned()),
            user_id: UserId("user-1".to_owned()),
            asset: AssetId {
                chain: ChainId("ethereum".to_owned()),
                asset: "native".to_owned(),
            },
            address: CanonicalAddress {
                chain: ChainId("ethereum".to_owned()),
                value: "0x1111111111111111111111111111111111111111".to_owned(),
            },
            key: KeyLocator::Identifier("legacy-key-locator".to_owned()),
            key_purpose: "payment-service-deposit-address-v1".to_owned(),
            expected: amount(100),
            birthday: BlockHeight(10),
            expires_at: 10_000,
            created_at: 1_000,
        },
        ledger_recorded_at: 1_000,
    }
}

async fn clear_namespace(
    storage: &RocksDbStorage,
    namespace: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let namespace = Namespace(namespace.to_owned());
    let page = storage
        .scan(ScanRequest {
            namespace: namespace.clone(),
            prefix: Vec::new(),
            after: None,
            limit: 100,
        })
        .await?;
    storage
        .commit(WriteBatch {
            conditions: Vec::new(),
            operations: page
                .entries
                .into_iter()
                .map(|(key, _)| Operation::Delete {
                    namespace: namespace.clone(),
                    key,
                })
                .collect(),
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn unbound_legacy_payment_rows_are_validated_then_bound()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    repository.create_or_replay(create_deposit_job()).await?;

    let report = repository
        .migrate_and_bind(migrate_command("sepolia", policy("policy-v1", 1)))
        .await?;
    assert_eq!(report.previous_domain_schema_version, None);
    assert_eq!(
        report.metadata.domain_schema_version,
        PAYMENT_DOMAIN_SCHEMA_VERSION
    );
    assert_eq!(report.metadata.scope, scope("sepolia"));
    assert_eq!(report.users, 1);
    assert_eq!(report.jobs, 1);
    assert_eq!(report.deposits, 0);

    let reopened = repository
        .initialize_or_validate(InitializePaymentDatabase {
            scope: scope("sepolia"),
            active_policy: policy("policy-v1", 1),
            initialized_at: 9_999,
        })
        .await?;
    assert_eq!(reopened, report.metadata);
    Ok(())
}

#[tokio::test]
async fn wrong_operator_scope_fails_without_binding_metadata()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    repository.create_or_replay(create_deposit_job()).await?;

    let error = repository
        .migrate_and_bind(migrate_command("mainnet", policy("policy-v1", 1)))
        .await
        .expect_err("legacy scope evidence must reject an incorrect operator assertion");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    assert_eq!(repository.database_metadata().await?, None);
    Ok(())
}

#[tokio::test]
async fn indexer_owned_database_is_never_adopted_by_migration()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let storage = RocksDbStorage::open(directory.path())?;
    storage
        .commit(WriteBatch {
            conditions: Vec::new(),
            operations: vec![Operation::Put {
                namespace: Namespace("ix.semantic.v1".to_owned()),
                key: Key(vec![1, 1]),
                value: Value(vec![1]),
            }],
        })
        .await?;
    let repository = PersistentPaymentRepository::new(storage);
    let error = repository
        .migrate_and_bind(migrate_command("sepolia", policy("policy-v1", 1)))
        .await
        .expect_err("PS migration must reject an IX-owned database");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    assert_eq!(repository.database_metadata().await?, None);
    Ok(())
}

#[tokio::test]
async fn migration_rebuilds_deposit_association_indexes_before_binding()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    repository
        .ensure_user(EnsureUser {
            id: UserId("user-1".to_owned()),
            owner: CommandPrincipal("exchange-backend".to_owned()),
            first_seen_at: 1_000,
        })
        .await?;
    repository.create_with_ledger(create_deposit()).await?;
    for namespace in [
        "ps.v1.user_deposit",
        "ps.v1.deposit_state",
        "ps.v1.user_deposit_state",
        "ps.v1.deposit_index_metadata",
    ] {
        clear_namespace(repository.storage(), namespace).await?;
    }

    let report = repository
        .migrate_and_bind(migrate_command("sepolia", policy("policy-v1", 1)))
        .await?;
    assert_eq!(report.deposits, 1);
    assert_eq!(report.ledger_entries, 1);
    assert_eq!(report.deposit_indexes_rebuilt, 1);
    let filtered = repository
        .deposits(DepositPageRequest {
            after: None,
            limit: 10,
            user_id: Some(UserId("user-1".to_owned())),
            state: Some(DepositStateKind::AwaitingWatch),
        })
        .await?;
    assert_eq!(filtered.deposits.len(), 1);
    assert_eq!(filtered.deposits[0].id, DepositId("deposit-1".to_owned()));
    Ok(())
}

#[tokio::test]
async fn current_metadata_is_never_silently_overwritten() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    let original = repository
        .initialize_or_validate(InitializePaymentDatabase {
            scope: scope("sepolia"),
            active_policy: policy("policy-v1", 1),
            initialized_at: 100,
        })
        .await?;

    let error = repository
        .migrate_and_bind(migrate_command("sepolia", policy("policy-v2", 2)))
        .await
        .expect_err("current metadata cannot be rebound by the migration command");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    assert_eq!(repository.database_metadata().await?, Some(original));
    Ok(())
}

#[derive(Encode)]
struct LegacyScopeRecordV1 {
    chain: String,
    network: String,
}

#[derive(Encode)]
struct LegacyDatabaseMetadataRecordV1 {
    record_version: u16,
    service_owner: String,
    domain_schema_version: u16,
    scope: LegacyScopeRecordV1,
    active_policy_version: String,
    active_policy_digest: [u8; 32],
    initialized_at: u64,
}

#[tokio::test]
async fn explicit_migration_upgrades_an_older_bound_schema()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let storage = RocksDbStorage::open(directory.path())?;
    let value = bincode::encode_to_vec(
        LegacyDatabaseMetadataRecordV1 {
            record_version: 1,
            service_owner: PAYMENT_SERVICE_OWNER.to_owned(),
            domain_schema_version: 1,
            scope: LegacyScopeRecordV1 {
                chain: "ethereum".to_owned(),
                network: "sepolia".to_owned(),
            },
            active_policy_version: "policy-v1".to_owned(),
            active_policy_digest: [1; 32],
            initialized_at: 100,
        },
        bincode::config::standard()
            .with_fixed_int_encoding()
            .with_big_endian(),
    )?;
    storage
        .commit(WriteBatch {
            conditions: Vec::new(),
            operations: vec![Operation::Put {
                namespace: Namespace("ps.v1.database_metadata".to_owned()),
                key: Key(b"identity".to_vec()),
                value: Value(value),
            }],
        })
        .await?;
    let repository = PersistentPaymentRepository::new(storage);
    let serve_error = repository
        .initialize_or_validate(InitializePaymentDatabase {
            scope: scope("sepolia"),
            active_policy: policy("policy-v1", 1),
            initialized_at: 200,
        })
        .await
        .expect_err("normal serve must fail closed on an older domain schema");
    assert_eq!(serve_error.kind, DepositErrorKind::Conflict);

    let report = repository
        .migrate_and_bind(migrate_command("sepolia", policy("policy-v2", 2)))
        .await?;
    assert_eq!(report.previous_domain_schema_version, Some(1));
    assert_eq!(
        report.metadata.domain_schema_version,
        PAYMENT_DOMAIN_SCHEMA_VERSION
    );
    assert_eq!(report.metadata.active_policy, policy("policy-v2", 2));
    assert_eq!(report.metadata.initialized_at, 100);
    Ok(())
}

#[derive(Encode)]
struct LegacyAddressRecordV1 {
    chain: String,
    value: String,
}

#[derive(Encode)]
enum LegacyKeyLocatorRecordV1 {
    Identifier(String),
}

#[derive(Encode)]
#[allow(dead_code)]
enum LegacyDepositStateRecordV1 {
    AwaitingWatch,
    Active(String),
    Expired,
    Closed,
}

#[derive(Encode)]
struct LegacyDepositRecordV1 {
    version: u16,
    id: String,
    idempotency_key: String,
    user_id: String,
    asset_chain: String,
    asset: String,
    address: LegacyAddressRecordV1,
    key: LegacyKeyLocatorRecordV1,
    expected: [u8; 32],
    birthday: u64,
    expires_at: u64,
    state: LegacyDepositStateRecordV1,
    created_at: u64,
}

#[tokio::test]
async fn unrecoverable_version_one_expired_row_leaves_database_unbound()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let storage = RocksDbStorage::open(directory.path())?;
    let value = bincode::encode_to_vec(
        LegacyDepositRecordV1 {
            version: 1,
            id: "legacy-expired".to_owned(),
            idempotency_key: "legacy-create".to_owned(),
            user_id: "legacy-user".to_owned(),
            asset_chain: "ethereum".to_owned(),
            asset: "native".to_owned(),
            address: LegacyAddressRecordV1 {
                chain: "ethereum".to_owned(),
                value: "0x2222222222222222222222222222222222222222".to_owned(),
            },
            key: LegacyKeyLocatorRecordV1::Identifier("legacy-key".to_owned()),
            expected: [0; 32],
            birthday: 10,
            expires_at: 2_000,
            state: LegacyDepositStateRecordV1::Expired,
            created_at: 1_000,
        },
        bincode::config::standard()
            .with_fixed_int_encoding()
            .with_big_endian(),
    )?;
    storage
        .commit(WriteBatch {
            conditions: Vec::new(),
            operations: vec![Operation::Put {
                namespace: Namespace("ps.v1.deposit".to_owned()),
                key: Key(b"legacy-expired".to_vec()),
                value: Value(value),
            }],
        })
        .await?;
    let repository = PersistentPaymentRepository::new(storage);

    let error = repository
        .migrate_and_bind(migrate_command("sepolia", policy("policy-v1", 1)))
        .await
        .expect_err("legacy Expired lacks the IX watch ID required by the current schema");
    assert_eq!(error.kind, DepositErrorKind::Storage);
    assert!(error.message.contains("explicit migration"));
    assert_eq!(repository.database_metadata().await?, None);
    Ok(())
}
