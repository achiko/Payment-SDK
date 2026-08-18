use super::*;

impl<S> WatchQueue for PaymentStore<S>
where
    S: Store,
{
    fn awaiting_watch<'a>(
        &'a self,
        request: AwaitingQuery,
    ) -> BoxFuture<'a, Result<AwaitingPage, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > 1_000 {
                return Err(invalid(
                    "AwaitingWatch page size must be between 1 and 1000",
                ));
            }
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: awaiting_watch_ns(),
                    prefix: Vec::new(),
                    after: request.after.as_ref().map(|id| key_text(&id.0)),
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let mut deposits = Vec::with_capacity(page.entries.len());
            for (_, stored) in page.entries {
                let index: IdRecord = decode(&stored)?;
                ensure_version(index.version)?;
                let deposit = self
                    .deposit(&DepositId(index.id))
                    .await?
                    .ok_or_else(|| storage_error("AwaitingWatch index is dangling"))?;
                if deposit.state != DepositState::AwaitingWatch {
                    return Err(storage_error(
                        "AwaitingWatch index references a non-awaiting deposit",
                    ));
                }
                deposits.push(deposit);
            }
            Ok(AwaitingPage {
                deposits,
                next: page
                    .next
                    .map(|key| DepositId(String::from_utf8_lossy(&key.0).into_owned())),
            })
        })
    }

    fn activate_watch<'a>(
        &'a self,
        id: &'a DepositId,
        idempotency_key: &'a IdempotencyKey,
        watch_id: WatchId,
    ) -> BoxFuture<'a, Result<Deposit, DepositError>> {
        Box::pin(async move {
            let (mut deposit, stored) = self
                .stored_deposit(id)
                .await?
                .ok_or_else(|| not_found("deposit does not exist"))?;
            if &deposit.idempotency_key != idempotency_key {
                return Err(conflict(
                    "deposit activation idempotency key does not match creation",
                ));
            }
            match &deposit.state {
                DepositState::Active { watch_id: existing } if existing == &watch_id => {
                    return Ok(deposit);
                }
                DepositState::Active { .. } => {
                    return Err(conflict("deposit is active under a different IX watch"));
                }
                DepositState::AwaitingWatch => {}
                _ => return Err(invalid_state("deposit cannot be activated from its state")),
            }
            let expected_watch_id = watch_id.clone();
            deposit.state = DepositState::Active { watch_id };
            let active_state_key = state_deposit_key(DepositStateKind::Active, id)?;
            let active_user_state_key =
                user_state_deposit_key(&deposit.user_id, DepositStateKind::Active, id)?;
            let index = IdRecord {
                version: RECORD_VERSION,
                id: id.0.clone(),
            };
            let commit = self
                .storage
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Version {
                            namespace: deposit_ns(),
                            key: key_text(&id.0),
                            expected: stored.version,
                        },
                        Condition::Missing {
                            namespace: deposit_state_ns(),
                            key: active_state_key.clone(),
                        },
                        Condition::Missing {
                            namespace: user_deposit_state_ns(),
                            key: active_user_state_key.clone(),
                        },
                    ],
                    operations: vec![
                        Operation::Put {
                            namespace: deposit_ns(),
                            key: key_text(&id.0),
                            value: encode(&DepositRecord::from(&deposit))?,
                        },
                        Operation::Delete {
                            namespace: awaiting_watch_ns(),
                            key: key_text(&id.0),
                        },
                        Operation::Delete {
                            namespace: deposit_state_ns(),
                            key: state_deposit_key(DepositStateKind::AwaitingWatch, id)?,
                        },
                        Operation::Delete {
                            namespace: user_deposit_state_ns(),
                            key: user_state_deposit_key(
                                &deposit.user_id,
                                DepositStateKind::AwaitingWatch,
                                id,
                            )?,
                        },
                        Operation::Put {
                            namespace: deposit_state_ns(),
                            key: active_state_key,
                            value: encode(&index)?,
                        },
                        Operation::Put {
                            namespace: user_deposit_state_ns(),
                            key: active_user_state_key,
                            value: encode(&index)?,
                        },
                    ],
                })
                .await;
            match commit {
                Ok(_) => Ok(deposit),
                Err(error) if error.kind == ErrorKind::Conflict => {
                    self.replay_watch_activation(id, idempotency_key, &expected_watch_id, error)
                        .await
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
    async fn replay_watch_activation(
        &self,
        id: &DepositId,
        idempotency_key: &IdempotencyKey,
        expected_watch_id: &WatchId,
        storage_failure: Error,
    ) -> Result<Deposit, DepositError> {
        let Some((concurrent, _)) = self.stored_deposit(id).await? else {
            return Err(map_storage(storage_failure));
        };
        let replayed = &concurrent.idempotency_key == idempotency_key
            && concurrent.state
                == (DepositState::Active {
                    watch_id: expected_watch_id.clone(),
                });
        if replayed {
            Ok(concurrent)
        } else {
            Err(map_storage(storage_failure))
        }
    }
}
