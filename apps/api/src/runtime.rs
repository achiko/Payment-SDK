use std::{fmt, time::SystemTime};

use chain_identity::ChainId;
use deposits::{
    AwaitingWatchPageRequest, BoxFuture, ConsumerCheckpointName, DepositAddressRequest,
    DepositAddressSource, DepositError, DepositErrorKind, DepositStore, DepositWatchCoordinator,
    GeneratedDepositAddress, MirrorObservation, MirrorOutcome, MirroredObservation,
    ObservationConsumerCheckpoints, ObservationEventLog, ObservationLogRequest,
    PersistentPaymentRepository,
};
use indexing::{EventCursor, IndexError, IndexScope};
use storage_rocksdb::RocksDbStorage;

use crate::{
    config::{IngestOptions, ProjectionStatusOptions, ReconcileOptions},
    indexer_client::IndexerClient,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub batches: usize,
    pub activated: usize,
    pub exhausted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestionReport {
    pub pages: usize,
    pub appended: usize,
    pub duplicates: usize,
    pub checkpoint: Option<EventCursor>,
    pub exhausted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectionStatusReport {
    pub ingestion_cursor: Option<EventCursor>,
    pub projection_cursor: Option<EventCursor>,
    pub pending_sample: usize,
    pub more_pending: bool,
}

/// Retry the durable half of the deposit-address/watch handshake.
///
/// This path never asks WS for a key or address. It only scans PS-owned
/// `AwaitingWatch` rows and uses their captured birthday/address to perform the
/// idempotent IX acknowledgement.
pub async fn reconcile_watches(
    options: &ReconcileOptions,
) -> Result<ReconcileReport, RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    let client = IndexerClient::new(&options.indexer)?;
    let scope = ethereum_scope(&options.indexer.network);
    let no_address_generation = DisabledAddressGeneration;
    let coordinator =
        DepositWatchCoordinator::new(&repository, &client, &no_address_generation, scope);

    let mut report = ReconcileReport::default();
    while report.batches < options.max_batches {
        let activated = coordinator.resume_awaiting(options.page_size).await?;
        report.batches = report
            .batches
            .checked_add(1)
            .ok_or_else(|| RuntimeError::invariant("reconcile batch counter overflowed"))?;
        report.activated = report
            .activated
            .checked_add(activated)
            .ok_or_else(|| RuntimeError::invariant("reconcile activation counter overflowed"))?;
        if activated < options.page_size {
            return Ok(report);
        }
    }

    let remaining = repository
        .awaiting_watch(AwaitingWatchPageRequest {
            after: None,
            limit: 1,
        })
        .await?;
    report.exhausted = !remaining.deposits.is_empty();
    Ok(report)
}

/// Mirror IX facts and the ingestion cursor atomically without assigning
/// deposit, accounting, collection, or other Payment Service semantics.
pub async fn ingest_events(options: &IngestOptions) -> Result<IngestionReport, RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    let client = IndexerClient::new(&options.indexer)?;
    let scope = ethereum_scope(&options.indexer.network);
    let checkpoint = repository
        .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
        .await?;
    let mut report = IngestionReport {
        checkpoint: checkpoint.cursor,
        ..IngestionReport::default()
    };

    while report.pages < options.max_pages {
        let page = client
            .events(&scope, report.checkpoint, options.page_size)
            .await?;
        report.pages = report
            .pages
            .checked_add(1)
            .ok_or_else(|| RuntimeError::invariant("ingestion page counter overflowed"))?;
        if page.events.is_empty() {
            return Ok(report);
        }

        for event in page.events {
            if event.transaction.scope != scope {
                return Err(RuntimeError::invariant(
                    "Indexer event does not belong to the configured PS scope",
                ));
            }
            let existing = repository.observation(&event.id).await?;
            let received_at = match &existing {
                Some(existing) if existing.event == event => existing.received_at,
                Some(_) => {
                    return Err(RuntimeError::invariant(
                        "mirrored IX event ID was reused with a different payload",
                    ));
                }
                None => unix_timestamp()?,
            };

            if report
                .checkpoint
                .is_some_and(|checkpoint| event.cursor < checkpoint)
            {
                // A stale at-least-once delivery is harmless only when the
                // immutable event is already mirrored byte-for-byte.
                if existing.is_none() {
                    return Err(RuntimeError::invariant(
                        "Indexer delivered an unknown event behind the ingestion cursor",
                    ));
                }
                report.duplicates = report
                    .duplicates
                    .checked_add(1)
                    .ok_or_else(|| RuntimeError::invariant("duplicate counter overflowed"))?;
                continue;
            }

            let cursor = event.cursor;
            match repository
                .mirror_and_advance(MirrorObservation {
                    expected_cursor: report.checkpoint,
                    observation: MirroredObservation { event, received_at },
                })
                .await?
            {
                MirrorOutcome::Appended { .. } => {
                    report.appended = report
                        .appended
                        .checked_add(1)
                        .ok_or_else(|| RuntimeError::invariant("append counter overflowed"))?;
                }
                MirrorOutcome::AlreadyPresent { .. } => {
                    report.duplicates = report
                        .duplicates
                        .checked_add(1)
                        .ok_or_else(|| RuntimeError::invariant("duplicate counter overflowed"))?;
                }
            }
            report.checkpoint = Some(cursor);
        }

        if page.next_cursor.is_none() {
            return Ok(report);
        }
    }

    report.exhausted = true;
    Ok(report)
}

