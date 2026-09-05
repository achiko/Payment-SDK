use std::{path::PathBuf, task::Poll};

use super::*;
use storage::{Condition, ErrorKind, Operation, Version};
use tempfile::TempDir;

fn namespace(name: &str) -> Namespace {
    Namespace(name.to_owned())
}

fn key(value: &str) -> Key {
    Key(value.as_bytes().to_vec())
}

fn value(value: &str) -> storage::Value {
    storage::Value(value.as_bytes().to_vec())
}

fn database_path(directory: &TempDir) -> PathBuf {
    directory.path().join("database.redb")
}

fn put(namespace: &Namespace, key: &Key, value: &str) -> Operation {
    Operation::Put {
        namespace: namespace.clone(),
        key: key.clone(),
        value: self::value(value),
    }
}

#[tokio::test]
async fn cancellation_after_enqueue_does_not_cancel_the_accepted_commit() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let storage = Redb::open(database_path(&directory))?;
    let records = namespace("cancelled-request");
    let record_key = key("accepted-write");
    let release = storage.hold_owner_for_test().await?;
    let mut commit = storage.commit(WriteBatch {
        conditions: Vec::new(),
        operations: vec![put(&records, &record_key, "durable")],
    });

    std::future::poll_fn(|context| match commit.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("owner hold must keep the accepted commit pending"),
    })
    .await;
    drop(commit);
    release
        .send(())
        .map_err(|_| other("failed to release redb owner test hold"))?;

    let stored = storage.get(&records, &record_key).await?;
    assert_eq!(stored.map(|stored| stored.value), Some(value("durable")));
    Ok(())
}

#[tokio::test]
async fn ambiguous_commit_successful_reopen_does_not_replay() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let storage = Redb::open(database_path(&directory))?;
    let records = namespace("records");
    let primary = key("primary");
    storage.fail_after_next_commit_for_test().await?;

    let error = storage
        .commit(WriteBatch {
            conditions: vec![],
            operations: vec![put(&records, &primary, "persisted-once")],
        })
        .await
        .expect_err("the injected post-persistence result must be ambiguous");

    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert_eq!(storage.reopen_count_for_test().await?, 1);
    assert_eq!(
        storage.get(&records, &primary).await?,
        Some(StoredValue {
            value: value("persisted-once"),
            version: Version(1),
        })
    );
    assert_eq!(storage.reopen_count_for_test().await?, 1);
    Ok(())
}

#[tokio::test]
async fn read_recovers_after_ambiguous_commit_reopen_failure() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let storage = Redb::open(database_path(&directory))?;
    let records = namespace("records");
    let primary = key("primary");
    storage.fail_after_next_commit_for_test().await?;
    storage.fail_next_reopen_for_test().await?;

    let error = storage
        .commit(WriteBatch {
            conditions: vec![],
            operations: vec![put(&records, &primary, "persisted-once")],
        })
        .await
        .expect_err("the injected post-persistence result must be ambiguous");

    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert_eq!(storage.reopen_count_for_test().await?, 0);
    assert_eq!(
        storage.get(&records, &primary).await?,
        Some(StoredValue {
            value: value("persisted-once"),
            version: Version(1),
        })
    );
    assert_eq!(storage.reopen_count_for_test().await?, 1);
    let page = storage
        .scan(ScanRequest {
            namespace: records.clone(),
            prefix: b"primary".to_vec(),
            after: None,
            limit: 1,
        })
        .await?;
    assert_eq!(
        page.entries,
        vec![(
            primary.clone(),
            StoredValue {
                value: value("persisted-once"),
                version: Version(1),
            }
        )]
    );
    assert_eq!(page.next, None);
    Ok(())
}

