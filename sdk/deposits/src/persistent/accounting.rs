use super::ledger::{projection_entry, resolved_effect, resolved_network_fee};
use super::reconciliation_support::opaque_command_ledger_entry_id;
use super::*;

fn accounting_entry(command: &AccountingCommand, current: &LedgerEntry) -> LedgerEntry {
    let mut balances = current.balances.clone();
    balances.accounted = command.next_accounted.clone();
    LedgerEntry {
        id: opaque_command_ledger_entry_id("accounting", &command.command),
        deposit_id: command.deposit_id.clone(),
        previous: Some(current.id.clone()),
        cause: LedgerEntryCause::Accounting {
            idempotency_key: command.command.client_key.clone(),
            reason: command.reason.clone(),
        },
        balances,
        recorded_at: command.recorded_at,
    }
}

impl AccountingCommand {
    fn validate(&self) -> Result<(), DepositError> {
        let command = self;
        if command.command.operation != CommandOperation::Accounting {
            return Err(invalid(
                "accounting command identity must use the accounting operation",
            ));
        }
        if command.command.principal.0.is_empty()
            || command.command.client_key.0.is_empty()
            || command.deposit_id.0.is_empty()
        {
            return Err(invalid(
                "accounting principal, client key, and deposit ID must be non-empty",
            ));
        }
        if command.reason.trim().is_empty() {
            return Err(invalid("accounting reason must not be blank"));
        }
        if command.reason.len() > MAX_ACCOUNTING_REASON_BYTES {
            return Err(invalid(format!(
                "accounting reason must not exceed {MAX_ACCOUNTING_REASON_BYTES} bytes"
            )));
        }
        Ok(())
    }
}

