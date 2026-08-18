use storage::{Key, Namespace};

use crate::{
    BlockHeight, CanonicalAddress, EventCursor, IndexScope, ObservationRevision, RebuildGeneration,
    TransactionRef,
};

pub(super) const NAMESPACE: &str = "ix.semantic";

const KEY_FORMAT: u8 = 1;
const META: u8 = 1;
const MUTATION_GUARD: u8 = 2;
const ACTIVE_GENERATION: u8 = 3;
const WATCH_VERSION: u8 = 4;
const WATCH_COUNTER: u8 = 5;
const EVENT_COUNTER: u8 = 6;
const STATUS: u8 = 7;
const REBUILD_COUNTER: u8 = 8;
const REBUILD_STATE: u8 = 9;
const PROJECTION_REVISION: u8 = 13;
const WATCH: u8 = 16;
const WATCH_IDEMPOTENCY: u8 = 17;
const WATCH_BACKFILL: u8 = 18;
const WATCH_BACKFILL_APPLIED: u8 = 19;
const WATCH_BACKFILL_APPLIED_HEIGHT: u8 = 20;
const CHECKPOINT: u8 = 31;
const CANONICAL: u8 = 32;
const BUNDLE: u8 = 33;
const CURRENT_OBSERVATION: u8 = 34;
const OBSERVATION_REVISION: u8 = 35;
const PENDING_CONFIRMATION: u8 = 36;
const ADDRESS_TRANSACTION: u8 = 37;
const PREPARED_REBUILD_EVENT: u8 = 38;
const PROJECTION: u8 = 39;
const BACKFILL_PROJECTION_ROLLBACK: u8 = 40;
const EVENT: u8 = 48;
const EVENT_ID: u8 = 49;

#[must_use]
pub(super) fn namespace() -> Namespace {
    Namespace(NAMESPACE.to_owned())
}

fn scope_prefix(scope: &IndexScope, tag: u8) -> Vec<u8> {
    let mut key = vec![KEY_FORMAT];
    component(&mut key, scope.chain.0.as_bytes());
    component(&mut key, scope.network.as_bytes());
    key.push(tag);
    key
}

fn component(key: &mut Vec<u8>, value: &[u8]) {
    let length =
        u64::try_from(value.len()).expect("usize values fit in u64 on every supported Rust target");
    key.extend_from_slice(&length.to_be_bytes());
    key.extend_from_slice(value);
}

fn generation_prefix(scope: &IndexScope, tag: u8, generation: RebuildGeneration) -> Vec<u8> {
    let mut key = scope_prefix(scope, tag);
    key.extend_from_slice(&generation.0.to_be_bytes());
    key
}

fn scoped_value(key: &mut Vec<u8>, scope: &IndexScope, value: &str) {
    component(key, scope.chain.0.as_bytes());
    component(key, scope.network.as_bytes());
    component(key, value.as_bytes());
}

#[must_use]
pub(super) fn meta(_scope: &IndexScope) -> Key {
    // Repository identity is deliberately path-global. A database opened with
    // another chain/network configuration must observe and reject the existing
    // metadata rather than silently creating a second scoped repository.
    Key(vec![KEY_FORMAT, META])
}

#[must_use]
pub(super) fn mutation_guard(scope: &IndexScope) -> Key {
    Key(scope_prefix(scope, MUTATION_GUARD))
}

#[must_use]
pub(super) fn active_generation(scope: &IndexScope) -> Key {
    Key(scope_prefix(scope, ACTIVE_GENERATION))
}

#[must_use]
pub(super) fn watch_version(scope: &IndexScope) -> Key {
    Key(scope_prefix(scope, WATCH_VERSION))
}

#[must_use]
pub(super) fn watch_counter(scope: &IndexScope) -> Key {
    Key(scope_prefix(scope, WATCH_COUNTER))
}

#[must_use]
pub(super) fn event_counter(scope: &IndexScope) -> Key {
    Key(scope_prefix(scope, EVENT_COUNTER))
}

#[must_use]
pub(super) fn status(scope: &IndexScope) -> Key {
    Key(scope_prefix(scope, STATUS))
}

#[must_use]
pub(super) fn rebuild_counter(scope: &IndexScope) -> Key {
    Key(scope_prefix(scope, REBUILD_COUNTER))
}

#[must_use]
pub(super) fn rebuild_state(scope: &IndexScope) -> Key {
    Key(scope_prefix(scope, REBUILD_STATE))
}

#[must_use]
pub(super) fn projection_revision(scope: &IndexScope) -> Key {
    Key(scope_prefix(scope, PROJECTION_REVISION))
}

#[must_use]
pub(super) fn watch_prefix(scope: &IndexScope) -> Vec<u8> {
    scope_prefix(scope, WATCH)
}

#[must_use]
pub(super) fn watch(scope: &IndexScope, id: &str) -> Key {
    let mut key = watch_prefix(scope);
    component(&mut key, id.as_bytes());
    Key(key)
}