#[tokio::test]
async fn put_get_and_paginated_scan_are_ordered() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let storage = Redb::open(database_path(&directory))?;
    let observations = namespace("observations");
    let other_namespace = namespace("other");

    let result = storage
        .commit(WriteBatch {
            conditions: vec![],
            operations: vec![
                put(&observations, &key("tx/a"), "a"),
                put(&observations, &key("tx/b"), "b"),
                put(&observations, &key("tx/c"), "c"),
                put(&other_namespace, &key("tx/a"), "isolated"),
            ],
        })
        .await?;
    assert_eq!(result.version, Version(1));
    assert_eq!(
        storage.get(&observations, &key("tx/b")).await?,
        Some(StoredValue {
            value: value("b"),
            version: Version(1),
        })
    );

    let first = storage
        .scan(ScanRequest {
            namespace: observations.clone(),
            prefix: b"tx/".to_vec(),
            after: None,
            limit: 2,
        })
        .await?;
    assert_eq!(
        first
            .entries
            .iter()
            .map(|(entry_key, _)| entry_key.clone())
            .collect::<Vec<_>>(),
        vec![key("tx/a"), key("tx/b")]
    );
    assert_eq!(first.next, Some(key("tx/b")));

    let second = storage
        .scan(ScanRequest {
            namespace: observations,
            prefix: b"tx/".to_vec(),
            after: first.next,
            limit: 2,
        })
        .await?;
    assert_eq!(
        second
            .entries
            .iter()
            .map(|(entry_key, _)| entry_key.clone())
            .collect::<Vec<_>>(),
        vec![key("tx/c")]
    );
    assert_eq!(second.next, None);
    Ok(())
}

#[tokio::test]
async fn stale_condition_rejects_the_complete_batch() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let storage = Redb::open(database_path(&directory))?;
    let records = namespace("records");
    let primary = key("primary");
    let side_effect = key("side-effect");

    storage
        .commit(WriteBatch {
            conditions: vec![Condition::Missing {
                namespace: records.clone(),
                key: primary.clone(),
            }],
            operations: vec![put(&records, &primary, "v1")],
        })
        .await?;
    storage
        .commit(WriteBatch {
            conditions: vec![Condition::Version {
                namespace: records.clone(),
                key: primary.clone(),
                expected: Version(1),
            }],
            operations: vec![put(&records, &primary, "v2")],
        })
        .await?;

    let error = storage
        .commit(WriteBatch {
            conditions: vec![Condition::Version {
                namespace: records.clone(),
                key: primary.clone(),
                expected: Version(1),
            }],
            operations: vec![
                put(&records, &primary, "stale"),
                put(&records, &side_effect, "must-not-commit"),
            ],
        })
        .await
        .expect_err("a stale compare-and-swap must fail");
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert_eq!(storage.get(&records, &side_effect).await?, None);
    assert_eq!(
        storage.get(&records, &primary).await?,
        Some(StoredValue {
            value: value("v2"),
            version: Version(2),
        })
    );

    let next = storage
        .commit(WriteBatch {
            conditions: vec![Condition::Missing {
                namespace: records.clone(),
                key: side_effect.clone(),
            }],
            operations: vec![put(&records, &side_effect, "committed")],
        })
        .await?;
    assert_eq!(next.version, Version(3));
    Ok(())
}

