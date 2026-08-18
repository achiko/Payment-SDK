use super::*;

impl Repository {
    pub(super) async fn append_pending(
        &self,
        batch: &mut WriteBatch,
        transaction_id: &TransactionRef,
        change: &PendingChange,
    ) -> Result<(), IndexError> {
        let inclusion = match change {
            PendingChange::None => return Ok(()),
            PendingChange::Add { inclusion } | PendingChange::Remove { inclusion } => *inclusion,
        };
        let key = keys::pending_confirmation(&self.scope, inclusion, transaction_id);
        let current = self.get_record::<PendingConfirmation>(&key).await?;
        match change {
            PendingChange::None => Ok(()),
            PendingChange::Add { .. } => {
                if current.is_some() {
                    return Ok(());
                }
                Self::condition_for::<PendingConfirmation>(batch, key.clone(), None);
                Self::put(
                    batch,
                    key,
                    &PendingConfirmation {
                        transaction_id: record::ScopedValue::from_transaction(transaction_id),
                        inclusion_height: inclusion.0,
                    },
                )
            }
            PendingChange::Remove { .. } => {
                if let Some(current) = current {
                    Self::condition_for(batch, key.clone(), Some(&current));
                    Self::delete(batch, key);
                }
                Ok(())
            }
        }
    }

    pub(super) async fn active_watch_ids(
        &self,
        height: BlockHeight,
    ) -> Result<BTreeSet<WatchId>, IndexError> {
        let records = self
            .scan_records::<WatchRecord>(keys::watch_prefix(&self.scope))
            .await?;
        records
            .into_iter()
            .filter(|(_, watch)| watch.value.start_height <= height.0)
            .map(|(_, watch)| {
                let scope = record::ScopeRecord::into_domain(watch.value.scope.clone());
                record::ensure_record_scope(&self.scope, &scope, "watch")?;
                Ok(WatchId(watch.value.id))
            })
            .collect()
    }

    pub(super) fn append_transition(
        &self,
        batch: &mut WriteBatch,
        transition: &Transition,
    ) -> Result<(), IndexError> {
        let transaction =
            record::ObservationRecord::into_domain(transition.next.transaction.clone())?;
        let current_key = keys::current_observation(&self.scope, &transaction.transaction_id);
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
            &self.scope,
            &transaction.transaction_id,
            transaction.revision,
        );
        batch.conditions.push(Condition::Missing {
            namespace: namespace.clone(),
            key: revision_key.clone(),
        });
        Self::put(batch, revision_key, &transition.next.transaction)?;

        for address in transition
            .prior_addresses
            .difference(&transition.next_addresses)
        {
            Self::delete(
                batch,
                keys::address_transaction(&self.scope, address, &transaction.transaction_id),
            );
        }
        let transaction_record = record::ScopedValue::from_transaction(&transaction.transaction_id);
        for address in transition
            .next_addresses
            .difference(&transition.prior_addresses)
        {
            Self::put(
                batch,
                keys::address_transaction(&self.scope, address, &transaction.transaction_id),
                &transaction_record,
            )?;
        }

        Ok(())
    }
}
