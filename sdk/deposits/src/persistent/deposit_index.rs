use super::*;

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn deposit_indexes_complete(&self) -> Result<bool, DepositError> {
        let Some(stored) = self
            .storage
            .get(&deposit_index_metadata_ns(), &deposit_index_complete_key())
            .await
            .map_err(map_storage)?
        else {
            return Ok(false);
        };
        let record: IdRecord = decode(&stored)?;
        ensure_version(record.version)?;
        if record.id != "complete" {
            return Err(storage_error(
                "deposit index completion marker has an invalid value",
            ));
        }
        Ok(true)
    }

    pub(super) async fn ensure_deposit_indexes(&self, id: &DepositId) -> Result<(), DepositError> {
        for _ in 0..3 {
            if self.ensure_deposit_index_attempt(id).await? {
                return Ok(());
            }
        }
        Err(conflict(
            "deposit association indexes changed concurrently during rebuild",
        ))
    }

    async fn ensure_deposit_index_attempt(&self, id: &DepositId) -> Result<bool, DepositError> {
        let (deposit, stored_deposit) = self
            .stored_deposit(id)
            .await?
            .ok_or_else(|| storage_error("deposit disappeared during index rebuild"))?;
        let index = IdRecord {
            version: RECORD_VERSION,
            id: deposit.id.0.clone(),
        };
        let specifications = [
            (
                user_deposit_ns(),
                user_deposit_key(&deposit.user_id, &deposit.id)?,
            ),
            (
                deposit_state_ns(),
                state_deposit_key(deposit.state.kind(), &deposit.id)?,
            ),
            (
                user_deposit_state_ns(),
                user_state_deposit_key(&deposit.user_id, deposit.state.kind(), &deposit.id)?,
            ),
        ];
        let mut conditions = vec![Condition::Version {
            namespace: deposit_ns(),
            key: key_text(&deposit.id.0),
            expected: stored_deposit.version,
        }];
        let mut operations = Vec::new();
        for (namespace, key) in &specifications {
            self.ensure_deposit_index(namespace, key, &index, &mut conditions, &mut operations)
                .await?;
        }
        if operations.is_empty() {
            return Ok(true);
        }
        match self
            .storage
            .commit(WriteBatch {
                conditions,
                operations,
            })
            .await
            .map_err(map_storage)
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind == DepositErrorKind::Conflict => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn ensure_deposit_index(
        &self,
        namespace: &Namespace,
        key: &Key,
        index: &IdRecord,
        conditions: &mut Vec<Condition>,
        operations: &mut Vec<Operation>,
    ) -> Result<(), DepositError> {
        let Some(stored) = self
            .storage
            .get(namespace, key)
            .await
            .map_err(map_storage)?
        else {
            conditions.push(Condition::Missing {
                namespace: namespace.clone(),
                key: key.clone(),
            });
            operations.push(Operation::Put {
                namespace: namespace.clone(),
                key: key.clone(),
                value: encode(index)?,
            });
            return Ok(());
        };
        let persisted: IdRecord = decode(&stored)?;
        ensure_version(persisted.version)?;
        if persisted != *index {
            return Err(storage_error(
                "deposit association index points to a different deposit",
            ));
        }
        Ok(())
    }
}
