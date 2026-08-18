use super::reconciliation_support::*;
use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    async fn replay_reconciliation_resolution(
        &self,
        command: &ResolveReconciliation,
        idempotency_key: &Key,
    ) -> Result<Option<ReconciliationCase>, DepositError> {
        let Some(stored) = self
            .storage
            .get(&reconciliation_resolution_idempotency_ns(), idempotency_key)
            .await
            .map_err(map_storage)?
        else {
            return Ok(None);
        };
        let record: ResolutionIdentity = decode(&stored)?;
        ensure_version(record.version)?;
        let stored_command = CommandIdentity::try_from(record.command)?;
        if stored_command != command.command || record.case_id != command.case_id.0 {
            return Err(conflict(
                "reconciliation idempotency key was reused with different request content",
            ));
        }
        let case = self
            .case(&command.case_id)
            .await?
            .ok_or_else(|| storage_error("reconciliation idempotency index is dangling"))?;
        match &case.state {
            ReconciliationState::Resolved { resolution, .. }
                if resolution.command == command.command
                    && resolution.decision == command.decision =>
            {
                Ok(Some(case))
            }
            ReconciliationState::Resolved { .. } => Err(conflict(
                "reconciliation idempotency key was reused with different request content",
            )),
            ReconciliationState::Open => Err(storage_error(
                "reconciliation idempotency index does not reference a typed result",
            )),
        }
    }
}

impl<S> CaseOpener for PaymentStore<S>
where
    S: Store,
{
    fn open_case<'a>(
        &'a self,
        case: ReconciliationCase,
    ) -> BoxFuture<'a, Result<ReconciliationCase, DepositError>> {
        Box::pin(async move {
            case.validate_open()?;
            if let Some(existing) = self.case(&case.id).await? {
                return if existing == case {
                    Ok(existing)
                } else {
                    Err(conflict(
                        "reconciliation case ID was reused with a different payload",
                    ))
                };
            }
            if self.deposit(&case.deposit_id).await?.is_none() {
                return Err(not_found(
                    "cannot open a reconciliation case for a missing deposit",
                ));
            }

            let case_key = key_text(&case.id.0);
            let deposit_key = reconciliation_deposit_key(&case.deposit_id, &case.id)?;
            let (generation_condition, generation_operation) = self
                .reconciliation_generation_change(&case.deposit_id)
                .await?;
            let result = self
                .storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: reconciliation_ns(),
                            key: case_key.clone(),
                        },
                        Condition::Missing {
                            namespace: reconciliation_deposit_ns(),
                            key: deposit_key.clone(),
                        },
                        generation_condition,
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: reconciliation_ns(),
                            key: case_key,
                            value: encode(&ReconciliationRecord::try_from(&case)?)?,
                        },
                        Operation::Put {
                            namespace: reconciliation_deposit_ns(),
                            key: deposit_key,
                            value: encode(&IdRecord {
                                version: RECORD_VERSION,
                                id: case.id.0.clone(),
                            })?,
                        },
                        generation_operation,
                    ],
                })
                .await;
            match result {
                Ok(_) => Ok(case),
                Err(error) if error.kind == ErrorKind::Conflict => {
                    self.replay_open_case(&case, error).await
                }
                Err(error) => Err(map_storage(error)),
            }
        })
    }
}

