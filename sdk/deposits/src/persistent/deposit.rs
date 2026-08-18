use super::*;

impl DepositPlan {
    fn validate(&self) -> Result<(), DepositError> {
        let command = self;
        if command.id.0.is_empty()
            || command.idempotency_key.0.is_empty()
            || command.address.value.is_empty()
            || command.key_purpose.trim().is_empty()
        {
            return Err(invalid(
                "deposit ID, idempotency key, canonical address, and key purpose must be non-empty",
            ));
        }
        if command.key_purpose.len() > MAX_KEY_PURPOSE_BYTES
            || command
                .key_purpose
                .bytes()
                .any(|byte| byte.is_ascii_control())
        {
            return Err(invalid(
                "deposit key purpose must contain between 1 and 1024 safe bytes",
            ));
        }
        if command.asset.chain != command.address.scope.chain
            || command.address.scope.network.is_empty()
        {
            return Err(invalid(
                "deposit asset and address must belong to the same chain",
            ));
        }
        if command.expires_at < command.created_at {
            return Err(invalid("deposit expiration precedes its creation time"));
        }
        Ok(())
    }
}

fn validate_deposit_page(limit: usize) -> Result<(), DepositError> {
    if limit == 0 || limit > MAX_PAGE_SIZE {
        Err(invalid("deposit page size must be between 1 and 1000"))
    } else {
        Ok(())
    }
}

pub(super) fn transition_allowed(current: &DepositState, next: &DepositState) -> bool {
    current == next
        || match (current, next) {
            (DepositState::AwaitingWatch, DepositState::Active { .. }) => true,
            (
                DepositState::Active {
                    watch_id: current_watch,
                },
                DepositState::Expired {
                    watch_id: next_watch,
                },
            ) => current_watch == next_watch,
            _ => false,
        }
}

impl<S> DepositCreator for PaymentStore<S>
where
    S: Store,
{
    fn create_with_ledger<'a>(
        &'a self,
        command: OpenDeposit,
    ) -> BoxFuture<'a, Result<CreatedDeposit, DepositError>> {
        Box::pin(async move {
            command.deposit.validate()?;
            let ledger = open_entry(&command);
            if let Some(deposit) = self.idempotent_deposit(&command.deposit).await? {
                let existing = self
                    .stored_ledger_entry(&deposit.id, &ledger.id)
                    .await?
                    .ok_or_else(|| {
                        storage_error(
                            "idempotent deposit exists without its required opening ledger row",
                        )
                    })?;
                if existing != ledger {
                    return Err(conflict(
                        "deposit idempotency key resolved to a different opening ledger row",
                    ));
                }
                return Ok(CreatedDeposit {
                    deposit,
                    ledger: existing,
                });
            }

            let deposit = deposit_from_create(&command.deposit);
            match self.store_new_deposit(&deposit, Some(&ledger)).await {
                Ok(()) => Ok(CreatedDeposit { deposit, ledger }),
                Err(error) if error.kind == DepositErrorKind::Conflict => {
                    let existing = self
                        .idempotent_deposit(&command.deposit)
                        .await?
                        .ok_or(error)?;
                    let existing_ledger = self
                        .stored_ledger_entry(&existing.id, &ledger.id)
                        .await?
                        .ok_or_else(|| {
                            storage_error(
                                "idempotent deposit exists without its opening ledger row",
                            )
                        })?;
                    Ok(CreatedDeposit {
                        deposit: existing,
                        ledger: existing_ledger,
                    })
                }
                Err(error) => Err(error),
            }
        })
    }

    fn create<'a>(&'a self, command: DepositPlan) -> BoxFuture<'a, Result<Deposit, DepositError>> {
        Box::pin(async move {
            command.validate()?;
            if let Some(deposit) = self.idempotent_deposit(&command).await? {
                return Ok(deposit);
            }
            let deposit = deposit_from_create(&command);
            match self.store_new_deposit(&deposit, None).await {
                Ok(()) => Ok(deposit),
                Err(error) if error.kind == DepositErrorKind::Conflict => {
                    self.idempotent_deposit(&command).await?.ok_or(error)
                }
                Err(error) => Err(error),
            }
        })
    }
}

