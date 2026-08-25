use std::{collections::BTreeSet, sync::Arc};

use crate::{
    AddressFilter, BlockRef, BoxFuture, Checkpoint, History, HistoryQuery, IndexError,
    IndexErrorKind, IndexScope, Indexer, SyncStatus, TransactionPage,
};

/// Routes the same indexing contract across independently implemented chains.
pub struct Composer {
    indexers: Vec<Arc<dyn Indexer>>,
    scopes: Vec<IndexScope>,
}

impl Composer {
    pub fn new(indexers: Vec<Arc<dyn Indexer>>) -> Result<Self, IndexError> {
        if indexers.is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "at least one indexer is required",
                false,
            ));
        }
        let mut unique = BTreeSet::new();
        let mut scopes = Vec::new();
        for scope in indexers.iter().flat_map(|indexer| indexer.scopes()) {
            if !unique.insert(scope.clone()) {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "indexers contain the same scope",
                    false,
                ));
            }
            scopes.push(scope.clone());
        }
        Ok(Self { indexers, scopes })
    }

    fn indexer(&self, scope: &IndexScope) -> Result<&dyn Indexer, IndexError> {
        self.indexers
            .iter()
            .find(|indexer| indexer.scopes().contains(scope))
            .map(AsRef::as_ref)
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "index scope is not configured",
                    false,
                )
            })
    }
}

impl Checkpoint for Composer {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        match self.indexer(scope) {
            Ok(indexer) => indexer.checkpoint(scope),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

impl History for Composer {
    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        match self.indexer(&request.address.scope) {
            Ok(indexer) => indexer.history(request),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

impl Indexer for Composer {
    fn scopes(&self) -> &[IndexScope] {
        &self.scopes
    }

    fn sync<'a>(
        &'a self,
        selection: &'a dyn crate::FilterSource,
    ) -> BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>> {
        Box::pin(async move {
            let mut addresses = BTreeSet::new();
            for filter in &selection.filters()? {
                self.indexer(&filter.address.scope)?;
                if filter.address.value.is_empty() || !addresses.insert(filter.address.clone()) {
                    return Err(IndexError::new(
                        IndexErrorKind::InvalidRequest,
                        "address filters must be non-empty and unique",
                        false,
                    ));
                }
            }
            let mut statuses = Vec::with_capacity(self.indexers.len());
            for indexer in &self.indexers {
                // Narrowing happens per read rather than once over a snapshot,
                // so each indexer still reads the selection after it observes
                // its own tip.
                let scoped = Scoped {
                    selection,
                    scopes: indexer.scopes(),
                };
                statuses.extend(indexer.sync(&scoped).await?);
            }
            Ok(statuses)
        })
    }
}

/// One indexer's view of the shared selection.
///
/// Holds the selection rather than a copy of its addresses so the narrowing is
/// applied to whatever the inner indexer reads, at the moment it reads it.
struct Scoped<'a> {
    selection: &'a dyn crate::FilterSource,
    scopes: &'a [IndexScope],
}

impl crate::FilterSource for Scoped<'_> {
    fn filters(&self) -> Result<Vec<AddressFilter>, IndexError> {
        Ok(self
            .selection
            .filters()?
            .into_iter()
            .filter(|filter| self.scopes.contains(&filter.address.scope))
            .collect())
    }
}