#[tokio::test]
async fn absent_or_present_condition_conflicts_preserve_batch_and_version() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let storage = Redb::open(database_path(&directory))?;
    let records = namespace("records");
    let other_namespace = namespace("other");
    let primary = key("primary");
    let missing = key("missing");
    let side_effect = key("side-effect");
    storage
        .commit(WriteBatch {
            conditions: vec![],
            operations: vec![put(&records, &primary, "original")],
        })
        .await?;

    for (condition, message) in [
        (
            Condition::Missing {
                namespace: records.clone(),
                key: primary.clone(),
            },
            "missing condition failed in namespace `records` because the key exists",
        ),
        (
            Condition::Version {
                namespace: records.clone(),
                key: missing.clone(),
                expected: Version(2),
            },
            "version condition failed in namespace `records` because the key is missing",
        ),
    ] {
        let error = storage
            .commit(WriteBatch {
                conditions: vec![condition],
                operations: vec![
                    Operation::Delete {
                        namespace: records.clone(),
                        key: primary.clone(),
                    },
                    put(&records, &missing, "must-not-commit"),
                    put(&other_namespace, &side_effect, "must-not-commit"),
                ],
            })
            .await
            .expect_err("conditions must inspect the state before any batch operation");

        assert_eq!(error.kind, ErrorKind::Conflict);
        assert_eq!(error.message, message);
        assert_eq!(storage.get(&records, &missing).await?, None);
        assert_eq!(storage.get(&other_namespace, &side_effect).await?, None);
        assert_eq!(
            storage.get(&records, &primary).await?,
            Some(StoredValue {
                value: value("original"),
                version: Version(1),
            })
        );
    }

    let next = storage
        .commit(WriteBatch {
            conditions: vec![Condition::Missing {
                namespace: records.clone(),
                key: missing.clone(),
            }],
            operations: vec![put(&records, &missing, "committed")],
        })
        .await?;
    assert_eq!(next.version, Version(2));
    Ok(())
}

#[tokio::test]
async fn concurrent_compare_and_swap_has_one_winner() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let storage = Redb::open(database_path(&directory))?;
    let records = namespace("records");
    let primary = key("primary");
    storage
        .commit(WriteBatch {
            conditions: vec![],
            operations: vec![put(&records, &primary, "v1")],
        })
        .await?;

    let first = storage.clone();
    let second = storage.clone();
    let first_batch = WriteBatch {
        conditions: vec![Condition::Version {
            namespace: records.clone(),
            key: primary.clone(),
            expected: Version(1),
        }],
        operations: vec![put(&records, &primary, "first")],
    };
    let second_batch = WriteBatch {
        conditions: vec![Condition::Version {
            namespace: records.clone(),
            key: primary.clone(),
            expected: Version(1),
        }],
        operations: vec![put(&records, &primary, "second")],
    };

    let (first_result, second_result) =
        tokio::join!(first.commit(first_batch), second.commit(second_batch));
    let successes = usize::from(first_result.is_ok()) + usize::from(second_result.is_ok());
    let conflicts = usize::from(matches!(
        first_result,
        Err(Error {
            kind: ErrorKind::Conflict,
            ..
        })
    )) + usize::from(matches!(
        second_result,
        Err(Error {
            kind: ErrorKind::Conflict,
            ..
        })
    ));
    assert_eq!(successes, 1);
    assert_eq!(conflicts, 1);
    Ok(())
}

#[tokio::test]
async fn delete_removes_the_value() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let storage = Redb::open(database_path(&directory))?;
    let records = namespace("records");
    let primary = key("primary");
    storage
        .commit(WriteBatch {
            conditions: vec![],
            operations: vec![put(&records, &primary, "v1")],
        })
        .await?;

    let result = storage
        .commit(WriteBatch {
            conditions: vec![Condition::Version {
                namespace: records.clone(),
                key: primary.clone(),
                expected: Version(1),
            }],
            operations: vec![Operation::Delete {
                namespace: records.clone(),
                key: primary.clone(),
            }],
        })
        .await?;

    assert_eq!(result.version, Version(2));
    assert_eq!(storage.get(&records, &primary).await?, None);
    Ok(())
}

#[tokio::test]
async fn persisted_values_and_global_version_survive_reopen() -> Result<(), Error> {
    let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
    let database = database_path(&directory);
    let records = namespace("records");
    let primary = key("primary");
    {
        let storage = Redb::open(&database)?;
        storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &primary, "v1")],
            })
            .await?;
    }

    let reopened = Redb::open(&database)?;
    assert_eq!(
        reopened.get(&records, &primary).await?,
        Some(StoredValue {
            value: value("v1"),
            version: Version(1),
        })
    );
    let result = reopened
        .commit(WriteBatch {
            conditions: vec![],
            operations: vec![put(&records, &key("second"), "v2")],
        })
        .await?;
    assert_eq!(result.version, Version(2));
    Ok(())
}
