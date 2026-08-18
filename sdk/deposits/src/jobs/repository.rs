use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn resolve_job_users(
        &self,
        payload: &JobPayload,
        owner: &CommandPrincipal,
    ) -> Result<(UserId, Vec<UserId>), DepositError> {
        if let Some(user_id) = payload.user_id() {
            return Ok((user_id.clone(), vec![user_id.clone()]));
        }
        let deposit_ids = payload
            .deposit_ids()
            .ok_or_else(|| invalid("job payload has no user or deposit associations"))?;
        let mut primary = None;
        let mut associated = BTreeSet::new();
        for deposit_id in deposit_ids {
            let deposit = self
                .deposit(deposit_id)
                .await?
                .ok_or_else(|| not_found("UTXO-batch job deposit was not found"))?;
            let user = self
                .stored_user_record(&deposit.user_id)
                .await?
                .ok_or_else(|| storage_error("UTXO-batch deposit user record is missing"))?;
            if &user.owner != owner {
                return Err(conflict(
                    "UTXO-batch deposit user belongs to another authenticated owner",
                ));
            }
            primary.get_or_insert_with(|| deposit.user_id.clone());
            associated.insert(deposit.user_id);
        }
        let primary = primary.ok_or_else(|| invalid("UTXO-batch job has no participant user"))?;
        Ok((primary, associated.into_iter().collect()))
    }

    pub(super) async fn stored_database_metadata(
        &self,
    ) -> Result<Option<DatabaseIdentity>, DepositError> {
        self.storage()
            .get(&database_metadata_ns(), &database_metadata_key())
            .await
            .map_err(map_storage)?
            .map(|stored| decode::<DatabaseRecord>(&stored)?.try_into())
            .transpose()
    }

    pub(super) async fn namespace_has_records(
        &self,
        namespace: Namespace,
    ) -> Result<bool, DepositError> {
        Ok(!self
            .storage()
            .scan(ScanRequest {
                namespace,
                prefix: Vec::new(),
                after: None,
                limit: 1,
            })
            .await
            .map_err(map_storage)?
            .entries
            .is_empty())
    }

    pub(super) async fn has_principal_scoped_ps_records(&self) -> Result<bool, DepositError> {
        // These authoritative rows and indexes all imply pre-existing
        // role-scoped identities. The deposit-index completion marker is
        // deliberately excluded: it is derived, contains no principal, and
        // may survive an interrupted validation of an otherwise empty store.
        for namespace in [
            "ps.v1.deposit",
            "ps.v1.deposit_address",
            "ps.v1.deposit_idem",
            "ps.v1.awaiting_watch",
            "ps.v1.closed_deposit_watch",
            "ps.v1.user_deposit",
            "ps.v1.deposit_state",
            "ps.v1.user_deposit_state",
            "ps.v1.ledger_head",
            "ps.v1.ledger_entry",
            "ps.v1.projection",
            "ps.v1.accounting_idem",
            "ps.v1.observation",
            "ps.v1.observation_cursor",
            "ps.v1.deposit_observation",
            "ps.v1.consumer_checkpoint",
            "ps.v1.reconciliation",
            "ps.v1.reconciliation_deposit",
            "ps.v1.reconciliation_resolution_idem",
            "ps.v1.reconciliation_generation",
            "ps.v1.user",
            "ps.v1.job",
            "ps.v1.command_job",
            "ps.v1.user_job",
            "ps.v1.resource_job",
            "ps.v1.ready_job",
            "ps.v1.collection",
            "ps.v1.collection_job",
            "ps.v1.deposit_collection",
            "ps.v1.active_collection_reservation",
            "ps.v1.collection_eligibility_generation",
            "ps.v1.collection_transaction",
            "ps.v1.signed_collection_envelope",
            "ps.v2.active_collection_spend_resource",
        ] {
            if self.namespace_has_records(ns(namespace)).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) async fn has_unbound_ps_records(&self) -> Result<bool, DepositError> {
        // Normal startup also treats a lone derived completion marker as an
        // unbound database and cannot be adopted by normal startup.
        if self.has_principal_scoped_ps_records().await? {
            return Ok(true);
        }
        self.namespace_has_records(ns("ps.v1.deposit_index_metadata"))
            .await
    }

    pub(super) async fn stored_user_record(
        &self,
        id: &UserId,
    ) -> Result<Option<User>, DepositError> {
        self.storage()
            .get(&user_ns(), &key_text(&id.0))
            .await
            .map_err(map_storage)?
            .map(|stored| decode::<UserRecord>(&stored)?.try_into())
            .transpose()
    }

    pub(super) async fn stored_job_record(
        &self,
        id: &JobId,
    ) -> Result<Option<(Job, StoredValue)>, DepositError> {
        self.storage()
            .get(&job_ns(), &key_text(&id.0))
            .await
            .map_err(map_storage)?
            .map(|stored| {
                let record: JobRecord = decode(&stored)?;
                Ok((record.try_into()?, stored))
            })
            .transpose()
    }

    pub(super) async fn idempotent_job(
        &self,
        identity: &CommandIdentity,
    ) -> Result<Option<Job>, DepositError> {
        let Some(stored) = self
            .storage()
            .get(&command_job_ns(), &command_key(identity)?)
            .await
            .map_err(map_storage)?
        else {
            return Ok(None);
        };
        let record: CommandIndex = decode(&stored)?;
        ensure_version(record.version)?;
        let persisted_identity: CommandIdentity = record.command.into();
        if persisted_identity.principal != identity.principal
            || persisted_identity.operation != identity.operation
            || persisted_identity.client_key != identity.client_key
        {
            return Err(storage_error(
                "command idempotency index identity does not match its key",
            ));
        }
        if persisted_identity.request_hash != identity.request_hash {
            return Err(conflict(
                "command idempotency key was reused with a different request",
            ));
        }
        let job = self
            .stored_job_record(&JobId(record.job_id))
            .await?
            .map(|(job, _)| job)
            .ok_or_else(|| storage_error("command idempotency index points to a missing job"))?;
        if job.command != persisted_identity {
            return Err(storage_error(
                "command idempotency index points to a different job command",
            ));
        }
        Ok(Some(job))
    }

    pub(super) async fn indexed_jobs(
        &self,
        namespace: Namespace,
        prefix: Vec<u8>,
        after: Option<Key>,
        limit: usize,
    ) -> Result<JobPage, DepositError> {
        let page = self
            .storage()
            .scan(ScanRequest {
                namespace,
                prefix,
                after,
                limit,
            })
            .await
            .map_err(map_storage)?;
        let has_next = page.next.is_some();
        let mut jobs = Vec::with_capacity(page.entries.len());
        for (_, stored) in page.entries {
            let index: JobIndex = decode(&stored)?;
            ensure_version(index.version)?;
            jobs.push(
                self.stored_job_record(&JobId(index.job_id))
                    .await?
                    .map(|(job, _)| job)
                    .ok_or_else(|| storage_error("job association index is dangling"))?,
            );
        }
        let next = has_next
            .then(|| jobs.last().map(|job| job.id.clone()))
            .flatten();
        Ok(JobPage { jobs, next })
    }

    pub(super) async fn store_new_job(
        &self,
        job: &Job,
        associated_users: &[UserId],
        missing_users: &[UserId],
    ) -> Result<(), DepositError> {
        let job_key = key_text(&job.id.0);
        let command_key = command_key(&job.command)?;
        let resource_job_key = resource_job_key(&job.resource, &job.id)?;
        let ready_key = ready_job_key(job.created_at, &job.id);
        if associated_users.is_empty()
            || !associated_users.contains(&job.user_id)
            || associated_users
                .windows(2)
                .any(|users| users[0] >= users[1])
        {
            return Err(storage_error(
                "job associated users must be canonical and include the primary user",
            ));
        }
        let mut conditions = vec![
            Condition::Missing {
                namespace: job_ns(),
                key: job_key.clone(),
            },
            Condition::Missing {
                namespace: command_job_ns(),
                key: command_key.clone(),
            },
            Condition::Missing {
                namespace: resource_job_ns(),
                key: resource_job_key.clone(),
            },
            Condition::Missing {
                namespace: ready_job_ns(),
                key: ready_key.clone(),
            },
        ];
        let index = JobIndex {
            version: RECORD_VERSION,
            job_id: job.id.0.clone(),
        };
        let mut operations = vec![
            Operation::Put {
                namespace: job_ns(),
                key: job_key,
                value: encode(&JobRecord::from(job))?,
            },
            Operation::Put {
                namespace: command_job_ns(),
                key: command_key,
                value: encode(&CommandIndex {
                    version: RECORD_VERSION,
                    command: CommandRecord::from(&job.command),
                    job_id: job.id.0.clone(),
                })?,
            },
            Operation::Put {
                namespace: resource_job_ns(),
                key: resource_job_key,
                value: encode(&index)?,
            },
            Operation::Put {
                namespace: ready_job_ns(),
                key: ready_key,
                value: encode(&index)?,
            },
        ];
        for user_id in associated_users {
            let key = user_job_key(user_id, &job.id)?;
            conditions.push(Condition::Missing {
                namespace: user_job_ns(),
                key: key.clone(),
            });
            operations.push(Operation::Put {
                namespace: user_job_ns(),
                key,
                value: encode(&index)?,
            });
        }
        for user_id in missing_users {
            if !associated_users.contains(user_id) {
                return Err(storage_error(
                    "missing job user is not one of the associated users",
                ));
            }
            let user = User {
                id: user_id.clone(),
                owner: job.user_owner.clone(),
                first_seen_at: job.created_at,
            };
            conditions.push(Condition::Missing {
                namespace: user_ns(),
                key: key_text(&user.id.0),
            });
            operations.push(Operation::Put {
                namespace: user_ns(),
                key: key_text(&user.id.0),
                value: encode(&UserRecord::from(&user))?,
            });
        }
        self.storage()
            .commit(WriteBatch {
                conditions,
                operations,
            })
            .await
            .map_err(map_storage)?;
        Ok(())
    }

    pub(super) async fn update_job_state(
        &self,
        current: &Job,
        stored: &StoredValue,
        next: &Job,
    ) -> Result<(), DepositError> {
        let mut conditions = vec![Condition::Version {
            namespace: job_ns(),
            key: key_text(&current.id.0),
            expected: stored.version,
        }];
        let mut operations = Vec::new();
        let current_ready_key = current
            .ready_at()
            .map(|ready_at| ready_job_key(ready_at, &current.id));
        if let Some(key) = &current_ready_key {
            operations.push(Operation::Delete {
                namespace: ready_job_ns(),
                key: key.clone(),
            });
        }
        if let Some(ready_at) = next.ready_at() {
            let key = ready_job_key(ready_at, &next.id);
            if current_ready_key.as_ref() != Some(&key) {
                conditions.push(Condition::Missing {
                    namespace: ready_job_ns(),
                    key: key.clone(),
                });
            }
            operations.push(Operation::Put {
                namespace: ready_job_ns(),
                key,
                value: encode(&JobIndex {
                    version: RECORD_VERSION,
                    job_id: next.id.0.clone(),
                })?,
            });
        }
        operations.push(Operation::Put {
            namespace: job_ns(),
            key: key_text(&next.id.0),
            value: encode(&JobRecord::from(next))?,
        });
        self.storage()
            .commit(WriteBatch {
                conditions,
                operations,
            })
            .await
            .map_err(map_storage)?;
        Ok(())
    }
}
