use chain_identity::{AssetId, AtomicAmount, ChainId};
use deposits::{
    ClaimJob, CloseDepositJob, CommandIdentity, CommandOperation, CommandPrincipal,
    CreateDepositJob, CreateJob, CreateJobOutcome, DepositErrorKind, DepositId, IdempotencyKey,
    InitializePaymentDatabase, JobError, JobId, JobPageRequest, JobPayload, JobResource, JobState,
    JobStore, PAYMENT_DOMAIN_SCHEMA_VERSION, PAYMENT_SERVICE_OWNER, PaymentDatabaseMetadataStore,
    PersistentPaymentRepository, PolicyIdentity, PrincipalScopeMode, RequestHash, TransitionJob,
    UserId, UserStore,
};
use indexing::IndexScope;
use storage::{Key, Namespace, Operation, Storage, Value, WriteBatch};
use storage_rocksdb::RocksDbStorage;
use tempfile::TempDir;

fn amount(value: u64) -> AtomicAmount {
    let mut bytes = [0_u8; 32];
    bytes[24..].copy_from_slice(&value.to_be_bytes());
    AtomicAmount(bytes)
}

fn create_deposit_job(id: &str, hash: u8, created_at: u64) -> CreateJob {
    let chain = ChainId("ethereum".to_owned());
    CreateJob {
        id: JobId(id.to_owned()),
        command: CommandIdentity {
            principal: CommandPrincipal("exchange-backend".to_owned()),
            operation: CommandOperation::CreateDeposit,
            client_key: IdempotencyKey("exchange-order-8472".to_owned()),
            request_hash: RequestHash([hash; 32]),
        },
        payload: JobPayload::CreateDeposit(CreateDepositJob {
            deposit_id: DepositId("deposit-456".to_owned()),
            user_id: UserId("user-15".to_owned()),
            scope: IndexScope {
                chain: chain.clone(),
                network: "sepolia".to_owned(),
            },
            asset: AssetId {
                chain,
                asset: "native".to_owned(),
            },
            expected: amount(100),
            expires_at: created_at + 3_600,
            created_at,
            key_purpose: "deposit-address".to_owned(),
        }),
        user_owner: CommandPrincipal("exchange-backend".to_owned()),
        policy: PolicyIdentity {
            version: "policy-v1".to_owned(),
            digest: [9; 32],
        },
        created_at,
    }
}

fn close_deposit_job(id: &str, key: &str, created_at: u64) -> CreateJob {
    CreateJob {
        id: JobId(id.to_owned()),
        command: CommandIdentity {
            principal: CommandPrincipal("exchange-backend".to_owned()),
            operation: CommandOperation::CloseDeposit,
            client_key: IdempotencyKey(key.to_owned()),
            request_hash: RequestHash([created_at as u8; 32]),
        },
        payload: JobPayload::CloseDeposit(CloseDepositJob {
            deposit_id: DepositId("deposit-456".to_owned()),
            user_id: UserId("user-15".to_owned()),
        }),
        user_owner: CommandPrincipal("exchange-backend".to_owned()),
        policy: PolicyIdentity {
            version: "policy-v1".to_owned(),
            digest: [9; 32],
        },
        created_at,
    }
}

fn retryable_error() -> JobError {
    JobError {
        code: "indexer_unavailable".to_owned(),
        message: "Indexer is temporarily unavailable".to_owned(),
        retryable: true,
    }
}

fn metadata(scope_network: &str, policy: &str) -> InitializePaymentDatabase {
    InitializePaymentDatabase {
        scope: IndexScope {
            chain: ChainId("ethereum".to_owned()),
            network: scope_network.to_owned(),
        },
        active_policy: PolicyIdentity {
            version: policy.to_owned(),
            digest: [policy.len() as u8; 32],
        },
        initialized_at: 50,
    }
}

#[tokio::test]
async fn database_metadata_binds_owner_scope_schema_and_policy()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    let initialized = repository
        .initialize_or_validate(metadata("sepolia", "policy-v1"))
        .await?;
    assert_eq!(initialized.service_owner, PAYMENT_SERVICE_OWNER);
    assert_eq!(
        initialized.domain_schema_version,
        PAYMENT_DOMAIN_SCHEMA_VERSION
    );
    assert_eq!(
        repository
            .initialize_or_validate(metadata("sepolia", "policy-v1"))
            .await?,
        initialized
    );
    let policy_bound_job = repository
        .create_or_replay(create_deposit_job("policy-bound-job", 12, 100))
        .await?;
    assert_eq!(policy_bound_job.job().policy, initialized.active_policy);
    let mut wrong_policy_job = close_deposit_job("wrong-policy-job", "wrong-policy", 110);
    wrong_policy_job.policy = PolicyIdentity {
        version: "policy-v2".to_owned(),
        digest: [9; 32],
    };
    let job_policy_error = repository
        .create_or_replay(wrong_policy_job)
        .await
        .expect_err("every new job must bind the database active policy");
    assert_eq!(job_policy_error.kind, DepositErrorKind::Conflict);

    let scope_error = repository
        .initialize_or_validate(metadata("mainnet", "policy-v1"))
        .await
        .expect_err("one PS database cannot be rebound to another network");
    assert_eq!(scope_error.kind, DepositErrorKind::Conflict);
    let policy_error = repository
        .initialize_or_validate(metadata("sepolia", "policy-v2"))
        .await
        .expect_err("active policy identity is immutable without migration");
    assert_eq!(policy_error.kind, DepositErrorKind::Conflict);
    Ok(())
}

