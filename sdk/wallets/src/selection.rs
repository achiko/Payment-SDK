use std::sync::Arc;

use indexing::{
    AddressFilter, FilterSource, IndexError, IndexErrorKind, IndexScope, PublicationPermit,
    SyncPlan,
};

use crate::{Error, ErrorKind, Wallets};

pub(crate) fn admission<I: Ord, F: Ord>(
    wallets: &Wallets<I, F>,
    scope: &IndexScope,
) -> Result<Arc<indexing::ScopeAdmission>, Error> {
    wallets.admissions.get(scope).cloned().ok_or_else(|| {
        Error::new(
            ErrorKind::Unsupported,
            "wallet family scope has no address admission",
        )
    })
}

pub(crate) fn publication(permit: Option<PublicationPermit>) -> Result<PublicationPermit, Error> {
    permit.ok_or_else(|| {
        Error::new(
            ErrorKind::Unavailable,
            "runtime wallet storage has no publication permit",
        )
    })
}

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
        Wallets::filters(self).map_err(filter_error)
    }

    fn plan(
        &self,
        scope: &IndexScope,
        checkpoint: Option<base::BlockRef>,
    ) -> Result<SyncPlan, IndexError> {
        let admission = admission(self, scope).map_err(filter_error)?;
        admission.plan(checkpoint, || {
            Ok(Wallets::filters(self)
                .map_err(filter_error)?
                .into_iter()
                .filter(|filter| filter.address.belongs_to(scope))
                .collect())
        })
    }
}

fn filter_error(error: Error) -> IndexError {
    let kind = if error.kind == ErrorKind::Unavailable {
        IndexErrorKind::Store
    } else {
        IndexErrorKind::InvalidRequest
    };
    IndexError::new(kind, error.message, false)
}
