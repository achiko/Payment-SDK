use super::*;

impl Repository {
    pub(super) fn encode<T: Encode>(value: &T) -> Result<Value, IndexError> {
        bincode::encode_to_vec(value, config::standard())
            .map(Value)
            .map_err(|error| {
                IndexError::new(
                    IndexErrorKind::Store,
                    format!("failed to encode an IX Record: {error}"),
                    false,
                )
            })
    }

    pub(super) fn decode<T: Decode<()>>(value: &[u8]) -> Result<T, IndexError> {
        let (decoded, consumed) = bincode::decode_from_slice::<T, _>(value, config::standard())
            .map_err(|error| {
                IndexError::new(
                    IndexErrorKind::Store,
                    format!("failed to decode an IX Record: {error}"),
                    false,
                )
            })?;
        if consumed != value.len() {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "persisted IX record contains trailing bytes",
                false,
            ));
        }
        Ok(decoded)
    }

    pub(super) fn storage_error(error: Error) -> IndexError {
        match error.kind {
            ErrorKind::Conflict => IndexError::new(IndexErrorKind::Conflict, error.message, true),
            ErrorKind::Unavailable => IndexError::new(IndexErrorKind::Store, error.message, true),
            ErrorKind::CorruptData | ErrorKind::InvalidRequest | ErrorKind::Other => {
                IndexError::new(IndexErrorKind::Store, error.message, false)
            }
        }
    }

    pub(super) fn check_scope(&self, scope: &IndexScope) -> Result<(), IndexError> {
        if scope == &self.scope {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "request scope does not match the persistent repository scope",
                false,
            ))
        }
    }

    pub(super) async fn get_record<T: Decode<()>>(
        &self,
        key: &Key,
    ) -> Result<Option<StoredRecord<T>>, IndexError> {
        let stored = self
            .storage
            .get(&keys::namespace(), key)
            .await
            .map_err(Self::storage_error)?;
        stored
            .map(|stored| {
                Self::decode(&stored.value.0).map(|value| StoredRecord {
                    value,
                    version: stored.version,
                })
            })
            .transpose()
    }

    pub(super) async fn get_projection_record(
        &self,
        key: &Key,
    ) -> Result<Option<StoredRecord<Vec<u8>>>, IndexError> {
        self.storage
            .get(&keys::namespace(), key)
            .await
            .map_err(Self::storage_error)
            .map(|stored| {
                stored.map(|stored| StoredRecord {
                    value: stored.value.0,
                    version: stored.version,
                })
            })
    }

    pub(super) async fn scan_records<T: Decode<()>>(
        &self,
        prefix: Vec<u8>,
    ) -> Result<Vec<(Key, StoredRecord<T>)>, IndexError> {
        let mut after = None;
        let mut records = Vec::new();
        loop {
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: keys::namespace(),
                    prefix: prefix.clone(),
                    after,
                    limit: SCAN_CHUNK,
                })
                .await
                .map_err(Self::storage_error)?;
            for (key, stored) in page.entries {
                records.push((
                    key,
                    StoredRecord {
                        value: Self::decode(&stored.value.0)?,
                        version: stored.version,
                    },
                ));
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        Ok(records)
    }

    pub(super) async fn verify_metadata(&self) -> Result<(), IndexError> {
        let key = keys::meta(&self.scope);
        if let Some(meta) = self.get_record::<RepositoryMeta>(&key).await? {
            self.validate_meta(&meta.value)?;
        }
        Ok(())
    }

    pub(crate) fn expected_meta(&self) -> RepositoryMeta {
        RepositoryMeta {
            format: REPOSITORY_FORMAT,
            scope: record::ScopeRecord::from_domain(&self.scope),
        }
    }

    pub(crate) fn validate_meta(&self, meta: &RepositoryMeta) -> Result<(), IndexError> {
        if meta.format != REPOSITORY_FORMAT {
            return Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                format!(
                    "persisted IX record format {} is incompatible with required format {}",
                    meta.format, REPOSITORY_FORMAT
                ),
                false,
            ));
        }
        if meta == &self.expected_meta() {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::PolicyMismatch,
                "persisted IX scope, bootstrap height, confirmation policy, or retention differs from runtime configuration",
                false,
            ))
        }
    }

    pub(super) async fn mutation_batch(&self) -> Result<WriteBatch, IndexError> {
        let namespace = keys::namespace();
        let meta_key = keys::meta(&self.scope);
        let guard_key = keys::mutation_guard(&self.scope);
        let meta = self.get_record::<RepositoryMeta>(&meta_key).await?;
        let guard = self.get_record::<CounterRecord>(&guard_key).await?;
        let mut batch = WriteBatch::default();

        match meta {
            Some(meta) => {
                self.validate_meta(&meta.value)?;
                batch.conditions.push(Condition::Version {
                    namespace: namespace.clone(),
                    key: meta_key,
                    expected: meta.version,
                });
            }
            None => {
                batch.conditions.push(Condition::Missing {
                    namespace: namespace.clone(),
                    key: meta_key.clone(),
                });
                batch.operations.push(Operation::Put {
                    namespace: namespace.clone(),
                    key: meta_key,
                    value: Self::encode(&self.expected_meta())?,
                });
            }
        }

        let next_guard = match guard {
            Some(guard) => {
                batch.conditions.push(Condition::Version {
                    namespace: namespace.clone(),
                    key: guard_key.clone(),
                    expected: guard.version,
                });
                guard.value.value.checked_add(1).ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::Store,
                        "IX mutation guard is exhausted",
                        false,
                    )
                })?
            }
            None => {
                batch.conditions.push(Condition::Missing {
                    namespace: namespace.clone(),
                    key: guard_key.clone(),
                });
                1
            }
        };
        batch.operations.push(Operation::Put {
            namespace,
            key: guard_key,
            value: Self::encode(&CounterRecord { value: next_guard })?,
        });
        Ok(batch)
    }

    pub(super) fn condition_for<T>(
        batch: &mut WriteBatch,
        key: Key,
        record: Option<&StoredRecord<T>>,
    ) {
        let namespace = keys::namespace();
        match record {
            Some(record) => batch.conditions.push(Condition::Version {
                namespace,
                key,
                expected: record.version,
            }),
            None => batch.conditions.push(Condition::Missing { namespace, key }),
        }
    }

    pub(super) fn put<T: Encode>(
        batch: &mut WriteBatch,
        key: Key,
        value: &T,
    ) -> Result<(), IndexError> {
        batch.operations.push(Operation::Put {
            namespace: keys::namespace(),
            key,
            value: Self::encode(value)?,
        });
        Ok(())
    }

    pub(super) fn delete(batch: &mut WriteBatch, key: Key) {
        batch.operations.push(Operation::Delete {
            namespace: keys::namespace(),
            key,
        });
    }
}
