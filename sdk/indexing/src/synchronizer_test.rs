use crate::{ChainId, IndexErrorKind, IndexScope, SyncConfig};

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("test".into()),
        network: "mainnet".into(),
    }
}

#[test]
fn configuration_requires_bounded_rollback_and_confirmation() {
    assert!(SyncConfig::new(scope(), 1, 1, 100).is_ok());
    assert_eq!(
        SyncConfig::new(scope(), 1, 0, 100).unwrap_err().kind,
        IndexErrorKind::InvalidRequest
    );
}
