//! Checkpoint-consistent read projections over the committed lifecycle.
//!
//! A history page costs four round trips and an output page costs three. Each
//! page executes all of its checkpoint and projection reads in one read-only
//! repeatable-read transaction so they share one snapshot.

use std::collections::HashMap;

use deadpool_postgres::Transaction;
use indexing::{
    AssetId, CanonicalPage, CanonicalStatus, CanonicalTransaction, ChainId, HistoryCursor,
    HistoryPosition, HistoryQuery, IndexError, IndexErrorKind, MovementId, MovementKind,
    NetworkFee, OutputCursor, OutputPage, OutputRequest, TransactionRef, Transactions,
    ValueMovement,
};
use tokio_postgres::{IsolationLevel, Row};

use crate::{Repository, prepare_in, row};

const MAX_PAGE: usize = 1_000;

const HISTORY_PAGE: &str = "\
SELECT height, transaction_id, status, failure_reason, block_position AS position,
       block_hash AS hash, block_parent_position AS parent_position, block_parent AS parent,
       block_timestamp AS timestamp, fee_asset, fee_amount::text AS fee_amount, fee_payer
FROM history WHERE chain = $1 AND network = $2 AND address = $3
  AND (height, transaction_id) > ($4, $5)
ORDER BY height, transaction_id LIMIT $6";

/// Every movement belonging to one page of history, in one statement.
///
/// The page is a contiguous run of `(height, transaction_id)` for a single
/// address, so its movements are exactly the rows between the first and last
/// entry — a range scan on the movement primary key, rather than a query per
/// transaction.
const PAGE_MOVEMENTS: &str = "\
SELECT height, transaction_id, kind, movement_id, asset_chain, asset, amount::text AS amount,
       from_address, to_address
FROM movement WHERE chain = $1 AND network = $2 AND address = $3
  AND (height, transaction_id) >= ($4, $5)
  AND (height, transaction_id) <= ($6, $7)
ORDER BY height, transaction_id, ordinal";

/// Ordered by the output identity itself rather than by a concatenation of it,
/// so the index supplies the order and the cursor is a plain range bound.
const OUTPUT_PAGE: &str = "\
SELECT transaction_id, output_index, address, asset_chain, asset, amount::text AS amount,
       evidence, created_at, coinbase
FROM output WHERE chain = $1 AND network = $2 AND address = $3
  AND (transaction_id, output_index) > ($4, $5)
ORDER BY transaction_id, output_index LIMIT $6";

/// Identifies one transaction within a page, for joining movements to it.
type Position = (i64, String);

impl Repository {
    pub(crate) async fn list_history(
        &self,
        request: HistoryQuery,
    ) -> Result<CanonicalPage, IndexError> {
        self.check_scope(&request.scope)?;
        self.check_address(&request.address)?;
        validate_limit(request.limit)?;
        let mut client = self.client().await?;
        let transaction = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .map_err(crate::store)?;
        let checkpoint = self.checkpoint_in(&transaction).await?;
        if request
            .after
            .as_ref()
            .is_some_and(|cursor| cursor.checkpoint != checkpoint)
        {
            return Err(conflict("history changed during pagination"));
        }

        // One extra row reveals whether another page exists without a count.
        let limit = i64::try_from(request.limit.saturating_add(1))
            .map_err(|_| row::store("page limit exceeds the query range"))?;
        let (after_height, after_transaction) = match &request.after {
            Some(cursor) => (
                row::as_i64(cursor.position.height.0, "cursor height")?,
                cursor.position.transaction.value.clone(),
            ),
            None => (-1, String::new()),
        };
        let statement = prepare_in(&transaction, HISTORY_PAGE).await?;
        let rows = transaction
            .query(
                &statement,
                &[
                    &request.scope.chain.0,
                    &request.scope.network,
                    &request.address.value,
                    &after_height,
                    &after_transaction,
                    &limit,
                ],
            )
            .await
            .map_err(crate::store)?;

        let has_more = rows.len() > request.limit;
        let page = &rows[..rows.len().min(request.limit)];
        let mut movements = self.page_movements(&transaction, &request, page).await?;

        let mut transactions = Vec::with_capacity(page.len());
        for entry in page {
            let height: i64 = entry.try_get("height").map_err(crate::store)?;
            let transaction_id: String = entry.try_get("transaction_id").map_err(crate::store)?;
            let owned = movements
                .remove(&(height, transaction_id.clone()))
                .unwrap_or_default();
            transactions.push(self.canonical(&request, entry, transaction_id, owned)?);
        }

        // The checkpoint must not have moved while the page was assembled, or
        // the page would mix two views of canonical history.
        if self.checkpoint_in(&transaction).await? != checkpoint {
            return Err(conflict("history changed during pagination"));
        }
        let next = has_more
            .then(|| {
                transactions.last().map(|last| HistoryCursor {
                    checkpoint: checkpoint.clone(),
                    position: HistoryPosition {
                        height: last.block().height,
                        transaction: last.transaction_id.clone(),
                    },
                })
            })
            .flatten();
        let page = CanonicalPage {
            checkpoint,
            transactions,
            next,
        };
        transaction.commit().await.map_err(crate::store)?;
        Ok(page)
    }

