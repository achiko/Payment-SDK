mod allocation;
mod classification;
mod facts;
mod gas;

use std::sync::Arc;

use deposits::{
    CollectionHistory, CollectionReader, CollectionRetry, ConsumerCheckpointName, ConsumerProgress,
    DepositError, DepositReader, EventReader, LedgerReader, LegOutcome, LogQuery,
    MirrorObservation, MirroredObservation, ProjectObservation,
};
use indexing::{EventQuery, IndexError, IndexScope, Observer};

pub trait ObservationStore:
    ConsumerProgress
    + EventReader
    + DepositReader
    + LedgerReader
    + CollectionReader
    + CollectionHistory
    + LegOutcome
    + CollectionRetry
    + Send
    + Sync
{
}

impl<T> ObservationStore for T where
    T: ConsumerProgress
        + EventReader
        + DepositReader
        + LedgerReader
        + CollectionReader
        + CollectionHistory
        + LegOutcome
        + CollectionRetry
        + Send
        + Sync
{
}

/// Moves one scope's IX facts into the durable payment-domain journal.
///
/// Intake and projection use independent durable cursors. A crash can leave
/// projection behind intake, but it cannot lose an event or expose a ledger
/// update without its mirrored source fact.
pub struct DepositObserver {
    scope: IndexScope,
    indexer: Arc<dyn Observer>,
    store: Arc<dyn ObservationStore>,
}

impl DepositObserver {
    #[must_use]
    pub fn new(
        scope: IndexScope,
        indexer: Arc<dyn Observer>,
        store: Arc<dyn ObservationStore>,
    ) -> Self {
        Self {
            scope,
            indexer,
            store,
        }
    }

    #[must_use]
    pub const fn scope(&self) -> &IndexScope {
        &self.scope
    }

    /// Performs bounded intake followed by bounded projection.
    pub async fn pass(&self, limit: usize, received_at: u64) -> Result<Pass, ObserveError> {
        if limit == 0 {
            return Err(ObserveError::Configuration(
                "deposit observer page limit must be positive",
            ));
        }
        let mirrored = self.mirror(limit, received_at).await?;
        let projected = self.project(limit).await?;
        Ok(Pass {
            mirrored,
            projected,
        })
    }

    async fn mirror(&self, limit: usize, received_at: u64) -> Result<usize, ObserveError> {
        let checkpoint = self
            .store
            .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
            .await?;
        let page = self
            .indexer
            .events(EventQuery {
                scope: self.scope.clone(),
                after: checkpoint.cursor,
                limit,
            })
            .await?;
        let mut expected = checkpoint.cursor;
        for event in &page.events {
            if event.transaction.scope != self.scope {
                return Err(ObserveError::Scope);
            }
            self.store
                .mirror_and_advance(MirrorObservation {
                    expected_cursor: expected,
                    observation: MirroredObservation {
                        event: event.clone(),
                        received_at,
                    },
                })
                .await?;
            expected = Some(event.cursor);
        }
        Ok(page.events.len())
    }

    async fn project(&self, limit: usize) -> Result<usize, ObserveError> {
        let checkpoint = self
            .store
            .consumer_checkpoint(ConsumerCheckpointName::IxProjection)
            .await?;
        let page = self
            .store
            .observations(LogQuery {
                after: checkpoint.cursor,
                limit,
            })
            .await?;
        let mut expected = checkpoint.cursor;
        let mut projected = 0;
        for observation in page.observations {
            if observation.event.transaction.scope != self.scope {
                return Err(ObserveError::Scope);
            }
            let projection =
                classification::Projection::classify(self.store.as_ref(), &observation).await?;
            self.store
                .project_and_advance(ProjectObservation {
                    expected_cursor: expected,
                    through: observation.event.cursor,
                    affected_deposits: projection.deposits,
                    ledger_updates: projection.updates,
                    reconciliation_cases: projection.cases,
                    fee_treatment: projection.fees,
                    utxo_batch_transition: projection.batch,
                })
                .await?;
            expected = Some(observation.event.cursor);
            projected += 1;
        }
        Ok(projected)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pass {
    pub mirrored: usize,
    pub projected: usize,
}

#[derive(Debug)]
pub enum ObserveError {
    Configuration(&'static str),
    Scope,
    Index(IndexError),
    Deposit(DepositError),
}

impl From<IndexError> for ObserveError {
    fn from(error: IndexError) -> Self {
        Self::Index(error)
    }
}

impl From<DepositError> for ObserveError {
    fn from(error: DepositError) -> Self {
        Self::Deposit(error)
    }
}

impl std::fmt::Display for ObserveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(message) => formatter.write_str(message),
            Self::Scope => formatter.write_str("deposit observer received another scope's event"),
            Self::Index(error) => write!(formatter, "index observation failed: {error}"),
            Self::Deposit(error) => write!(formatter, "deposit projection failed: {error}"),
        }
    }
}

impl std::error::Error for ObserveError {}