#[tokio::test]
async fn database_metadata_binds_principal_scope_mode_without_switching()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    let initialized = repository
        .initialize_or_validate_principal_scope(
            metadata("sepolia", "policy-v1"),
            PrincipalScopeMode::GlobalTrusted,
        )
        .await?;
    assert_eq!(
        initialized.principal_scope_mode,
        PrincipalScopeMode::GlobalTrusted
    );
    assert_eq!(
        repository
            .initialize_or_validate_principal_scope(
                metadata("sepolia", "policy-v1"),
                PrincipalScopeMode::GlobalTrusted,
            )
            .await?,
        initialized
    );

    let error = repository
        .initialize_or_validate_principal_scope(
            metadata("sepolia", "policy-v1"),
            PrincipalScopeMode::RoleScoped,
        )
        .await
        .expect_err("a bound principal-scope mode must not switch");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    assert_eq!(repository.database_metadata().await?, Some(initialized));
    Ok(())
}

#[tokio::test]
async fn database_metadata_rejects_indexer_and_unbound_payment_data()
-> Result<(), Box<dyn std::error::Error>> {
    let ix_directory = TempDir::new()?;
    let ix_storage = RocksDbStorage::open(ix_directory.path())?;
    ix_storage
        .commit(WriteBatch {
            conditions: Vec::new(),
            operations: vec![Operation::Put {
                namespace: Namespace("ix.semantic.v1".to_owned()),
                key: Key(vec![1, 1]),
                value: Value(vec![1]),
            }],
        })
        .await?;
    let ix_repository = PersistentPaymentRepository::new(ix_storage);
    let ix_error = ix_repository
        .initialize_or_validate(metadata("sepolia", "policy-v1"))
        .await
        .expect_err("PS must not adopt an IX-owned database");
    assert_eq!(ix_error.kind, DepositErrorKind::Conflict);

    let legacy_directory = TempDir::new()?;
    let legacy_repository =
        PersistentPaymentRepository::new(RocksDbStorage::open(legacy_directory.path())?);
    legacy_repository
        .create_or_replay(create_deposit_job("legacy-job", 5, 100))
        .await?;
    let legacy_error = legacy_repository
        .initialize_or_validate(metadata("sepolia", "policy-v1"))
        .await
        .expect_err("unbound PS records require the explicit migration path");
    assert_eq!(legacy_error.kind, DepositErrorKind::Conflict);
    Ok(())
}

#[tokio::test]
async fn bound_payment_database_rejects_later_indexer_ownership()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let storage = RocksDbStorage::open(directory.path())?;
    let repository = PersistentPaymentRepository::new(storage.clone());
    repository
        .initialize_or_validate(metadata("sepolia", "policy-v1"))
        .await?;

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

    let error = repository
        .initialize_or_validate(metadata("sepolia", "policy-v1"))
        .await
        .expect_err("PS startup must reject IX ownership even after PS metadata was bound");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    Ok(())
}

#[tokio::test]
async fn database_metadata_rejects_each_unbound_collection_namespace()
-> Result<(), Box<dyn std::error::Error>> {
    for namespace in [
        "ps.v1.collection",
        "ps.v1.collection_job",
        "ps.v1.deposit_collection",
        "ps.v1.active_collection_reservation",
        "ps.v1.collection_eligibility_generation",
        "ps.v1.collection_transaction",
        "ps.v1.signed_collection_envelope",
        "ps.v1.closed_deposit_watch",
        "ps.v1.reconciliation_generation",
    ] {
        let directory = TempDir::new()?;
        let storage = RocksDbStorage::open(directory.path())?;
        storage
            .commit(WriteBatch {
                conditions: Vec::new(),
                operations: vec![Operation::Put {
                    namespace: Namespace(namespace.to_owned()),
                    key: Key(vec![1]),
                    value: Value(vec![1]),
                }],
            })
            .await?;
        let repository = PersistentPaymentRepository::new(storage);
        let error = repository
            .initialize_or_validate(metadata("sepolia", "policy-v1"))
            .await
            .expect_err("unbound collection records require explicit metadata migration");
        assert_eq!(error.kind, DepositErrorKind::Conflict, "{namespace}");
    }
    Ok(())
}