    /// Reads the movements of every transaction on a page in one query, keyed
    /// by the transaction they belong to.
    async fn page_movements(
        &self,
        transaction: &Transaction<'_>,
        request: &HistoryQuery,
        page: &[Row],
    ) -> Result<HashMap<Position, Vec<ValueMovement>>, IndexError> {
        let (Some(first), Some(last)) = (page.first(), page.last()) else {
            return Ok(HashMap::new());
        };
        let first_height: i64 = first.try_get("height").map_err(crate::store)?;
        let first_transaction: String = first.try_get("transaction_id").map_err(crate::store)?;
        let last_height: i64 = last.try_get("height").map_err(crate::store)?;
        let last_transaction: String = last.try_get("transaction_id").map_err(crate::store)?;

        let statement = prepare_in(transaction, PAGE_MOVEMENTS).await?;
        let rows = transaction
            .query(
                &statement,
                &[
                    &request.scope.chain.0,
                    &request.scope.network,
                    &request.address.value,
                    &first_height,
                    &first_transaction,
                    &last_height,
                    &last_transaction,
                ],
            )
            .await
            .map_err(crate::store)?;

        // Rows arrive ordered by transaction and then ordinal, so appending in
        // arrival order preserves the movement order within a transaction.
        let mut grouped: HashMap<Position, Vec<ValueMovement>> = HashMap::new();
        for entry in &rows {
            let height: i64 = entry.try_get("height").map_err(crate::store)?;
            let transaction_id: String = entry.try_get("transaction_id").map_err(crate::store)?;
            grouped
                .entry((height, transaction_id))
                .or_default()
                .push(self.movement(entry)?);
        }
        Ok(grouped)
    }

    fn canonical(
        &self,
        request: &HistoryQuery,
        entry: &Row,
        transaction_id: String,
        movements: Vec<ValueMovement>,
    ) -> Result<CanonicalTransaction, IndexError> {
        let status: String = entry.try_get("status").map_err(crate::store)?;
        let reason: Option<String> = entry.try_get("failure_reason").map_err(crate::store)?;
        let block = row::block(entry, "")?;
        let fee_asset: Option<String> = entry.try_get("fee_asset").map_err(crate::store)?;
        let fee_amount: Option<String> = entry.try_get("fee_amount").map_err(crate::store)?;
        let fee_payer: Option<String> = entry.try_get("fee_payer").map_err(crate::store)?;

        Ok(CanonicalTransaction {
            scope: request.scope.clone(),
            transaction_id: TransactionRef {
                scope: request.scope.clone(),
                value: transaction_id,
            },
            status: match status.as_str() {
                "included" => CanonicalStatus::Included { block },
                "failed" => CanonicalStatus::Failed { block, reason },
                _ => return Err(row::store("stored transaction status is unknown")),
            },
            movements,
            fee: match (fee_asset, fee_amount) {
                (Some(asset), Some(amount)) => Some(NetworkFee {
                    asset: AssetId {
                        chain: request.scope.chain.clone(),
                        asset,
                    },
                    amount: row::decimal(&amount)?,
                    payer: fee_payer.map(|value| indexing::CanonicalAddress {
                        scope: request.scope.clone(),
                        value,
                    }),
                }),
                _ => None,
            },
        })
    }