impl<S> CaseReader for PaymentStore<S>
where
    S: Store,
{
    fn case<'a>(
        &'a self,
        id: &'a CaseId,
    ) -> BoxFuture<'a, Result<Option<ReconciliationCase>, DepositError>> {
        Box::pin(async move {
            self.storage
                .get(&reconciliation_ns(), &key_text(&id.0))
                .await
                .map_err(map_storage)?
                .map(|stored| decode_reconciliation(&stored))
                .transpose()
        })
    }

    fn cases<'a>(
        &'a self,
        request: CaseQuery,
    ) -> BoxFuture<'a, Result<ReconciliationPage, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > 1_000 {
                return Err(invalid(
                    "reconciliation page size must be between 1 and 1000",
                ));
            }

            let (namespace, prefix, mut after) = match &request.deposit_id {
                Some(deposit_id) => (
                    reconciliation_deposit_ns(),
                    reconciliation_deposit_prefix(deposit_id)?,
                    request
                        .after
                        .as_ref()
                        .map(|case_id| reconciliation_deposit_key(deposit_id, case_id))
                        .transpose()?,
                ),
                None => (
                    reconciliation_ns(),
                    Vec::new(),
                    request.after.as_ref().map(|case_id| key_text(&case_id.0)),
                ),
            };

            let mut cases = Vec::with_capacity(request.limit);
            let mut exhausted = false;
            while cases.len() < request.limit && !exhausted {
                let page = self
                    .storage
                    .scan(ScanRequest {
                        namespace: namespace.clone(),
                        prefix: prefix.clone(),
                        after,
                        limit: request.limit,
                    })
                    .await
                    .map_err(map_storage)?;
                exhausted = page.next.is_none();
                after = page.next;

                if self
                    .append_case_page(&request, page.entries, &mut cases)
                    .await?
                {
                    break;
                }
            }

            let next = if exhausted {
                None
            } else {
                cases.last().map(|case| case.id.clone())
            };
            Ok(ReconciliationPage { cases, next })
        })
    }
}

impl<S> CaseResolver for PaymentStore<S>
where
    S: Store,
{
    fn resolve_case<'a>(
        &'a self,
        command: ResolveReconciliation,
    ) -> BoxFuture<'a, Result<ReconciliationCase, DepositError>> {
        Box::pin(async move {
            command.validate()?;
            let idempotency_key = reconciliation_command_key(&command.command)?;
            if let Some(case) = self
                .replay_reconciliation_resolution(&command, &idempotency_key)
                .await?
            {
                return Ok(case);
            }
            let key = key_text(&command.case_id.0);
            let stored = self
                .storage
                .get(&reconciliation_ns(), &key)
                .await
                .map_err(map_storage)?
                .ok_or_else(|| not_found("reconciliation case does not exist"))?;
            let mut case = decode_reconciliation(&stored)?;
            match &case.state {
                ReconciliationState::Open => {}
                ReconciliationState::Resolved { .. } => {
                    return Err(conflict("reconciliation case has already been resolved"));
                }
            }

            let (generation_condition, generation_operation) = self
                .reconciliation_generation_change(&case.deposit_id)
                .await?;

            let mut conditions = vec![
                Condition::Missing {
                    namespace: reconciliation_resolution_idempotency_ns(),
                    key: idempotency_key.clone(),
                },
                Condition::Version {
                    namespace: reconciliation_ns(),
                    key: key.clone(),
                    expected: stored.version,
                },
                generation_condition,
            ];
            let mut operations = vec![generation_operation];
            let ledger_entry = match &command.decision {
                ReconciliationDecision::ReverseCredit { expected_head, .. } => {
                    let (current, head_stored) = self
                        .stored_head(&case.deposit_id)
                        .await?
                        .ok_or_else(|| not_found("reconciliation deposit ledger is not open"))?;
                    if expected_head != &current.id {
                        return Err(conflict(
                            "reconciliation expected ledger head does not match current head",
                        ));
                    }
                    let entry = reconciliation_resolution_entry(&command, &current);
                    if entry.balances.accounted > entry.balances.confirmed {
                        return Err(invalid(
                            "reverse-credit resolution left accounted above confirmed",
                        ));
                    }
                    conditions.extend([
                        Condition::Version {
                            namespace: ledger_head_ns(),
                            key: key_text(&case.deposit_id.0),
                            expected: head_stored.version,
                        },
                        Condition::Missing {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&case.deposit_id, &entry.id)?,
                        },
                    ]);
                    operations.extend([
                        Operation::Put {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&case.deposit_id, &entry.id)?,
                            value: encode(&LedgerRecord::from(&entry))?,
                        },
                        Operation::Put {
                            namespace: ledger_head_ns(),
                            key: key_text(&case.deposit_id.0),
                            value: encode(&IdRecord {
                                version: RECORD_VERSION,
                                id: entry.id.0.clone(),
                            })?,
                        },
                    ]);
                    Some(entry)
                }
                ReconciliationDecision::AcceptLiability { .. }
                | ReconciliationDecision::ExternalDebtRecorded { .. } => None,
            };
            let resolution = ReconciliationResolution {
                command: command.command.clone(),
                decision: command.decision.clone(),
                ledger_entry_id: ledger_entry.as_ref().map(|entry| entry.id.clone()),
            };
            case.state = ReconciliationState::Resolved {
                resolution,
                resolved_at: command.resolved_at,
            };
            operations.extend([
                Operation::Put {
                    namespace: reconciliation_ns(),
                    key,
                    value: encode(&ReconciliationRecord::try_from(&case)?)?,
                },
                Operation::Put {
                    namespace: reconciliation_resolution_idempotency_ns(),
                    key: idempotency_key.clone(),
                    value: encode(&ResolutionIdentity {
                        version: RECORD_VERSION,
                        command: ReconciliationIdentity::from(&command.command),
                        case_id: command.case_id.0.clone(),
                    })?,
                },
            ]);
            let result = self
                .storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await;
            match result {
                Ok(_) => Ok(case),
                Err(error) if error.kind == ErrorKind::Conflict => {
                    self.resolve_case_conflict(&command, &idempotency_key).await
                }
                Err(error) => Err(map_storage(error)),
            }
        })
    }
}

