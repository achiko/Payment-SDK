use storage::{Key, Namespace};

use crate::{BlockHeight, CanonicalAddress, IndexScope, OutputKey, TransactionRef};

pub(super) const NAMESPACE: &str = "index";

const FORMAT: u8 = 1;
const CHECKPOINT: u8 = 1;
const JOURNAL: u8 = 2;
const HISTORY: u8 = 3;
const OUTPUT: u8 = 4;

#[must_use]
pub(super) fn namespace() -> Namespace {
    Namespace(NAMESPACE.to_owned())
}

pub(super) fn checkpoint(scope: &IndexScope) -> Key {
    Key(prefix(scope, CHECKPOINT))
}

pub(super) fn journal(scope: &IndexScope, height: BlockHeight) -> Key {
    let mut key = prefix(scope, JOURNAL);
    key.extend_from_slice(&height.0.to_be_bytes());
    Key(key)
}

pub(super) fn history_prefix(scope: &IndexScope, address: &CanonicalAddress) -> Vec<u8> {
    let mut key = prefix(scope, HISTORY);
    component(&mut key, address.value.as_bytes());
    key
}

pub(super) fn is_history(scope: &IndexScope, key: &[u8]) -> bool {
    key.starts_with(&prefix(scope, HISTORY))
}

pub(super) fn history(
    scope: &IndexScope,
    address: &CanonicalAddress,
    height: BlockHeight,
    transaction: &TransactionRef,
) -> Key {
    let mut key = history_prefix(scope, address);
    key.extend_from_slice(&height.0.to_be_bytes());
    component(&mut key, transaction.value.as_bytes());
    Key(key)
}

pub(super) fn output_prefix(scope: &IndexScope, address: &CanonicalAddress) -> Vec<u8> {
    let mut key = prefix(scope, OUTPUT);
    component(&mut key, address.value.as_bytes());
    key
}

pub(super) fn is_output(scope: &IndexScope, key: &[u8]) -> bool {
    key.starts_with(&prefix(scope, OUTPUT))
}

pub(super) fn output(scope: &IndexScope, output: &OutputKey) -> Key {
    let mut key = output_prefix(scope, &output.address);
    component(&mut key, output.output.transaction.value.as_bytes());
    key.extend_from_slice(&output.output.index.to_be_bytes());
    Key(key)
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