    fn movement(&self, entry: &Row) -> Result<ValueMovement, IndexError> {
        let kind: String = entry.try_get("kind").map_err(crate::store)?;
        let amount: String = entry.try_get("amount").map_err(crate::store)?;
        let id = MovementId(entry.try_get("movement_id").map_err(crate::store)?);
        let asset = AssetId {
            chain: ChainId(entry.try_get("asset_chain").map_err(crate::store)?),
            asset: entry.try_get("asset").map_err(crate::store)?,
        };
        let amount = row::decimal(&amount)?;
        let from: Option<String> = entry.try_get("from_address").map_err(crate::store)?;
        let to: Option<String> = entry.try_get("to_address").map_err(crate::store)?;
        let address = |value: Option<String>| {
            value.map(|value| indexing::CanonicalAddress {
                scope: self.scope.clone(),
                value,
            })
        };
        let required = |value: Option<indexing::CanonicalAddress>| {
            value.ok_or_else(|| row::store("stored movement is missing a required endpoint"))
        };
        Ok(match kind.as_str() {
            "transfer" => ValueMovement::Transfer {
                id,
                asset,
                amount,
                from: required(address(from))?,
                to: required(address(to))?,
            },
            "input" => ValueMovement::Input {
                id,
                asset,
                amount,
                owner: address(from),
            },
            "output" => ValueMovement::Output {
                id,
                asset,
                amount,
                owner: address(to),
            },
            "mint" => ValueMovement::Mint {
                id,
                asset,
                amount,
                to: required(address(to))?,
            },
            "burn" => ValueMovement::Burn {
                id,
                asset,
                amount,
                from: required(address(from))?,
            },
            _ => return Err(row::store("stored movement kind is unknown")),
        })
    }

    pub(crate) async fn list_outputs(
        &self,
        request: OutputRequest,
    ) -> Result<OutputPage, IndexError> {
        self.check_scope(&request.scope)?;
        self.check_address(&request.address)?;
        validate_limit(request.limit)?;
        let mut client = self.client().await?;
        let transaction = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .map_err(crate::store)?;
        let checkpoint = self.checkpoint_in(&transaction).await?;
        if request
            .after
            .as_ref()
            .is_some_and(|cursor| cursor.checkpoint != checkpoint)
        {
            return Err(conflict("outputs changed during pagination"));
        }
        let (after_transaction, after_index) = match &request.after {
            Some(cursor) => decode_position(&cursor.position)?,
            None => (String::new(), -1),
        };
        let limit = i64::try_from(request.limit.saturating_add(1))
            .map_err(|_| row::store("page limit exceeds the query range"))?;
        let statement = prepare_in(&transaction, OUTPUT_PAGE).await?;
        let rows = transaction
            .query(
                &statement,
                &[
                    &request.scope.chain.0,
                    &request.scope.network,
                    &request.address.value,
                    &after_transaction,
                    &after_index,
                    &limit,
                ],
            )
            .await
            .map_err(crate::store)?;

        let has_more = rows.len() > request.limit;
        let outputs = rows
            .iter()
            .take(request.limit)
            .map(|entry| row::output(&request.scope, entry))
            .collect::<Result<Vec<_>, _>>()?;
        if self.checkpoint_in(&transaction).await? != checkpoint {
            return Err(conflict("outputs changed during pagination"));
        }
        let next = has_more
            .then(|| {
                outputs.last().map(|last| OutputCursor {
                    checkpoint: checkpoint.clone(),
                    position: encode_position(&last.id.transaction.value, last.id.index),
                })
            })
            .flatten();
        let page = OutputPage {
            checkpoint,
            outputs,
            next,
        };
        transaction.commit().await.map_err(crate::store)?;
        Ok(page)
    }
}

impl Transactions for Repository {
    fn list<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> indexing::BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        Box::pin(async move { self.list_history(request).await })
    }
}

impl indexing::Outputs for Repository {
    fn list<'a>(
        &'a self,
        request: OutputRequest,
    ) -> indexing::BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move { self.list_outputs(request).await })
    }
}

/// An output cursor is the output's identity, not a rendering of it: the page
/// query compares it as a row value so the index can supply the order.
fn encode_position(transaction: &str, index: u32) -> Vec<u8> {
    format!("{transaction}\u{0}{index}").into_bytes()
}

fn decode_position(position: &[u8]) -> Result<(String, i32), IndexError> {
    let text =
        std::str::from_utf8(position).map_err(|_| row::store("output cursor is not valid"))?;
    let (transaction, index) = text
        .rsplit_once('\u{0}')
        .ok_or_else(|| row::store("output cursor is not valid"))?;
    let index = index
        .parse::<i32>()
        .map_err(|_| row::store("output cursor is not valid"))?;
    Ok((transaction.to_owned(), index))
}

fn conflict(message: &'static str) -> IndexError {
    IndexError::new(IndexErrorKind::Conflict, message, true)
}

fn validate_limit(limit: usize) -> Result<(), IndexError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(IndexError::new(
            IndexErrorKind::InvalidRequest,
            "page limit must be between one and one thousand",
            false,
        ));
    }
    Ok(())
}

/// Unused today but kept so the movement mapper can name every variant.
#[allow(dead_code)]
const fn kinds() -> [MovementKind; 5] {
    [
        MovementKind::Transfer,
        MovementKind::Input,
        MovementKind::Output,
        MovementKind::Mint,
        MovementKind::Burn,
    ]
}
