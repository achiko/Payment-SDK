use indexing::{AddressFilter, FilterSource, IndexError, IndexErrorKind, IndexScope, SyncPlan};

use crate::{Error, ErrorKind, Wallets};

pub(crate) fn publication_error() -> Error {
    Error::new(
        ErrorKind::Unavailable,
        "runtime wallet storage has no publication permit",
    )
}

impl<I, F> FilterSource for Wallets<I, F>
where
    I: Clone + Ord + Send + Sync + 'static,
    F: Clone + Ord + Send + Sync + 'static,
{
    fn filters(&self) -> Result<Vec<AddressFilter>, IndexError> {
        Wallets::filters(self).map_err(Error::into_filter_error)
    }

    fn plan(
        &self,
        scope: &IndexScope,
        checkpoint: Option<base::BlockRef>,
    ) -> Result<SyncPlan, IndexError> {
        let admission = self.admission(scope).map_err(Error::into_filter_error)?;
        admission.plan(checkpoint, || {
            Ok(Wallets::filters(self)
                .map_err(Error::into_filter_error)?
                .into_iter()
                .filter(|filter| filter.address.belongs_to(scope))
                .collect())
        })
    }
}

impl Error {
    fn into_filter_error(self) -> IndexError {
        let kind = if self.kind == ErrorKind::Unavailable {
            IndexErrorKind::Store
        } else {
            IndexErrorKind::InvalidRequest
        };
        IndexError::new(kind, self.message, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_failures_remain_terminal_with_exact_message() {
        for (kind, expected) in [
            (ErrorKind::Unavailable, IndexErrorKind::Store),
            (ErrorKind::Unsupported, IndexErrorKind::InvalidRequest),
            (ErrorKind::Conflict, IndexErrorKind::InvalidRequest),
            (ErrorKind::SourceBusy, IndexErrorKind::InvalidRequest),
            (ErrorKind::Transaction, IndexErrorKind::InvalidRequest),
        ] {
            let error = Error::new(kind, "selection failed").into_filter_error();
            assert_eq!(error.kind, expected);
            assert_eq!(error.message, "selection failed");
            assert!(!error.retryable);
        }
    }
}
