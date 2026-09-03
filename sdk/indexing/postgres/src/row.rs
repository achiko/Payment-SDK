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

pub(crate) fn as_i64(value: u64, what: &'static str) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| {
        IndexError::new(
            IndexErrorKind::InvalidRequest,
            format!("{what} exceeds the storage range"),
            false,
        )
    })
}

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
