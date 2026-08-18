use indexing::{
    BackfillReader, BoxFuture, IndexError, IndexErrorKind, IndexScope, IndexedOutput, OutputCursor,
    OutputPage, OutputQuery, OutputRequest, OutputSnapshot, WatchSelector,
};

use crate::{
    IndexRecords, ProjectionCursor, ProjectionEntry, ProjectionGet, ProjectionQuery,
    ProjectionScan, ProjectionSnapshot,
};

pub struct OutputReader<R> {
    repository: R,
}

impl<R> OutputReader<R> {
    #[must_use]
    pub const fn new(repository: R) -> Self {
        Self { repository }
    }
}

impl<R> OutputReader<R>
where
    R: ProjectionQuery,
{
    async fn unspent_outputs(
        &self,
        scope: &IndexScope,
        snapshot: &ProjectionSnapshot,
        entries: Vec<ProjectionEntry>,
    ) -> Result<Vec<IndexedOutput>, IndexError> {
        let mut outputs = Vec::with_capacity(entries.len());
        for entry in entries {
            let output = IndexRecords::decode_output(&entry.key, &entry.value)?;
            if self.is_unspent(scope, snapshot, &output).await? {
                outputs.push(output);
            }
        }
        Ok(outputs)
    }

    async fn is_unspent(
        &self,
        scope: &IndexScope,
        snapshot: &ProjectionSnapshot,
        output: &IndexedOutput,
    ) -> Result<bool, IndexError> {
        let marker_key = IndexRecords::spent_key(&output.key())?;
        let marker = self
            .repository
            .projection_get(ProjectionGet {
                scope: scope.clone(),
                key: marker_key.clone(),
                expected_snapshot: Some(snapshot.clone()),
            })
            .await?;
        if marker.snapshot != *snapshot {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "active output projection changed during pagination",
                true,
            ));
        }
        let Some(value) = marker.value else {
            return Ok(true);
        };
        IndexRecords::decode_spent(&marker_key, &value)?;
        Ok(false)
    }
}

impl<R> OutputQuery for OutputReader<R>
where
    R: ProjectionQuery + BackfillReader<Target = WatchSelector>,
{
    fn outputs<'a>(
        &'a self,
        request: OutputRequest,
    ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move {
            if !self
                .repository
                .pending_watch_backfills(&request.scope, 1)
                .await?
                .is_empty()
            {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "outputs are unavailable while historical watch backfill is pending",
                    true,
                ));
            }
            let page = self
                .repository
                .projection_scan(ProjectionScan {
                    scope: request.scope.clone(),
                    prefix: IndexRecords::output_prefix(&request.address)?,
                    after: request.after.map(projection_cursor),
                    limit: request.limit,
                })
                .await?;
            let outputs = self
                .unspent_outputs(&request.scope, &page.snapshot, page.entries)
                .await?;
            Ok(OutputPage {
                snapshot: output_snapshot(page.snapshot),
                outputs,
                next: page.next.map(output_cursor),
            })
        })
    }
}

fn output_snapshot(snapshot: ProjectionSnapshot) -> OutputSnapshot {
    OutputSnapshot {
        generation: snapshot.generation,
        revision: snapshot.revision,
        checkpoint: snapshot.checkpoint,
    }
}

fn projection_snapshot(snapshot: OutputSnapshot) -> ProjectionSnapshot {
    ProjectionSnapshot {
        generation: snapshot.generation,
        revision: snapshot.revision,
        checkpoint: snapshot.checkpoint,
    }
}

fn output_cursor(cursor: ProjectionCursor) -> OutputCursor {
    OutputCursor {
        snapshot: output_snapshot(cursor.snapshot),
        position: cursor.key,
    }
}

fn projection_cursor(cursor: OutputCursor) -> ProjectionCursor {
    ProjectionCursor {
        snapshot: projection_snapshot(cursor.snapshot),
        key: cursor.position,
    }
}