/// Projection intentionally remains separate: classifying mirrored IX facts
/// requires PS deposit/collection/accounting policy that this maintenance
/// runtime is not configured to invent. This command exposes the independent
/// cursor and a bounded backlog sample so an operator can supervise that gap.
pub async fn projection_status(
    options: &ProjectionStatusOptions,
) -> Result<ProjectionStatusReport, RuntimeError> {
    options.validate().map_err(RuntimeError::configuration)?;
    let storage = RocksDbStorage::open(&options.database.database_path)?;
    let repository = PersistentPaymentRepository::new(storage);
    let ingestion = repository
        .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
        .await?;
    let projection = repository
        .consumer_checkpoint(ConsumerCheckpointName::IxProjection)
        .await?;
    if projection.cursor > ingestion.cursor {
        return Err(RuntimeError::invariant(
            "PS projection cursor is ahead of its ingestion cursor",
        ));
    }
    let pending = repository
        .observations(ObservationLogRequest {
            after: projection.cursor,
            limit: options.sample_limit,
        })
        .await?;
    Ok(ProjectionStatusReport {
        ingestion_cursor: ingestion.cursor,
        projection_cursor: projection.cursor,
        pending_sample: pending.observations.len(),
        more_pending: pending.next.is_some(),
    })
}

fn ethereum_scope(network: &str) -> IndexScope {
    IndexScope {
        chain: ChainId("ethereum".to_owned()),
        network: network.to_owned(),
    }
}

fn unix_timestamp() -> Result<u64, RuntimeError> {
    SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|duration| duration.as_secs())
        .map_err(|_| RuntimeError::invariant("system clock precedes the Unix epoch"))
}

struct DisabledAddressGeneration;

impl DepositAddressSource for DisabledAddressGeneration {
    fn address<'a>(
        &'a self,
        _request: DepositAddressRequest,
    ) -> BoxFuture<'a, Result<GeneratedDepositAddress, DepositError>> {
        Box::pin(async {
            Err(DepositError {
                kind: DepositErrorKind::InvalidState,
                message: "AwaitingWatch reconciliation must never generate a new key or address"
                    .to_owned(),
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    fn configuration(error: impl fmt::Display) -> Self {
        Self {
            message: format!("invalid Payment Service configuration: {error}"),
        }
    }

    fn invariant(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

impl From<storage::StorageError> for RuntimeError {
    fn from(error: storage::StorageError) -> Self {
        Self {
            message: error.message,
        }
    }
}

impl From<DepositError> for RuntimeError {
    fn from(error: DepositError) -> Self {
        Self {
            message: error.message,
        }
    }
}

impl From<IndexError> for RuntimeError {
    fn from(error: IndexError) -> Self {
        Self {
            message: error.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::{Json, Router, extract::Query, http::StatusCode, routing::get};
    use deposits::{ConsumerCheckpointName, ObservationConsumerCheckpoints};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    use super::*;
    use crate::config::{DatabaseOptions, IndexerOptions};

    async fn spawn(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener must bind");
        let address = listener.local_addr().expect("listener address must exist");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test server must run");
        });
        format!("http://{address}")
    }

    fn indexer(endpoint: String) -> IndexerOptions {
        IndexerOptions {
            indexer_url: endpoint.parse().expect("test endpoint must parse"),
            network: "test".to_owned(),
            bearer_token: None,
            request_timeout_seconds: 2,
            retry_attempts: 1,
            retry_initial_millis: 0,
            retry_max_millis: 0,
        }
    }

    #[tokio::test]
    async fn ingestion_reuses_the_durable_cursor_and_accepts_identical_redelivery()
    -> Result<(), Box<dyn std::error::Error>> {
        async fn events(Query(query): Query<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
            // Deliberately return event 1 even after cursor 1 to exercise the
            // at-least-once duplicate/restart path.
            assert!(matches!(
                query.get("after_cursor").map(String::as_str),
                None | Some("1")
            ));
            (
                StatusCode::OK,
                Json(json!({
                    "events": [event_json()],
                    "next_cursor": null
                })),
            )
        }

        let endpoint = spawn(Router::new().route("/v1/events", get(events))).await;
        let directory = TempDir::new()?;
        let options = IngestOptions {
            database: DatabaseOptions {
                database_path: directory.path().join("payment-service"),
            },
            indexer: indexer(endpoint),
            page_size: 10,
            max_pages: 2,
        };

        let first = ingest_events(&options).await?;
        assert_eq!(first.appended, 1);
        assert_eq!(first.checkpoint, Some(EventCursor(1)));

        let second = ingest_events(&options).await?;
        assert_eq!(second.appended, 0);
        assert_eq!(second.duplicates, 1);
        assert_eq!(second.checkpoint, Some(EventCursor(1)));

        let storage = RocksDbStorage::open(&options.database.database_path)?;
        let repository = PersistentPaymentRepository::new(storage);
        let durable = repository
            .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
            .await?;
        assert_eq!(durable.cursor, Some(EventCursor(1)));
        Ok(())
    }

    fn event_json() -> Value {
        json!({
            "id": "event-1",
            "cursor": "1",
            "watch_ids": ["watch-1"],
            "previous_status": null,
            "transaction": {
                "scope": {"chain": "ethereum", "network": "test"},
                "transaction_id": format!("0x{}", "22".repeat(32)),
                "revision": "1",
                "status": {
                    "kind": "included",
                    "block": {
                        "height": "42",
                        "hash": format!("0x{}", "11".repeat(32)),
                        "parent_hash": format!("0x{}", "10".repeat(32)),
                        "timestamp": "1000"
                    },
                    "confirmations": "1"
                },
                "movements": [],
                "fee": null,
                "first_seen_at": "1000",
                "observed_at": "1001"
            }
        })
    }
}