#[must_use]
pub(super) fn watch_idempotency(scope: &IndexScope, idempotency_key: &str) -> Key {
    let mut key = scope_prefix(scope, WATCH_IDEMPOTENCY);
    component(&mut key, idempotency_key.as_bytes());
    Key(key)
}

#[must_use]
pub(super) fn watch_backfill_prefix(scope: &IndexScope) -> Vec<u8> {
    scope_prefix(scope, WATCH_BACKFILL)
}

#[must_use]
pub(super) fn watch_backfill(scope: &IndexScope, watch_id: &str) -> Key {
    let mut key = watch_backfill_prefix(scope);
    component(&mut key, watch_id.as_bytes());
    Key(key)
}

#[must_use]
pub(super) fn watch_backfill_applied(
    scope: &IndexScope,
    watch_id: &str,
    height: BlockHeight,
) -> Key {
    let mut key = scope_prefix(scope, WATCH_BACKFILL_APPLIED);
    component(&mut key, watch_id.as_bytes());
    key.extend_from_slice(&height.0.to_be_bytes());
    Key(key)
}

#[must_use]
pub(super) fn watch_backfill_applied_height_prefix(
    scope: &IndexScope,
    height: BlockHeight,
) -> Vec<u8> {
    let mut key = scope_prefix(scope, WATCH_BACKFILL_APPLIED_HEIGHT);
    key.extend_from_slice(&height.0.to_be_bytes());
    key
}

#[must_use]
pub(super) fn watch_backfill_applied_height(
    scope: &IndexScope,
    height: BlockHeight,
    watch_id: &str,
) -> Key {
    let mut key = watch_backfill_applied_height_prefix(scope, height);
    component(&mut key, watch_id.as_bytes());
    Key(key)
}

#[must_use]
pub(super) fn canonical_checkpoint(scope: &IndexScope, generation: RebuildGeneration) -> Key {
    Key(generation_prefix(scope, CHECKPOINT, generation))
}

#[must_use]
pub(super) fn canonical(
    scope: &IndexScope,
    generation: RebuildGeneration,
    height: BlockHeight,
) -> Key {
    let mut key = generation_prefix(scope, CANONICAL, generation);
    key.extend_from_slice(&height.0.to_be_bytes());
    Key(key)
}

#[must_use]
pub(super) fn canonical_prefix(scope: &IndexScope, generation: RebuildGeneration) -> Vec<u8> {
    generation_prefix(scope, CANONICAL, generation)
}

#[must_use]
pub(super) fn bundle(
    scope: &IndexScope,
    generation: RebuildGeneration,
    height: BlockHeight,
) -> Key {
    let mut key = generation_prefix(scope, BUNDLE, generation);
    key.extend_from_slice(&height.0.to_be_bytes());
    Key(key)
}

#[must_use]
pub(super) fn bundle_prefix(scope: &IndexScope, generation: RebuildGeneration) -> Vec<u8> {
    generation_prefix(scope, BUNDLE, generation)
}

#[must_use]
pub(super) fn current_observation(
    scope: &IndexScope,
    generation: RebuildGeneration,
    transaction: &TransactionRef,
) -> Key {
    let mut key = generation_prefix(scope, CURRENT_OBSERVATION, generation);
    scoped_value(&mut key, &transaction.scope, &transaction.value);
    Key(key)
}

#[must_use]
pub(super) fn current_observation_prefix(
    scope: &IndexScope,
    generation: RebuildGeneration,
) -> Vec<u8> {
    generation_prefix(scope, CURRENT_OBSERVATION, generation)
}

#[must_use]
pub(super) fn observation_revision(
    scope: &IndexScope,
    generation: RebuildGeneration,
    transaction: &TransactionRef,
    revision: ObservationRevision,
) -> Key {
    let mut key = generation_prefix(scope, OBSERVATION_REVISION, generation);
    scoped_value(&mut key, &transaction.scope, &transaction.value);
    key.extend_from_slice(&revision.0.to_be_bytes());
    Key(key)
}

#[must_use]
pub(super) fn pending_confirmation(
    scope: &IndexScope,
    generation: RebuildGeneration,
    inclusion_height: BlockHeight,
    transaction: &TransactionRef,
) -> Key {
    let mut key = generation_prefix(scope, PENDING_CONFIRMATION, generation);
    key.extend_from_slice(&inclusion_height.0.to_be_bytes());
    scoped_value(&mut key, &transaction.scope, &transaction.value);
    Key(key)
}

#[must_use]
pub(super) fn pending_confirmation_prefix(
    scope: &IndexScope,
    generation: RebuildGeneration,
) -> Vec<u8> {
    generation_prefix(scope, PENDING_CONFIRMATION, generation)
}

#[must_use]
pub(super) fn address_transaction(
    scope: &IndexScope,
    generation: RebuildGeneration,
    address: &CanonicalAddress,
    transaction: &TransactionRef,
) -> Key {
    let mut key = address_transaction_prefix(scope, generation, address);
    scoped_value(&mut key, &transaction.scope, &transaction.value);
    Key(key)
}

