use super::*;
use ::storage::Store;

impl Transactions for Repository {
    fn list<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> indexing::BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        Box::pin(async move {
            self.check_scope(&request.scope)?;
            self.check_address(&request.address)?;
            Self::validate_limit(request.limit)?;
            let checkpoint = self.current_checkpoint().await?;
            if request
                .after
                .as_ref()
                .is_some_and(|cursor| cursor.checkpoint != checkpoint)
            {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "history changed during pagination",
                    true,
                ));
            }
            let after = request.after.as_ref().map(|cursor| {
                keys::history(
                    &self.scope,
                    &request.address,
                    cursor.position.height,
                    &cursor.position.transaction,
                )
            });
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: keys::namespace(),
                    prefix: keys::history_prefix(&self.scope, &request.address),
                    after,
                    limit: request.limit,
                })
                .await
                .map_err(Self::storage_error)?;
            let has_more = page.next.is_some();
            let transactions = page
                .entries
                .into_iter()
                .map(|(_, stored)| {
                    Self::decode::<record::TransactionRecord>(&stored.value.0)?.into_domain()
                })
                .collect::<Result<Vec<_>, IndexError>>()?;
            self.ensure_checkpoint(&checkpoint).await?;
            let next = has_more.then(|| {
                transactions.last().map(|transaction| HistoryCursor {
                    checkpoint: checkpoint.clone(),
                    position: indexing::HistoryPosition {
                        height: transaction.block().height,
                        transaction: transaction.transaction_id.clone(),
                    },
                })
            });
            Ok(CanonicalPage {
                checkpoint,
                transactions,
                next: next.flatten(),
            })
        })
    }
}