impl<S> DepositReader for PaymentStore<S>
where
    S: Store,
{
    fn deposit<'a>(
        &'a self,
        id: &'a DepositId,
    ) -> BoxFuture<'a, Result<Option<Deposit>, DepositError>> {
        Box::pin(async move { Ok(self.stored_deposit(id).await?.map(|(deposit, _)| deposit)) })
    }

    fn by_address<'a>(
        &'a self,
        address: &'a CanonicalAddress,
    ) -> BoxFuture<'a, Result<Option<Deposit>, DepositError>> {
        Box::pin(async move {
            let Some(stored) = self
                .storage
                .get(&deposit_address_ns(), &address_key(address)?)
                .await
                .map_err(map_storage)?
            else {
                return Ok(None);
            };
            let index: IdRecord = decode(&stored)?;
            ensure_version(index.version)?;
            self.deposit(&DepositId(index.id)).await
        })
    }

    fn deposits<'a>(
        &'a self,
        request: DepositQuery,
    ) -> BoxFuture<'a, Result<DepositPage, DepositError>> {
        Box::pin(async move {
            validate_deposit_page(request.limit)?;
            if (request.user_id.is_none() && request.state.is_none())
                || !self.deposit_indexes_complete().await?
            {
                return self.scan_authoritative_deposits(&request).await;
            }
            match (&request.user_id, request.state) {
                (None, None) => self.scan_authoritative_deposits(&request).await,
                (Some(user_id), None) => {
                    self.scan_indexed_deposits(
                        user_deposit_ns(),
                        user_deposit_prefix(user_id)?,
                        request
                            .after
                            .as_ref()
                            .map(|id| user_deposit_key(user_id, id))
                            .transpose()?,
                        &request,
                    )
                    .await
                }
                (None, Some(state)) => {
                    self.scan_indexed_deposits(
                        deposit_state_ns(),
                        state_deposit_prefix(state)?,
                        request
                            .after
                            .as_ref()
                            .map(|id| state_deposit_key(state, id))
                            .transpose()?,
                        &request,
                    )
                    .await
                }
                (Some(user_id), Some(state)) => {
                    self.scan_indexed_deposits(
                        user_deposit_state_ns(),
                        user_state_deposit_prefix(user_id, state)?,
                        request
                            .after
                            .as_ref()
                            .map(|id| user_state_deposit_key(user_id, state, id))
                            .transpose()?,
                        &request,
                    )
                    .await
                }
            }
        })
    }
}

impl<S> IndexRebuilder for PaymentStore<S>
where
    S: Store,
{
    fn rebuild_deposit_indexes<'a>(
        &'a self,
        request: RebuildRequest,
    ) -> BoxFuture<'a, Result<IndexRebuild, DepositError>> {
        Box::pin(async move {
            validate_deposit_page(request.limit)?;
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
            let scanned = page.entries.len();
            let mut last_scanned = None;
            for (key, stored) in page.entries {
                let deposit = decode_deposit(&stored)?;
                if key != key_text(&deposit.id.0) {
                    return Err(storage_error(
                        "deposit row key does not match its record ID",
                    ));
                }
                last_scanned = Some(deposit.id.clone());
                self.ensure_deposit_indexes(&deposit.id).await?;
            }
            let next = has_next.then_some(last_scanned).flatten();
            let complete = next.is_none();
            if complete {
                self.storage
                    .commit(WriteBatch {
                        conditions: Vec::new(),
                        operations: vec![Operation::Put {
                            namespace: deposit_index_metadata_ns(),
                            key: deposit_index_complete_key(),
                            value: encode(&IdRecord {
                                version: RECORD_VERSION,
                                id: "complete".to_owned(),
                            })?,
                        }],
                    })
                    .await
                    .map_err(map_storage)?;
            }
            Ok(IndexRebuild {
                scanned,
                next,
                complete,
            })
        })
    }
}
