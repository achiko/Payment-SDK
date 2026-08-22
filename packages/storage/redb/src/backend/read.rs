use redb::ReadableDatabase;
use storage::{Error, ErrorKind, Key, Namespace, ScanPage, ScanRequest, StoredValue};

use crate::codec::{
    decode_physical_key, decode_stored_value, encode_physical_key, namespace_prefix,
};

use super::{
    Backend, DATA_TABLE, invalid_request, operation_error, table_error, transaction_error,
};

impl Backend {
    pub(super) fn get(
        &mut self,
        namespace: &Namespace,
        key: &Key,
    ) -> Result<Option<StoredValue>, Error> {
        let physical_key = encode_physical_key(namespace, key)?;
        self.read_with_reopen(|backend| backend.get_once(&physical_key))
    }

    fn get_once(&self, physical_key: &[u8]) -> Result<Option<StoredValue>, Error> {
        let transaction = self
            .database()?
            .begin_read()
            .map_err(|error| transaction_error(error, "failed to begin redb point read"))?;
        let table = transaction
            .open_table(DATA_TABLE)
            .map_err(|error| table_error(error, "failed to open redb data table"))?;
        let raw = table
            .get(physical_key)
            .map_err(|error| operation_error(error, "redb point read failed"))?;
        raw.map(|value| decode_stored_value(value.value()))
            .transpose()
    }

    pub(super) fn scan(&mut self, request: ScanRequest) -> Result<ScanPage, Error> {
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

        let mut physical_prefix = namespace_prefix(&request.namespace)?;
        physical_prefix.extend_from_slice(&request.prefix);
        let start = match &request.after {
            Some(after) => encode_physical_key(&request.namespace, after)?,
            None => physical_prefix.clone(),
        };

        self.read_with_reopen(|backend| {
            backend.scan_once(&request, &physical_prefix, &start, read_limit)
        })
    }

    fn scan_once(
        &self,
        request: &ScanRequest,
        physical_prefix: &[u8],
        start: &[u8],
        read_limit: usize,
    ) -> Result<ScanPage, Error> {
        let transaction = self
            .database()?
            .begin_read()
            .map_err(|error| transaction_error(error, "failed to begin redb prefix scan"))?;
        let table = transaction
            .open_table(DATA_TABLE)
            .map_err(|error| table_error(error, "failed to open redb data table"))?;
        let iterator = table
            .range(start..)
            .map_err(|error| operation_error(error, "failed to start redb prefix scan"))?;
        let mut entries = Vec::with_capacity(request.limit.min(256));
        for item in iterator {
            let (physical_key, raw_value) =
                item.map_err(|error| operation_error(error, "redb prefix scan failed"))?;
            if !physical_key.value().starts_with(physical_prefix) {
                break;
            }

            let logical_key = decode_physical_key(physical_key.value(), &request.namespace)?;
            if request
                .after
                .as_ref()
                .is_some_and(|after| logical_key <= *after)
            {
                continue;
            }
            entries.push((logical_key, decode_stored_value(raw_value.value())?));
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

    fn read_with_reopen<T>(
        &mut self,
        read: impl Fn(&Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.ensure_open()?;
        match read(self) {
            Err(error) if error.kind == ErrorKind::Unavailable => {
                self.reopen()?;
                match read(self) {
                    Err(error) if error.kind == ErrorKind::Unavailable => {
                        // A redb I/O failure latches the handle. Leave it closed
                        // so a later read can make a fresh reopen attempt.
                        self.db = None;
                        Err(error)
                    }
                    result => result,
                }
            }
            result => result,
        }
    }
}
