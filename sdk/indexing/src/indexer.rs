use crate::{
    BlockRef, BlockSelector, Blocks, BoxFuture, CanonicalPage, CanonicalStatus, HistoryQuery,
    IndexError, IndexErrorKind, IndexScope, ObservedTransaction, TransactionPage,
    TransactionStatus, Transactions,
};

#[derive(Clone)]
pub(crate) struct Index<R> {
    repository: R,
    confirmations: u64,
}

impl<R> Index<R> {
    #[must_use]
    pub(crate) const fn new(repository: R, confirmations: u64) -> Self {
        Self {
            repository,
            confirmations,
        }
    }
}

impl<R: Blocks> Checkpoint for Index<R> {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.repository.get(BlockSelector::Tip(scope.clone()))
    }
}

impl<R: Transactions> History for Index<R> {
    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        Box::pin(async move {
            if !request.address.belongs_to(&request.scope) {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "history address belongs to another scope",
                    false,
                ));
            }
            let expected_checkpoint = request
                .after
                .as_ref()
                .map(|cursor| cursor.checkpoint.clone());
            let page = self.repository.list(request).await?;
            if expected_checkpoint.is_some_and(|checkpoint| checkpoint != page.checkpoint) {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "history changed between pages; restart from the first page",
                    true,
                ));
            }
            observe(page, self.confirmations)
        })
    }
}

fn observe(page: CanonicalPage, minimum_confirmations: u64) -> Result<TransactionPage, IndexError> {
    let transactions = page
        .transactions
        .into_iter()
        .map(|transaction| {
            let status = match transaction.status {
                CanonicalStatus::Included { block } => {
                    let confirmations = match page.checkpoint.as_ref() {
                        Some(tip) => tip
                            .height
                            .0
                            .checked_sub(block.height.0)
                            .and_then(|value| value.checked_add(1))
                            .ok_or_else(|| {
                                IndexError::new(
                                    IndexErrorKind::Store,
                                    "history contains a transaction beyond its checkpoint",
                                    false,
                                )
                            })?,
                        None => {
                            return Err(IndexError::new(
                                IndexErrorKind::Store,
                                "history exists without a checkpoint",
                                false,
                            ));
                        }
                    };
                    if confirmations >= minimum_confirmations {
                        TransactionStatus::Confirmed {
                            block,
                            confirmations,
                        }
                    } else {
                        TransactionStatus::Included {
                            block,
                            confirmations,
                        }
                    }
                }
                CanonicalStatus::Failed { block, reason } => {
                    TransactionStatus::Failed { block, reason }
                }
            };
            Ok(ObservedTransaction {
                scope: transaction.scope,
                transaction_id: transaction.transaction_id,
                status,
                movements: transaction.movements,
                fee: transaction.fee,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TransactionPage {
        checkpoint: page.checkpoint,
        transactions,
        next: page.next,
    })
}

pub trait Checkpoint: Send + Sync {
    fn checkpoint<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;
}

pub trait History: Send + Sync {
    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;
}

/// Complete indexing surface shared by one chain index and a multi-chain composer.
pub trait Indexer: Checkpoint + History {
    fn scopes(&self) -> &[IndexScope];

    /// Advances every scope this indexer owns.
    ///
    /// The selection is read during the pass rather than passed in, so it is
    /// never older than the tip the pass indexes towards. See
    /// [`crate::FilterSource`].
    fn sync<'a>(
        &'a self,
        selection: &'a dyn crate::FilterSource,
    ) -> BoxFuture<'a, Result<Vec<crate::SyncStatus>, IndexError>>;
}

#[cfg(test)]
mod tests {
    use base::BlockHeight;

    use super::*;
    use crate::{
        BlockHash, CanonicalTransaction, ChainId, HistoryCursor, HistoryPosition, TransactionRef,
    };

    #[derive(Clone)]
    struct Repository(CanonicalPage);

    impl Transactions for Repository {
        fn list<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("test".into()),
            network: "mainnet".into(),
        }
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent_hash: None,
            timestamp: None,
        }
    }

    fn transaction(scope: &IndexScope, height: u64) -> CanonicalTransaction {
        CanonicalTransaction {
            scope: scope.clone(),
            transaction_id: TransactionRef {
                scope: scope.clone(),
                value: "tx".into(),
            },
            status: CanonicalStatus::Included {
                block: block(height),
            },
            movements: Vec::new(),
            fee: None,
        }
    }

    #[test]
    fn history_derives_confirmation_from_page_checkpoint() {
        let scope = scope();
        let page = CanonicalPage {
            checkpoint: Some(block(3)),
            transactions: vec![transaction(&scope, 2)],
            next: None,
        };
        let result = observe(page, 2).expect("canonical history");
        assert!(matches!(
            result.transactions[0].status,
            TransactionStatus::Confirmed { .. }
        ));
    }

    #[test]
    fn history_rejects_cursor_from_another_checkpoint() {
        let scope = scope();
        let repository = Repository(CanonicalPage {
            checkpoint: Some(block(4)),
            transactions: Vec::new(),
            next: None,
        });
        let index = Index::new(repository, 1);
        let query = HistoryQuery {
            scope: scope.clone(),
            address: crate::CanonicalAddress {
                scope: scope.clone(),
                value: "owner".into(),
            },
            after: Some(HistoryCursor {
                checkpoint: Some(block(3)),
                position: HistoryPosition {
                    height: BlockHeight(2),
                    transaction: TransactionRef {
                        scope,
                        value: "tx".into(),
                    },
                },
            }),
            limit: 10,
        };
        let error = futures_executor::block_on(index.history(query)).unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::Conflict);
    }
}
