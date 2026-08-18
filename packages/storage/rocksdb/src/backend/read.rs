use rocksdb::{Direction, IteratorMode};
use storage::{Error, Key, Namespace, ScanPage, ScanRequest, StoredValue};

use crate::codec::{
    decode_physical_key, decode_stored_value, encode_physical_key, namespace_prefix,
};

use super::{Backend, invalid_request, unavailable};

impl Backend {
    pub(super) fn get(
        &self,
        namespace: &Namespace,
        key: &Key,
    ) -> Result<Option<StoredValue>, Error> {
        let physical_key = encode_physical_key(namespace, key)?;
        let raw = self
            .db
            .get_cf(self.data_cf()?, physical_key)
            .map_err(|error| unavailable(format!("RocksDB point read failed: {error}")))?;
        raw.as_deref().map(decode_stored_value).transpose()
    }

    pub(super) fn scan(&self, request: ScanRequest) -> Result<ScanPage, Error> {
        if request.limit == 0 {
            return Err(invalid_request("scan limit must be greater than zero"));
        }
        let read_limit = request
            .limit
            .checked_add(1)
            .ok_or_else(|| invalid_request("scan limit is too large"))?;
        if request
            .after
            .as_ref()
            .is_some_and(|after| !after.0.starts_with(&request.prefix))
        {
            return Err(invalid_request(
                "scan continuation key does not match the requested prefix",
            ));
        }

        let namespace_prefix = namespace_prefix(&request.namespace)?;
        let mut physical_prefix = namespace_prefix.clone();
        physical_prefix.extend_from_slice(&request.prefix);
        let start = match &request.after {
            Some(after) => encode_physical_key(&request.namespace, after)?,
            None => physical_prefix.clone(),
        };

        let mut entries = Vec::with_capacity(request.limit.min(256));
        let iterator = self.db.full_iterator_cf(
            self.data_cf()?,
            IteratorMode::From(&start, Direction::Forward),
        );
        for item in iterator {
            let (physical_key, raw_value) =
                item.map_err(|error| unavailable(format!("RocksDB prefix scan failed: {error}")))?;
            if !physical_key.starts_with(&physical_prefix) {
                break;
            }

            let logical_key = decode_physical_key(&physical_key, &request.namespace)?;
            if request
                .after
                .as_ref()
                .is_some_and(|after| logical_key <= *after)
            {
                continue;
            }
            entries.push((logical_key, decode_stored_value(&raw_value)?));
            if entries.len() == read_limit {
                break;
            }
        }

        let next = if entries.len() > request.limit {
            entries.pop();
            entries.last().map(|(key, _)| key.clone())
        } else {
            None
        };
        Ok(ScanPage { entries, next })
    }
}