#[tokio::test]
async fn same_command_replays_stable_job_and_changed_request_conflicts()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);

    let first = repository
        .create_or_replay(create_deposit_job("job-original", 7, 100))
        .await?;
    assert!(matches!(first, CreateJobOutcome::Created { .. }));
    assert_eq!(first.job().id, JobId("job-original".to_owned()));
    assert_eq!(first.job().attempt_count, 0);
    assert_eq!(first.job().state, JobState::Queued);
    assert_eq!(first.job().policy.version, "policy-v1");

    let replay_candidate = create_deposit_job("job-new-candidate", 7, 200);
    assert_eq!(
        repository
            .job_for_command(&replay_candidate.command)
            .await?,
        Some(first.job().clone())
    );
    // The atomic create path still resolves a concurrent race after a caller
    // generated a fresh candidate following an initial lookup miss.
    let replay = repository.create_or_replay(replay_candidate).await?;
    assert!(matches!(replay, CreateJobOutcome::Replayed { .. }));
    assert_eq!(replay.job(), first.job());
    assert_eq!(
        repository
            .job(&JobId("job-new-candidate".to_owned()))
            .await?,
        None
    );

    let error = repository
        .create_or_replay(create_deposit_job("job-conflict", 8, 100))
        .await
        .expect_err("the same client key cannot identify changed request content");
    assert_eq!(error.kind, DepositErrorKind::Conflict);
    assert_eq!(
        repository.job(&JobId("job-conflict".to_owned())).await?,
        None
    );

    let mut wrong_owner = create_deposit_job("job-owner-conflict", 7, 100);
    wrong_owner.user_owner = CommandPrincipal("another-exchange".to_owned());
    let owner_error = repository
        .create_or_replay(wrong_owner)
        .await
        .expect_err("a replay cannot silently change opaque user ownership");
    assert_eq!(owner_error.kind, DepositErrorKind::Conflict);
    Ok(())
}

#[tokio::test]
async fn jobs_retain_payload_and_claim_state_across_restarts()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let expected_job = {
        let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
        let created = repository
            .create_or_replay(create_deposit_job("job-restart", 9, 1_000))
            .await?;
        created.job().clone()
    };

    let claimed = {
        let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
        assert_eq!(
            repository.job(&expected_job.id).await?,
            Some(expected_job.clone())
        );
        assert_eq!(
            repository
                .user(&UserId("user-15".to_owned()))
                .await?
                .expect("job creation must durably own the opaque user")
                .owner,
            CommandPrincipal("exchange-backend".to_owned())
        );
        let claimed = repository
            .claim_next(ClaimJob {
                now: 1_000,
                lease_expires_at: 1_100,
                scan_limit: 10,
            })
            .await?
            .expect("the queued job must be claimable");
        assert_eq!(claimed.attempt_count, 1);
        assert_eq!(
            claimed.state,
            JobState::Running {
                lease_expires_at: 1_100
            }
        );
        claimed
    };

    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    assert_eq!(repository.job(&claimed.id).await?, Some(claimed.clone()));
    assert_eq!(
        repository
            .claim_next(ClaimJob {
                now: 1_099,
                lease_expires_at: 1_200,
                scan_limit: 10,
            })
            .await?,
        None
    );
    let reclaimed = repository
        .claim_next(ClaimJob {
            now: 1_100,
            lease_expires_at: 1_200,
            scan_limit: 10,
        })
        .await?
        .expect("an expired running lease must recover after restart");
    assert_eq!(reclaimed.attempt_count, 2);
    assert_eq!(
        reclaimed.state,
        JobState::Running {
            lease_expires_at: 1_200
        }
    );
    assert!(matches!(
        reclaimed.payload,
        JobPayload::CreateDeposit(CreateDepositJob {
            key_purpose,
            ..
        }) if key_purpose == "deposit-address"
    ));
    Ok(())
}

