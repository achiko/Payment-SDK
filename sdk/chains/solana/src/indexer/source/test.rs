use std::time::Duration;

use indexing::{BlockHeight, BlockPosition, BlockSource};
use serde_json::{Value, json};
use solana_hash::Hash;

use super::*;
use crate::rpc::test_support::Scripted;

fn hash(byte: u8) -> String {
    Hash::new_from_array([byte; 32]).to_string()
}

fn block(slot: u64, height: u64, parent_slot: u64, byte: u8, parent: u8) -> Value {
    json!({
        "blockhash": hash(byte),
        "previousBlockhash": hash(parent),
        "parentSlot": parent_slot,
        "transactions": [],
        "blockTime": slot,
        "blockHeight": height,
    })
}

fn enumeration(start: u64, end: u64, floor: u64, values: Value) -> (&'static str, Value, Value) {
    (
        "getBlocks",
        json!([start, end, {
            "commitment": "finalized",
            "minContextSlot": floor,
        }]),
        values,
    )
}

fn full(slot: u64, value: Value) -> (&'static str, Value, Value) {
    (
        "getBlock",
        json!([slot, {
            "commitment": "finalized",
            "encoding": "json",
            "transactionDetails": "full",
            "maxSupportedTransactionVersion": 0,
            "rewards": false,
        }]),
        value,
    )
}

