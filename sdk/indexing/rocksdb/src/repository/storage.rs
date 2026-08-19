use super::*;
use ::storage::Store;

impl Repository {
    pub(super) fn check_scope(&self, scope: &IndexScope) -> Result<(), IndexError> {
        if &self.scope == scope {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "request belongs to another repository scope",
                false,
            ))
        }
    }

    pub(super) async fn get<T: Decode<()>>(
        &self,
        key: &Key,
    ) -> Result<Option<Stored<T>>, IndexError> {
        self.storage
            .get(&keys::namespace(), key)
            .await
            .map_err(Self::storage_error)?
            .map(|stored| {
                Ok(Stored {
                    value: Self::decode(&stored.value.0)?,
                    version: stored.version,
                })
            })
            .transpose()
    }

    pub(super) fn put<T: Encode>(
        batch: &mut WriteBatch,
        key: Key,
        value: &T,
    ) -> Result<(), IndexError> {
        batch.operations.push(Operation::Put {
            namespace: keys::namespace(),
            key,
            value: Value(Self::encode(value)?),
        });
        Ok(())
    }

    pub(super) fn delete(batch: &mut WriteBatch, key: Key) {
        batch.operations.push(Operation::Delete {
            namespace: keys::namespace(),
            key,
        });
    }

    pub(super) fn expect<T>(batch: &mut WriteBatch, key: Key, value: Option<&Stored<T>>) {
        batch.conditions.push(match value {
            Some(value) => Condition::Version {
                namespace: keys::namespace(),
                key,
                expected: value.version,
            },
            None => Condition::Missing {
                namespace: keys::namespace(),
                key,
            },
        });
    }

    pub(super) fn encode<T: Encode>(value: &T) -> Result<Vec<u8>, IndexError> {
        bincode::encode_to_vec(value, config::standard())
            .map_err(|error| Self::record_error(format!("cannot encode record: {error}")))
    }

    pub(super) fn decode<T: Decode<()>>(value: &[u8]) -> Result<T, IndexError> {
        let (record, consumed) = bincode::decode_from_slice(value, config::standard())
            .map_err(|error| Self::record_error(format!("cannot decode record: {error}")))?;
        if consumed != value.len() {
            return Err(Self::record_error("record has trailing bytes"));
        }
        Ok(record)
    }

    pub(super) fn validate_limit(limit: usize) -> Result<(), IndexError> {
        if (1..=MAX_PAGE).contains(&limit) {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "query limit must be between 1 and 1000",
                false,
            ))
        }
    }

    pub(crate) fn record_error(message: impl Into<String>) -> IndexError {
        IndexError::new(IndexErrorKind::Store, message, false)
    }

    pub(super) fn storage_error(error: ::storage::Error) -> IndexError {
        let (kind, retryable) = match error.kind {
            ::storage::ErrorKind::Conflict => (IndexErrorKind::Conflict, true),
            ::storage::ErrorKind::Unavailable => (IndexErrorKind::Store, true),
            ::storage::ErrorKind::CorruptData
            | ::storage::ErrorKind::InvalidRequest
            | ::storage::ErrorKind::Other => (IndexErrorKind::Store, false),
        };
        IndexError::new(kind, error.message, retryable)
    }

    pub(super) fn check_address(
        &self,
        address: &indexing::CanonicalAddress,
    ) -> Result<(), IndexError> {
        if address.scope == self.scope && !address.value.is_empty() {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "address belongs to another repository scope",
                false,
            ))
        }
    }

    pub(super) async fn current_checkpoint(&self) -> Result<Option<BlockRef>, IndexError> {
        self.get::<record::BlockRecord>(&keys::checkpoint(&self.scope))
            .await
            .map(|value| value.map(|stored| stored.value.into_domain()))
    }

    pub(super) async fn ensure_checkpoint(
        &self,
        expected: &Option<BlockRef>,
    ) -> Result<(), IndexError> {
        if &self.current_checkpoint().await? == expected {
            Ok(())
        } else {
            Err(IndexError::new(
                IndexErrorKind::Conflict,
                "canonical state changed while reading a page",
                true,
            ))
        }
    }
}
