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
            self.observe(page)
        })
    }
}

impl<R> Index<R> {
    fn observe(&self, page: CanonicalPage) -> Result<TransactionPage, IndexError> {
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
                        if confirmations >= self.confirmations {
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
            position: crate::BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent: None,
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
        let result = Index::new((), 2).observe(page).expect("canonical history");
        assert!(matches!(
            result.transactions[0].status,
            TransactionStatus::Confirmed { .. }
        ));
    }

    #[test]
    fn confirmation_threshold_uses_produced_height_instead_of_native_position() {
        let checkpoint = BlockRef {
            position: crate::BlockPosition(1_000),
            ..block(3)
        };
        let page = CanonicalPage {
            checkpoint: Some(checkpoint),
            transactions: vec![transaction(&scope(), 2)],
            next: None,
        };
        let included = Index::new((), 3)
            .observe(page.clone())
            .expect("below threshold");
        assert!(matches!(
            included.transactions[0].status,
            TransactionStatus::Included {
                confirmations: 2,
                ..
            }
        ));
        let confirmed = Index::new((), 2).observe(page).expect("at threshold");
        assert!(matches!(
            confirmed.transactions[0].status,
            TransactionStatus::Confirmed {
                confirmations: 2,
                ..
            }
        ));
    }

    #[test]
    fn observation_rejects_missing_checkpoint_and_invalid_confirmation_arithmetic() {
        for (checkpoint, height, message) in [
            (None, 0, "history exists without a checkpoint"),
            (
                Some(block(2)),
                3,
                "history contains a transaction beyond its checkpoint",
            ),
            (
                Some(block(u64::MAX)),
                0,
                "history contains a transaction beyond its checkpoint",
            ),
        ] {
            let page = CanonicalPage {
                checkpoint,
                transactions: vec![transaction(&scope(), height)],
                next: None,
            };
            let error = Index::new((), 1)
                .observe(page)
                .expect_err("invalid included history");
            assert_eq!(error.kind, IndexErrorKind::Store);
            assert_eq!(error.message, message);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn failed_history_keeps_its_existing_passthrough_semantics() {
        let mut failed = transaction(&scope(), 2);
        failed.status = CanonicalStatus::Failed {
            block: block(2),
            reason: Some("reverted".into()),
        };
        let page = CanonicalPage {
            checkpoint: None,
            transactions: vec![failed],
            next: None,
        };
        let observed = Index::new((), u64::MAX)
            .observe(page)
            .expect("failed observation");
        assert_eq!(
            observed.transactions[0].status,
            TransactionStatus::Failed {
                block: block(2),
                reason: Some("reverted".into())
            }
        );
        assert_eq!(observed.checkpoint, None);
        assert_eq!(observed.next, None);
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
