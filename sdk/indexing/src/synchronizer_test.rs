use crate::{
    AddressFilter, BlockHeight, CanonicalAddress, ChainId, IndexErrorKind, IndexScope, SyncConfig,
};

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("test".into()),
        network: "mainnet".into(),
    }
}

#[test]
fn fresh_sync_anchors_at_tip_or_immediately_before_the_first_birthday() {
    let filters = |starts: &[u64]| {
        starts
            .iter()
            .map(|height| AddressFilter {
                address: CanonicalAddress {
                    scope: scope(),
                    value: format!("address-{height}"),
                },
                start_height: BlockHeight(*height),
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(
        super::synchronizer::anchor_height(&filters(&[]), BlockHeight(1_000)),
        Some(BlockHeight(1_000))
    );
    assert_eq!(
        super::synchronizer::anchor_height(&filters(&[900, 950]), BlockHeight(1_000)),
        Some(BlockHeight(899))
    );
    assert_eq!(
        super::synchronizer::anchor_height(&filters(&[0]), BlockHeight(1_000)),
        None
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
