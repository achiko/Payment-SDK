use storage::{Key, Namespace};

use crate::{BlockHeight, CanonicalAddress, IndexScope, ObservationRevision, TransactionRef};

pub(super) const NAMESPACE: &str = "ix.semantic";

const FORMAT: u8 = 1;
const META: u8 = 1;
const MUTATION_GUARD: u8 = 2;
const WATCH_VERSION: u8 = 3;
const WATCH_COUNTER: u8 = 4;
const STATUS: u8 = 5;
const PROJECTION_REVISION: u8 = 6;
const WATCH: u8 = 16;
const WATCH_IDEMPOTENCY: u8 = 17;
const CHECKPOINT: u8 = 32;
const CANONICAL: u8 = 33;
const BUNDLE: u8 = 34;
const CURRENT_OBSERVATION: u8 = 35;
const OBSERVATION_REVISION: u8 = 36;
const PENDING_CONFIRMATION: u8 = 37;
const ADDRESS_TRANSACTION: u8 = 38;
const PROJECTION: u8 = 39;

#[must_use]
pub(super) fn namespace() -> Namespace {
    Namespace(NAMESPACE.to_owned())
}

fn prefix(scope: &IndexScope, tag: u8) -> Vec<u8> {
    let mut key = vec![FORMAT, tag];
    component(&mut key, scope.chain.0.as_bytes());
    component(&mut key, scope.network.as_bytes());
    key
}

fn component(key: &mut Vec<u8>, value: &[u8]) {
    key.extend_from_slice(&(value.len() as u64).to_be_bytes());
    key.extend_from_slice(value);
}

fn scoped(key: &mut Vec<u8>, scope: &IndexScope, value: &str) {
    component(key, scope.chain.0.as_bytes());
    component(key, scope.network.as_bytes());
    component(key, value.as_bytes());
}

pub(super) fn meta(scope: &IndexScope) -> Key {
    Key(prefix(scope, META))
}
pub(super) fn mutation_guard(scope: &IndexScope) -> Key {
    Key(prefix(scope, MUTATION_GUARD))
}
pub(super) fn watch_version(scope: &IndexScope) -> Key {
    Key(prefix(scope, WATCH_VERSION))
}
pub(super) fn watch_counter(scope: &IndexScope) -> Key {
    Key(prefix(scope, WATCH_COUNTER))
}
pub(super) fn status(scope: &IndexScope) -> Key {
    Key(prefix(scope, STATUS))
}
pub(super) fn projection_revision(scope: &IndexScope) -> Key {
    Key(prefix(scope, PROJECTION_REVISION))
}
pub(super) fn canonical_checkpoint(scope: &IndexScope) -> Key {
    Key(prefix(scope, CHECKPOINT))
}

pub(super) fn watch_prefix(scope: &IndexScope) -> Vec<u8> {
    prefix(scope, WATCH)
}
pub(super) fn watch(scope: &IndexScope, id: &str) -> Key {
    let mut key = watch_prefix(scope);
    component(&mut key, id.as_bytes());
    Key(key)
}
pub(super) fn watch_idempotency(scope: &IndexScope, id: &str) -> Key {
    let mut key = prefix(scope, WATCH_IDEMPOTENCY);
    component(&mut key, id.as_bytes());
    Key(key)
}

pub(super) fn canonical(scope: &IndexScope, height: BlockHeight) -> Key {
    let mut key = prefix(scope, CANONICAL);
    key.extend_from_slice(&height.0.to_be_bytes());
    Key(key)
}
pub(super) fn bundle(scope: &IndexScope, height: BlockHeight) -> Key {
    let mut key = prefix(scope, BUNDLE);
    key.extend_from_slice(&height.0.to_be_bytes());
    Key(key)
}
pub(super) fn current_observation(scope: &IndexScope, transaction: &TransactionRef) -> Key {
    let mut key = prefix(scope, CURRENT_OBSERVATION);
    scoped(&mut key, &transaction.scope, &transaction.value);
    Key(key)
}
pub(super) fn observation_revision(
    scope: &IndexScope,
    transaction: &TransactionRef,
    revision: ObservationRevision,
) -> Key {
    let mut key = prefix(scope, OBSERVATION_REVISION);
    scoped(&mut key, &transaction.scope, &transaction.value);
    key.extend_from_slice(&revision.0.to_be_bytes());
    Key(key)
}
pub(super) fn pending_confirmation_prefix(scope: &IndexScope) -> Vec<u8> {
    prefix(scope, PENDING_CONFIRMATION)
}
pub(super) fn pending_confirmation(
    scope: &IndexScope,
    height: BlockHeight,
    transaction: &TransactionRef,
) -> Key {
    let mut key = pending_confirmation_prefix(scope);
    key.extend_from_slice(&height.0.to_be_bytes());
    scoped(&mut key, &transaction.scope, &transaction.value);
    Key(key)
}
pub(super) fn address_transaction_prefix(
    scope: &IndexScope,
    address: &CanonicalAddress,
) -> Vec<u8> {
    let mut key = prefix(scope, ADDRESS_TRANSACTION);
    scoped(&mut key, &address.scope, &address.value);
    key
}
pub(super) fn address_transaction(
    scope: &IndexScope,
    address: &CanonicalAddress,
    transaction: &TransactionRef,
) -> Key {
    let mut key = address_transaction_prefix(scope, address);
    scoped(&mut key, &transaction.scope, &transaction.value);
    Key(key)
}
pub(super) fn projection_prefix(scope: &IndexScope, relative: &[u8]) -> Vec<u8> {
    let mut key = prefix(scope, PROJECTION);
    key.extend_from_slice(relative);
    key
}
pub(super) fn projection(scope: &IndexScope, relative: &[u8]) -> Key {
    Key(projection_prefix(scope, relative))
}
