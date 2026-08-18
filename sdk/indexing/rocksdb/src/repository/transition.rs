use super::*;

impl<S, C> Repository<S, C>
where
    S: Store,
    C: IndexRecordCodec,
{
    pub(super) fn validate_draft(
        &self,
        draft: &ObservationDraft,
        active_watch_ids: &BTreeSet<WatchId>,
    ) -> Result<(), IndexError> {
        if draft.scope != self.config.scope {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "observation draft belongs to another scope",
                false,
            ));
        }
        self.validate_transaction_id(&draft.transaction_id)?;
        if matches!(draft.status, ObservationDraftStatus::Failed { .. })
            && !draft.movements.is_empty()
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "failed observation draft contains movements",
                false,
            ));
        }
        let mut movement_ids = BTreeSet::new();
        for movement in &draft.movements {
            if movement.id().0.is_empty() || !movement_ids.insert(movement.id().clone()) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "observation draft contains an empty or duplicate movement ID",
                    false,
                ));
            }
            if movement.asset().chain != self.config.scope.chain
                || movement
                    .from()
                    .is_some_and(|address| address.scope != self.config.scope)
                || movement
                    .to()
                    .is_some_and(|address| address.scope != self.config.scope)
            {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "observation movement belongs to another chain",
                    false,
                ));
            }
        }
        if draft.fee.as_ref().is_some_and(|fee| {
            fee.asset.chain != self.config.scope.chain
                || fee
                    .payer
                    .as_ref()
                    .is_some_and(|payer| payer.scope != self.config.scope)
        }) {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "observation fee belongs to another chain",
                false,
            ));
        }
        let mut watch_ids = BTreeSet::new();
        for watch_id in &draft.watch_ids {
            if !watch_ids.insert(watch_id.clone()) || !active_watch_ids.contains(watch_id) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidWatch,
                    "observation draft references a duplicate, unknown, or inactive watch",
                    false,
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn active_watch_ids(
        &self,
        height: BlockHeight,
    ) -> Result<BTreeSet<WatchId>, IndexError> {
        let records = self
            .scan_records::<WatchRecord>(keys::watch_prefix(&self.config.scope))
            .await?;
        records
            .into_iter()
            .filter(|(_, watch)| {
                watch.value.start_height <= height.0
                    && watch
                        .value
                        .inactive_from
                        .is_none_or(|inactive| height.0 < inactive)
            })
            .map(|(_, watch)| {
                let scope = record::ScopeRecord::into_domain(watch.value.scope.clone());
                record::ensure_record_scope(&self.config.scope, &scope, "watch")?;
                Ok(WatchId(watch.value.id))
            })
            .collect()
    }

    pub(super) fn next_observation(
        &self,
        prior: Option<&CurrentObservation>,
        transaction_id: &TransactionRef,
        status: TransactionStatus,
        draft: Option<&ObservationDraft>,
        observed_at: u64,
    ) -> Result<CurrentObservation, IndexError> {
        let prior_domain = prior
            .map(|prior| record::ObservationRecord::into_domain(prior.transaction.clone()))
            .transpose()?;
        let revision = prior_domain.as_ref().map_or(Ok(1), |prior| {
            prior.revision.0.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "observation revision is exhausted",
                    false,
                )
            })
        })?;
        let mut watch_ids = draft.map_or_else(
            || {
                prior
                    .map(|prior| prior.watch_ids.clone())
                    .unwrap_or_default()
            },
            |draft| draft.watch_ids.iter().map(|id| id.0.clone()).collect(),
        );
        watch_ids.sort();
        watch_ids.dedup();
        let transaction = ObservedTransaction {
            scope: self.config.scope.clone(),
            transaction_id: transaction_id.clone(),
            revision: ObservationRevision(revision),
            status,
            movements: draft.map_or_else(
                || {
                    prior_domain
                        .as_ref()
                        .map(|prior| prior.movements.clone())
                        .unwrap_or_default()
                },
                |draft| draft.movements.clone(),
            ),
            fee: draft.map_or_else(
                || prior_domain.as_ref().and_then(|prior| prior.fee.clone()),
                |draft| draft.fee.clone(),
            ),
            first_seen_at: draft.map_or_else(
                || {
                    prior_domain
                        .as_ref()
                        .map_or(observed_at, |prior| prior.first_seen_at)
                },
                |draft| {
                    prior_domain
                        .as_ref()
                        .map_or(draft.first_seen_at, |prior| prior.first_seen_at)
                },
            ),
            observed_at,
        };
        Ok(CurrentObservation {
            transaction: record::ObservationRecord::from_domain(&transaction),
            watch_ids,
        })
    }

    pub(super) fn observation_addresses(
        observation: &CurrentObservation,
    ) -> Result<BTreeSet<CanonicalAddress>, IndexError> {
        Ok(
            record::ObservationRecord::into_domain(observation.transaction.clone())?
                .movements
                .into_iter()
                .flat_map(|movement| {
                    movement
                        .from()
                        .cloned()
                        .into_iter()
                        .chain(movement.to().cloned())
                })
                .collect(),
        )
    }

    pub(super) fn event_id(cursor: EventCursor, revision: ObservationRevision) -> String {
        format!("ix-event-{:020}-{:020}", cursor.0, revision.0)
    }

    pub(super) fn append_transition(
        &self,
        batch: &mut WriteBatch,
        generation: RebuildGeneration,
        transition: &Transition,
        cursor: Option<EventCursor>,
    ) -> Result<(), IndexError> {
        let transaction =
            record::ObservationRecord::into_domain(transition.next.transaction.clone())?;
        let current_key =
            keys::current_observation(&self.config.scope, generation, &transaction.transaction_id);
        let namespace = keys::namespace();
        match transition.prior_version {
            Some(expected) => batch.conditions.push(Condition::Version {
                namespace: namespace.clone(),
                key: current_key.clone(),
                expected,
            }),
            None => batch.conditions.push(Condition::Missing {
                namespace: namespace.clone(),
                key: current_key.clone(),
            }),
        }
        Self::put(batch, current_key, &transition.next)?;

        let revision_key = keys::observation_revision(
            &self.config.scope,
            generation,
            &transaction.transaction_id,
            transaction.revision,
        );
        batch.conditions.push(Condition::Missing {
            namespace: namespace.clone(),
            key: revision_key.clone(),
        });
        Self::put(batch, revision_key, &transition.next.transaction)?;

        let prior_addresses = if transition.prior_indexed_in_generation {
            transition
                .prior
                .as_ref()
                .map(Self::observation_addresses)
                .transpose()?
                .unwrap_or_default()
        } else {
            BTreeSet::new()
        };
        let next_addresses = Self::observation_addresses(&transition.next)?;
        for address in prior_addresses.difference(&next_addresses) {
            Self::delete(
                batch,
                keys::address_transaction(
                    &self.config.scope,
                    generation,
                    address,
                    &transaction.transaction_id,
                ),
            );
        }
        let transaction_record = record::ScopedValue::from_transaction(&transaction.transaction_id);
        for address in next_addresses.difference(&prior_addresses) {
            Self::put(
                batch,
                keys::address_transaction(
                    &self.config.scope,
                    generation,
                    address,
                    &transaction.transaction_id,
                ),
                &transaction_record,
            )?;
        }

        if let Some(cursor) = cursor {
            let id = Self::event_id(cursor, transaction.revision);
            let event = EventRecord {
                id: id.clone(),
                cursor: cursor.0,
                watch_ids: transition.next.watch_ids.clone(),
                previous_status: transition
                    .prior
                    .as_ref()
                    .map(|prior| prior.transaction.status.clone()),
                transaction: transition.next.transaction.clone(),
            };
            let event_key = keys::event(&self.config.scope, cursor);
            let event_id_key = keys::event_id(&self.config.scope, &id);
            batch.conditions.push(Condition::Missing {
                namespace: namespace.clone(),
                key: event_key.clone(),
            });
            batch.conditions.push(Condition::Missing {
                namespace,
                key: event_id_key.clone(),
            });
            Self::put(batch, event_key, &event)?;
            Self::put(batch, event_id_key, &EventPointer { cursor: cursor.0 })?;
        }
        Ok(())
    }
}
