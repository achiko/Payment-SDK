use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn stored_deposit(
        &self,
        id: &DepositId,
    ) -> Result<Option<(Deposit, StoredValue)>, DepositError> {
        let stored = self
            .storage
            .get(&deposit_ns(), &key_text(&id.0))
            .await
            .map_err(map_storage)?;
        stored
            .map(|stored| Ok((decode_deposit(&stored)?, stored)))
            .transpose()
    }

    pub(super) async fn stored_ledger_entry(
        &self,
        deposit_id: &DepositId,
        entry_id: &EntryId,
    ) -> Result<Option<LedgerEntry>, DepositError> {
        self.storage
            .get(&ledger_entry_ns(), &ledger_entry_key(deposit_id, entry_id)?)
            .await
            .map_err(map_storage)?
            .map(|stored| decode::<LedgerRecord>(&stored)?.try_into())
            .transpose()
    }

    pub(super) async fn stored_head(
        &self,
        deposit_id: &DepositId,
    ) -> Result<Option<(LedgerEntry, StoredValue)>, DepositError> {
        let Some(stored_head) = self
            .storage
            .get(&ledger_head_ns(), &key_text(&deposit_id.0))
            .await
            .map_err(map_storage)?
        else {
            return Ok(None);
        };
        let head: IdRecord = decode(&stored_head)?;
        ensure_version(head.version)?;
        let entry_id = EntryId(head.id);
        let entry = self
            .stored_ledger_entry(deposit_id, &entry_id)
            .await?
            .ok_or_else(|| storage_error("PS ledger head points to a missing immutable entry"))?;
        Ok(Some((entry, stored_head)))
    }

    pub(crate) async fn expected_ledger_head_condition(
        &self,
        deposit_id: &DepositId,
        expected_head: &EntryId,
    ) -> Result<Condition, DepositError> {
        let (head, stored) = self
            .stored_head(deposit_id)
            .await?
            .ok_or_else(|| not_found("collection participant ledger is not open"))?;
        if &head.id != expected_head {
            return Err(conflict(
                "collection participant expected ledger head does not match current head",
            ));
        }
        Ok(Condition::Version {
            namespace: ledger_head_ns(),
            key: key_text(&deposit_id.0),
            expected: stored.version,
        })
    }

    pub(super) async fn reconciliation_generation_change(
        &self,
        deposit_id: &DepositId,
    ) -> Result<(Condition, Operation), DepositError> {
        let key = key_text(&deposit_id.0);
        let stored = self
            .storage
            .get(&reconciliation_generation_ns(), &key)
            .await
            .map_err(map_storage)?;
        if let Some(stored) = &stored {
            let record: IdRecord = decode(stored)?;
            ensure_version(record.version)?;
            if record.id != deposit_id.0 {
                return Err(storage_error(
                    "reconciliation generation belongs to another deposit",
                ));
            }
        }
        let condition = stored.map_or_else(
            || Condition::Missing {
                namespace: reconciliation_generation_ns(),
                key: key.clone(),
            },
            |stored| Condition::Version {
                namespace: reconciliation_generation_ns(),
                key: key.clone(),
                expected: stored.version,
            },
        );
        let operation = Operation::Put {
            namespace: reconciliation_generation_ns(),
            key,
            value: encode(&IdRecord {
                version: RECORD_VERSION,
                id: deposit_id.0.clone(),
            })?,
        };
        Ok((condition, operation))
    }

    pub(super) async fn idempotent_deposit(
        &self,
        command: &DepositPlan,
    ) -> Result<Option<Deposit>, DepositError> {
        let Some(stored) = self
            .storage
            .get(
                &deposit_idempotency_ns(),
                &key_text(&command.idempotency_key.0),
            )
            .await
            .map_err(map_storage)?
        else {
            return Ok(None);
        };
        let index: IdRecord = decode(&stored)?;
        ensure_version(index.version)?;
        let deposit = self
            .stored_deposit(&DepositId(index.id))
            .await?
            .map(|(deposit, _)| deposit)
            .ok_or_else(|| storage_error("deposit idempotency index points to a missing record"))?;
        let expected = deposit_from_create(command);
        if deposit == expected {
            Ok(Some(deposit))
        } else {
            Err(conflict(
                "deposit idempotency key was reused with a different request",
            ))
        }
    }

    pub(super) async fn store_new_deposit(
        &self,
        deposit: &Deposit,
        ledger: Option<&LedgerEntry>,
    ) -> Result<(), DepositError> {
        let deposit_key = key_text(&deposit.id.0);
        let address_key = address_key(&deposit.address)?;
        let idempotency_key = key_text(&deposit.idempotency_key.0);
        let awaiting_key = key_text(&deposit.id.0);
        let user_key = user_deposit_key(&deposit.user_id, &deposit.id)?;
        let state_key = state_deposit_key(deposit.state.kind(), &deposit.id)?;
        let user_state_key =
            user_state_deposit_key(&deposit.user_id, deposit.state.kind(), &deposit.id)?;
        let mut conditions = vec![
            Condition::Missing {
                namespace: deposit_ns(),
                key: deposit_key.clone(),
            },
            Condition::Missing {
                namespace: deposit_address_ns(),
                key: address_key.clone(),
            },
            Condition::Missing {
                namespace: deposit_idempotency_ns(),
                key: idempotency_key.clone(),
            },
            Condition::Missing {
                namespace: awaiting_watch_ns(),
                key: awaiting_key.clone(),
            },
            Condition::Missing {
                namespace: user_deposit_ns(),
                key: user_key.clone(),
            },
            Condition::Missing {
                namespace: deposit_state_ns(),
                key: state_key.clone(),
            },
            Condition::Missing {
                namespace: user_deposit_state_ns(),
                key: user_state_key.clone(),
            },
        ];
        let id_record = IdRecord {
            version: RECORD_VERSION,
            id: deposit.id.0.clone(),
        };
        let mut operations = vec![
            Operation::Put {
                namespace: deposit_ns(),
                key: deposit_key,
                value: encode(&DepositRecord::from(deposit))?,
            },
            Operation::Put {
                namespace: deposit_address_ns(),
                key: address_key,
                value: encode(&id_record)?,
            },
            Operation::Put {
                namespace: deposit_idempotency_ns(),
                key: idempotency_key,
                value: encode(&id_record)?,
            },
            Operation::Put {
                namespace: awaiting_watch_ns(),
                key: awaiting_key,
                value: encode(&id_record)?,
            },
            Operation::Put {
                namespace: user_deposit_ns(),
                key: user_key,
                value: encode(&id_record)?,
            },
            Operation::Put {
                namespace: deposit_state_ns(),
                key: state_key,
                value: encode(&id_record)?,
            },
            Operation::Put {
                namespace: user_deposit_state_ns(),
                key: user_state_key,
                value: encode(&id_record)?,
            },
        ];
        if let Some(ledger) = ledger {
            let head_key = key_text(&deposit.id.0);
            let entry_key = ledger_entry_key(&deposit.id, &ledger.id)?;
            conditions.extend([
                Condition::Missing {
                    namespace: ledger_head_ns(),
                    key: head_key.clone(),
                },
                Condition::Missing {
                    namespace: ledger_entry_ns(),
                    key: entry_key.clone(),
                },
            ]);
            operations.extend([
                Operation::Put {
                    namespace: ledger_entry_ns(),
                    key: entry_key,
                    value: encode(&LedgerRecord::from(ledger))?,
                },
                Operation::Put {
                    namespace: ledger_head_ns(),
                    key: head_key,
                    value: encode(&IdRecord {
                        version: RECORD_VERSION,
                        id: ledger.id.0.clone(),
                    })?,
                },
            ]);
        }
        self.storage
            .commit(WriteBatch {
                conditions,
                operations,
            })
            .await
            .map_err(map_storage)?;
        Ok(())
    }

    pub(super) async fn scan_authoritative_deposits(
        &self,
        request: &DepositQuery,
    ) -> Result<DepositPage, DepositError> {
        let page = self
            .storage
            .scan(ScanRequest {
                namespace: deposit_ns(),
                prefix: Vec::new(),
                after: request.after.as_ref().map(|id| key_text(&id.0)),
                limit: request.limit,
            })
            .await
            .map_err(map_storage)?;
        let has_next = page.next.is_some();
        let mut last_scanned = None;
        let mut deposits = Vec::with_capacity(page.entries.len());
        for (key, stored) in page.entries {
            let deposit = decode_deposit(&stored)?;
            if key != key_text(&deposit.id.0) {
                return Err(storage_error(
                    "deposit row key does not match its record ID",
                ));
            }
            last_scanned = Some(deposit.id.clone());
            if request
                .user_id
                .as_ref()
                .is_some_and(|user_id| user_id != &deposit.user_id)
                || request
                    .state
                    .is_some_and(|state| state != deposit.state.kind())
            {
                continue;
            }
            deposits.push(deposit);
        }
        Ok(DepositPage {
            deposits,
            next: has_next.then_some(last_scanned).flatten(),
        })
    }

    pub(super) async fn scan_indexed_deposits(
        &self,
        namespace: Namespace,
        prefix: Vec<u8>,
        after: Option<Key>,
        request: &DepositQuery,
    ) -> Result<DepositPage, DepositError> {
        let page = self
            .storage
            .scan(ScanRequest {
                namespace,
                prefix,
                after,
                limit: request.limit,
            })
            .await
            .map_err(map_storage)?;
        let has_next = page.next.is_some();
        let mut deposits = Vec::with_capacity(page.entries.len());
        for (_, stored) in page.entries {
            let index: IdRecord = decode(&stored)?;
            ensure_version(index.version)?;
            let deposit = self
                .stored_deposit(&DepositId(index.id))
                .await?
                .map(|(deposit, _)| deposit)
                .ok_or_else(|| storage_error("deposit association index is dangling"))?;
            if request
                .user_id
                .as_ref()
                .is_some_and(|user_id| user_id != &deposit.user_id)
                || request
                    .state
                    .is_some_and(|state| state != deposit.state.kind())
            {
                return Err(storage_error(
                    "deposit association index does not match its filter",
                ));
            }
            deposits.push(deposit);
        }
        let next = has_next
            .then(|| deposits.last().map(|deposit| deposit.id.clone()))
            .flatten();
        Ok(DepositPage { deposits, next })
    }
}
