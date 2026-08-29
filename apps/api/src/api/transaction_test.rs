use base::{BlockHash, BlockHeight, BlockRef, Decimal};
use indexing::{AssetId, CanonicalAddress, ChainId, IndexScope, MovementId, TransactionRef};

use super::*;

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("bitcoin".to_owned()),
        network: "regtest".to_owned(),
    }
}

fn asset() -> wallets::HistoryAsset {
    wallets::HistoryAsset {
        id: AssetId {
            chain: scope().chain,
            asset: "native".to_owned(),
        },
        name: Some("Bitcoin".to_owned()),
        ticker: Some("BTC".to_owned()),
        decimals: 8,
    }
}

fn address(value: &str) -> CanonicalAddress {
    CanonicalAddress {
        scope: scope(),
        value: value.to_owned(),
    }
}

#[test]
fn history_conversion_preserves_typed_facts() {
    let block = BlockRef {
        position: base::BlockPosition(12),
        height: BlockHeight(12),
        hash: BlockHash(vec![0xab; 32]),
        parent: Some(base::BlockParent {
            position: base::BlockPosition(11),
            hash: BlockHash(vec![0xcd; 32]),
        }),
        timestamp: Some(44),
    };
    let history = wallets::History {
        checkpoint: Some(block.clone()),
        transactions: vec![wallets::HistoryEntry {
            scope: scope(),
            transaction_id: TransactionRef {
                scope: scope(),
                value: "tx-1".to_owned(),
            },
            status: wallets::HistoryStatus::Confirmed {
                block,
                confirmations: 7,
            },
            movements: vec![
                wallets::HistoryMovement {
                    id: MovementId("input:0".to_owned()),
                    kind: indexing::MovementKind::Input,
                    asset: asset(),
                    amount: "1.23000000".parse::<Decimal>().expect("decimal"),
                    from: Some(address("sender")),
                    to: None,
                },
                wallets::HistoryMovement {
                    id: MovementId("output:0".to_owned()),
                    kind: indexing::MovementKind::Output,
                    asset: asset(),
                    amount: "1.22000000".parse::<Decimal>().expect("decimal"),
                    from: None,
                    to: Some(address("recipient")),
                },
            ],
            fee: Some(wallets::HistoryFee {
                asset: asset(),
                amount: "0.01000000".parse::<Decimal>().expect("decimal"),
                payer: Some(address("sender")),
            }),
        }],
        next: None,
    };

    let page = TransactionPage::try_from(history).expect("typed history");
    let transaction = &page.transactions[0];
    assert_eq!(page.checkpoint.as_ref().map(|value| value.height), Some(12));
    assert_eq!(transaction.scope.network, "regtest");
    assert_eq!(transaction.transaction_id, "tx-1");
    assert_eq!(transaction.movements.len(), 2);
    assert_eq!(transaction.movements[0].amount, "1.23");
    assert_eq!(transaction.movements[1].kind, MovementKind::Output);
    assert_eq!(transaction.fee.as_ref().expect("fee").amount, "0.01");
    assert!(matches!(
        transaction.status,
        Status::Confirmed {
            confirmations: 7,
            ..
        }
    ));
    let status = serde_json::to_value(&transaction.status).expect("status serializes");
    assert_eq!(status["kind"], "confirmed");
    assert_eq!(status["confirmations"], 7);
    assert_eq!(status["block"]["position"], 12);
    assert_eq!(status["block"]["height"], 12);
    assert_eq!(status["block"]["parent"]["position"], 11);
    assert_eq!(status["block"]["parent"]["hash"], "cd".repeat(32));
    assert!(status["block"].get("parent_hash").is_none());
    assert!(status.get("proof").is_none());
}

#[test]
fn history_cursor_preserves_checkpoint_and_position() {
    let checkpoint = BlockRef {
        position: base::BlockPosition(12),
        height: BlockHeight(12),
        hash: BlockHash(vec![0xab; 32]),
        parent: Some(base::BlockParent {
            position: base::BlockPosition(11),
            hash: BlockHash(vec![0xcd; 32]),
        }),
        timestamp: Some(44),
    };
    let cursor = indexing::HistoryCursor {
        checkpoint: Some(checkpoint),
        position: indexing::HistoryPosition {
            height: BlockHeight(9),
            transaction: TransactionRef {
                scope: scope(),
                value: "tx-1".to_owned(),
            },
        },
    };

    let encoded = HistoryCursor::encode(&cursor).expect("cursor encodes");
    let decoded = HistoryCursor::decode(&encoded).expect("cursor decodes");

    assert_eq!(decoded, cursor);
}

#[test]
fn history_cursor_rejects_height_only_checkpoint() {
    use base64::Engine;

    let old = serde_json::json!({
        "chain": "bitcoin",
        "network": "regtest",
        "transaction": "tx-1",
        "height": 9,
        "checkpoint": {
            "height": 12,
            "hash": "abab",
            "parent_hash": "cdcd",
            "timestamp": 44
        }
    });
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&old).expect("old cursor serializes"));

    assert!(
        HistoryCursor::decode(&encoded).is_err(),
        "height-only cursors must not decode after the coordinate cutover"
    );
}

#[test]
fn history_cursor_rejects_a_partial_parent_reference() {
    use base64::Engine;

    let invalid = serde_json::json!({
        "chain": "bitcoin",
        "network": "regtest",
        "transaction": "tx-1",
        "height": 9,
        "checkpoint": {
            "position": 12,
            "height": 12,
            "hash": "abab",
            "parent": { "hash": "cdcd" },
            "timestamp": 44
        }
    });
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&invalid).expect("invalid cursor serializes"));

    assert!(
        HistoryCursor::decode(&encoded).is_err(),
        "a parent must contain position and hash atomically"
    );
}
