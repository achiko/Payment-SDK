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

// design-lint: allow unclassified-free-function -- encodes the shared checkpoint key for reads and atomic writes; foreign scope and key types keep storage-format policy in this adapter
pub(super) fn checkpoint(scope: &IndexScope) -> Key {
    Key(prefix(scope, CHECKPOINT))
}

// design-lint: allow unclassified-free-function -- shared redb journal-key encoding preserves scope framing and produced-height byte ordering for writes, retention and rollback without leaking backend format into domain values
pub(super) fn journal(scope: &IndexScope, height: BlockHeight) -> Key {
    let mut key = prefix(scope, JOURNAL);
    key.extend_from_slice(&height.0.to_be_bytes());
    Key(key)
}

// design-lint: allow unclassified-free-function -- shared redb history-key framing keeps persisted scope and address bytes identical for address-primary writes and prefix scans without leaking storage policy into domain values
pub(super) fn history_prefix(scope: &IndexScope, address: &CanonicalAddress) -> Vec<u8> {
    let mut key = prefix(scope, HISTORY);
    component(&mut key, address.value.as_bytes());
    key
}

// design-lint: allow unclassified-free-function -- redb rollback history-key classification checks the persisted format and exact scope prefix without exposing storage bytes on domain values
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

// design-lint: allow unclassified-free-function -- redb rollback output-key classification shares the persisted format and scope guard for removal and restoration while full record validation stays with the repository
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

// design-lint: allow unclassified-free-function -- shared length-prefix byte-append algorithm for scope, address and transaction key components; neither byte buffer is a domain receiver
fn component(key: &mut Vec<u8>, value: &[u8]) {
    key.extend_from_slice(&(value.len() as u64).to_be_bytes());
    key.extend_from_slice(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexing::ChainId;

    #[test]
    fn checkpoint_has_the_persisted_format_and_scope_framing() {
        let scope = IndexScope {
            chain: ChainId("a".into()),
            network: "bc".into(),
        };
        assert_eq!(
            checkpoint(&scope).0,
            [
                1, 1, 0, 0, 0, 0, 0, 0, 0, 1, b'a', 0, 0, 0, 0, 0, 0, 0, 2, b'b', b'c'
            ]
        );
        let differently_split_scope = IndexScope {
            chain: ChainId("ab".into()),
            network: "c".into(),
        };
        assert_ne!(checkpoint(&scope), checkpoint(&differently_split_scope));
        for tag in [JOURNAL, HISTORY, OUTPUT] {
            assert_ne!(checkpoint(&scope).0, prefix(&scope, tag));
        }
    }

    #[test]
    fn components_preserve_empty_binary_and_utf8_bytes() {
        let mut key = vec![0x7f];
        component(&mut key, &[]);
        component(&mut key, &[0, 0xff]);
        component(&mut key, "é".as_bytes());
        assert_eq!(
            key,
            [
                0x7f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0xff, 0, 0, 0, 0, 0, 0, 0,
                2, 0xc3, 0xa9,
            ]
        );
    }

    #[test]
    fn journal_keys_keep_numeric_height_order() {
        let scope = IndexScope {
            chain: ChainId("a".into()),
            network: "bc".into(),
        };
        let heights = [0, 1, 255, 256, u32::MAX as u64, u64::MAX];
        let keys = heights.map(|height| journal(&scope, BlockHeight(height)).0);
        assert_eq!(
            journal(&scope, BlockHeight(42)).0,
            b"\x01\x02\0\0\0\0\0\0\0\x01a\0\0\0\0\0\0\0\x02bc\0\0\0\0\0\0\0\x2a"
        );
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        for (key, height) in keys.iter().zip(heights) {
            assert!(key.starts_with(&prefix(&scope, JOURNAL)));
            assert_eq!(&key[key.len() - 8..], &height.to_be_bytes());
        }
    }

    #[test]
    fn rollback_key_classification_respects_format_tag_and_exact_scope() {
        let scope = IndexScope {
            chain: ChainId("a".into()),
            network: "b".into(),
        };
        for (key, history, output) in [
            (
                b"\x01\x03\0\0\0\0\0\0\0\x01a\0\0\0\0\0\0\0\x01b".as_slice(),
                true,
                false,
            ),
            (
                b"\x01\x04\0\0\0\0\0\0\0\x01a\0\0\0\0\0\0\0\x01b\x00\xff".as_slice(),
                false,
                true,
            ),
            (
                b"\x01\x02\0\0\0\0\0\0\0\x01a\0\0\0\0\0\0\0\x01b".as_slice(),
                false,
                false,
            ),
            (
                b"\x02\x03\0\0\0\0\0\0\0\x01a\0\0\0\0\0\0\0\x01b".as_slice(),
                false,
                false,
            ),
            (
                b"\x01\x03\0\0\0\0\0\0\0\x01a\0\0\0\0\0\0\0\x01c".as_slice(),
                false,
                false,
            ),
            (
                b"\x01\x04\0\0\0\0\0\0\0\x01c\0\0\0\0\0\0\0\x01b".as_slice(),
                false,
                false,
            ),
            (b"\x01\x03".as_slice(), false, false),
            (b"".as_slice(), false, false),
        ] {
            assert_eq!(is_history(&scope, key), history);
            assert_eq!(is_output(&scope, key), output);
        }
    }

    #[test]
    fn history_prefix_preserves_persisted_scope_and_address_bytes() {
        let scope = IndexScope {
            chain: ChainId("a".into()),
            network: "b".into(),
        };
        let address = CanonicalAddress {
            scope: scope.clone(),
            value: "é".into(),
        };
        let expected = b"\x01\x03\0\0\0\0\0\0\0\x01a\0\0\0\0\0\0\0\x01b\0\0\0\0\0\0\0\x02\xc3\xa9";
        assert_eq!(history_prefix(&scope, &address), expected);
        let transaction = TransactionRef {
            scope: scope.clone(),
            value: "tx".into(),
        };
        assert!(
            history(&scope, &address, BlockHeight(42), &transaction)
                .0
                .starts_with(expected)
        );

        for other_scope in [
            IndexScope {
                chain: ChainId("other".into()),
                ..scope.clone()
            },
            IndexScope {
                network: "other".into(),
                ..scope.clone()
            },
        ] {
            assert_ne!(history_prefix(&other_scope, &address), expected);
        }
        let longer_address = CanonicalAddress {
            value: "éx".into(),
            ..address
        };
        assert!(!history_prefix(&scope, &longer_address).starts_with(expected));
    }
}