#[must_use]
pub(super) fn address_transaction_prefix(
    scope: &IndexScope,
    generation: RebuildGeneration,
    address: &CanonicalAddress,
) -> Vec<u8> {
    let mut key = generation_prefix(scope, ADDRESS_TRANSACTION, generation);
    scoped_value(&mut key, &address.scope, &address.value);
    key
}

#[must_use]
pub(super) fn address_transaction_generation_prefix(
    scope: &IndexScope,
    generation: RebuildGeneration,
) -> Vec<u8> {
    generation_prefix(scope, ADDRESS_TRANSACTION, generation)
}

#[must_use]
pub(super) fn prepared_rebuild_event(
    scope: &IndexScope,
    generation: RebuildGeneration,
    cursor: EventCursor,
) -> Key {
    let mut key = prepared_rebuild_event_prefix(scope, generation);
    key.extend_from_slice(&cursor.0.to_be_bytes());
    Key(key)
}

#[must_use]
pub(super) fn prepared_rebuild_event_prefix(
    scope: &IndexScope,
    generation: RebuildGeneration,
) -> Vec<u8> {
    generation_prefix(scope, PREPARED_REBUILD_EVENT, generation)
}

#[must_use]
pub(super) fn projection(
    scope: &IndexScope,
    generation: RebuildGeneration,
    relative_key: &[u8],
) -> Key {
    let mut key = projection_prefix(scope, generation, &[]);
    key.extend_from_slice(relative_key);
    Key(key)
}

#[must_use]
pub(super) fn projection_prefix(
    scope: &IndexScope,
    generation: RebuildGeneration,
    relative_prefix: &[u8],
) -> Vec<u8> {
    let mut key = generation_prefix(scope, PROJECTION, generation);
    key.extend_from_slice(relative_prefix);
    key
}

#[must_use]
pub(super) fn backfill_projection_rollback(
    scope: &IndexScope,
    generation: RebuildGeneration,
    height: BlockHeight,
) -> Key {
    let mut key = backfill_projection_rollback_prefix(scope, generation);
    key.extend_from_slice(&height.0.to_be_bytes());
    Key(key)
}

#[must_use]
pub(super) fn backfill_projection_rollback_prefix(
    scope: &IndexScope,
    generation: RebuildGeneration,
) -> Vec<u8> {
    generation_prefix(scope, BACKFILL_PROJECTION_ROLLBACK, generation)
}

#[must_use]
pub(super) fn event(scope: &IndexScope, cursor: EventCursor) -> Key {
    let mut key = scope_prefix(scope, EVENT);
    key.extend_from_slice(&cursor.0.to_be_bytes());
    Key(key)
}

#[must_use]
pub(super) fn event_prefix(scope: &IndexScope) -> Vec<u8> {
    scope_prefix(scope, EVENT)
}

#[must_use]
pub(super) fn event_id(scope: &IndexScope, id: &str) -> Key {
    let mut key = scope_prefix(scope, EVENT_ID);
    component(&mut key, id.as_bytes());
    Key(key)
}

#[must_use]
pub(super) fn generation_prefixes(
    scope: &IndexScope,
    generation: RebuildGeneration,
) -> Vec<Vec<u8>> {
    vec![
        generation_prefix(scope, CHECKPOINT, generation),
        canonical_prefix(scope, generation),
        bundle_prefix(scope, generation),
        current_observation_prefix(scope, generation),
        pending_confirmation_prefix(scope, generation),
        address_transaction_generation_prefix(scope, generation),
        prepared_rebuild_event_prefix(scope, generation),
        projection_prefix(scope, generation, &[]),
        backfill_projection_rollback_prefix(scope, generation),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChainId;

    fn scope(network: &str) -> IndexScope {
        IndexScope {
            chain: ChainId("chain".to_owned()),
            network: network.to_owned(),
        }
    }

    #[test]
    fn identity_keys_do_not_collide_between_networks() {
        let repository_scope = scope("repository");
        let generation = RebuildGeneration(0);
        let first_transaction = TransactionRef {
            scope: scope("first"),
            value: "same".to_owned(),
        };
        let second_transaction = TransactionRef {
            scope: scope("second"),
            value: "same".to_owned(),
        };
        assert_ne!(
            current_observation(&repository_scope, generation, &first_transaction),
            current_observation(&repository_scope, generation, &second_transaction)
        );

        let first_address = CanonicalAddress {
            scope: scope("first"),
            value: "same".to_owned(),
        };
        let second_address = CanonicalAddress {
            scope: scope("second"),
            value: "same".to_owned(),
        };
        assert_ne!(
            address_transaction(
                &repository_scope,
                generation,
                &first_address,
                &first_transaction,
            ),
            address_transaction(
                &repository_scope,
                generation,
                &second_address,
                &second_transaction,
            )
        );
    }
}