#[tokio::test]
async fn finds_a_complete_tip_below_empty_finalized_slots() {
    let rpc = Scripted::new([
        (
            "getSlot",
            json!([{"commitment":"finalized"}]),
            json!(20_005),
        ),
        ("getFirstAvailableBlock", json!([]), json!(1)),
        enumeration(10_006, 20_005, 20_005, json!([])),
        enumeration(6, 10_005, 20_005, json!([])),
        enumeration(1, 5, 20_005, json!([3])),
        full(3, block(3, 2, 1, 3, 1)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
    ]);
    let source = Source::new(RpcClient::new(rpc.clone()));

    let tip = source.tip().await.unwrap();
    assert_eq!(tip.position, BlockPosition(3));
    assert_eq!(tip.height, BlockHeight(2));
    rpc.assert_finished();
}

#[tokio::test]
async fn fetches_sparse_ranges_in_order_and_truncates_by_block_count() {
    let rpc = Scripted::new([
        ("getSlot", json!([{"commitment":"finalized"}]), json!(107)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
        enumeration(1, 107, 107, json!([1, 100, 103, 107])),
        full(107, block(107, 4, 103, 4, 3)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
        enumeration(100, 107, 107, json!([100, 103, 107])),
        full(100, block(100, 2, 1, 2, 1)),
        full(103, block(103, 3, 100, 3, 2)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
    ]);
    let source = Source::new(RpcClient::new(rpc.clone()));

    let blocks = source
        .blocks(BlockPosition(100), BlockPosition(107), 2)
        .await
        .unwrap();
    assert_eq!(
        blocks
            .iter()
            .map(|value| value.reference().position)
            .collect::<Vec<_>>(),
        [BlockPosition(100), BlockPosition(103)]
    );
    rpc.assert_finished();
}

#[tokio::test]
async fn returns_empty_above_the_proved_tip_without_range_enumeration() {
    let rpc = Scripted::new([
        ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
        enumeration(1, 10, 10, json!([7])),
        full(7, block(7, 2, 4, 2, 1)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
    ]);
    let source = Source::new(RpcClient::new(rpc.clone()));

    assert!(
        source
            .blocks(BlockPosition(8), BlockPosition(20), 5)
            .await
            .unwrap()
            .is_empty()
    );
    rpc.assert_finished();
}

#[tokio::test]
async fn rejects_pruning_that_moves_past_the_required_start() {
    let rpc = Scripted::new([
        ("getSlot", json!([{"commitment":"finalized"}]), json!(107)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
        enumeration(1, 107, 107, json!([100, 107])),
        full(107, block(107, 2, 100, 2, 1)),
        ("getFirstAvailableBlock", json!([]), json!(101)),
    ]);
    let source = Source::new(RpcClient::new(rpc.clone()));

    let error = source
        .blocks(BlockPosition(100), BlockPosition(107), 2)
        .await
        .unwrap_err();
    assert!(!error.retryable);
    assert!(error.message.contains("pruned"));
    rpc.assert_finished();
}

#[tokio::test]
async fn proves_omission_only_with_both_pruning_witnesses_and_older_tip() {
    let rpc = Scripted::new([
        ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
        enumeration(7, 7, 10, json!([])),
        ("getFirstAvailableBlock", json!([]), json!(1)),
    ]);
    let source = Source::new(RpcClient::new(rpc.clone()));
    assert_eq!(source.canonical_at(BlockPosition(7)).await.unwrap(), None);
    rpc.assert_finished();
}

#[tokio::test]
async fn returns_a_changed_complete_canonical_block() {
    let rpc = Scripted::new([
        ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
        enumeration(7, 7, 10, json!([7])),
        full(7, block(7, 5, 6, 9, 8)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
    ]);
    let source = Source::new(RpcClient::new(rpc.clone()));

    let canonical = source
        .canonical_at(BlockPosition(7))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(canonical.hash, indexing::BlockHash([9; 32].to_vec()));
    rpc.assert_finished();
}

#[tokio::test]
async fn refuses_same_slot_omission_and_late_pruning() {
    let same_slot = Scripted::new([
        ("getSlot", json!([{"commitment":"finalized"}]), json!(7)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
        enumeration(7, 7, 7, json!([])),
    ]);
    assert!(
        Source::new(RpcClient::new(same_slot.clone()))
            .canonical_at(BlockPosition(7))
            .await
            .unwrap_err()
            .retryable
    );
    same_slot.assert_finished();

    let pruned = Scripted::new([
        ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
        ("getFirstAvailableBlock", json!([]), json!(1)),
        enumeration(7, 7, 10, json!([])),
        ("getFirstAvailableBlock", json!([]), json!(8)),
    ]);
    let error = Source::new(RpcClient::new(pruned.clone()))
        .canonical_at(BlockPosition(7))
        .await
        .unwrap_err();
    assert!(!error.retryable);
    assert_eq!(
        error.message,
        "Solana required position was pruned during source acquisition"
    );
    pruned.assert_finished();
}

#[tokio::test]
async fn deadline_and_enumeration_budget_fail_retryably() {
    let rpc = RpcClient::new(Scripted::new([]));
    let mut expired = Attempt {
        rpc: &rpc,
        deadline: Instant::now() - Duration::from_millis(1),
        enumerations: 0,
    };
    let error = expired.enumerate(1, 1, 1).await.unwrap_err();
    assert!(error.retryable);
    assert!(error.message.contains("deadline"));

    let mut exhausted = Attempt {
        rpc: &rpc,
        deadline: Instant::now() + Duration::from_secs(1),
        enumerations: MAX_ENUMERATIONS,
    };
    let error = exhausted.enumerate(1, 1, 1).await.unwrap_err();
    assert!(error.retryable);
    assert!(error.message.contains("64-call"));
}

#[tokio::test]
async fn rejects_broken_produced_height_or_parent_sequence() {
    for second in [
        block(103, 4, 100, 3, 2),
        block(103, 3, 99, 3, 2),
        block(103, 3, 100, 3, 9),
    ] {
        let rpc = Scripted::new([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(103)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(1, 103, 103, json!([1, 100, 103])),
            full(103, second.clone()),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(100, 103, 103, json!([100, 103])),
            full(100, block(100, 2, 1, 2, 1)),
            full(103, second),
        ]);
        let error = Source::new(RpcClient::new(rpc.clone()))
            .blocks(BlockPosition(100), BlockPosition(103), 2)
            .await
            .unwrap_err();
        assert!(error.message.contains("strict canonical sequence"));
        rpc.assert_finished();
    }
}

#[tokio::test]
async fn within_preserves_rpc_error_messages_and_retryability() {
    let rpc = RpcClient::new(Scripted::new([]));
    let attempt = Attempt::new(&rpc);
    for (kind, retryable) in [
        (ErrorKind::InvalidBatch, false),
        (ErrorKind::InvalidBudget, false),
        (ErrorKind::InvalidIdentity, false),
        (ErrorKind::InvalidSecret, false),
        (ErrorKind::Generation, false),
        (ErrorKind::Signing, false),
        (ErrorKind::InvalidRpcConfiguration, false),
        (ErrorKind::RpcTimeout, true),
        (ErrorKind::RpcUnavailable, true),
        (ErrorKind::RpcHttpStatus(400), true),
        (ErrorKind::RpcHttpStatus(503), true),
        (ErrorKind::RpcRemote(-32000), true),
        (ErrorKind::MalformedRpc, true),
        (ErrorKind::ResponseTooLarge, true),
        (ErrorKind::BelowFloor, true),
        (ErrorKind::UnsupportedDestination, false),
        (ErrorKind::Simulation, true),
    ] {
        let message = format!("source RPC fixture {kind:?}");
        let error = attempt
            .within::<()>(std::future::ready(Err(Error::new(kind, message.clone()))))
            .await
            .expect_err("RPC error must remain an error");

        assert_eq!(error.message, message, "{kind:?}");
        assert_eq!(error.retryable, retryable, "{kind:?}");
    }
}

#[tokio::test]
async fn within_preserves_successful_values() {
    let rpc = RpcClient::new(Scripted::new([]));
    let attempt = Attempt::new(&rpc);
    let value = attempt
        .within(std::future::ready(Ok(vec![3, 100, 107])))
        .await
        .expect("successful RPC must retain its value");

    assert_eq!(value, vec![3, 100, 107]);
}

#[tokio::test]
async fn within_times_out_a_pending_future_at_the_existing_deadline() {
    let rpc = RpcClient::new(Scripted::new([]));
    let deadline = Instant::now() - Duration::from_millis(1);
    let attempt = Attempt {
        rpc: &rpc,
        deadline,
        enumerations: 17,
    };
    let value = attempt.within(std::future::ready(Ok(42))).await.unwrap();
    assert_eq!(
        value, 42,
        "a ready future retains timeout_at polling precedence"
    );
    let error = attempt
        .within(std::future::pending::<Result<(), Error>>())
        .await
        .expect_err("pending RPC must respect the elapsed attempt deadline");

    assert_eq!(
        error.message,
        "Solana source exceeded its 30-second deadline"
    );
    assert!(error.retryable);
    assert_eq!(attempt.deadline, deadline);
    assert_eq!(attempt.enumerations, 17);
}

#[test]
fn deadline_guard_is_strict_and_does_not_reset_attempt_state() {
    let rpc = RpcClient::new(Scripted::new([]));
    let deadline = Instant::now();
    let attempt = Attempt {
        rpc: &rpc,
        deadline,
        enumerations: 17,
    };

    assert!(
        attempt
            .ensure_before_deadline(deadline - Duration::from_nanos(1))
            .is_ok()
    );
    for observed in [deadline, deadline + Duration::from_nanos(1)] {
        let error = attempt.ensure_before_deadline(observed).unwrap_err();
        assert_eq!(
            error.message,
            "Solana source exceeded its 30-second deadline"
        );
        assert!(error.retryable);
    }
    assert_eq!(attempt.deadline, deadline);
    assert_eq!(attempt.enumerations, 17);
}

#[tokio::test]
async fn expired_attempt_rejects_before_spending_enumeration_budget_or_rpc() {
    let scripted = Scripted::new([]);
    let rpc = RpcClient::new(scripted.clone());
    let deadline = Instant::now() - Duration::from_nanos(1);
    for enumerations in [0, MAX_ENUMERATIONS] {
        let mut attempt = Attempt {
            rpc: &rpc,
            deadline,
            enumerations,
        };
        let error = attempt.enumerate(1, 1, 1).await.unwrap_err();
        assert_eq!(
            error.message,
            "Solana source exceeded its 30-second deadline"
        );
        assert!(error.retryable);
        assert_eq!(attempt.enumerations, enumerations);
        assert_eq!(attempt.deadline, deadline);
    }
    scripted.assert_finished();
}

#[tokio::test]
async fn selected_anchor_accepts_retention_equality_but_retries_later_pruning() {
    for closing in [7, 8] {
        let rpc = Scripted::new([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(1, 10, 10, json!([7])),
            full(7, block(7, 5, 6, 9, 8)),
            ("getFirstAvailableBlock", json!([]), json!(closing)),
        ]);
        let result = Source::new(RpcClient::new(rpc.clone())).tip().await;
        if closing == 7 {
            assert_eq!(result.unwrap().position, BlockPosition(7));
        } else {
            let error = result.unwrap_err();
            assert!(error.retryable);
            assert_eq!(
                error.message,
                "Solana selected anchor was pruned during source acquisition"
            );
        }
        rpc.assert_finished();
    }
}

#[tokio::test]
async fn canonical_retention_equality_succeeds_and_opening_pruning_precedes_tip_check() {
    let rpc = Scripted::new([
        ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
        ("getFirstAvailableBlock", json!([]), json!(7)),
        enumeration(7, 7, 10, json!([7])),
        full(7, block(7, 5, 6, 9, 8)),
        ("getFirstAvailableBlock", json!([]), json!(7)),
    ]);
    let result = Source::new(RpcClient::new(rpc.clone()))
        .canonical_at(BlockPosition(7))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.position, BlockPosition(7));
    rpc.assert_finished();

    let pruned = Scripted::new([
        ("getSlot", json!([{"commitment":"finalized"}]), json!(6)),
        ("getFirstAvailableBlock", json!([]), json!(8)),
    ]);
    let error = Source::new(RpcClient::new(pruned.clone()))
        .canonical_at(BlockPosition(7))
        .await
        .unwrap_err();
    assert!(!error.retryable);
    assert_eq!(
        error.message,
        "Solana required position was pruned during source acquisition"
    );
    pruned.assert_finished();
}

#[test]
fn produced_height_overflow_precedes_other_connection_errors() {
    let previous =
        Block::parse(7, serde_json::to_vec(&block(7, u64::MAX, 6, 7, 6)).unwrap()).unwrap();
    let current = Block::parse(7, serde_json::to_vec(&block(7, 0, 6, 9, 8)).unwrap()).unwrap();
    let error = current.require_connection(&previous).unwrap_err();
    assert!(error.retryable);
    assert_eq!(error.message, "Solana produced height is exhausted");
}
