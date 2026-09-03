use crate::{
    AddressFilter, BlockPosition, CanonicalAddress, ChainId, IndexErrorKind, IndexScope, SyncConfig,
};

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("test".into()),
        network: "mainnet".into(),
    }
}

#[test]
fn fresh_sync_uses_the_earliest_native_position_without_inventing_a_parent() {
    let filters = |starts: &[u64]| {
        starts
            .iter()
            .map(|position| AddressFilter {
                address: CanonicalAddress {
                    scope: scope(),
                    value: format!("address-{position}"),
                },
                start_position: BlockPosition(*position),
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(super::synchronizer::earliest_position(&filters(&[])), None);
    assert_eq!(
        super::synchronizer::earliest_position(&filters(&[900, 950])),
        Some(crate::BlockPosition(900))
    );
    assert_eq!(
        super::synchronizer::earliest_position(&filters(&[0])),
        Some(crate::BlockPosition(0))
    );
}

#[test]
fn configuration_requires_bounded_rollback_and_confirmation() {
    assert!(SyncConfig::new(scope(), 1, 1, 100).is_ok());
    assert_eq!(
        SyncConfig::new(scope(), 1, 0, 100).unwrap_err().kind,
        IndexErrorKind::InvalidRequest
    );
}