#[tokio::test]
async fn expected_state_allows_only_one_concurrent_transition()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    repository
        .create_or_replay(create_deposit_job("job-transition", 10, 100))
        .await?;
    let running = repository
        .claim_next(ClaimJob {
            now: 100,
            lease_expires_at: 200,
            scan_limit: 10,
        })
        .await?
        .expect("the queued job must be claimed first");

    let retry_repository = repository.clone();
    let fail_repository = repository.clone();
    let retry = TransitionJob {
        id: running.id.clone(),
        expected_state: running.state.clone(),
        next_state: JobState::WaitingRetry {
            next_attempt_at: 300,
        },
        error: Some(retryable_error()),
        updated_at: 201,
    };
    let fail = TransitionJob {
        id: running.id.clone(),
        expected_state: running.state,
        next_state: JobState::Failed,
        error: Some(JobError {
            code: "invalid_policy".to_owned(),
            message: "The active policy rejects this request".to_owned(),
            retryable: false,
        }),
        updated_at: 201,
    };
    let (retry_result, fail_result) = tokio::join!(
        retry_repository.transition(retry),
        fail_repository.transition(fail)
    );
    assert_eq!(
        usize::from(retry_result.is_ok()) + usize::from(fail_result.is_ok()),
        1
    );
    let loser = if let Err(error) = retry_result {
        error
    } else {
        fail_result.expect_err("exactly one transition must lose")
    };
    assert_eq!(loser.kind, DepositErrorKind::Conflict);
    Ok(())
}

#[tokio::test]
async fn retry_success_failure_and_user_resource_indexes_are_durable()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let repository = PersistentPaymentRepository::new(RocksDbStorage::open(directory.path())?);
    let first = repository
        .create_or_replay(create_deposit_job("job-a", 11, 100))
        .await?
        .job()
        .clone();
    let running = repository
        .claim_next(ClaimJob {
            now: 100,
            lease_expires_at: 150,
            scan_limit: 10,
        })
        .await?
        .expect("first job must be claimable");
    let waiting = repository
        .transition(TransitionJob {
            id: running.id.clone(),
            expected_state: running.state,
            next_state: JobState::WaitingRetry {
                next_attempt_at: 200,
            },
            error: Some(retryable_error()),
            updated_at: 151,
        })
        .await?;
    assert_eq!(
        repository
            .claim_next(ClaimJob {
                now: 199,
                lease_expires_at: 250,
                scan_limit: 10,
            })
            .await?,
        None
    );
    let second_attempt = repository
        .claim_next(ClaimJob {
            now: 200,
            lease_expires_at: 250,
            scan_limit: 10,
        })
        .await?
        .expect("retry must become claimable at its durable deadline");
    assert_eq!(second_attempt.attempt_count, 2);
    assert_eq!(second_attempt.last_error, waiting.last_error);
    let succeeded = repository
        .transition(TransitionJob {
            id: second_attempt.id.clone(),
            expected_state: second_attempt.state,
            next_state: JobState::Succeeded,
            error: None,
            updated_at: 220,
        })
        .await?;
    assert_eq!(succeeded.state, JobState::Succeeded);
    assert_eq!(succeeded.last_error, None);

    let mut admin_close = close_deposit_job("job-b", "close-456", 300);
    admin_close.command.principal = CommandPrincipal("administrator".to_owned());
    let second = repository
        .create_or_replay(admin_close)
        .await?
        .job()
        .clone();
    let second_running = repository
        .claim_next(ClaimJob {
            now: 300,
            lease_expires_at: 350,
            scan_limit: 10,
        })
        .await?
        .expect("second job must be claimable");
    let failed = repository
        .transition(TransitionJob {
            id: second_running.id.clone(),
            expected_state: second_running.state,
            next_state: JobState::Failed,
            error: Some(JobError {
                code: "balance_nonzero".to_owned(),
                message: "Deposit balance is not zero".to_owned(),
                retryable: false,
            }),
            updated_at: 320,
        })
        .await?;
    assert_eq!(failed.state, JobState::Failed);

    let user_jobs = repository
        .jobs_for_user(
            &UserId("user-15".to_owned()),
            JobPageRequest {
                after: None,
                limit: 10,
            },
        )
        .await?;
    assert_eq!(
        user_jobs
            .jobs
            .iter()
            .map(|job| job.id.clone())
            .collect::<Vec<_>>(),
        vec![first.id.clone(), second.id.clone()]
    );
    let resource_jobs = repository
        .jobs_for_resource(
            &JobResource::Deposit(DepositId("deposit-456".to_owned())),
            JobPageRequest {
                after: None,
                limit: 10,
            },
        )
        .await?;
    assert_eq!(resource_jobs.jobs.len(), 2);
    assert_eq!(
        repository
            .user(&UserId("user-15".to_owned()))
            .await?
            .expect("user record must survive all associated jobs")
            .first_seen_at,
        100
    );

    let mut foreign = close_deposit_job("job-foreign", "foreign-close", 400);
    foreign.command.principal = CommandPrincipal("administrator".to_owned());
    foreign.user_owner = CommandPrincipal("another-exchange".to_owned());
    let owner_error = repository
        .create_or_replay(foreign)
        .await
        .expect_err("one exchange principal cannot take ownership of another user's ID");
    assert_eq!(owner_error.kind, DepositErrorKind::Conflict);
    assert_eq!(
        repository.job(&JobId("job-foreign".to_owned())).await?,
        None
    );
    Ok(())
}
