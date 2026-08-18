use std::sync::Arc;

use storage::{
    BoxFuture, Condition, Error, Key, Namespace, Operation, ScanRequest, Store as StorageBackend,
    Value, Version, WriteBatch,
};

use indexing::{EventCursor, IndexScope};

use crate::{Payment, Scope, Stage};

const NAMESPACE: &str = "payments";
const CURSOR_NAMESPACE: &str = "payment-observer-cursors";
const SCAN_LIMIT: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPayment {
    pub payment: Payment,
    pub version: Version,
}

pub trait Repository: Send + Sync {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<StoredPayment>, Error>>;

    fn create<'a>(&'a self, payment: Payment) -> BoxFuture<'a, Result<StoredPayment, Error>>;

    fn update<'a>(
        &'a self,
        payment: Payment,
        expected: Version,
    ) -> BoxFuture<'a, Result<StoredPayment, Error>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCursor {
    pub cursor: EventCursor,
    pub version: Version,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileState {
    pub cursor: Option<StoredCursor>,
    pub payments: Vec<StoredPayment>,
}

pub struct ReconcileBatch {
    pub scope: IndexScope,
    pub cursor: Option<StoredCursor>,
    pub next: Option<EventCursor>,
    pub payments: Vec<StoredPayment>,
}

pub trait ReconcileStore: Send + Sync {
    fn reconcile_state<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<ReconcileState, Error>>;

    fn commit_reconciliation<'a>(
        &'a self,
        batch: ReconcileBatch,
    ) -> BoxFuture<'a, Result<(), Error>>;
}

pub trait Storage: Repository + ReconcileStore {}

impl<T> Storage for T where T: Repository + ReconcileStore {}

pub struct StorageRepository {
    storage: Arc<dyn StorageBackend>,
    namespace: Namespace,
    cursor_namespace: Namespace,
}

impl StorageRepository {
    #[must_use]
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage,
            namespace: Namespace(NAMESPACE.to_owned()),
            cursor_namespace: Namespace(CURSOR_NAMESPACE.to_owned()),
        }
    }

    fn key(id: &str) -> Key {
        Key(id.as_bytes().to_vec())
    }
}

impl ReconcileStore for StorageRepository {
    fn reconcile_state<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<ReconcileState, Error>> {
        Box::pin(async move {
            let cursor_key = scope_key(scope);
            let cursor = self
                .storage
                .get(&self.cursor_namespace, &cursor_key)
                .await?
                .map(|stored| {
                    decode_cursor(&stored.value).map(|cursor| StoredCursor {
                        cursor,
                        version: stored.version,
                    })
                })
                .transpose()?;
            let mut payments = Vec::new();
            let mut after = None;
            loop {
                let page = self
                    .storage
                    .scan(ScanRequest {
                        namespace: self.namespace.clone(),
                        prefix: Vec::new(),
                        after,
                        limit: SCAN_LIMIT,
                    })
                    .await?;
                extend_candidates(&mut payments, scope, page.entries)?;
                let Some(next) = page.next else {
                    break;
                };
                after = Some(next);
            }
            Ok(ReconcileState { cursor, payments })
        })
    }

    fn commit_reconciliation<'a>(
        &'a self,
        batch: ReconcileBatch,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut write = WriteBatch::default();
            for stored in batch.payments {
                let key = Self::key(&stored.payment.id);
                write.conditions.push(Condition::Version {
                    namespace: self.namespace.clone(),
                    key: key.clone(),
                    expected: stored.version,
                });
                write.operations.push(Operation::Put {
                    namespace: self.namespace.clone(),
                    key,
                    value: encode(&stored.payment)?,
                });
            }
            self.add_cursor(&mut write, &batch.scope, batch.cursor.as_ref(), batch.next)?;
            if !write.operations.is_empty() {
                self.storage.commit(write).await?;
            }
            Ok(())
        })
    }
}

impl StorageRepository {
    fn add_cursor(
        &self,
        write: &mut WriteBatch,
        scope: &IndexScope,
        cursor: Option<&StoredCursor>,
        next: Option<EventCursor>,
    ) -> Result<(), Error> {
        if next == cursor.map(|cursor| cursor.cursor) {
            return Ok(());
        }
        let Some(next) = next else {
            return Ok(());
        };
        let key = scope_key(scope);
        write.conditions.push(match cursor {
            Some(cursor) => Condition::Version {
                namespace: self.cursor_namespace.clone(),
                key: key.clone(),
                expected: cursor.version,
            },
            None => Condition::Missing {
                namespace: self.cursor_namespace.clone(),
                key: key.clone(),
            },
        });
        write.operations.push(Operation::Put {
            namespace: self.cursor_namespace.clone(),
            key,
            value: encode_cursor(next)?,
        });
        Ok(())
    }
}