impl<S> LedgerWriter for PaymentStore<S>
where
    S: Store,
{
    fn open<'a>(&'a self, command: OpenLedger) -> BoxFuture<'a, Result<ApplyResult, DepositError>> {
        Box::pin(async move {
            if let Some((current, _)) = self.stored_head(&command.deposit_id).await? {
                let expected_cause = LedgerEntryCause::Opened {
                    idempotency_key: command.idempotency_key.clone(),
                };
                if current.previous.is_none()
                    && current.cause == expected_cause
                    && current.balances == DepositBalances::default()
                {
                    return Ok(ApplyResult::AlreadyPresent { entry: current });
                }
                return Err(conflict(
                    "deposit ledger is already open under a different command",
                ));
            }
            let deposit = self
                .deposit(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("cannot open a ledger for a missing deposit"))?;
            if deposit.idempotency_key != command.idempotency_key {
                return Err(conflict(
                    "ledger-open idempotency key does not match deposit creation",
                ));
            }
            let entry = LedgerEntry {
                id: EntryId(format!("open:{}", command.deposit_id.0)),
                deposit_id: command.deposit_id.clone(),
                previous: None,
                cause: LedgerEntryCause::Opened {
                    idempotency_key: command.idempotency_key,
                },
                balances: DepositBalances::default(),
                recorded_at: command.recorded_at,
            };
            self.storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: ledger_head_ns(),
                            key: key_text(&entry.deposit_id.0),
                        },
                        Condition::Missing {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&entry.deposit_id, &entry.id)?,
                        },
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&entry.deposit_id, &entry.id)?,
                            value: encode(&LedgerRecord::from(&entry))?,
                        },
                        Operation::Put {
                            namespace: ledger_head_ns(),
                            key: key_text(&entry.deposit_id.0),
                            value: encode(&IdRecord {
                                version: RECORD_VERSION,
                                id: entry.id.0.clone(),
                            })?,
                        },
                    ],
                })
                .await
                .map_err(map_storage)?;
            Ok(ApplyResult::Appended { entry })
        })
    }

    fn record_observation<'a>(
        &'a self,
        command: RecordObservation,
    ) -> BoxFuture<'a, Result<ApplyResult, DepositError>> {
        Box::pin(async move {
            let mirrored = self
                .mirrored_observation(&command.event_id)
                .await?
                .map(|(observation, _)| observation)
                .ok_or_else(|| not_found("observation projection requires a mirrored IX event"))?;
            let deposit = self
                .deposit(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("observation projection deposit does not exist"))?;
            let expected_head_id = command
                .expected_head
                .as_ref()
                .ok_or_else(|| conflict("observation projection requires an expected head"))?;
            let expected_head = self
                .stored_ledger_entry(&command.deposit_id, expected_head_id)
                .await?
                .ok_or_else(|| conflict("ledger expected head does not exist"))?;
            let entry = projection_entry(
                &command,
                &mirrored.event,
                &expected_head,
                resolved_effect(&mirrored.event, &deposit, &command.effect)?,
                if matches!(command.effect, LedgerEffect::NetBalanceChange { .. }) {
                    None
                } else {
                    resolved_network_fee(&mirrored.event, &deposit)
                },
            )?;
            let projection_id = ProjectionId::for_observation(
                &mirrored.event.id,
                mirrored.event.transaction.revision,
                &command.deposit_id,
            );
            let projection_key = key_text(&projection_id.0);
            let deposit_observation_key =
                deposit_observation_key(&command.deposit_id, mirrored.event.cursor)?;
            let stored_deposit_observation = self
                .storage
                .get(&deposit_observation_ns(), &deposit_observation_key)
                .await
                .map_err(map_storage)?;
            if let Some(stored) = &stored_deposit_observation {
                let index: IdRecord = decode(stored)?;
                ensure_version(index.version)?;
                if index.id != mirrored.event.id.0 {
                    return Err(conflict(
                        "deposit observation cursor is assigned to a different IX event",
                    ));
                }
            }
            if let Some(stored) = self
                .storage
                .get(&projection_ns(), &projection_key)
                .await
                .map_err(map_storage)?
            {
                let index: IdRecord = decode(&stored)?;
                ensure_version(index.version)?;
                let existing = self
                    .stored_ledger_entry(&command.deposit_id, &EntryId(index.id))
                    .await?
                    .ok_or_else(|| storage_error("projection index is dangling"))?;
                if existing == entry {
                    if stored_deposit_observation.is_none() {
                        self.storage
                            .commit(WriteBatch {
                                conditions: vec![Condition::Missing {
                                    namespace: deposit_observation_ns(),
                                    key: deposit_observation_key.clone(),
                                }],
                                operations: vec![Operation::Put {
                                    namespace: deposit_observation_ns(),
                                    key: deposit_observation_key,
                                    value: encode(&IdRecord {
                                        version: RECORD_VERSION,
                                        id: mirrored.event.id.0.clone(),
                                    })?,
                                }],
                            })
                            .await
                            .map_err(map_storage)?;
                    }
                    return Ok(ApplyResult::AlreadyPresent { entry: existing });
                }
                return Err(conflict(
                    "deterministic projection identity was reused with a different ledger effect",
                ));
            }
            let (current, head_stored) = self
                .stored_head(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("deposit ledger is not open"))?;
            if command.expected_head.as_ref() != Some(&current.id) {
                return Err(conflict("ledger expected head does not match current head"));
            }
            let mut conditions = vec![
                Condition::Missing {
                    namespace: projection_ns(),
                    key: projection_key.clone(),
                },
                Condition::Version {
                    namespace: ledger_head_ns(),
                    key: key_text(&command.deposit_id.0),
                    expected: head_stored.version,
                },
                Condition::Missing {
                    namespace: ledger_entry_ns(),
                    key: ledger_entry_key(&command.deposit_id, &entry.id)?,
                },
            ];
            let mut operations = vec![
                Operation::Put {
                    namespace: ledger_entry_ns(),
                    key: ledger_entry_key(&command.deposit_id, &entry.id)?,
                    value: encode(&LedgerRecord::from(&entry))?,
                },
                Operation::Put {
                    namespace: ledger_head_ns(),
                    key: key_text(&command.deposit_id.0),
                    value: encode(&IdRecord {
                        version: RECORD_VERSION,
                        id: entry.id.0.clone(),
                    })?,
                },
                Operation::Put {
                    namespace: projection_ns(),
                    key: projection_key,
                    value: encode(&IdRecord {
                        version: RECORD_VERSION,
                        id: entry.id.0.clone(),
                    })?,
                },
            ];
            if stored_deposit_observation.is_none() {
                conditions.push(Condition::Missing {
                    namespace: deposit_observation_ns(),
                    key: deposit_observation_key.clone(),
                });
                operations.push(Operation::Put {
                    namespace: deposit_observation_ns(),
                    key: deposit_observation_key,
                    value: encode(&IdRecord {
                        version: RECORD_VERSION,
                        id: mirrored.event.id.0.clone(),
                    })?,
                });
            }
            self.storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await
                .map_err(map_storage)?;
            Ok(ApplyResult::Appended { entry })
        })
    }

    fn record_accounting<'a>(
        &'a self,
        command: AccountingCommand,
    ) -> BoxFuture<'a, Result<ApplyResult, DepositError>> {
        Box::pin(async move {
            command.validate()?;
            let idempotency_key = accounting_command_key(&command.command)?;
            if let Some(stored) = self
                .storage
                .get(&accounting_idempotency_ns(), &idempotency_key)
                .await
                .map_err(map_storage)?
            {
                let index: AccountingReplay = decode(&stored)?;
                ensure_version(index.version)?;
                let stored_command = CommandIdentity::try_from(index.command)?;
                if stored_command != command.command {
                    return Err(conflict(
                        "accounting idempotency key was reused with a different request hash",
                    ));
                }
                let existing = self
                    .stored_ledger_entry(&command.deposit_id, &EntryId(index.ledger_entry_id))
                    .await?
                    .ok_or_else(|| storage_error("accounting idempotency index is dangling"))?;
                return Ok(ApplyResult::AlreadyPresent { entry: existing });
            }
            if self.automatic_actions_blocked(&command.deposit_id).await? {
                return Err(invalid_state(
                    "automatic accounting is blocked by an open reconciliation case",
                ));
            }
            let (current, head_stored) = self
                .stored_head(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("deposit ledger is not open"))?;
            if command.expected_head.as_ref() != Some(&current.id) {
                return Err(conflict("ledger expected head does not match current head"));
            }
            if command.next_accounted > current.balances.confirmed {
                return Err(invalid(
                    "accounted value cannot exceed confirmation-qualified value",
                ));
            }
            let entry = accounting_entry(&command, &current);
            self.storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: accounting_idempotency_ns(),
                            key: idempotency_key.clone(),
                        },
                        Condition::Version {
                            namespace: ledger_head_ns(),
                            key: key_text(&command.deposit_id.0),
                            expected: head_stored.version,
                        },
                        Condition::Missing {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&command.deposit_id, &entry.id)?,
                        },
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: ledger_entry_ns(),
                            key: ledger_entry_key(&command.deposit_id, &entry.id)?,
                            value: encode(&LedgerRecord::from(&entry))?,
                        },
                        Operation::Put {
                            namespace: ledger_head_ns(),
                            key: key_text(&command.deposit_id.0),
                            value: encode(&IdRecord {
                                version: RECORD_VERSION,
                                id: entry.id.0.clone(),
                            })?,
                        },
                        Operation::Put {
                            namespace: accounting_idempotency_ns(),
                            key: idempotency_key,
                            value: encode(&AccountingReplay {
                                version: RECORD_VERSION,
                                command: AccountingIdentity::from(&command.command),
                                ledger_entry_id: entry.id.0.clone(),
                            })?,
                        },
                    ],
                })
                .await
                .map_err(map_storage)?;
            Ok(ApplyResult::Appended { entry })
        })
    }
}