impl<S> PaymentStore<S>
where
    S: Store,
{
    async fn replay_open_case(
        &self,
        case: &ReconciliationCase,
        storage_failure: Error,
    ) -> Result<ReconciliationCase, DepositError> {
        let existing = self
            .case(&case.id)
            .await?
            .ok_or_else(|| map_storage(storage_failure))?;
        if existing == *case {
            Ok(existing)
        } else {
            Err(conflict(
                "reconciliation case ID was concurrently reused with a different payload",
            ))
        }
    }

    async fn append_case_page(
        &self,
        request: &CaseQuery,
        entries: Vec<(Key, StoredValue)>,
        cases: &mut Vec<ReconciliationCase>,
    ) -> Result<bool, DepositError> {
        for (_, stored) in entries {
            let case = if request.deposit_id.is_some() {
                let index: IdRecord = decode(&stored)?;
                ensure_version(index.version)?;
                self.case(&CaseId(index.id))
                    .await?
                    .ok_or_else(|| storage_error("reconciliation deposit index is dangling"))?
            } else {
                decode_reconciliation(&stored)?
            };
            if request.open_only && case.state != ReconciliationState::Open {
                continue;
            }
            cases.push(case);
            if cases.len() == request.limit {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn resolve_case_conflict(
        &self,
        command: &ResolveReconciliation,
        idempotency_key: &Key,
    ) -> Result<ReconciliationCase, DepositError> {
        self.replay_reconciliation_resolution(command, idempotency_key)
            .await?
            .ok_or_else(|| conflict("reconciliation case or ledger head changed concurrently"))
    }
}

impl<S> ActionGuard for PaymentStore<S>
where
    S: Store,
{
    fn automatic_actions_blocked<'a>(
        &'a self,
        deposit_id: &'a DepositId,
    ) -> BoxFuture<'a, Result<bool, DepositError>> {
        Box::pin(async move {
            let mut after = None;
            let prefix = reconciliation_deposit_prefix(deposit_id)?;
            loop {
                let page = self
                    .storage
                    .scan(ScanRequest {
                        namespace: reconciliation_deposit_ns(),
                        prefix: prefix.clone(),
                        after,
                        limit: 256,
                    })
                    .await
                    .map_err(map_storage)?;
                if self.page_has_open_case(page.entries).await? {
                    return Ok(true);
                }
                let Some(next) = page.next else {
                    return Ok(false);
                };
                after = Some(next);
            }
        })
    }
}

impl<S> PaymentStore<S>
where
    S: Store,
{
    async fn page_has_open_case(
        &self,
        entries: Vec<(Key, StoredValue)>,
    ) -> Result<bool, DepositError> {
        for (_, stored) in entries {
            let index: IdRecord = decode(&stored)?;
            ensure_version(index.version)?;
            let case = self
                .case(&CaseId(index.id))
                .await?
                .ok_or_else(|| storage_error("reconciliation deposit index is dangling"))?;
            if case.state == ReconciliationState::Open {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
