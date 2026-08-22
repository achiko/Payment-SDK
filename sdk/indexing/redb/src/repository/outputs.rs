use super::*;
use ::storage::Store;

impl Outputs for Repository {
    fn list<'a>(
        &'a self,
        request: OutputRequest,
    ) -> indexing::BoxFuture<'a, Result<OutputPage, IndexError>> {
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
                    "outputs changed during pagination",
                    true,
                ));
            }
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: keys::namespace(),
                    prefix: keys::output_prefix(&self.scope, &request.address),
                    after: request.after.map(|cursor| Key(cursor.position)),
                    limit: request.limit,
                })
                .await
                .map_err(Self::storage_error)?;
            let has_more = page.next.is_some();
            let position = page.entries.last().map(|(key, _)| key.0.clone());
            let outputs = page
                .entries
                .into_iter()
                .map(|(_, stored)| {
                    Self::decode::<record::OutputRecord>(&stored.value.0)?.into_domain()
                })
                .collect::<Result<Vec<_>, IndexError>>()?;
            self.ensure_checkpoint(&checkpoint).await?;
            let next = if has_more {
                Some(OutputCursor {
                    checkpoint: checkpoint.clone(),
                    position: position.ok_or_else(|| {
                        Self::record_error("paginated output page has no final key")
                    })?,
                })
            } else {
                None
            };
            Ok(OutputPage {
                checkpoint,
                outputs,
                next,
            })
        })
    }
}