fn candidate(
    scope: &IndexScope,
    stored: storage::StoredValue,
) -> Result<Option<StoredPayment>, Error> {
    let payment = decode(&stored.value)?;
    let relevant = payment.scope == Scope::from(scope)
        && matches!(
            payment.stage,
            Stage::Submitted { .. } | Stage::Confirmed { .. }
        );
    Ok(relevant.then_some(StoredPayment {
        payment,
        version: stored.version,
    }))
}

fn extend_candidates(
    payments: &mut Vec<StoredPayment>,
    scope: &IndexScope,
    entries: Vec<(Key, storage::StoredValue)>,
) -> Result<(), Error> {
    for (_, stored) in entries {
        if let Some(payment) = candidate(scope, stored)? {
            payments.push(payment);
        }
    }
    Ok(())
}

impl Repository for StorageRepository {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<StoredPayment>, Error>> {
        Box::pin(async move {
            self.storage
                .get(&self.namespace, &Self::key(id))
                .await?
                .map(|stored| {
                    decode(&stored.value).map(|payment| StoredPayment {
                        payment,
                        version: stored.version,
                    })
                })
                .transpose()
        })
    }

    fn create<'a>(&'a self, payment: Payment) -> BoxFuture<'a, Result<StoredPayment, Error>> {
        Box::pin(async move {
            let key = Self::key(&payment.id);
            let value = encode(&payment)?;
            let result = self
                .storage
                .commit(WriteBatch {
                    conditions: vec![Condition::Missing {
                        namespace: self.namespace.clone(),
                        key: key.clone(),
                    }],
                    operations: vec![Operation::Put {
                        namespace: self.namespace.clone(),
                        key,
                        value,
                    }],
                })
                .await?;
            Ok(StoredPayment {
                payment,
                version: result.version,
            })
        })
    }

    fn update<'a>(
        &'a self,
        payment: Payment,
        expected: Version,
    ) -> BoxFuture<'a, Result<StoredPayment, Error>> {
        Box::pin(async move {
            let key = Self::key(&payment.id);
            let value = encode(&payment)?;
            let result = self
                .storage
                .commit(WriteBatch {
                    conditions: vec![Condition::Version {
                        namespace: self.namespace.clone(),
                        key: key.clone(),
                        expected,
                    }],
                    operations: vec![Operation::Put {
                        namespace: self.namespace.clone(),
                        key,
                        value,
                    }],
                })
                .await?;
            Ok(StoredPayment {
                payment,
                version: result.version,
            })
        })
    }
}

fn decode(value: &Value) -> Result<Payment, Error> {
    serde_json::from_slice(&value.0).map_err(|_| corrupt("stored payment is invalid"))
}

fn scope_key(scope: &IndexScope) -> Key {
    let chain = scope.chain.0.as_bytes();
    let network = scope.network.as_bytes();
    let mut key = Vec::with_capacity(16 + chain.len() + network.len());
    key.extend_from_slice(&(chain.len() as u64).to_be_bytes());
    key.extend_from_slice(chain);
    key.extend_from_slice(&(network.len() as u64).to_be_bytes());
    key.extend_from_slice(network);
    Key(key)
}

fn encode_cursor(cursor: EventCursor) -> Result<Value, Error> {
    serde_json::to_vec(&cursor.0)
        .map(Value)
        .map_err(|_| corrupt("observer cursor could not be encoded"))
}

fn decode_cursor(value: &Value) -> Result<EventCursor, Error> {
    serde_json::from_slice(&value.0)
        .map(EventCursor)
        .map_err(|_| corrupt("stored observer cursor is invalid"))
}

fn encode(payment: &Payment) -> Result<Value, Error> {
    serde_json::to_vec(payment)
        .map(Value)
        .map_err(|_| corrupt("payment could not be encoded as JSON"))
}

fn corrupt(message: impl Into<String>) -> Error {
    Error {
        kind: storage::ErrorKind::CorruptData,
        message: message.into(),
    }
}
