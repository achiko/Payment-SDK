use super::*;

impl<S> DatabaseInitializer for PaymentStore<S>
where
    S: Store,
{
    fn initialize_or_validate_principal_scope<'a>(
        &'a self,
        command: InitializeDatabase,
        principal_scope_mode: PrincipalScopeMode,
    ) -> BoxFuture<'a, Result<DatabaseIdentity, DepositError>> {
        Box::pin(async move {
            command.validate()?;
            let expected = expected_metadata(command, principal_scope_mode);
            // Bound PS metadata does not make a mixed-owner database safe.
            // Re-check IX ownership on every startup before the metadata fast path.
            if self.namespace_has_records(ix_semantic_ns()).await? {
                return Err(conflict(
                    "database contains Indexer Service records and cannot be owned by Payment Service",
                ));
            }
            if let Some(persisted) = self.stored_database_metadata().await? {
                return validate_persisted_metadata(persisted, &expected);
            }
            if self.has_unbound_ps_records().await? {
                return Err(conflict(
                    "existing Payment Service records lack current database metadata",
                ));
            }
            let commit = self
                .storage()
                .commit(WriteBatch {
                    conditions: vec![
                        Condition::Missing {
                            namespace: database_metadata_ns(),
                            key: database_metadata_key(),
                        },
                        // IX writes this path-global identity key with its first
                        // semantic mutation. Refuse an already-owned IX path in
                        // the same atomic check as PS initialization.
                        Condition::Missing {
                            namespace: ix_semantic_ns(),
                            key: Key(vec![1, 1]),
                        },
                    ],
                    operations: vec![Operation::Put {
                        namespace: database_metadata_ns(),
                        key: database_metadata_key(),
                        value: encode(&DatabaseRecord::from(&expected))?,
                    }],
                })
                .await
                .map_err(map_storage);
            match commit {
                Ok(_) => Ok(expected),
                Err(error) if error.kind == DepositErrorKind::Conflict => self
                    .stored_database_metadata()
                    .await?
                    .ok_or(error)
                    .and_then(|persisted| validate_persisted_metadata(persisted, &expected)),
                Err(error) => Err(error),
            }
        })
    }
}

impl<S> MetadataReader for PaymentStore<S>
where
    S: Store,
{
    fn database_metadata(&self) -> BoxFuture<'_, Result<Option<DatabaseIdentity>, DepositError>> {
        Box::pin(async move { self.stored_database_metadata().await })
    }
}
