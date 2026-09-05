//! Row decoding shared by the read and write paths.

use indexing::{
    AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, CanonicalAddress,
    ChainId, Decimal, IndexError, IndexErrorKind, IndexScope, IndexedOutput, OutputId,
    TransactionRef,
};
use tokio_postgres::Row;

/// Amounts are bound as canonical base-10 text and cast to `numeric` in SQL.
///
/// The SDK's `Decimal` keeps its scale, so `0.50` and `0.5` are different
/// values; reading back through `::text` preserves that, and the round-trip is
/// verified rather than assumed.
pub(crate) fn decimal(encoded: &str) -> Result<Decimal, IndexError> {
    let value = encoded
        .parse::<Decimal>()
        .map_err(|_| store("stored amount is not a valid decimal"))?;
    if value.to_string() != encoded {
        return Err(store("stored amount is not canonical"));
    }
    Ok(value)
}

pub(crate) fn height(value: i64) -> Result<BlockHeight, IndexError> {
    Ok(BlockHeight(
        u64::try_from(value).map_err(|_| store("stored height is negative"))?,
    ))
}

pub(crate) fn position(value: i64) -> Result<BlockPosition, IndexError> {
    Ok(BlockPosition(
        u64::try_from(value).map_err(|_| store("stored block position is negative"))?,
    ))
}

// design-lint: allow unclassified-free-function -- checked PostgreSQL BIGINT boundary conversion shared by multiple numeric fields; preserves caller-specific range errors
pub(crate) fn as_i64(value: u64, what: &'static str) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| {
        IndexError::new(
            IndexErrorKind::InvalidRequest,
            format!("{what} exceeds the storage range"),
            false,
        )
    })
}

// design-lint: allow unclassified-free-function -- PostgreSQL row-to-block boundary conversion owns column aliases and stored-value validation without adding database concerns to BlockRef
pub(crate) fn block(row: &Row, prefix: &str) -> Result<BlockRef, IndexError> {
    let raw_height: i64 = get(row, &format!("{prefix}height"))?;
    let raw_position: i64 = get(row, &format!("{prefix}position"))?;
    let parent_position: Option<i64> = get(row, &format!("{prefix}parent_position"))?;
    let parent_hash: Option<Vec<u8>> = get(row, &format!("{prefix}parent"))?;
    let parent = match (parent_position, parent_hash) {
        (Some(position), Some(hash)) => Some(BlockParent {
            position: self::position(position)?,
            hash: BlockHash(hash),
        }),
        (None, None) => None,
        _ => return Err(store("stored block parent is incomplete")),
    };
    Ok(BlockRef {
        position: position(raw_position)?,
        height: height(raw_height)?,
        hash: BlockHash(get(row, &format!("{prefix}hash"))?),
        parent,
        timestamp: get::<Option<i64>>(row, &format!("{prefix}timestamp"))?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| store("stored block timestamp is negative"))?,
    })
}

pub(crate) fn output(scope: &IndexScope, row: &Row) -> Result<IndexedOutput, IndexError> {
    let index: i32 = get(row, "output_index")?;
    let created: i64 = get(row, "created_at")?;
    let amount: String = get(row, "amount")?;
    Ok(IndexedOutput {
        id: OutputId {
            transaction: TransactionRef {
                scope: scope.clone(),
                value: get(row, "transaction_id")?,
            },
            index: u32::try_from(index).map_err(|_| store("stored output index is negative"))?,
        },
        address: CanonicalAddress {
            scope: scope.clone(),
            value: get(row, "address")?,
        },
        asset: AssetId {
            chain: ChainId(get(row, "asset_chain")?),
            asset: get(row, "asset")?,
        },
        amount: decimal(&amount)?,
        evidence: get(row, "evidence")?,
        created_at: height(created)?,
        coinbase: get(row, "coinbase")?,
    })
}

fn get<'a, T: tokio_postgres::types::FromSql<'a>>(
    row: &'a Row,
    column: &str,
) -> Result<T, IndexError> {
    row.try_get(column)
        .map_err(|error| IndexError::new(IndexErrorKind::Store, error.to_string(), false))
}

pub(crate) fn store(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_i64_preserves_values_at_the_storage_boundaries() {
        assert_eq!(as_i64(0, "block height"), Ok(0));
        assert_eq!(as_i64(i64::MAX as u64, "block position"), Ok(i64::MAX));
    }

    #[test]
    fn as_i64_rejects_overflow_with_the_callers_context() {
        for (context, message) in [
            ("block height", "block height exceeds the storage range"),
            (
                "parent block position",
                "parent block position exceeds the storage range",
            ),
            ("start position", "start position exceeds the storage range"),
        ] {
            for value in [i64::MAX as u64 + 1, u64::MAX] {
                let error = as_i64(value, context).expect_err("overflow must be rejected");
                assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
                assert_eq!(error.message, message);
                assert!(!error.retryable);
            }
        }
    }
}
