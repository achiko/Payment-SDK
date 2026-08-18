use super::*;

impl<S> EventWriter for PaymentStore<S>
where
    S: Store,
{
    fn append<'a>(
        &'a self,
        command: AppendObservation,
    ) -> BoxFuture<'a, Result<AppendOutcome, DepositError>> {
        Box::pin(async move { self.append_mirror_only(&command.observation).await })
    }
}

impl<S> EventReader for PaymentStore<S>
where
    S: Store,
{
    fn observation<'a>(
        &'a self,
        event_id: &'a EventId,
    ) -> BoxFuture<'a, Result<Option<MirroredObservation>, DepositError>> {
        Box::pin(async move {
            Ok(self
                .mirrored_observation(event_id)
                .await?
                .map(|(observation, _)| observation))
        })
    }

    fn observations<'a>(
        &'a self,
        request: LogQuery,
    ) -> BoxFuture<'a, Result<LogPage, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > 1_000 {
                return Err(invalid("observation page size must be between 1 and 1000"));
            }
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: observation_cursor_ns(),
                    prefix: Vec::new(),
                    after: request.after.map(cursor_key),
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let has_next = page.next.is_some();
            let mut observations = Vec::with_capacity(page.entries.len());
            for (_, stored) in page.entries {
                let index: IdRecord = decode(&stored)?;
                ensure_version(index.version)?;
                observations.push(
                    self.observation(&EventId(index.id))
                        .await?
                        .ok_or_else(|| storage_error("observation cursor index is dangling"))?,
                );
            }
            let next = if has_next {
                observations
                    .last()
                    .map(|observation| observation.event.cursor)
            } else {
                None
            };
            Ok(LogPage { observations, next })
        })
    }

    fn observations_for_deposit<'a>(
        &'a self,
        request: DepositFilter,
    ) -> BoxFuture<'a, Result<DepositEvents, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > MAX_PAGE_SIZE {
                return Err(invalid(
                    "deposit observation page size must be between 1 and 1000",
                ));
            }
            if request.deposit_id.0.is_empty() {
                return Err(invalid("deposit observation lookup requires a deposit ID"));
            }
            if self.deposit(&request.deposit_id).await?.is_none() {
                return Err(not_found(
                    "deposit observation lookup deposit does not exist",
                ));
            }
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: deposit_observation_ns(),
                    prefix: deposit_observation_prefix(&request.deposit_id)?,
                    after: request
                        .after
                        .map(|cursor| deposit_observation_key(&request.deposit_id, cursor))
                        .transpose()?,
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let has_next = page.next.is_some();
            let mut observations = Vec::with_capacity(page.entries.len());
            for (key, stored) in page.entries {
                let index: IdRecord = decode(&stored)?;
                ensure_version(index.version)?;
                let observation = self
                    .observation(&EventId(index.id))
                    .await?
                    .ok_or_else(|| storage_error("deposit observation index is dangling"))?;
                if key != deposit_observation_key(&request.deposit_id, observation.event.cursor)? {
                    return Err(storage_error(
                        "deposit observation index key does not match its mirrored IX cursor",
                    ));
                }
                observations.push(observation);
            }
            let next = if has_next {
                observations
                    .last()
                    .map(|observation| observation.event.cursor)
            } else {
                None
            };
            Ok(DepositEvents { observations, next })
        })
    }
}

pub(super) fn expected_next_cursor(
    current: Option<EventCursor>,
) -> Result<EventCursor, DepositError> {
    match current {
        Some(cursor) => cursor
            .0
            .checked_add(1)
            .map(EventCursor)
            .ok_or_else(|| invalid("PS consumer cursor is exhausted")),
        None => Ok(EventCursor(1)),
    }
}

pub(super) fn checkpoint_condition(
    name: ConsumerCheckpointName,
    stored: Option<&StoredValue>,
) -> Condition {
    match stored {
        Some(stored) => Condition::Version {
            namespace: consumer_checkpoint_ns(),
            key: checkpoint_key(name),
            expected: stored.version,
        },
        None => Condition::Missing {
            namespace: consumer_checkpoint_ns(),
            key: checkpoint_key(name),
        },
    }
}

impl<S> ProgressReader for PaymentStore<S>
where
    S: Store,
{
    fn consumer_checkpoint<'a>(
        &'a self,
        name: ConsumerCheckpointName,
    ) -> BoxFuture<'a, Result<ConsumerCheckpoint, DepositError>> {
        Box::pin(async move { Ok(self.stored_checkpoint(name).await?.0) })
    }
}
